//! AI token 速度：多来源聚合（OpenCode / DSH / WorkBuddy）。
//!
//! 和 C# 版 `AiSpeedMeter` 同口径：
//! - 今日累计 = 今天所有响应（assistant 消息）的 input + output 之和；
//! - 实时速度 = 最近 3 秒内完成的响应按重叠比例摊进去，再除以 3；
//! - 正在生成时，用最近几次实测平均速度补一个在飞估算。
//!
//! 所有文件来源都按「路径 + 修改时间」缓存，只有变过的文件才重新解析。

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;


const WINDOW_MS: i64 = 3000;

#[derive(Debug, Clone, Copy, Default)]
pub struct AiSnapshot {
    pub speed: f64,
    /// 今日累计：输入 + 输出（和历史窗口同口径）
    pub today_tokens: i64,
    pub generating: bool,
    pub sources: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Response {
    start: i64,
    end: i64,
    output: i64,
    /// 输入 token（含缓存命中/写入，口径和历史窗口一致）
    input: i64,
}

/// 一个文件解析出来的响应（key 已带来源前缀）
type FileRows = HashMap<String, Response>;

/// 一轮采集的增量：只回传变化的行，引擎按 key 覆盖/删除
#[derive(Default)]
struct Delta {
    changed: Vec<(String, Response)>,
    removed: Vec<String>,
}

/// 采集哪些来源：设置里关掉的来源连文件/数据库都不打开
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sources {
    pub opencode: bool,
    pub dsh: bool,
    pub workbuddy: bool,
    pub workbuddy_ai: bool,
    pub claude: bool,
    pub codex: bool,
}

impl Sources {
    pub fn all() -> Self {
        Self {
            opencode: true,
            dsh: true,
            workbuddy: true,
            workbuddy_ai: true,
            claude: true,
            codex: true,
        }
    }

    pub fn any(&self) -> bool {
        self.opencode
            || self.dsh
            || self.workbuddy
            || self.workbuddy_ai
            || self.claude
            || self.codex
    }
}

// ---------------------------------------------------------------- 引擎

pub struct AiEngine {
    responses: HashMap<String, Response>,
    recent: Vec<(i64, f64)>,
    in_flight: i64,
    today_start: i64,
    today_tokens: i64,
    sources: Sources,
    opencode: OpenCodeSource,
    dsh: FileSource,
    workbuddy: FileSource,
    workbuddy_ai: FileSource,
    claude: FileSource,
    codex: FileSource,
    last_poll: i64,
    /// 最近是否在生成：决定采集间隔（在飞 700ms，空闲 1.5s）
    busy: bool,
    last_error: Option<String>,
}

impl AiEngine {
    pub fn new(sources: Sources) -> Self {
        Self::with_home(home_dir(), sources)
    }

