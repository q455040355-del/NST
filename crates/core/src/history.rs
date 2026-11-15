//! 用量历史：多来源加载 + 按天/来源/模型聚合。
//!
//! 数据源和 AI 采集一致：OpenCode（SQLite）、DSH（zstd）、WorkBuddy（jsonl）。
//! 这里的解析比实时采集宽松：整文件读、按时间戳过滤，结果按「天」聚合。

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

pub const SOURCE_OPENCODE: &str = "opencode";
pub const SOURCE_DSH: &str = "dsh";
pub const SOURCE_WORKBUDDY: &str = "workbuddy";
pub const SOURCE_WORKBUDDY_AI: &str = "workbuddyAi";

pub const DAY_MS: i64 = 86_400_000;

pub use super::ai::format_tokens;

#[derive(Debug, Clone)]
pub struct UsageRecord {
    /// 本地小时（0-23）：单日视图按小时聚合用
    pub hour: u8,
    /// 本地日期序号（now_ms / DAY_MS 的整数部分，按本地时区偏移修正过）
    pub day: i64,
    pub source: &'static str,
    pub model: String,
    pub output: i64,
    pub input: i64,
    /// 其中来自缓存命中的输入 token（prompt cache）
    pub cache_read: i64,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DailyRow {
    pub day: i64,
    pub output: i64,
    pub input: i64,
    /// 缓存命中的输入 token，用来算命中率
    pub cache_read: i64,
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

/// 读文件来源（DSH / WorkBuddy）。
///
/// 13,000+ 个文件、3 GB，全量解析要二三十秒。所以这里落一个磁盘索引：
/// 路径 → (mtime, 长度, 该文件的全部记录)。没变过的文件直接复用上次结果，
/// 变过的才重新解析；解析按 CPU 核心数并行（文件大小差异大，按字节数装箱分配）。
pub fn load_files_only(days: i64) -> Vec<UsageRecord> {
    let now = super::ai::now_ms();
    let from_ms = if days <= 0 {
        0
    } else {
        super::ai::local_midnight_ms(now) - (days - 1) * DAY_MS
    };
    let min_day = super::ai::day_index(from_ms) - 1;

    let home = home_dir();
    let roots = [
        (home.join(".dsh").join("sessions"), "zstd", SOURCE_DSH),
        (home.join(".workbuddy").join("projects"), "jsonl", SOURCE_WORKBUDDY),
        (home.join(".workbuddy-ai").join("projects"), "jsonl", SOURCE_WORKBUDDY_AI),
    ];

    // (路径, (mtime, 长度), 后缀, 来源)
    let mut files: Vec<(PathBuf, (u64, u64), &'static str, &'static str)> = Vec::new();
    for (root, suffix, source) in roots {
        if !root.exists() {
            continue;
        }

        files.extend(
            collect_stamped(&root, suffix, from_ms - DAY_MS)
                .into_iter()
                .map(|(path, stamp)| (path, stamp, suffix, source)),
        );
    }

    // 1) 索引里还新鲜的文件直接复用
    let mut index = Index::load();
    let mut entries: Vec<(PathBuf, (u64, u64), &'static str, Vec<UsageRecord>)> = Vec::new();
    let mut todo: Vec<(PathBuf, (u64, u64), &'static str, &'static str)> = Vec::new();
    // 有文件是新出现或变过的才值得重写索引（12 MB 写一遍 200ms+）
    let mut dirty = false;

    for (path, stamp, suffix, source) in files {
        match index.take(&path, stamp, source) {
            Some(records) => entries.push((path, stamp, source, records)),
            None => {
                dirty = true;
                todo.push((path, stamp, suffix, source));
            }
        }
    }

    // 2) 剩下的并行解析
    if !todo.is_empty() {
        let workers = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(4)
            .clamp(1, 8);

        // 按文件大小装箱：大文件先放，尽量让每个线程的总字节数接近
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); workers];
        let mut load = vec![0_u64; workers];
        let mut order: Vec<usize> = (0..todo.len()).collect();
        order.sort_by_key(|index| std::cmp::Reverse(todo[*index].1 .1));
        for index in order {
            let target = load
                .iter()
                .enumerate()
                .min_by_key(|(_, bytes)| **bytes)
                .map(|(slot, _)| slot)
                .unwrap_or(0);
            load[target] += todo[index].1 .1.max(1);
            buckets[target].push(index);
        }

        let mut parsed: Vec<Option<Vec<UsageRecord>>> = vec![None; todo.len()];
        let todo_ref = &todo;
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for bucket in &buckets {
                handles.push(scope.spawn(move || {
                    bucket
                        .iter()
                        .map(|&slot| {
                            let (path, _, suffix, source) = &todo_ref[slot];
                            let records = if *suffix == "zstd" {
                                parse_dsh(path, source)
                            } else {
                                parse_workbuddy(path, source)
                            };
                            (slot, records)
                        })
                        .collect::<Vec<_>>()
                }));
            }

            for handle in handles {
                if let Ok(part) = handle.join() {
                    for (slot, records) in part {
                        parsed[slot] = Some(records);
                    }
                }
            }
        });

