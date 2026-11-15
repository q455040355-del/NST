//! AI token 速度：多来源聚合（OpenCode / DSH / WorkBuddy）。
//!
//! 和 C# 版 `AiSpeedMeter` 同口径：
//! - 今日累计 = 今天所有响应（assistant 消息）的 output 之和；
//! - 实时速度 = 最近 3 秒内完成的响应按重叠比例摊进去，再除以 3；
//! - 正在生成时，用最近几次实测平均速度补一个在飞估算。
//!
//! 所有文件来源都按「路径 + 修改时间」缓存，只有变过的文件才重新解析。

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;


const WINDOW_MS: i64 = 3000;

#[derive(Debug, Clone, Copy, Default)]
pub struct AiSnapshot {
    pub speed: f64,
    pub today_output: i64,
    pub generating: bool,
    pub sources: u32,
}

#[derive(Debug, Clone, Copy)]
struct Response {
    start: i64,
    end: i64,
    output: i64,
}

/// 每个文件解析出来的响应（key = 来源内唯一 id）。
type FileRows = Vec<(String, Response)>;

// ---------------------------------------------------------------- 引擎

pub struct AiEngine {
    responses: HashMap<String, Response>,
    recent: Vec<(i64, f64)>,
    in_flight: i64,
    today_start: i64,
    today_output: i64,
    opencode: OpenCodeSource,
    dsh: FileSource,
    workbuddy: FileSource,
    workbuddy_ai: FileSource,
    last_poll: i64,
    last_error: Option<String>,
}