    /// 指定 home 目录（测试用；正常调用走 `new`）
    pub fn with_home(home: PathBuf, sources: Sources) -> Self {
        Self {
            responses: HashMap::new(),
            recent: Vec::new(),
            in_flight: 0,
            today_start: 0,
            today_tokens: 0,
            sources,
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
            claude: FileSource::new(
                home.join(".claude").join("projects"),
                "jsonl",
                FileKind::Claude,
                "claude",
            ),
            codex: FileSource::new(
                home.join(".codex").join("sessions"),
                "jsonl",
                FileKind::Codex,
                "codex",
            ),
            last_poll: 0,
            busy: false,
            last_error: None,
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 采集一次（阻塞，调用方放在后台线程）。内部按忙/闲节流。
    pub fn poll(&mut self, now_ms: i64) {
        if !self.sources.any() {
            return;
        }
        let midnight = local_midnight_ms(now_ms);
        if midnight != self.today_start {
            self.today_start = midnight;
            self.responses.clear();
            self.recent.clear();
            self.opencode.reset();
            self.dsh.reset();
            self.workbuddy.reset();
            self.workbuddy_ai.reset();
            self.claude.reset();
            self.codex.reset();
        }

        // 在飞时 700ms 一轮（速度要跟手），空闲 1.5s（少唤醒几次 CPU）
        let throttle = if self.busy { 700 } else { 1500 };
        if self.last_poll != 0 && now_ms - self.last_poll < throttle {
            return;
        }
        self.last_poll = now_ms;

        // 昨天的响应不会再影响今日合计，直接丢掉（引擎常驻，不清理会一直涨）
        let midnight = self.today_start;
        self.responses
            .retain(|_, response| response.end <= 0 || response.end >= midnight);

        let mut file_delta = Delta::default();
        if self.sources.opencode {
            // OpenCode 每次查回来的是「今天以来」的全量：直接覆盖，不用增量
            for (key, response) in self.opencode.poll(midnight) {
                self.responses.insert(key, response);
            }
        }

        if self.sources.dsh {
            match self.dsh.poll(now_ms, midnight) {
                Ok(delta) => merge_delta(&mut file_delta, delta),
                Err(error) => self.last_error = Some(error),
            }
        }
        if self.sources.workbuddy {
            match self.workbuddy.poll(now_ms, midnight) {
                Ok(delta) => merge_delta(&mut file_delta, delta),
                Err(error) => self.last_error = Some(error),
            }
        }
        if self.sources.workbuddy_ai {
            match self.workbuddy_ai.poll(now_ms, midnight) {
                Ok(delta) => merge_delta(&mut file_delta, delta),
                Err(error) => self.last_error = Some(error),
            }
        }
        if self.sources.claude {
            match self.claude.poll(now_ms, midnight) {
                Ok(delta) => merge_delta(&mut file_delta, delta),
                Err(error) => self.last_error = Some(error),
            }
        }
        if self.sources.codex {
            match self.codex.poll(now_ms, midnight) {
                Ok(delta) => merge_delta(&mut file_delta, delta),
                Err(error) => self.last_error = Some(error),
            }
        }

        self.apply(file_delta);
        self.recompute(now_ms);
    }

    fn apply(&mut self, delta: Delta) {
        // 先删后插：文件重写时同一批 key 既在 removed 里又在 changed 里
        for key in delta.removed {
            self.responses.remove(&key);
        }
        for (key, response) in delta.changed {
            self.responses.insert(key, response);
        }
    }

    fn recompute(&mut self, now_ms: i64) {
        let mut today = 0_i64;
        let mut recent = Vec::new();
        self.in_flight = 0;

        for response in self.responses.values() {
            if response.end >= self.today_start {
                today += response.output + response.input;
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

        self.today_tokens = today;
        self.busy = self.in_flight > 0
            || recent
                .last()
                .map(|(end, _)| (now_ms - *end) < 15_000)
                .unwrap_or(false);
        self.recent = recent;
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
            today_tokens: self.today_tokens.max(0),
            generating: self.in_flight > 0,
            sources: self.responses.len() as u32,
        }
    }
}

/// 把一轮增量并进另一轮
fn merge_delta(into: &mut Delta, from: Delta) {
    into.changed.extend(from.changed);
    into.removed.extend(from.removed);
}

impl Default for AiEngine {
    fn default() -> Self {
        Self::new(Sources::all())
    }
}

// ---------------------------------------------------------------- OpenCode（SQLite）

struct OpenCodeSource {
    connection: Option<Connection>,
    /// 今天以来的响应：key 是数据库里的 id（不带来源前缀）
    rows: HashMap<String, Response>,
    initialized: bool,
    max_created: i64,
    /// opencode.db-wal 的最后修改时间：没变就不用重查（全表扫很贵）
    wal_stamp: u64,
}

impl OpenCodeSource {
    fn new() -> Self {
        Self {
            connection: None,
            rows: HashMap::new(),
            initialized: false,
            max_created: 0,
            wal_stamp: 0,
        }
    }

    fn reset(&mut self) {
        self.rows.clear();
        self.initialized = false;
    }



    fn poll(&mut self, midnight: i64) -> Vec<(String, Response)> {
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
            // 没新写入就什么都不用推：引擎里已经有这些行了
            return Vec::new();
        }
        self.wal_stamp = stamp;

        if self.connection.is_none() {
            let Some(path) = database_path() else {
                return Vec::new();
            };

            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            match Connection::open_with_flags(path, flags) {
                Ok(connection) => {
                    let _ = connection.busy_timeout(std::time::Duration::from_millis(300));
                    self.connection = Some(connection);
                }
                Err(_) => return Vec::new(),
            }
        }

        let Some(connection) = self.connection.as_ref() else {
            return Vec::new();
        };

        // 只按 time_created 增量查（走索引）；还在生成的行用 id 精确补查，
        // 避免每次 OR time_updated 造成全表扫（实测 30~50% CPU）
        let from = if self.initialized {
            self.max_created
        } else {
            midnight
        };

        let mut in_flight = String::new();
        for (id, response) in &self.rows {
            if response.end <= 0 {
                if !in_flight.is_empty() {
                    in_flight.push(',');
                }

                in_flight.push('\'');
                in_flight.push_str(&id.replace('\'', "''"));
                in_flight.push('\'');
            }
        }

        let sql = if in_flight.is_empty() {
            "SELECT id, \
             IFNULL(json_extract(data,'$.time.created'),0), \
             IFNULL(json_extract(data,'$.time.streamed'),0), \
             IFNULL(json_extract(data,'$.tokens.output'),0) + IFNULL(json_extract(data,'$.tokens.reasoning'),0), \
             IFNULL(json_extract(data,'$.tokens.input'),0) + IFNULL(json_extract(data,'$.tokens.cache.read'),0) + IFNULL(json_extract(data,'$.tokens.cache.write'),0) \
             FROM session_message \
             WHERE type = 'assistant' AND time_created > ?1"
                .to_string()
        } else {
            format!(
                "SELECT id, \
                 IFNULL(json_extract(data,'$.time.created'),0), \
                 IFNULL(json_extract(data,'$.time.streamed'),0), \
                 IFNULL(json_extract(data,'$.tokens.output'),0) + IFNULL(json_extract(data,'$.tokens.reasoning'),0), \
                 IFNULL(json_extract(data,'$.tokens.input'),0) + IFNULL(json_extract(data,'$.tokens.cache.read'),0) + IFNULL(json_extract(data,'$.tokens.cache.write'),0) \
                 FROM session_message \
                 WHERE type = 'assistant' AND (time_created > ?1 OR id IN ({in_flight}))"
            )
        };

        let Ok(mut statement) = connection.prepare(&sql) else {
            return Vec::new();
        };

        let Ok(rows) = statement.query_map([from], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Response {
                    start: row.get(1)?,
                    end: row.get(2)?,
                    output: row.get(3)?,
                    input: row.get(4)?,
                },
            ))
        }) else {
            return Vec::new();
        };

        // rows 里存的是「没加前缀的 id」：以前在这里又 format 一次前缀，
        // 每轮 WAL 变化都会生成一批 `oc:oc:...` 的新 key，今日合计会越滚越大
        let mut seen: HashMap<String, Response> = std::mem::take(&mut self.rows);
        let mut changed = Vec::new();
        for (id, response) in rows.flatten() {
            if response.start > self.max_created {
                self.max_created = response.start;
            }

            // 只推新出现/被改写的行。以前每轮都把今天全量克隆一遍，
            // 几千行 String 分配 × 每 700ms，白烧 CPU
            if seen.get(&id) != Some(&response) {
                changed.push((format!("oc:{id}"), response));
            }
            seen.insert(id, response);
        }

        self.initialized = true;
        self.rows = seen;
        changed
    }
}

// ---------------------------------------------------------------- 文件来源（DSH / WorkBuddy）

#[derive(Clone, Copy)]
enum FileKind {
    Dsh,
    WorkBuddy,
    Claude,
    Codex,
}

struct FileSource {
    root: PathBuf,
    suffix: &'static str,
    kind: FileKind,
    prefix: &'static str,
    cache: HashMap<PathBuf, FileEntry>,
    /// 目录扫描结果 + 扫描时的 (mtime 秒, 长度)
    files: Vec<(PathBuf, (u64, u64))>,
    /// 最近 10 分钟改过的文件：只有这些才需要每轮查 mtime（其余文件不会追加）
    active: Vec<PathBuf>,
    last_scan: i64,
}