        for (slot, (path, stamp, _, source)) in todo.into_iter().enumerate() {
            let records = parsed[slot].take().unwrap_or_default();
            entries.push((path, stamp, source, records));
        }
    }

    // 3) 先把索引写回去（下次打开就只剩目录扫描），再按天数过滤出结果
    if dirty {
        index.save(&entries);
    }

    let mut out = Vec::new();
    for (_, _, _, records) in entries {
        for record in records {
            if record.day >= min_day {
                out.push(record);
            }
        }
    }
    out
}

// ------------------------------------------------------------ 磁盘索引

const INDEX_VERSION: u32 = 2;
const INDEX_MAGIC: &[u8; 4] = b"NSUI";

struct IndexEntry {
    stamp: (u64, u64),
    source: &'static str,
    records: Vec<UsageRecord>,
}

/// 索引按「文件 mtime 的那一天」分片存。
///
/// 以前是一个 12 MB 的大文件，只要有一个会话文件变了就得整份重写（AI 一聊天就变）。
/// 现在只有今天这一片会被重写（几百 KB），历史那些天的字节原封不动。
/// 每片的头部存一个内容指纹，指纹没变就直接跳过——不用重算要不要写。
struct Index {
    entries: HashMap<PathBuf, IndexEntry>,
    /// 每片读进来时的内容指纹（day → hash）
    shards: HashMap<i64, u64>,
    /// 数据是从老的单文件索引读出来的：下次保存时顺手迁移
    legacy: bool,
}

impl Index {
    fn dir() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join("NetSpeedTaskbar").join("index"))
    }

    /// v1 的单文件索引（只在迁移时读一次）
    fn legacy_path() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join("NetSpeedTaskbar").join("usage-index.bin"))
    }

    fn shard_path(day: i64) -> Option<PathBuf> {
        Some(Self::dir()?.join(format!("usage-{day}.bin")))
    }

    fn load() -> Self {
        let mut entries = HashMap::new();
        let mut shards = HashMap::new();

        if let Some(dir) = Self::dir() {
            if let Ok(list) = std::fs::read_dir(&dir) {
                for item in list.flatten() {
                    let path = item.path();
                    let Some(day) = shard_day(&path) else {
                        continue;
                    };
                    if let Ok((hash, loaded)) = read_index(&path) {
                        shards.insert(day, hash);
                        entries.extend(loaded);
                    }
                }
            }
        }

        // 没有分片就找老索引：读完在下次保存时迁到分片目录
        let mut legacy = false;
        if entries.is_empty() {
            if let Some(path) = Self::legacy_path() {
                if let Ok((_, loaded)) = read_index(&path) {
                    legacy = !loaded.is_empty();
                    entries = loaded;
                }
            }
        }

        Self {
            entries,
            shards,
            legacy,
        }
    }

    /// 取出一个文件的记录：mtime、长度、来源都对得上才算新鲜
    fn take(&mut self, path: &Path, stamp: (u64, u64), source: &'static str) -> Option<Vec<UsageRecord>> {
        let entry = self.entries.remove(path)?;
        if entry.stamp != stamp || entry.source != source {
            return None;
        }
        Some(entry.records)
    }

    fn save(&mut self, entries: &[(PathBuf, (u64, u64), &'static str, Vec<UsageRecord>)]) {
        let Some(dir) = Self::dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);

        // 按 mtime 所在的那一天分组：只有今天写入的文件会落到今天的片上
        let mut groups: HashMap<i64, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            let day = (entry.1 .0 / 86_400) as i64;
            groups.entry(day).or_default().push(index);
        }

        let mut written: HashMap<i64, u64> = HashMap::new();
        for (day, picks) in &groups {
            let hash = fingerprint(entries, picks);
            if self.shards.get(day) == Some(&hash) {
                written.insert(*day, hash);
                continue;
            }

            let Some(path) = Self::shard_path(*day) else {
                continue;
            };
            let temp = path.with_extension("bin.tmp");
            let Ok(file) = std::fs::File::create(&temp) else {
                continue;
            };
            let mut writer = std::io::BufWriter::with_capacity(256 * 1024, file);
            let ok = write_index(&mut writer, hash, entries, picks).is_ok();
            if ok {
                use std::io::Write;
                let _ = writer.flush();
            }
            drop(writer);

            if ok && std::fs::rename(&temp, &path).is_ok() {
                written.insert(*day, hash);
                continue;
            }
            let _ = std::fs::remove_file(&temp);
        }

        // 已经不存在的那一天：把整片删掉
        for day in self.shards.keys() {
            if !groups.contains_key(day) {
                if let Some(path) = Self::shard_path(*day) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }

        // 老的单文件索引已经迁移完，删掉省 12 MB
        if self.legacy {
            if let Some(path) = Self::legacy_path() {
                let _ = std::fs::remove_file(path);
            }
            self.legacy = false;
        }

        self.shards = written;
    }
}

