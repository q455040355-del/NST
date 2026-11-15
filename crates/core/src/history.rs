//! 用量历史：多来源加载 + 按天/来源/模型聚合。
//!
//! 数据源和 AI 采集一致：OpenCode（SQLite）、DSH（zstd）、WorkBuddy（jsonl）。
//! 这里的解析比实时采集宽松：整文件读、按时间戳过滤，结果按「天」聚合。

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SOURCE_OPENCODE: &str = "opencode";
pub const SOURCE_DSH: &str = "dsh";
pub const SOURCE_WORKBUDDY: &str = "workbuddy";
pub const SOURCE_WORKBUDDY_AI: &str = "workbuddyAi";

pub const DAY_MS: i64 = 86_400_000;

pub use super::ai::format_tokens;

#[derive(Debug, Clone)]
pub struct UsageRecord {
    /// 本地日期序号（now_ms / DAY_MS 的整数部分，按本地时区偏移修正过）
    pub day: i64,
    pub source: &'static str,
    pub model: String,
    pub output: i64,
    pub input: i64,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DailyRow {
    pub day: i64,
    pub output: i64,
    pub input: i64,
    pub requests: i64,
    pub measured_output: i64,
    pub measured_ms: i64,
    pub peak_speed: f64,
    pub main_model: String,
}

impl DailyRow {
    pub fn speed(&self) -> f64 {
        if self.measured_ms <= 0 {
            return 0.0;
        }

        self.measured_output as f64 * 1000.0 / self.measured_ms as f64
    }
}

#[derive(Debug, Clone, Default)]
pub struct SourceRow {
    pub source: &'static str,
    pub output: i64,
    pub input: i64,
    pub requests: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ModelRow {
    pub source: &'static str,
    pub model: String,
    pub output: i64,
    pub input: i64,
    pub requests: i64,
}

pub fn source_name(source: &str) -> &'static str {
    match source {
        SOURCE_DSH => "DSH",
        SOURCE_WORKBUDDY => "WorkBuddy",
        SOURCE_WORKBUDDY_AI => "WorkBuddy AI",
        _ => "OpenCode",
    }
}

pub fn source_color(source: &str) -> u32 {
    // 0x00BBGGRR（GDI COLORREF）
    match source {
        SOURCE_DSH => 0x00FF_617B,
        SOURCE_WORKBUDDY => 0x008C_A92F,
        SOURCE_WORKBUDDY_AI => 0x0032_8AE0,
        _ => 0x00D8_8440,
    }
}

pub fn all_sources() -> [&'static str; 4] {
    [
        SOURCE_OPENCODE,
        SOURCE_DSH,
        SOURCE_WORKBUDDY,
        SOURCE_WORKBUDDY_AI,
    ]
}

/// 今天的日期序号（本地时区）。
pub fn today(now_ms: i64) -> i64 {
    super::ai::day_index(now_ms)
}

/// 只读 OpenCode（毫秒级），用于历史窗口首屏。
pub fn load_opencode() -> Vec<UsageRecord> {
    let mut records = Vec::new();
    load_opencode_into(&mut records, 0);
    records
}

/// 读文件来源（DSH / WorkBuddy），可能要几秒到几十秒，放在第二批。
pub fn load_files_only(days: i64) -> Vec<UsageRecord> {
    let now = super::ai::now_ms();
    let from_ms = if days <= 0 {
        0
    } else {
        super::ai::local_midnight_ms(now) - (days - 1) * DAY_MS
    };

    let mut records = Vec::new();
    let home = home_dir();
    load_files(&mut records, &home.join(".dsh").join("sessions"), "zstd", SOURCE_DSH, from_ms);
    load_files(&mut records, &home.join(".workbuddy").join("projects"), "jsonl", SOURCE_WORKBUDDY, from_ms);
    load_files(&mut records, &home.join(".workbuddy-ai").join("projects"), "jsonl", SOURCE_WORKBUDDY_AI, from_ms);
    records
}

/// 读取最近 `days` 天的记录（days <= 0 表示全部）。
pub fn load(days: i64) -> Vec<UsageRecord> {
    let mut records = load_opencode();
    records.extend(load_files_only(days));
    records
}

fn load_opencode_into(records: &mut Vec<UsageRecord>, from_ms: i64) {
    let Some(path) = super::ai::database_path() else {
        return;
    };

    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return;
    };

    let sql = "SELECT IFNULL(json_extract(data,'$.time.created'), time_created), \
               IFNULL(json_extract(data,'$.time.streamed'),0), \
               IFNULL(json_extract(data,'$.model.id'),''), \
               IFNULL(json_extract(data,'$.tokens.output'),0) + IFNULL(json_extract(data,'$.tokens.reasoning'),0), \
               IFNULL(json_extract(data,'$.tokens.input'),0) + IFNULL(json_extract(data,'$.tokens.cache.read'),0) + IFNULL(json_extract(data,'$.tokens.cache.write'),0) \
               FROM session_message WHERE type = 'assistant' AND time_created >= ?1";