/// 一个文件的解析结果 + 增量解析状态
struct FileEntry {
    /// (mtime 秒, 文件长度)：任一变化就重新解析
    stamp: (u64, u64),
    /// 已经消费到第几个字节（WorkBuddy 追加解析用；DSH 是压缩流，恒为 0）
    offset: u64,
    /// 当前全量结果（key 已带来源前缀）
    rows: FileRows,
    /// 文件开头 64 字节的指纹：头部变了说明是被重写，不是追加
    head: u64,
    /// 来源前缀（行 key 用）
    prefix: &'static str,
    /// 逐行追加解析的跨行状态（WorkBuddy / Claude / Codex）
    stream: Option<StreamState>,
}

impl FileEntry {
    fn new(kind: FileKind, prefix: &'static str) -> Self {
        Self {
            stamp: (0, 0),
            offset: 0,
            rows: HashMap::new(),
            head: 0,
            prefix,
            stream: match kind {
                FileKind::WorkBuddy | FileKind::Claude | FileKind::Codex => {
                    Some(StreamState::new(prefix))
                }
                FileKind::Dsh => None,
            },
        }
    }

    fn set_stamp(&mut self, path: &Path) {
        self.stamp = file_stamp(path);
    }
}

/// 逐行追加解析的跨行状态：文件追加时只解析新行，靠它把结果续上
struct StreamState {
    prefix: &'static str,
    /// 带前缀的 key → (合并中的响应, 插入时的上一行时间戳)
    merged: HashMap<String, (Response, i64)>,
    /// 上一行的 timestamp（推算 start 用）
    last_ts: i64,
    /// Codex 的 token_count 事件没有稳定 id，用「文件指纹 + 文件内序号」当 key
    seq: u64,
    /// 文件指纹：不同会话文件的序号都从 1 开始，不加这个会互相覆盖
    salt: u64,
}

impl StreamState {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            merged: HashMap::new(),
            last_ts: 0,
            seq: 0,
            salt: 0,
        }
    }
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
        // files 也要清掉，否则下一次 poll 会跳过扫描、白等 30 秒才把数据补回来
        self.files.clear();
        self.active.clear();
        self.last_scan = 0;
    }

    fn poll(&mut self, now_ms: i64, midnight: i64) -> Result<Delta, String> {
        if !self.root.exists() {
            return Ok(Delta::default());
        }

        let mut delta = Delta::default();

        // 目录树很大（几百个目录 + 几千个文件），全量扫描限流到 30 秒
        if now_ms - self.last_scan > 30_000 || self.files.is_empty() {
            self.last_scan = now_ms;
            // 扫描时顺手把 mtime/长度一起拿回来：以前这里 stat 一遍，
            // 下面筛「最近活跃」再 stat 一遍，13,000 个文件就是双倍开销
            self.files = collect_files(&self.root, self.suffix, midnight - 3_600_000);

            // 目录里已经删掉的文件：不在这次扫描结果里的就是没了。
            // 以前用 `cache.keys().filter(|path| !path.exists())`，等于又对
            // 13,000 个文件各做一次系统调用（扫描耗时的另一半）
            let scanned: HashSet<&Path> =
                self.files.iter().map(|(path, _)| path.as_path()).collect();
            let gone: Vec<PathBuf> = self
                .cache
                .keys()
                .filter(|path| !scanned.contains(path.as_path()))
                .cloned()
                .collect();
            for path in gone {
                if let Some(entry) = self.cache.remove(&path) {
                    delta.removed.extend(entry.rows.into_keys());
                }
            }
            self.active.retain(|path| scanned.contains(path.as_path()));

            let now_secs = (now_ms / 1000) as u64;
            self.active = self
                .files
                .iter()
                .filter(|(_, (modified, _))| now_secs.saturating_sub(*modified) < 600)
                .map(|(path, _)| path.clone())
                .collect();

            // 新出现的文件当场解析一次，之后就靠 active 的 mtime 检查
            for (path, stamp) in self.files.clone() {
                if !self.cache.contains_key(&path) {
                    let mut entry = FileEntry::new(self.kind, self.prefix);
                    let _ = parse_into(self.kind, &path, &mut entry, &mut delta);
                    entry.stamp = stamp;
                    self.cache.insert(path, entry);
                }
            }
        }

        let mut stale = Vec::new();

        // 只对活跃文件查 mtime：几百次 stat 在 OneDrive 目录上要 100ms+
        for path in self.active.clone() {
            let stamp = file_stamp(&path);
            match self.cache.get(&path) {
                Some(entry) if entry.stamp == stamp => {}
                _ => stale.push(path),
            }
        }

        // 变过的文件：WorkBuddy 只解析新增的字节，DSH 是压缩流，只能整段重来
        for path in stale {
            if let Some(entry) = self.cache.get_mut(&path) {
                let _ = parse_into(self.kind, &path, entry, &mut delta);
                entry.set_stamp(&path);
            }
        }

        Ok(delta)
    }
}

/// 把「旧集合 → 新集合」的差异写进 delta（只多出/变化的行，和消失的行）。
fn push_diff(old: &FileRows, new: &FileRows, delta: &mut Delta) {
    for (key, response) in new {
        if old.get(key) != Some(response) {
            delta.changed.push((key.clone(), *response));
        }
    }

    for key in old.keys() {
        if !new.contains_key(key) {
            delta.removed.push(key.clone());
        }
    }
}

fn collect_files(root: &Path, suffix: &str, modified_after_ms: i64) -> Vec<(PathBuf, (u64, u64))> {
    walk_files(root, suffix, modified_after_ms)
}

/// FILETIME 是从 1601-01-01 起的 100ns 计数
const FILETIME_UNIX_OFFSET_SECS: u64 = 11_644_473_600;