/// `usage-20772.bin` → 20772
fn shard_day(path: &Path) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("usage-")?.strip_suffix(".bin")?;
    rest.parse().ok()
}

/// 一片内容的指纹：路径、时间戳、来源、每条记录都参与
fn fingerprint(
    entries: &[(PathBuf, (u64, u64), &'static str, Vec<UsageRecord>)],
    picks: &[usize],
) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for index in picks {
        let (path, stamp, source, records) = &entries[*index];
        path.to_string_lossy().hash(&mut hasher);
        stamp.0.hash(&mut hasher);
        stamp.1.hash(&mut hasher);
        source_code(source).hash(&mut hasher);
        records.len().hash(&mut hasher);
        for record in records {
            record.day.hash(&mut hasher);
            record.hour.hash(&mut hasher);
            record.model.hash(&mut hasher);
            record.output.hash(&mut hasher);
            record.input.hash(&mut hasher);
            record.cache_read.hash(&mut hasher);
            record.duration_ms.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn source_code(source: &str) -> u8 {
    match source {
        SOURCE_DSH => 1,
        SOURCE_WORKBUDDY => 2,
        SOURCE_WORKBUDDY_AI => 3,
        _ => 0,
    }
}

fn source_of_code(code: u8) -> &'static str {
    match code {
        1 => SOURCE_DSH,
        2 => SOURCE_WORKBUDDY,
        3 => SOURCE_WORKBUDDY_AI,
        _ => SOURCE_OPENCODE,
    }
}

fn write_index(
    writer: &mut impl std::io::Write,
    hash: u64,
    entries: &[(PathBuf, (u64, u64), &'static str, Vec<UsageRecord>)],
    picks: &[usize],
) -> std::io::Result<()> {
    writer.write_all(INDEX_MAGIC)?;
    put_u32(writer, INDEX_VERSION)?;
    put_u64(writer, hash)?;
    put_u32(writer, picks.len() as u32)?;

    for index in picks {
        let (path, stamp, source, records) = &entries[*index];
        put_u8(writer, source_code(source))?;
        put_str(writer, &path.to_string_lossy())?;
        put_u64(writer, stamp.0)?;
        put_u64(writer, stamp.1)?;
        put_u32(writer, records.len() as u32)?;

        for record in records {
            put_i64(writer, record.day)?;
            put_u8(writer, record.hour)?;
            put_str(writer, &record.model)?;
            put_i64(writer, record.output)?;
            put_i64(writer, record.input)?;
            put_i64(writer, record.cache_read)?;
            put_i64(writer, record.duration_ms)?;
        }
    }

    Ok(())
}

fn read_index(path: &Path) -> std::io::Result<(u64, HashMap<PathBuf, IndexEntry>)> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != INDEX_MAGIC {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "magic"));
    }

    // v1 是单文件、没有指纹；读出来照用，下次保存会迁成 v2 分片
    let version = get_u32(&mut reader)?;
    if version != 1 && version != 2 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "version"));
    }
    let hash = if version >= 2 { get_u64(&mut reader)? } else { 0 };

    let count = get_u32(&mut reader)? as usize;
    let mut entries = HashMap::with_capacity(count);
    for _ in 0..count {
        let source = source_of_code(get_u8(&mut reader)?);
        let path = PathBuf::from(get_str(&mut reader)?);
        let stamp = (get_u64(&mut reader)?, get_u64(&mut reader)?);
        let record_count = get_u32(&mut reader)? as usize;

        let mut records = Vec::with_capacity(record_count);
        for _ in 0..record_count {
            records.push(UsageRecord {
                day: get_i64(&mut reader)?,
                hour: get_u8(&mut reader)?,
                source,
                model: get_str(&mut reader)?,
                output: get_i64(&mut reader)?,
                input: get_i64(&mut reader)?,
                cache_read: get_i64(&mut reader)?,
                duration_ms: get_i64(&mut reader)?,
            });
        }

        entries.insert(path, IndexEntry { stamp, source, records });
    }

    Ok((hash, entries))
}