impl AiEngine {
    pub fn new() -> Self {
        let home = home_dir();
        Self {
            responses: HashMap::new(),
            recent: Vec::new(),
            in_flight: 0,
            today_start: 0,
            today_output: 0,
            opencode: OpenCodeSource::new(),
            dsh: FileSource::new(
                home.join(".dsh").join("sessions"),
                "zstd",
                FileKind::Dsh,
                "dsh",
            ),
            workbuddy: FileSource::new(
                home.join(".workbuddy").join("projects"),
                "jsonl",
                FileKind::WorkBuddy,
                "workbuddy",
            ),
            workbuddy_ai: FileSource::new(
                home.join(".workbuddy-ai").join("projects"),
                "jsonl",
                FileKind::WorkBuddy,
                "workbuddyAi",
            ),
            last_poll: 0,
            last_error: None,
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 采集一次（阻塞，调用方放在后台线程）。内部 2 秒节流。
    pub fn poll(&mut self, now_ms: i64) {
        let midnight = local_midnight_ms(now_ms);
        if midnight != self.today_start {
            self.today_start = midnight;
            self.responses.clear();
            self.recent.clear();
            self.opencode.reset();
            self.dsh.reset();
            self.workbuddy.reset();
            self.workbuddy_ai.reset();
        }

        if now_ms - self.last_poll < 2000 && !self.responses.is_empty() {
            return;
        }
        self.last_poll = now_ms;

        // 2 分钟以前的响应没必要留着（速度只看 3 秒窗口）
        let cutoff = now_ms - 120_000;
        self.responses.retain(|_, response| response.end >= cutoff || response.end <= 0);

        self.responses.clear();
        self.in_flight = 0;

        let t0 = std::time::Instant::now();
        let opencode_rows = self.opencode.poll(now_ms, midnight);
        let opencode_ms = t0.elapsed().as_millis();
        for (key, response) in opencode_rows {
            self.merge(key, response);
        }

        let t1 = std::time::Instant::now();
        let dsh_rows = self.dsh.poll(now_ms, midnight);
        let workbuddy_rows = self.workbuddy.poll(now_ms, midnight);
        let workbuddy_ai_rows = self.workbuddy_ai.poll(now_ms, midnight);
        let files_ms = t1.elapsed().as_millis();


        for result in [dsh_rows, workbuddy_rows, workbuddy_ai_rows] {
            match result {
                Ok(rows) => {
                    for (key, response) in rows {
                        self.merge(key, response);
                    }
                }
                Err(error) => self.last_error = Some(error),
            }
        }

        self.recompute(now_ms);
    }

    fn merge(&mut self, key: String, response: Response) {
        self.responses.insert(key, response);
    }

    fn recompute(&mut self, now_ms: i64) {
        let mut today = 0_i64;
        let mut recent = Vec::new();
        self.in_flight = 0;

        for response in self.responses.values() {
            if response.end >= self.today_start {
                today += response.output;
            }

            // 只有“刚刚开始”的未完成响应才算在飞；库里可能残留很久以前的残缺行
            if response.end <= 0 {
                if response.start > 0 && now_ms - response.start < 15 * 60 * 1000 {
                    self.in_flight = self.in_flight.max(response.start);
                }
                continue;
            }

            if response.end > response.start && response.end > 0 && response.output > 0 {
                let duration = (response.end - response.start) as f64;
                if duration >= 200.0 {
                    recent.push((
                        response.end,
                        response.output as f64 * 1000.0 / duration,
                    ));
                }
            }
        }

        recent.sort_by_key(|(end, _)| *end);
        if recent.len() > 8 {
            recent.drain(..recent.len() - 8);
        }

        self.today_output = today;
        self.recent = recent;
        let _ = now_ms;
    }

    pub fn snapshot(&self, now_ms: i64) -> AiSnapshot {
        let window_start = now_ms - WINDOW_MS;
        let mut measured = 0.0_f64;

        for response in self.responses.values() {
            if response.end <= 0 || response.end > now_ms {
                continue;
            }

            if response.end - response.start > 15 * 60 * 1000 {
                continue;
            }

            let start = if response.end - response.start < 500 {
                response.end - 500
            } else {
                response.start
            };

            let overlap = now_ms.min(response.end) - window_start.max(start);
            if overlap <= 0 {
                continue;
            }

            measured += response.output as f64 * overlap as f64 / (response.end - start) as f64;
        }

        let mut live = measured / (WINDOW_MS as f64 / 1000.0);

        if self.in_flight > 0 && now_ms - self.in_flight < 15 * 60 * 1000 && live <= 0.0 {
            let mut total = 0.0;
            let mut count = 0;
            for (end, speed) in self.recent.iter().rev() {
                if now_ms - end > 10 * 60 * 1000 {
                    continue;
                }

                total += speed;
                count += 1;
                if count >= 3 {
                    break;
                }
            }

            if count > 0 {
                live = total / count as f64;
            }
        }

        AiSnapshot {
            speed: live.max(0.0),
            today_output: self.today_output.max(0),
            generating: self.in_flight > 0,
            sources: self.responses.len() as u32,
        }
    }
}

impl Default for AiEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- OpenCode（SQLite）

struct OpenCodeSource {
    connection: Option<Connection>,
    rows: Vec<(String, Response)>,
    initialized: bool,
    max_created: i64,
    /// opencode.db-wal 的最后修改时间：没变就不用重查（全表扫很贵）
    wal_stamp: u64,
}

impl OpenCodeSource {
    fn new() -> Self {
        Self {
            connection: None,
            rows: Vec::new(),
            initialized: false,
            max_created: 0,
            wal_stamp: 0,
        }
    }

    fn reset(&mut self) {
        self.rows.clear();
        self.initialized = false;
    }



    fn poll(&mut self, now_ms: i64, midnight: i64) -> Vec<(String, Response)> {
        // WAL 没变说明没有新写入，直接复用上次结果，避免每次全表扫
        let db = database_path();
        let stamp = db
            .as_ref()
            .and_then(|path| {
                let wal = path.with_extension("db-wal");
                std::fs::metadata(wal)
                    .or_else(|_| std::fs::metadata(path))
                    .and_then(|meta| meta.modified())
                    .ok()
            })
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);

        if self.initialized && stamp == self.wal_stamp {
            return self.rows.clone();
        }
        self.wal_stamp = stamp;

        if self.connection.is_none() {
            let Some(path) = database_path() else {
                return std::mem::take(&mut self.rows);
            };

            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            match Connection::open_with_flags(path, flags) {
                Ok(connection) => {
                    let _ = connection.busy_timeout(std::time::Duration::from_millis(300));
                    self.connection = Some(connection);
                }
                Err(_) => return std::mem::take(&mut self.rows),
            }
        }

        let Some(connection) = self.connection.as_ref() else {
            return std::mem::take(&mut self.rows);
        };

        // 只按 time_created 增量查（走索引）；还在生成的行用 id 精确补查，
        // 避免每次 OR time_updated 造成全表扫（实测 30~50% CPU）
        let from = if self.initialized {
            self.max_created
        } else {
            midnight
        };

        let mut in_flight = String::new();
        for (key, response) in &self.rows {
            if response.end <= 0 {
                if !in_flight.is_empty() {
                    in_flight.push(',');
                }

                let id = key.strip_prefix("oc:").unwrap_or(key);
                in_flight.push('\'');
                in_flight.push_str(&id.replace('\'', "''"));
                in_flight.push('\'');
            }
        }

        let sql = if in_flight.is_empty() {
            "SELECT id, \
             IFNULL(json_extract(data,'$.time.created'),0), \
             IFNULL(json_extract(data,'$.time.streamed'),0), \
             IFNULL(json_extract(data,'$.tokens.output'),0) + IFNULL(json_extract(data,'$.tokens.reasoning'),0) \
             FROM session_message \
             WHERE type = 'assistant' AND time_created > ?1"
                .to_string()
        } else {
            format!(
                "SELECT id, \
                 IFNULL(json_extract(data,'$.time.created'),0), \
                 IFNULL(json_extract(data,'$.time.streamed'),0), \
                 IFNULL(json_extract(data,'$.tokens.output'),0) + IFNULL(json_extract(data,'$.tokens.reasoning'),0) \
                 FROM session_message \
                 WHERE type = 'assistant' AND (time_created > ?1 OR id IN ({in_flight}))"
            )
        };

        let Ok(mut statement) = connection.prepare(&sql) else {
            return std::mem::take(&mut self.rows);
        };

        let Ok(rows) = statement.query_map([from], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Response {
                    start: row.get(1)?,
                    end: row.get(2)?,
                    output: row.get(3)?,
                },
            ))
        }) else {
            return std::mem::take(&mut self.rows);
        };

        let mut seen: HashMap<String, Response> =
            self.rows.drain(..).map(|(key, value)| (key, value)).collect();
        for (key, response) in rows.flatten() {
            if response.start > self.max_created {
                self.max_created = response.start;
            }
            seen.insert(key, response);
        }

        self.initialized = true;
        self.rows = seen.into_iter().map(|(key, value)| (format!("oc:{key}"), value)).collect();
        self.rows.clone()
    }
}