/// 遍历目录，返回 (路径, (mtime 秒, 文件长度))。
///
/// 用 `FindFirstFileW` 一次把目录项连 mtime/长度一起读回来。`std::fs::read_dir`
/// 只给名字，要时间戳就得对每个文件再 `metadata()` 一次 —— 13,000 个文件就是
/// 13,000 次系统调用（实测一次扫描 250ms，AI 开着时每 30 秒来一次）。
pub(crate) fn walk_files(
    root: &Path,
    suffix: &str,
    modified_after_ms: i64,
) -> Vec<(PathBuf, (u64, u64))> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW,
    };

    let mut result = Vec::new();
    let minimum =
        (modified_after_ms.max(0) as u64 / 1000 + FILETIME_UNIX_OFFSET_SECS) * 10_000_000;
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let mut pattern = long_path(&dir);
        if !pattern.ends_with('\\') {
            pattern.push('\\');
        }
        pattern.push('*');
        let wide: Vec<u16> = pattern.encode_utf16().chain(std::iter::once(0)).collect();

        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
        if handle == INVALID_HANDLE_VALUE {
            continue;
        }

        loop {
            let length = data
                .cFileName
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(data.cFileName.len());
            let name = String::from_utf16_lossy(&data.cFileName[..length]);

            if !name.is_empty() && name != "." && name != ".." {
                let path = dir.join(&name);
                let directory = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                if directory {
                    // 不跟 junction/符号链接：绕圈会把整次扫描拖死
                    if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        pending.push(path);
                    }
                } else if has_suffix(&name, suffix) {
                    let bytes =
                        ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
                    let ticks = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
                        | data.ftLastWriteTime.dwLowDateTime as u64;
                    if ticks >= minimum {
                        let secs = (ticks / 10_000_000)
                            .saturating_sub(FILETIME_UNIX_OFFSET_SECS);
                        result.push((path, (secs, bytes)));
                    }
                }
            }

            if unsafe { FindNextFileW(handle, &mut data) } == 0 {
                break;
            }
        }

        unsafe { FindClose(handle) };
    }

    result
}

fn has_suffix(name: &str, suffix: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.eq_ignore_ascii_case(suffix))
        .unwrap_or(false)
}

/// FindFirstFileW 想要 `\\?\` 前缀，超长路径也能走
fn long_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    if text.starts_with("\\\\?\\") {
        text
    } else {
        format!("\\\\?\\{text}")
    }
}

/// (mtime 秒, 文件长度)：追加解析要同时看这两个，同一秒内追写也能发现
pub(crate) fn file_stamp(path: &Path) -> (u64, u64) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            (mtime, meta.len())
        }
        Err(_) => (0, 0),
    }
}

fn parse_into(
    kind: FileKind,
    path: &Path,
    entry: &mut FileEntry,
    delta: &mut Delta,
) -> std::io::Result<()> {
    match kind {
        FileKind::Dsh => {
            let rows = parse_dsh(path, entry.prefix)?;
            push_diff(&entry.rows, &rows, delta);
            entry.rows = rows;
            entry.offset = 0;
        }
        FileKind::WorkBuddy | FileKind::Claude | FileKind::Codex => {
            let head = head_hash(path);
            let len = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);

            // 文件被重写（头部指纹变了）或者变短了：之前解析出来的东西全部作废
            if entry.offset > 0 && (head != entry.head || entry.offset > len) {
                delta.removed.extend(entry.rows.keys().cloned());
                entry.rows.clear();
                entry.stream = Some(StreamState::new(entry.prefix));
                entry.offset = 0;
            }

            let prefix = entry.prefix;
            let state = entry
                .stream
                .get_or_insert_with(|| StreamState::new(prefix));
            // 文件指纹只算一次：Codex 的 key 要用它区分不同会话
            if state.salt == 0 {
                state.salt = path_salt(path);
            }
            entry.offset = parse_stream_into(
                kind,
                path,
                state,
                &mut entry.rows,
                delta,
                entry.offset,
            )?;
            entry.head = head;
        }
    }
    Ok(())
}

/// 文件路径的稳定指纹（FNV-1a）：只用来给「没有 id 的事件」拼 key
fn path_salt(path: &Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.to_string_lossy().to_lowercase().bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // 0 是「还没算过」的哨兵值
    hash.max(1)
}

/// 文件开头 64 字节的指纹（FNV-1a）：只为区分「追加」和「重写」
fn head_hash(path: &Path) -> u64 {
    let Ok(mut file) = File::open(path) else {
        return 0;
    };
    let mut head = [0u8; 64];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(_) => return 0,
        }
    }

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in &head[..filled] {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn parse_dsh(path: &Path, prefix: &'static str) -> std::io::Result<FileRows> {
    let file = File::open(path)?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    // 逐行读：解压结果不再整段进内存，峰值只跟最长一行有关
    let mut reader = BufReader::with_capacity(64 * 1024, decoder);
    let mut buf = Vec::new();
    let mut rows = HashMap::new();
    let mut pending_start = 0_i64;

    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        if buf.len() > MAX_LINE {
            continue;
        }

        let line = trim_line(&buf);
        if line.contains("\"step/start\"") {
            if let Ok(value) = serde_json::from_str::<Value>(line.as_ref()) {
                pending_start = value["time"].as_i64().unwrap_or(0);
            }
            continue;
        }

        if !line.contains("\"assistant/message\"") {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(line.as_ref()) else {
            continue;
        };

        let usage = &value["data"]["usage"];
        let output = usage["outputTokens"].as_i64().unwrap_or(0);
        let cache_read = usage["cacheReadTokens"].as_i64().unwrap_or(0);
        let input = usage["inputTokens"].as_i64().unwrap_or(0)
            + cache_read
            + usage["cacheWriteTokens"].as_i64().unwrap_or(0);
        if output <= 0 && input <= 0 {
            continue;
        }

        let time = value["time"].as_i64().unwrap_or(0);
        let seq = value["seq"].as_i64().unwrap_or(0);
        let key = format!("{prefix}:{seq}");

        let start = if pending_start > 0 && time > pending_start && time - pending_start < 600_000 {
            pending_start
        } else {
            time
        };

        rows.insert(
            key,
            Response {
                start,
                end: time,
                output,
                input,
            },
        );

        pending_start = 0;
    }

    Ok(rows)
}

/// 从 `offset` 往后解析完整行，返回新的偏移（只前进到最后一个换行）。
///
/// 最后一行没写完时先「试着解析」但不前移偏移，等它写完再整行重来 ——
/// 同一个 messageId 覆盖，不会重复计数。文件重写/截断由 `parse_into` 提前识别。
fn parse_stream_into(
    kind: FileKind,
    path: &Path,
    state: &mut StreamState,
    rows: &mut FileRows,
    delta: &mut Delta,
    offset: u64,
) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();

    let mut start = offset;
    if start > len {
        start = 0;
    }

    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut buf = Vec::new();
    let mut consumed = start;

    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }

        let complete = buf.last() == Some(&b'\n');
        if buf.len() > MAX_LINE {
            // 坏文件里的超长行：跳过；完整的就把偏移推过去
            if complete {
                consumed += read as u64;
            } else {
                break;
            }
            continue;
        }

        apply_stream_line(kind, &trim_line(&buf), state, rows, delta);
        if complete {
            consumed += read as u64;
        } else {
            break;
        }
    }

    Ok(consumed)
}