    let Ok(mut statement) = connection.prepare(sql) else {
        return;
    };

    let Ok(rows) = statement.query_map([from_ms], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    }) else {
        return;
    };

    for (created, streamed, model, output, input) in rows.flatten() {
        if output <= 0 && input <= 0 {
            continue;
        }

        records.push(UsageRecord {
            day: super::ai::day_index(created),
            source: SOURCE_OPENCODE,
            model,
            output,
            input,
            duration_ms: if streamed > created { streamed - created } else { 0 },
        });
    }
}

fn load_files(
    records: &mut Vec<UsageRecord>,
    root: &Path,
    suffix: &str,
    source: &'static str,
    from_ms: i64,
) {
    if !root.exists() {
        return;
    }

    for path in collect_files(root, suffix, from_ms - DAY_MS) {
        let parsed = match suffix {
            "zstd" => parse_dsh(&path, source),
            _ => parse_workbuddy(&path, source),
        };

        for record in parsed {
            if record.day >= super::ai::day_index(from_ms) - 1 {
                records.push(record);
            }
        }
    }
}

pub(crate) fn collect_files(root: &Path, suffix: &str, modified_after_ms: i64) -> Vec<PathBuf> {
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

            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0);

            if modified < modified_after_ms {
                continue;
            }

            result.push(path);
        }
    }

    result
}

fn parse_dsh(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let Ok(mut decoder) = ruzstd::decoding::StreamingDecoder::new(file) else {
        return Vec::new();
    };

    let mut text = Vec::new();
    if decoder.read_to_end(&mut text).is_err() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&text);
    let mut records = Vec::new();
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
        let input = usage["inputTokens"].as_i64().unwrap_or(0)
            + usage["cacheReadTokens"].as_i64().unwrap_or(0)
            + usage["cacheWriteTokens"].as_i64().unwrap_or(0);
        if output <= 0 && input <= 0 {
            continue;
        }

        let time = value["time"].as_i64().unwrap_or(0);
        let model = value["data"]["message"]["source"]["model"]
            .as_str()
            .unwrap_or("")
            .to_string();

        records.push(UsageRecord {
            day: super::ai::day_index(time),
            source,
            model,
            output,
            input,
            duration_ms: if pending_start > 0 && time > pending_start {
                time - pending_start
            } else {
                0
            },
        });

        pending_start = 0;
    }

    records
}

fn parse_workbuddy(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut map: HashMap<String, UsageRecord> = HashMap::new();
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
        let model = provider["model"]
            .as_str()
            .or_else(|| provider["requestModelId"].as_str())
            .unwrap_or("")
            .to_string();

        let entry = map.entry(message_id.to_string()).or_insert_with(|| UsageRecord {
            day: super::ai::day_index(timestamp),
            source,
            model: model.clone(),
            output: 0,
            input: 0,
            duration_ms: 0,
        });

        if output > entry.output {
            entry.output = output;
        }

        if input > entry.input {
            entry.input = input;
        }

        if model.len() > 0 {
            entry.model = model;
        }

        if timestamp > 0 {
            entry.day = super::ai::day_index(timestamp);
            if last_ts > 0 && timestamp > last_ts && timestamp - last_ts < 180_000 {
                entry.duration_ms = timestamp - last_ts;
            }
        }

        if timestamp > last_ts {
            last_ts = timestamp;
        }
    }

    map.into_values().collect()
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

// ---------------------------------------------------------------- 聚合

/// 按天汇总（包含没有数据的空缺天，方便直接画柱状图）。
pub fn summarize(records: &[UsageRecord], from_day: i64, to_day: i64) -> Vec<DailyRow> {
    let mut map: HashMap<i64, DailyRow> = HashMap::new();
    let mut models: HashMap<i64, HashMap<String, i64>> = HashMap::new();

    for record in records {
        if record.day < from_day || record.day > to_day {
            continue;
        }

        let row = map.entry(record.day).or_insert_with(|| DailyRow {
            day: record.day,
            ..Default::default()
        });

        row.output += record.output;
        row.input += record.input;
        row.requests += 1;

        if record.duration_ms > 0 {
            row.measured_output += record.output;
            row.measured_ms += record.duration_ms;
            if record.duration_ms >= 200 {
                let speed = record.output as f64 * 1000.0 / record.duration_ms as f64;
                if speed > row.peak_speed {
                    row.peak_speed = speed;
                }
            }
        }

        if record.output > 0 {
            let per_model = models.entry(record.day).or_default();
            *per_model.entry(record.model.clone()).or_insert(0) += record.output;
        }
    }

    for (day, per_model) in models {
        if let Some(row) = map.get_mut(&day) {
            let mut best = 0;
            for (model, output) in &per_model {
                if *output > best {
                    best = *output;
                    row.main_model = model.clone();
                }
            }
        }
    }

    (from_day..=to_day)
        .map(|day| {
            map.remove(&day).unwrap_or_else(|| DailyRow {
                day,
                ..Default::default()
            })
        })
        .collect()
}