// ---------------------------------------------------------------- 文件来源（DSH / WorkBuddy）

#[derive(Clone, Copy)]
enum FileKind {
    Dsh,
    WorkBuddy,
}

struct FileSource {
    root: PathBuf,
    suffix: &'static str,
    kind: FileKind,
    prefix: &'static str,
    cache: HashMap<PathBuf, (u64, FileRows)>,
    files: Vec<PathBuf>,
    /// 最近 10 分钟改过的文件：只有这些才需要每轮查 mtime（其余文件不会追加）
    active: Vec<PathBuf>,
    last_scan: i64,
}

impl FileSource {
    fn new(root: PathBuf, suffix: &'static str, kind: FileKind, prefix: &'static str) -> Self {
        Self {
            root,
            suffix,
            kind,
            prefix,
            cache: HashMap::new(),
            files: Vec::new(),
            active: Vec::new(),
            last_scan: 0,
        }
    }

    fn reset(&mut self) {
        self.cache.clear();
    }

    fn poll(&mut self, now_ms: i64, midnight: i64) -> Result<FileRows, String> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        // 目录树很大（几百个目录 + 几千个文件），全量扫描限流到 30 秒
        if now_ms - self.last_scan > 30_000 || self.files.is_empty() {
            self.last_scan = now_ms;
            self.files = collect_files(&self.root, self.suffix, midnight - 3_600_000);
            self.active.retain(|path| path.exists());
            self.cache.retain(|path, _| path.exists());

            let now_secs = (now_ms / 1000) as u64;
            self.active = self
                .files
                .iter()
                .filter(|path| {
                    modified_secs(path)
                        .map(|stamp| now_secs.saturating_sub(stamp) < 600)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            // 新出现的文件当场解析一次，之后就靠 active 的 mtime 检查
            for path in self.files.clone() {
                if !self.cache.contains_key(&path) {
                    let modified = modified_secs(&path).unwrap_or(0);
                    let parsed = match self.kind {
                        FileKind::Dsh => parse_dsh(&path).unwrap_or_default(),
                        FileKind::WorkBuddy => parse_workbuddy(&path).unwrap_or_default(),
                    };
                    self.cache.insert(path, (modified, parsed));
                }
            }
        }

        let mut rows = Vec::new();
        let mut stale = Vec::new();

        // 只对活跃文件查 mtime：几百次 stat 在 OneDrive 目录上要 100ms+
        for path in self.active.clone() {
            let modified = modified_secs(&path).unwrap_or(0);
            match self.cache.get(&path) {
                Some((cached, _)) if *cached == modified => {}
                _ => stale.push((path, modified)),
            }
        }

        for (path, modified) in stale {
            let parsed = match self.kind {
                FileKind::Dsh => parse_dsh(&path).map_err(|error| error.to_string())?,
                FileKind::WorkBuddy => parse_workbuddy(&path).map_err(|error| error.to_string())?,
            };
            self.cache.insert(path, (modified, parsed));
        }

        // 目录里已删掉的文件只在扫描时清理（这里每轮 stat 500+ 个文件太贵）
        for rows_of_file in self.cache.values() {
            rows.extend(rows_of_file.1.iter().cloned());
        }

        Ok(rows
            .into_iter()
            .map(|(key, response)| (format!("{}:{key}", self.prefix), response))
            .collect())
    }
}

fn collect_files(root: &Path, suffix: &str, modified_after_ms: i64) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }

            if !path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case(suffix))
                .unwrap_or(false)
            {
                continue;
            }

            if modified_secs(&path).unwrap_or(0) < (modified_after_ms / 1000) as u64 {
                continue;
            }

            result.push(path);
        }
    }

    result
}