fn put_u8(writer: &mut impl std::io::Write, value: u8) -> std::io::Result<()> {
    writer.write_all(&[value])
}

fn put_u32(writer: &mut impl std::io::Write, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn put_u64(writer: &mut impl std::io::Write, value: u64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn put_i64(writer: &mut impl std::io::Write, value: i64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn put_str(writer: &mut impl std::io::Write, value: &str) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    put_u32(writer, bytes.len() as u32)?;
    writer.write_all(bytes)
}

fn get_u8(reader: &mut impl std::io::Read) -> std::io::Result<u8> {
    let mut buf = [0_u8; 1];
    reader.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn get_u32(reader: &mut impl std::io::Read) -> std::io::Result<u32> {
    let mut buf = [0_u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn get_u64(reader: &mut impl std::io::Read) -> std::io::Result<u64> {
    let mut buf = [0_u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn get_i64(reader: &mut impl std::io::Read) -> std::io::Result<i64> {
    let mut buf = [0_u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

fn get_str(reader: &mut impl std::io::Read) -> std::io::Result<String> {
    let len = get_u32(reader)? as usize;
    // 防御坏文件：单条字符串上限 1 MB
    if len > 1024 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "string too long"));
    }
    let mut buf = vec![0_u8; len];
    reader.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
               IFNULL(json_extract(data,'$.tokens.input'),0) + IFNULL(json_extract(data,'$.tokens.cache.read'),0) + IFNULL(json_extract(data,'$.tokens.cache.write'),0), \
               IFNULL(json_extract(data,'$.tokens.cache.read'),0) \
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
            row.get::<_, i64>(5)?,
        ))
    }) else {
        return;
    };

    for (created, streamed, model, output, input, cache_read) in rows.flatten() {
        if output <= 0 && input <= 0 {
            continue;
        }

        records.push(UsageRecord {
            day: super::ai::day_index(created),
            hour: super::ai::local_hour(created),
            source: SOURCE_OPENCODE,
            model,
            output,
            input,
            cache_read,
            duration_ms: if streamed > created { streamed - created } else { 0 },
        });
    }
}

/// 遍历目录，返回 (路径, (mtime 秒, 长度))：索引比对和过滤都要用。
fn collect_stamped(root: &Path, suffix: &str, modified_after_ms: i64) -> Vec<(PathBuf, (u64, u64))> {
    // 和 AI 采集共用同一套目录枚举（一次拿齐元数据，见 ai::walk_files）
    super::ai::walk_files(root, suffix, modified_after_ms)
}

fn parse_dsh(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let Ok(decoder) = ruzstd::decoding::StreamingDecoder::new(file) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    let mut pending_start = 0_i64;

    for_each_line(BufReader::new(decoder), |line| {
        if line.contains("\"step/start\"") {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                pending_start = value["time"].as_i64().unwrap_or(0);
            }
            return;
        }

        if !line.contains("\"assistant/message\"") {
            return;
        }

        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };

        let usage = &value["data"]["usage"];
        let output = usage["outputTokens"].as_i64().unwrap_or(0);
        let cache_read = usage["cacheReadTokens"].as_i64().unwrap_or(0);
        let input = usage["inputTokens"].as_i64().unwrap_or(0)
            + cache_read
            + usage["cacheWriteTokens"].as_i64().unwrap_or(0);
        if output <= 0 && input <= 0 {
            return;
        }

        let time = value["time"].as_i64().unwrap_or(0);
        let model = value["data"]["message"]["source"]["model"]
            .as_str()
            .unwrap_or("")
            .to_string();

        records.push(UsageRecord {
            day: super::ai::day_index(time),
            hour: super::ai::local_hour(time),
            source,
            model,
            output,
            input,
            cache_read,
            duration_ms: if pending_start > 0 && time > pending_start {
                time - pending_start
            } else {
                0
            },
        });

        pending_start = 0;
    });

    records
}