pub fn by_source(records: &[UsageRecord], from_day: i64, to_day: i64) -> Vec<SourceRow> {
    let mut map: HashMap<&'static str, SourceRow> = HashMap::new();

    for record in records {
        if record.day < from_day || record.day > to_day {
            continue;
        }

        let row = map.entry(record.source).or_insert_with(|| SourceRow {
            source: record.source,
            ..Default::default()
        });

        row.output += record.output;
        row.input += record.input;
        row.requests += 1;
    }

    let mut rows: Vec<SourceRow> = map.into_values().collect();
    rows.sort_by(|a, b| b.output.cmp(&a.output));
    rows
}

/// 按天 × 来源，给堆叠柱状图用：第 i 天第 k 个来源的值。
pub fn daily_by_source(
    records: &[UsageRecord],
    from_day: i64,
    to_day: i64,
    sources: &[&'static str],
) -> Vec<Vec<i64>> {
    let mut index: HashMap<(&'static str, i64), i64> = HashMap::new();
    for record in records {
        if record.day >= from_day && record.day <= to_day {
            *index.entry((record.source, record.day)).or_insert(0) += record.output;
        }
    }

    let mut rows = Vec::new();
    for day in from_day..=to_day {
        let mut row = vec![0_i64; sources.len()];
        for (slot, source) in sources.iter().enumerate() {
            row[slot] = *index.get(&(*source, day)).unwrap_or(&0);
        }
        rows.push(row);
    }

    rows
}

pub fn by_model(records: &[UsageRecord], from_day: i64, to_day: i64) -> Vec<ModelRow> {
    let mut map: HashMap<(&'static str, String), ModelRow> = HashMap::new();

    for record in records {
        if record.day < from_day || record.day > to_day {
            continue;
        }

        let key = (record.source, record.model.clone());
        let row = map.entry(key).or_insert_with(|| ModelRow {
            source: record.source,
            model: record.model.clone(),
            ..Default::default()
        });

        row.output += record.output;
        row.input += record.input;
        row.requests += 1;
    }

    let mut rows: Vec<ModelRow> = map.into_values().collect();
    rows.sort_by(|a, b| b.output.cmp(&a.output));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(day: i64, source: &'static str, model: &str, output: i64) -> UsageRecord {
        UsageRecord {
            day,
            source,
            model: model.to_string(),
            output,
            input: output * 10,
            duration_ms: 1000,
        }
    }

    #[test]
    fn summarize_fills_gaps_and_picks_top_model() {
        let records = vec![
            record(10, SOURCE_OPENCODE, "a", 100),
            record(10, SOURCE_DSH, "b", 300),
            record(12, SOURCE_OPENCODE, "a", 50),
        ];

        let rows = summarize(&records, 10, 12);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].output, 400);
        assert_eq!(rows[0].main_model, "b");
        assert_eq!(rows[1].output, 0);
        assert_eq!(rows[2].output, 50);
        assert!(rows[0].speed() > 0.0);
    }

    #[test]
    fn stack_matches_daily_totals() {
        let records = vec![
            record(5, SOURCE_OPENCODE, "a", 10),
            record(5, SOURCE_WORKBUDDY_AI, "b", 20),
            record(6, SOURCE_DSH, "c", 7),
        ];

        let sources = all_sources();
        let stack = daily_by_source(&records, 5, 6, &sources);
        assert_eq!(stack.len(), 2);
        assert_eq!(stack[0].iter().sum::<i64>(), 30);
        assert_eq!(stack[1].iter().sum::<i64>(), 7);
    }

    #[test]
    fn grouping_by_source_and_model() {
        let records = vec![
            record(1, SOURCE_OPENCODE, "a", 10),
            record(1, SOURCE_WORKBUDDY_AI, "b", 30),
            record(2, SOURCE_WORKBUDDY_AI, "b", 20),
        ];

        let sources = by_source(&records, 1, 2);
        assert_eq!(sources[0].source, SOURCE_WORKBUDDY_AI);
        assert_eq!(sources[0].output, 50);

        let models = by_model(&records, 1, 2);
        assert_eq!(models[0].model, "b");
        assert_eq!(models[0].output, 50);
    }

    #[test]
    fn source_names_and_colors() {
        assert_eq!(source_name(SOURCE_DSH), "DSH");
        assert_eq!(source_name(SOURCE_WORKBUDDY_AI), "WorkBuddy AI");
        assert_ne!(source_color(SOURCE_DSH), source_color(SOURCE_OPENCODE));
    }
}