fn modified_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn parse_dsh(path: &Path) -> std::io::Result<FileRows> {
    let file = File::open(path)?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let mut text = Vec::new();
    decoder.read_to_end(&mut text)?;

    let text = String::from_utf8_lossy(&text);
    let mut rows = Vec::new();
    let mut pending_start = 0_i64;

    for line in text.lines() {
        if line.contains("\"step/start\"") {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                pending_start = value["time"].as_i64().unwrap_or(0);
            }
            continue;
        }

        if !line.contains("\"assistant/message\"") {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        let usage = &value["data"]["usage"];
        let output = usage["outputTokens"].as_i64().unwrap_or(0);
        if output <= 0 {
            continue;
        }

        let time = value["time"].as_i64().unwrap_or(0);
        let seq = value["seq"].as_i64().unwrap_or(0);
        let key = format!("dsh:{}", seq);

        let start = if pending_start > 0 && time > pending_start && time - pending_start < 600_000 {
            pending_start
        } else {
            time
        };

        rows.push((
            key,
            Response {
                start,
                end: time,
                output,
            },
        ));

        pending_start = 0;
    }

    Ok(rows)
}

fn parse_workbuddy(path: &Path) -> std::io::Result<FileRows> {
    let text = std::fs::read_to_string(path)?;
    let mut rows: HashMap<String, (Response, i64)> = HashMap::new();
    let mut last_ts = 0_i64;

    for line in text.lines() {
        if !line.contains("\"rawUsage\"") || line.contains("\"isPartialAborted\":true") {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        let provider = &value["providerData"];
        let Some(message_id) = provider["messageId"].as_str() else {
            continue;
        };

        let raw = &provider["rawUsage"];
        let output = raw["completion_tokens"].as_i64().unwrap_or(0);
        let input = raw["prompt_tokens"].as_i64().unwrap_or(0);
        if output <= 0 && input <= 0 {
            continue;
        }

        let timestamp = value["timestamp"].as_i64().unwrap_or(0);
        let entry = rows.entry(message_id.to_string()).or_insert_with(|| {
            (
                Response {
                    start: timestamp,
                    end: timestamp,
                    output: 0,
                },
                last_ts,
            )
        });

        if output > entry.0.output {
            entry.0.output = output;
        }

        if timestamp > entry.0.end {
            if last_ts > 0 && timestamp - last_ts < 180_000 && timestamp > last_ts {
                entry.0.start = last_ts;
            }
            entry.0.end = timestamp;
        }

        if timestamp > last_ts {
            last_ts = timestamp;
        }
    }

    Ok(rows
        .into_iter()
        .map(|(key, (response, _))| (format!("wb:{key}"), response))
        .collect())
}


// ---------------------------------------------------------------- 工具

/// OpenCode 本地库路径（不存在返回 None）。
pub(crate) fn database_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().join(".local").join("share").into())?;
    let path = base.join("opencode").join("opencode.db");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// 当前时间的毫秒（与 SQLite 里的时间戳同一基准）。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 本地当天 0 点对应的时间戳（毫秒）。