fn apply_stream_line(
    kind: FileKind,
    line: &str,
    state: &mut StreamState,
    rows: &mut FileRows,
    delta: &mut Delta,
) {
    match kind {
        FileKind::WorkBuddy => apply_workbuddy_line(line, state, rows, delta),
        FileKind::Claude => apply_claude_line(line, state, rows, delta),
        FileKind::Codex => apply_codex_line(line, state, rows, delta),
        FileKind::Dsh => {}
    }
}

fn apply_workbuddy_line(
    line: &str,
    state: &mut StreamState,
    rows: &mut FileRows,
    delta: &mut Delta,
) {
    if !line.contains("\"rawUsage\"") || line.contains("\"isPartialAborted\":true") {
        return;
    }

    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };

    let provider = &value["providerData"];
    let Some(message_id) = provider["messageId"].as_str() else {
        return;
    };

    let raw = &provider["rawUsage"];
    let output = raw["completion_tokens"].as_i64().unwrap_or(0);
    let input = raw["prompt_tokens"].as_i64().unwrap_or(0);
    if output <= 0 && input <= 0 {
        return;
    }

    let timestamp = value["timestamp"].as_i64().unwrap_or(0);
    let key = format!("{}:{}", state.prefix, message_id);
    let last_ts = state.last_ts;
    let entry = state.merged.entry(key.clone()).or_insert_with(|| {
        (
            Response {
                start: timestamp,
                end: timestamp,
                output: 0,
                input: 0,
            },
            last_ts,
        )
    });

    if output > entry.0.output {
        entry.0.output = output;
    }

    if input > entry.0.input {
        entry.0.input = input;
    }

    if timestamp > entry.0.end {
        if last_ts > 0 && timestamp - last_ts < 180_000 && timestamp > last_ts {
            entry.0.start = last_ts;
        }
        entry.0.end = timestamp;
    }

    if timestamp > state.last_ts {
        state.last_ts = timestamp;
    }

    // 只有真的变了才推给引擎（追加解析时绝大多数行都是新的，但重复上报要覆盖）
    let response = entry.0;
    if rows.get(&key) != Some(&response) {
        rows.insert(key.clone(), response);
        delta.changed.push((key, response));
    }
}


// ---------------------------------------------------------------- Claude Code / Codex

/// Claude Code 的会话转录：`~/.claude/projects/<项目>/<session>.jsonl`
///
/// 每个助手回合一行，带 `message.usage`。流式输出时同一个 `message.id` 会出现多次
/// （用量逐步变大），这里按 id 合并取最大值，和各家统计工具的算法一致。
fn apply_claude_line(line: &str, state: &mut StreamState, rows: &mut FileRows, delta: &mut Delta) {
    // 先用字符串粗筛：整棵树里绝大多数行是用户消息/工具输出
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
        return;
    }

    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if value["type"].as_str() != Some("assistant") {
        return;
    }

    let usage = &value["message"]["usage"];
    let output = usage["output_tokens"].as_i64().unwrap_or(0);
    let cache_read = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
    let input = usage["input_tokens"].as_i64().unwrap_or(0)
        + cache_read
        + usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
    if output <= 0 && input <= 0 {
        return;
    }

    // 没有 id（极少数旧格式）就用 requestId / uuid 兜底
    let id = value["message"]["id"]
        .as_str()
        .or_else(|| value["requestId"].as_str())
        .or_else(|| value["uuid"].as_str())
        .unwrap_or("");
    if id.is_empty() {
        return;
    }

    let Some(timestamp) = value["timestamp"].as_str().and_then(parse_iso_ms) else {
        return;
    };

    let key = format!("{}:{}", state.prefix, id);
    let last_ts = state.last_ts;
    // 上一回合的结束时间 ≈ 这一回合的开始（间隔太远的当空闲，不参与速度）
    let start = if last_ts > 0 && timestamp > last_ts && timestamp - last_ts < 120_000 {
        last_ts
    } else {
        timestamp
    };
    let entry = state.merged.entry(key.clone()).or_insert_with(|| {
        (
            Response {
                start,
                end: timestamp,
                output: 0,
                input: 0,
            },
            last_ts,
        )
    });

    // 流式重复上报：只取更大的用量
    if output > entry.0.output {
        entry.0.output = output;
    }
    if input > entry.0.input {
        entry.0.input = input;
    }

    if timestamp > entry.0.end {
        if last_ts > 0 && timestamp - last_ts < 120_000 {
            entry.0.start = last_ts;
        }
        entry.0.end = timestamp;
    }

    if timestamp > state.last_ts {
        state.last_ts = timestamp;
    }

    let response = entry.0;
    if rows.get(&key) != Some(&response) {
        rows.insert(key.clone(), response);
        delta.changed.push((key, response));
    }
}