fn parse_workbuddy(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let mut map: HashMap<String, UsageRecord> = HashMap::new();
    let mut last_ts = 0_i64;

    for_each_line(BufReader::new(file), |line| {
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
        // OpenAI 风格在 prompt_tokens_details.cached_tokens，Anthropic 风格单独一个字段
        let cache_read = raw["prompt_tokens_details"]["cached_tokens"]
            .as_i64()
            .or_else(|| raw["cache_read_input_tokens"].as_i64())
            .unwrap_or(0);
        if output <= 0 && input <= 0 {
            return;
        }

        let timestamp = value["timestamp"].as_i64().unwrap_or(0);
        let model = provider["model"]
            .as_str()
            .or_else(|| provider["requestModelId"].as_str())
            .unwrap_or("")
            .to_string();

        let entry = map.entry(message_id.to_string()).or_insert_with(|| UsageRecord {
            day: super::ai::day_index(timestamp),
            hour: super::ai::local_hour(timestamp),
            source,
            model: model.clone(),
            output: 0,
            input: 0,
            cache_read: 0,
            duration_ms: 0,
        });

        if output > entry.output {
            entry.output = output;
        }

        if input > entry.input {
            entry.input = input;
        }

        if cache_read > entry.cache_read {
            entry.cache_read = cache_read;
        }

        if model.len() > 0 {
            entry.model = model;
        }

        if timestamp > 0 {
            entry.day = super::ai::day_index(timestamp);
            entry.hour = super::ai::local_hour(timestamp);
            if last_ts > 0 && timestamp > last_ts && timestamp - last_ts < 180_000 {
                entry.duration_ms = timestamp - last_ts;
            }
        }

        if timestamp > last_ts {
            last_ts = timestamp;
        }
    });

    map.into_values().collect()
}

/// 逐行读文件（流式）：每个文件最多只占「一行」的内存，坏字节只影响一行
fn for_each_line<R: BufRead>(mut reader: R, mut visit: impl FnMut(&str)) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() > super::ai::MAX_LINE {
            continue;
        }
        let line = super::ai::trim_line(&buf);
        visit(line.as_ref());
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

// ---------------------------------------------------------------- 聚合

/// 按天汇总（包含没有数据的空缺天，方便直接画柱状图）。
pub fn summarize(records: &[UsageRecord], from_day: i64, to_day: i64) -> Vec<DailyRow> {
    summarize_filtered(records, from_day, to_day, None)
}