pub fn local_midnight_ms(now_ms: i64) -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut local) };

    let seconds_today = (local.wHour as i64) * 3600
        + (local.wMinute as i64) * 60
        + local.wSecond as i64;
    now_ms - (seconds_today * 1000 + local.wMilliseconds as i64)
}

/// token 数压成 K/M/B。
/// 本地时区相对 UTC 的偏移（毫秒，东八区为 +28800000）。
pub fn local_offset_ms() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};

    unsafe {
        let mut info: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let state = GetTimeZoneInformation(&mut info);
        let mut bias = info.Bias as i64;
        // 2 = TIME_ZONE_ID_DAYLIGHT
        if state == 2 {
            bias += info.DaylightBias as i64;
        }
        -bias * 60_000
    }
}

/// 「天序号」：按本地日期切分，和 today() 保持同一坐标系。
pub fn day_index(ms: i64) -> i64 {
    (ms + local_offset_ms()).div_euclid(86_400_000)
}
pub fn format_tokens(value: i64) -> String {
    let value = value.max(0);
    if value < 1_000 {
        format!("{value}")
    } else if value < 1_000_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else if value < 1_000_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    }
}

/// 速度文本：`0.0 t/s` / `57 t/s` / `1.2K t/s`。
pub fn format_speed(speed: f64) -> String {
    if !speed.is_finite() || speed < 10.0 {
        format!("{:.1} t/s", speed.max(0.0))
    } else if speed < 1000.0 {
        format!("{:.0} t/s", speed)
    } else if speed < 1_000_000.0 {
        format!("{:.1}K t/s", speed / 1000.0)
    } else {
        format!("{:.1}M t/s", speed / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_units() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(format_tokens(21_000_000_000), "21.0B");
    }

    #[test]
    fn speed_units() {
        assert_eq!(format_speed(0.0), "0.0 t/s");
        assert_eq!(format_speed(4.25), "4.2 t/s");
        assert_eq!(format_speed(57.4), "57 t/s");
        assert_eq!(format_speed(1234.0), "1.2K t/s");
    }

    #[test]
    fn midnight_is_before_now() {
        let now = now_ms();
        let midnight = local_midnight_ms(now);
        assert!(midnight <= now);
        assert!(now - midnight < 24 * 3600 * 1000);
    }
}