/// Codex 的会话转录：`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
///
/// 每轮结束会写一条 `payload.type == "token_count"`，
/// `payload.info.last_token_usage` 是**这一轮**的增量（total_token_usage 是累计值）。
fn apply_codex_line(line: &str, state: &mut StreamState, rows: &mut FileRows, delta: &mut Delta) {
    if !line.contains("\"token_count\"") {
        return;
    }

    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let payload = &value["payload"];
    if payload["type"].as_str() != Some("token_count") {
        return;
    }

    let usage = &payload["info"]["last_token_usage"];
    let output = usage["output_tokens"].as_i64().unwrap_or(0);
    let input = usage["input_tokens"].as_i64().unwrap_or(0);
    if output <= 0 && input <= 0 {
        return;
    }

    let Some(timestamp) = value["timestamp"].as_str().and_then(parse_iso_ms) else {
        return;
    };

    // token_count 没有稳定 id：用「文件指纹 + 文件内序号」（只追加，序号稳定）
    state.seq += 1;
    let key = format!("{}:{:x}:{}", state.prefix, state.salt, state.seq);
    let last_ts = state.last_ts;

    let start = if last_ts > 0 && timestamp > last_ts && timestamp - last_ts < 300_000 {
        last_ts
    } else {
        timestamp
    };

    let response = Response {
        start,
        end: timestamp,
        output,
        input,
    };
    state.last_ts = timestamp;

    rows.insert(key.clone(), response);
    delta.changed.push((key, response));
}

/// ISO8601（`2026-11-15T21:03:12.345Z` / `…+08:00`）→ UTC 毫秒。
///
/// Claude / Codex 现在都写 `Z`，但有些工具写带偏移的本地时间，顺手支持上。
pub(crate) fn parse_iso_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }

    let number = |start: usize, len: usize| -> Option<i64> {
        text.get(start..start + len)?.parse::<i64>().ok()
    };

    let year = number(0, 4)?;
    let month = number(5, 2)?;
    let day = number(8, 2)?;
    let hour = number(11, 2)?;
    let minute = number(14, 2)?;
    let second = number(17, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // 小数秒（可选）
    let mut millis = 0_i64;
    let mut index = 19;
    if bytes.get(19) == Some(&b'.') {
        index = 20;
        let digits: String = text
            .get(20..)?
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            let take = digits.len().min(3);
            let frac = digits[..take].parse::<i64>().ok()?;
            millis = frac * 10_i64.pow((3 - take) as u32);
            index = 20 + digits.len();
        }
    }

    // 结尾：Z / +HH:MM / -HHMM / 什么都没有（按 UTC 算）
    let offset_minutes = match bytes.get(index) {
        Some(b'Z') | None => 0,
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            let rest = text.get(index + 1..)?;
            let digits: String = rest.chars().filter(|ch| ch.is_ascii_digit()).collect();
            let (hours, minutes) = match digits.len() {
                0 => (0, 0),
                1 | 2 => (digits.parse::<i64>().ok()?, 0),
                _ => (
                    digits[..2].parse::<i64>().ok()?,
                    digits[2..4].parse::<i64>().ok()?,
                ),
            };
            sign * (hours * 60 + minutes)
        }
        _ => return None,
    };

    let days = days_from_civil(year, month as u32, day as u32);
    let local = days * 86_400_000
        + hour * 3_600_000
        + minute * 60_000
        + second * 1000
        + millis;
    Some(local - offset_minutes * 60_000)
}

/// 公历年月日 → 1970-01-01 起的天数（Howard Hinnant 的算法）
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400; // [0, 399]
    let mp = (month as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}


// ---------------------------------------------------------------- 工具

/// 单行上限（坏文件兜底）：超过就跳过这一行，别把内存拖爆
pub(crate) const MAX_LINE: usize = 8 * 1024 * 1024;

/// 去掉行尾 \r\n，并按 lossy 解码：坏字节只影响这一行，不会废掉整个文件
pub(crate) fn trim_line(buf: &[u8]) -> std::borrow::Cow<'_, str> {
    let mut end = buf.len();
    while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
        end -= 1;
    }
    String::from_utf8_lossy(&buf[..end])
}

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
    // now_ms 和 GetLocalTime 是两次取时间，中间可能跨过一个毫秒，直接相减
    // 会让「0 点」时不时差 1ms；跟 today_start 一比就被误判成跨天、清空所有缓存。
    // 对齐到整秒，跨天判断才稳定。
    let midnight = now_ms - seconds_today * 1000;
    midnight - midnight.rem_euclid(1000)
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