/// 同上，但可以只看某一个来源（`None` = 全部来源）。
pub fn summarize_filtered(
    records: &[UsageRecord],
    from_day: i64,
    to_day: i64,
    source: Option<&str>,
) -> Vec<DailyRow> {
    let mut map: HashMap<i64, DailyRow> = HashMap::new();
    let mut models: HashMap<i64, HashMap<String, i64>> = HashMap::new();

    for record in records {
        if record.day < from_day || record.day > to_day {
            continue;
        }

        if let Some(wanted) = source {
            if record.source != wanted {
                continue;
            }
        }

        let row = map.entry(record.day).or_insert_with(|| DailyRow {
            day: record.day,
            ..Default::default()
        });

        row.output += record.output;
        row.input += record.input;
        row.cache_read += record.cache_read;
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

/// 单日按小时的汇总行（固定 24 格，没数据的小时是 0）
#[derive(Debug, Clone, Default)]
pub struct HourRow {
    pub hour: u8,
    pub output: i64,
    pub input: i64,
    /// 缓存命中的输入 token
    pub cache_read: i64,
    pub requests: i64,
    pub measured_output: i64,
    pub measured_ms: i64,
    pub peak_speed: f64,
    pub main_model: String,
}

impl HourRow {
    pub fn speed(&self) -> f64 {
        if self.measured_ms > 0 {
            self.measured_output as f64 * 1000.0 / self.measured_ms as f64
        } else {
            0.0
        }
    }
}

/// 单日按小时汇总（今天/昨天视图用）
pub fn summarize_hours(
    records: &[UsageRecord],
    day: i64,
    source: Option<&str>,
) -> Vec<HourRow> {
    let mut rows: Vec<HourRow> = (0..24)
        .map(|hour| HourRow {
            hour: hour as u8,
            ..Default::default()
        })
        .collect();
    let mut models: Vec<HashMap<String, i64>> = vec![HashMap::new(); 24];

    for record in records {
        if record.day != day {
            continue;
        }
        if let Some(wanted) = source {
            if record.source != wanted {
                continue;
            }
        }

        let index = (record.hour as usize).min(23);
        let row = &mut rows[index];
        row.output += record.output;
        row.input += record.input;
        row.cache_read += record.cache_read;
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
            *models[index].entry(record.model.clone()).or_insert(0) += record.output;
        }
    }

    for (index, per_model) in models.into_iter().enumerate() {
        let mut best = 0;
        let mut name = String::new();
        for (model, output) in per_model {
            if output > best {
                best = output;
                name = model;
            }
        }
        rows[index].main_model = name;
    }

    rows
}

/// 单日按「小时 × 来源」的堆叠值（24 行）
pub fn hourly_by_source(
    records: &[UsageRecord],
    day: i64,
    sources: &[&'static str],
) -> Vec<Vec<i64>> {
    let mut rows = vec![vec![0_i64; sources.len()]; 24];
    for record in records {
        if record.day != day {
            continue;
        }
        if let Some(slot) = sources.iter().position(|item| *item == record.source) {
            rows[(record.hour as usize).min(23)][slot] += record.output;
        }
    }
    rows
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
            hour: 12,
            source,
            model: model.to_string(),
            output,
            input: output * 10,
            cache_read: 0,
            duration_ms: 1000,
        }
    }

    #[test]
    fn shard_names_roundtrip() {
        assert_eq!(shard_day(Path::new("C:/x/usage-20772.bin")), Some(20772));
        assert_eq!(shard_day(Path::new("C:/x/usage-20772.bin.tmp")), None);
        assert_eq!(shard_day(Path::new("C:/x/other.bin")), None);
    }

    #[test]
    fn fingerprint_tracks_every_field() {
        let base = vec![(
            PathBuf::from("C:/a.jsonl"),
            (100_u64, 200_u64),
            SOURCE_WORKBUDDY_AI,
            vec![record(10, SOURCE_WORKBUDDY_AI, "m", 100)],
        )];
        let picks = vec![0];
        let original = fingerprint(&base, &picks);

        // 同一份数据指纹稳定（否则每次启动都会重写所有分片）
        assert_eq!(original, fingerprint(&base, &picks));

        // 时间戳、长度、记录内容任何一项变了都得换指纹
        let mut touched = base.clone();
        touched[0].1 .1 += 1;
        assert_ne!(original, fingerprint(&touched, &picks));

        let mut rewritten = base.clone();
        rewritten[0].3[0].output += 1;
        assert_ne!(original, fingerprint(&rewritten, &picks));
    }

    #[test]
    fn index_roundtrip_preserves_entries() {
        let entries = vec![
            (
                PathBuf::from("C:/x/a.jsonl"),
                (1_700_000_000_u64, 1234_u64),
                SOURCE_DSH,
                vec![record(10, SOURCE_DSH, "m1", 11)],
            ),
            (
                PathBuf::from("C:/x/b.jsonl"),
                (1_700_000_100_u64, 99_u64),
                SOURCE_WORKBUDDY,
                vec![record(11, SOURCE_WORKBUDDY, "m2", 22)],
            ),
        ];

        let mut buffer = Vec::new();
        let hash = fingerprint(&entries, &[0, 1]);
        write_index(&mut buffer, hash, &entries, &[0, 1]).unwrap();

        let dir = std::env::temp_dir().join("nst-index-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("usage-test.bin");
        std::fs::write(&path, &buffer).unwrap();

        let (read_hash, loaded) = read_index(&path).unwrap();
        assert_eq!(read_hash, hash);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[&PathBuf::from("C:/x/b.jsonl")].records[0].output, 22);
        let _ = std::fs::remove_file(path);
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