/// 本地小时（0-23），用于「今天/昨天」按小时看消耗。
pub fn local_hour(ms: i64) -> u8 {
    (((ms + local_offset_ms()).div_euclid(3_600_000)) % 24) as u8
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
    fn iso_timestamps_parse_to_utc_ms() {
        // 2026-11-15T21:03:12.345Z
        let ms = parse_iso_ms("2026-11-15T21:03:12.345Z").unwrap();
        assert_eq!(ms, 1_794_776_592_345);

        // 没有毫秒、没有小数点的写法
        let whole = parse_iso_ms("2026-11-15T21:03:12Z").unwrap();
        assert_eq!(whole, 1_794_776_592_000);

        // 带时区偏移：+08:00 的同一时刻要换算回 UTC
        let offset = parse_iso_ms("2026-11-16T05:03:12.345+08:00").unwrap();
        assert_eq!(offset, 1_794_776_592_345);
        let compact = parse_iso_ms("2026-11-16T05:03:12+0800").unwrap();
        assert_eq!(compact, 1_794_776_592_000);
        let west = parse_iso_ms("2026-11-15T13:03:12-08:00").unwrap();
        assert_eq!(west, 1_794_776_592_000);

        // 两位数/一位数的时间字段不该误判
        assert!(parse_iso_ms("2026-1-5T21:03:12Z").is_none());
        assert!(parse_iso_ms("not a time").is_none());
    }

    #[test]
    fn claude_lines_merge_streaming_usage() {
        let mut state = StreamState::new("claude");
        let mut rows = HashMap::new();
        let mut delta = Delta::default();

        // 同一条消息流式写了两遍，用量变大：只保留大的那份
        apply_claude_line(
            r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":10}},"timestamp":"2026-11-15T10:00:00.000Z"}"#,
            &mut state,
            &mut rows,
            &mut delta,
        );
        apply_claude_line(
            r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":42}},"timestamp":"2026-11-15T10:00:01.000Z"}"#,
            &mut state,
            &mut rows,
            &mut delta,
        );
        // 另一条消息：start 用上一条的结束时间（间隔小于 120 秒）
        apply_claude_line(
            r#"{"type":"assistant","message":{"id":"msg_2","usage":{"input_tokens":10,"output_tokens":7}},"timestamp":"2026-11-15T10:00:09.000Z"}"#,
            &mut state,
            &mut rows,
            &mut delta,
        );
        // 用户消息不算
        apply_claude_line(
            r#"{"type":"user","message":{"role":"user"},"timestamp":"2026-11-15T10:00:10.000Z"}"#,
            &mut state,
            &mut rows,
            &mut delta,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows["claude:msg_1"].output, 42);
        assert_eq!(rows["claude:msg_2"].output, 7);
        let first_end = parse_iso_ms("2026-11-15T10:00:01.000Z").unwrap();
        assert_eq!(rows["claude:msg_2"].start, first_end);
    }

    #[test]
    fn codex_token_counts_become_rows() {
        let mut state = StreamState::new("codex");
        let mut rows = HashMap::new();
        let mut delta = Delta::default();

        for (index, output) in [12_i64, 30, 5].iter().enumerate() {
            let line = format!(
                r#"{{"timestamp":"2026-11-15T10:00:0{index}.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":100,"output_tokens":{output}}}}}}}}}"#
            );
            apply_codex_line(&line, &mut state, &mut rows, &mut delta);
        }
        // 累计值那条（total_token_usage）不该被当成增量重复计入
        apply_codex_line(
            r#"{"timestamp":"2026-11-15T10:00:09.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":999,"output_tokens":999}}}}"#,
            &mut state,
            &mut rows,
            &mut delta,
        );

        assert_eq!(rows.len(), 3);
        let key = |seq: u64| format!("codex:{:x}:{seq}", state.salt);
        assert_eq!(rows[&key(1)].output, 12);
        assert_eq!(rows[&key(3)].output, 5);
        // 相邻事件间隔 1 秒：start 取上一条的时间
        assert!(rows[&key(2)].end - rows[&key(2)].start <= 1000);
    }

    #[test]
    fn engine_reads_claude_sessions_from_home() {
        let home = std::env::temp_dir().join("nst-engine-claude");
        let _ = std::fs::remove_dir_all(&home);
        let session = home.join(".claude").join("projects").join("demo");
        std::fs::create_dir_all(&session).unwrap();

        // 用「现在」的时间戳，保证落在今天里
        let now = now_ms();
        let stamp = iso_from_ms(now - 5_000);
        let line = format!(
            r#"{{"type":"assistant","message":{{"id":"msg_x","model":"claude-x","usage":{{"input_tokens":10,"output_tokens":321}}}},"timestamp":"{stamp}"}}"#
        );
        std::fs::write(session.join("s.jsonl"), line + "\n").unwrap();

        let mut engine = AiEngine::with_home(
            home.clone(),
            Sources {
                opencode: false,
                dsh: false,
                workbuddy: false,
                workbuddy_ai: false,
                claude: true,
                codex: false,
            },
        );
        engine.poll(now);
        let snapshot = engine.snapshot(now);
        assert_eq!(
            snapshot.today_tokens, 331,
            "今日 = 输入 10 + 输出 321，不能只算输出"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// 测试用：epoch 毫秒 → `YYYY-MM-DDTHH:MM:SS.mmmZ`
    fn iso_from_ms(ms: i64) -> String {
        let mut days = ms.div_euclid(86_400_000);
        let mut rest = ms.rem_euclid(86_400_000);
        // 从 1970-01-01 起逐日推算（跨闰年也够用）
        let mut year = 1970_i64;
        loop {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            let days_in_year = if leap { 366 } else { 365 };
            if days >= days_in_year {
                days -= days_in_year;
                year += 1;
            } else {
                break;
            }
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let months = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut month = 0;
        while month < 12 && days >= months[month] {
            days -= months[month];
            month += 1;
        }
        let hour = rest / 3_600_000;
        rest %= 3_600_000;
        let minute = rest / 60_000;
        rest %= 60_000;
        let second = rest / 1000;
        let milli = rest % 1000;
        format!(
            "{year:04}-{:02}-{:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z",
            month + 1,
            days + 1
        )
    }

    #[test]
    fn codex_files_do_not_collide() {
        // 两个 Codex 会话文件的 token_count 序号都从 1 开始，key 必须带文件指纹
        let dir = std::env::temp_dir().join("nst-codex-two");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line = r#"{"timestamp":"2026-11-15T10:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#;
        let a = dir.join("rollout-a.jsonl");
        let b = dir.join("rollout-b.jsonl");
        std::fs::write(&a, format!("{line}\n")).unwrap();
        std::fs::write(&b, format!("{line}\n")).unwrap();

        let mut entry_a = FileEntry::new(FileKind::Codex, "codex");
        let mut entry_b = FileEntry::new(FileKind::Codex, "codex");
        let mut delta_a = Delta::default();
        let mut delta_b = Delta::default();
        parse_into(FileKind::Codex, &a, &mut entry_a, &mut delta_a).unwrap();
        parse_into(FileKind::Codex, &b, &mut entry_b, &mut delta_b).unwrap();

        assert_eq!(delta_a.changed.len(), 1);
        assert_eq!(delta_b.changed.len(), 1);
        assert_ne!(
            delta_a.changed[0].0, delta_b.changed[0].0,
            "不同会话的同序号事件不能互相覆盖"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_files_finds_recent_jsonl() {
        let dir = std::env::temp_dir().join("nst-walk-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let nested = dir.join("project");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("session.jsonl");
        std::fs::write(&file, "{\"a\":1}\n").unwrap();

        let found = walk_files(&dir, "jsonl", now_ms() - 86_400_000);
        assert_eq!(found.len(), 1, "要找得到刚写的 jsonl");
        assert_eq!(found[0].0, file);

        // 太老的文件要被 mtime 过滤掉
        let old = walk_files(&dir, "jsonl", now_ms() + 86_400_000);
        assert!(old.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

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

    #[test]
    fn midnight_is_stable_across_calls() {
        // 两次调用之间即使跨了毫秒边界，算出来的「今天 0 点」也必须完全一样
        let mut previous = local_midnight_ms(now_ms());
        for _ in 0..500 {
            let next = local_midnight_ms(now_ms());
            assert_eq!(previous, next);
            previous = next;
        }
        assert_eq!(previous % 1000, 0, "应该对齐到整秒");
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("netspeed-ai-{}-{}", std::process::id(), name))
    }

    fn workbuddy_line(id: &str, timestamp: i64, output: i64) -> String {
        format!(
            r#"{{"providerData":{{"messageId":"{id}","rawUsage":{{"completion_tokens":{output},"prompt_tokens":11}}}},"timestamp":{timestamp}}}"#
        )
    }

    #[test]
    fn trim_line_strips_newlines() {
        assert_eq!(trim_line(b"abc\r\n").as_ref(), "abc");
        assert_eq!(trim_line(b"abc\n").as_ref(), "abc");
        assert_eq!(trim_line(b"abc").as_ref(), "abc");
        // 坏字节只影响这一行
        assert!(trim_line(&[0xff, 0xfe]).contains('\u{fffd}'));
    }

    fn workbuddy_entry() -> FileEntry {
        FileEntry::new(FileKind::WorkBuddy, "wb")
    }

    fn parse_file(entry: &mut FileEntry, path: &Path) -> Delta {
        let mut delta = Delta::default();
        parse_into(FileKind::WorkBuddy, path, entry, &mut delta).unwrap();
        delta
    }

    #[test]
    fn workbuddy_holds_back_partial_line() {
        let path = temp_path("wb-partial.jsonl");
        let first = workbuddy_line("m1", 1_000, 7);
        let second = workbuddy_line("m2", 2_000, 9);
        let (head, tail) = second.split_at(second.len() / 2);
        std::fs::write(&path, format!("{first}\n{head}").as_bytes()).unwrap();

        let mut entry = workbuddy_entry();
        let delta = parse_file(&mut entry, &path);
        assert_eq!(entry.rows.len(), 1, "没写完的那行不该算进来");
        assert_eq!(delta.changed.len(), 1);
        assert_eq!(entry.offset, (first.len() + 1) as u64);

        // 补完后半行：只解析新增的字节
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            write!(file, "{tail}\n").unwrap();
        }

        let delta = parse_file(&mut entry, &path);
        assert_eq!(entry.rows.len(), 2);
        assert_eq!(delta.changed.len(), 1, "只应该推新增的那一条");
        assert!(delta.removed.is_empty());
        assert_eq!(entry.offset, (first.len() + 1 + second.len() + 1) as u64);

        // 文件没变时再解析：什么都不推（否则引擎每轮都要重收全量）
        let delta = parse_file(&mut entry, &path);
        assert!(delta.changed.is_empty());
        assert!(delta.removed.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn workbuddy_restarts_when_file_is_rewritten() {
        let path = temp_path("wb-rewrite.jsonl");
        std::fs::write(&path, format!("{}\n", workbuddy_line("a", 10, 3)).as_bytes()).unwrap();

        let mut entry = workbuddy_entry();
        parse_file(&mut entry, &path);
        assert_eq!(entry.rows.len(), 1);

        // 同长度重写成另一条消息：长度不变，但头部指纹变了 → 从头重来
        std::fs::write(&path, format!("{}\n", workbuddy_line("b", 20, 4)).as_bytes()).unwrap();
        let delta = parse_file(&mut entry, &path);
        assert_eq!(entry.rows.len(), 1, "重写后旧数据要作废");
        assert_eq!(delta.removed, vec!["wb:a".to_string()]);
        assert_eq!(delta.changed.len(), 1);
        assert!(entry.rows.contains_key("wb:b"));
        assert!(entry.offset > 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn workbuddy_merges_same_message() {
        let path = temp_path("wb-merge.jsonl");
        // 同一条消息分两次上报（token 数递增）→ 取最大值，不重复累加
        let text = format!(
            "{}\n{}\n",
            workbuddy_line("m1", 1_000, 5),
            workbuddy_line("m1", 1_500, 12)
        );
        std::fs::write(&path, text.as_bytes()).unwrap();

        let mut entry = workbuddy_entry();
        parse_file(&mut entry, &path);
        assert_eq!(entry.rows.len(), 1);
        assert_eq!(entry.rows["wb:m1"].output, 12);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn engine_applies_incremental_deltas() {
        let mut engine = AiEngine::new(Sources {
            opencode: false,
            dsh: false,
            workbuddy: false,
            workbuddy_ai: false,
            claude: false,
            codex: false,
        });
        let now = 10_000_000;
        engine.today_start = local_midnight_ms(now);

        let mut delta = Delta::default();
        delta.changed.push((
            "wb:a".to_string(),
            Response {
                start: now - 1000,
                end: now,
                output: 10,
                input: 0,
            },
        ));
        engine.apply(delta);
        assert_eq!(engine.responses.len(), 1);

        // 同 key 覆盖：不重复累加
        let mut delta = Delta::default();
        delta.changed.push((
            "wb:a".to_string(),
            Response {
                start: now - 1000,
                end: now,
                output: 25,
                input: 0,
            },
        ));
        engine.apply(delta);
        assert_eq!(engine.responses.len(), 1);
        assert_eq!(engine.responses["wb:a"].output, 25);

        // 删除的 key 要真的消失
        let mut delta = Delta::default();
        delta.removed.push("wb:a".to_string());
        delta.changed.push((
            "wb:a".to_string(),
            Response {
                start: now - 500,
                end: now,
                output: 7,
                input: 0,
            },
        ));
        engine.apply(delta);
        assert_eq!(engine.responses["wb:a"].output, 7, "先删后插，重写的 key 要留下");
    }

    #[test]
    fn today_counts_input_plus_output() {
        let mut engine = AiEngine::new(Sources {
            opencode: false,
            dsh: false,
            workbuddy: false,
            workbuddy_ai: false,
            claude: false,
            codex: false,
        });
        let now = now_ms();
        engine.today_start = local_midnight_ms(now);

        let mut delta = Delta::default();
        delta.changed.push((
            "wb:a".to_string(),
            Response {
                start: now - 1_000,
                end: now,
                output: 7,
                input: 93,
            },
        ));
        engine.apply(delta);
        engine.recompute(now);

        let snapshot = engine.snapshot(now);
        assert_eq!(snapshot.today_tokens, 100, "今日应该是输入 + 输出");
        assert!(snapshot.speed > 0.0, "速度仍然只按输出算");
    }
}
