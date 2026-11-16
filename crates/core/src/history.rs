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

pub const SOURCE_CLAUDE: &str = "claude";
pub const SOURCE_CODEX: &str = "codex";

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
    /// 消息 id 的哈希（0 = 没有）。只用来跨文件去重：
    /// 会话被续写/分叉时同一条消息可能出现在两个转录文件里
    pub id: u64,
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
        SOURCE_CLAUDE => "Claude Code",
        SOURCE_CODEX => "Codex",
        _ => "OpenCode",
    }
}

pub fn source_color(source: &str) -> u32 {
    // 0x00BBGGRR（GDI COLORREF）
    match source {
        SOURCE_DSH => 0x00FF_617B,
        SOURCE_WORKBUDDY => 0x008C_A92F,
        SOURCE_WORKBUDDY_AI => 0x0032_8AE0,
        SOURCE_CLAUDE => 0x00B0_6A3C, // 暖橙棕
        SOURCE_CODEX => 0x00C8_C8_C8, // 浅灰
        _ => 0x00D8_8440,
    }
}

pub fn all_sources() -> [&'static str; 6] {
    [
        SOURCE_OPENCODE,
        SOURCE_DSH,
        SOURCE_WORKBUDDY,
        SOURCE_WORKBUDDY_AI,
        SOURCE_CLAUDE,
        SOURCE_CODEX,
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
        (home.join(".claude").join("projects"), "jsonl", SOURCE_CLAUDE),
        (home.join(".codex").join("sessions"), "jsonl", SOURCE_CODEX),
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
                            let (path, _, _suffix, source) = &todo_ref[slot];
                            let records = match *source {
                                SOURCE_DSH => parse_dsh(path, source),
                                SOURCE_CLAUDE => parse_claude(path, source),
                                SOURCE_CODEX => parse_codex(path, source),
                                _ => parse_workbuddy(path, source),
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
    //    有文件变动才值得重写索引
    if dirty {
        index.save(&entries, from_ms);
    }

    let mut out = Vec::new();
    for (_, _, _, records) in entries {
        for record in records {
            if record.day >= min_day {
                out.push(record);
            }
        }
    }
    dedupe_records(out)
}

/// 同一个消息 id 出现在多个文件里（会话续写/分叉/子代理转录）时只留一份。
///
/// 引擎侧本来就按 id 覆盖，历史这边是「按文件」累加的，不去重会多算 ——
/// WorkBuddy（AI）尤其明显：一次会话会被复制进好几个 `*.jsonl`，
/// 整份 2.4B 的输入里有 1.7B 是同一批消息的第二、第三份。
/// 只对带 id 的记录生效（WorkBuddy 用 messageId 的哈希，其它来源 id 恒为 0）。
fn dedupe_records(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    use std::collections::HashMap;

    let mut index: HashMap<(u64, &'static str), usize> = HashMap::new();
    let mut out: Vec<UsageRecord> = Vec::with_capacity(records.len());
    for record in records {
        if record.id == 0 {
            out.push(record);
            continue;
        }

        match index.get(&(record.id, record.source)) {
            // 重复的那一份不一定更完整：留「用量更大」的那条
            Some(&slot) => {
                let kept = &mut out[slot];
                if record.output + record.input > kept.output + kept.input {
                    *kept = record;
                }
            }
            None => {
                index.insert((record.id, record.source), out.len());
                out.push(record);
            }
        }
    }
    out
}

/// 消息 id → u64（FNV-1a，0 保留给「没有 id」）
fn hash_id(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash.max(1)
}

// ------------------------------------------------------------ 磁盘索引

const INDEX_VERSION: u32 = 4;
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
                    if let Ok((version, hash, loaded)) = read_index(&path) {
                        if version < INDEX_VERSION {
                            // 旧口径的记录不能继续用（比如 WorkBuddy 的记录还没带
                            // messageId，跨文件去重做不了）：整片删掉，
                            // 这些文件下次被扫到时重新解析一遍
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                        shards.insert(day, hash);
                        entries.extend(loaded);
                    }
                }
            }
        }

        // 没有分片就找老索引：读完在下次保存时迁到分片目录。
        // 老格式（没带 messageId）里可能已经有重复记录，只删不迁，让它重新扫一遍
        let mut legacy = false;
        if entries.is_empty() {
            if let Some(path) = Self::legacy_path() {
                if let Ok((version, _, loaded)) = read_index(&path) {
                    if version >= INDEX_VERSION && !loaded.is_empty() {
                        legacy = true;
                        entries = loaded;
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
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

    /// `from_ms`：这次扫描的起始时间（用来判断哪些老分片不该动）
    fn save(
        &mut self,
        entries: &[(PathBuf, (u64, u64), &'static str, Vec<UsageRecord>)],
        from_ms: i64,
    ) {
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

        // 已经不存在的那一天：把整片删掉。
        // 注意只删「这次扫过的时间范围」里的片：窗口调小（比如只看今天）时，
        // 更早那些天根本没扫，不能把它们的缓存一起清掉，否则下次看长区间要全量重解析
        let oldest_day = ((from_ms - 86_400_000) / 1000 / 86_400) as i64;
        for day in self.shards.keys() {
            if *day < oldest_day {
                continue;
            }
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
            record.id.hash(&mut hasher);
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
            put_u64(writer, record.id)?;
        }
    }

    Ok(())
}

fn read_index(path: &Path) -> std::io::Result<(u32, u64, HashMap<PathBuf, IndexEntry>)> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);

    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != INDEX_MAGIC {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "magic"));
    }

    // v1 是单文件、没有指纹；读出来照用，下次保存会迁成 v2 分片
    let version = get_u32(&mut reader)?;
    if !(1..=INDEX_VERSION).contains(&version) {
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
            let day = get_i64(&mut reader)?;
            let hour = get_u8(&mut reader)?;
            let model = get_str(&mut reader)?;
            let output = get_i64(&mut reader)?;
            let input = get_i64(&mut reader)?;
            let cache_read = get_i64(&mut reader)?;
            let duration_ms = get_i64(&mut reader)?;
            // v3 起多了消息 id（跨文件去重用）；老分片按 0 读
            let id = if version >= 3 { get_u64(&mut reader)? } else { 0 };
            records.push(UsageRecord {
                day,
                hour,
                source,
                model,
                output,
                input,
                cache_read,
                duration_ms,
                id,
            });
        }

        entries.insert(path, IndexEntry { stamp, source, records });
    }

    Ok((version, hash, entries))
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
            id: 0,
        });
    }
}

/// 遍历目录，返回 (路径, (mtime 秒, 长度))：索引比对和过滤都要用。
fn collect_stamped(root: &Path, suffix: &str, modified_after_ms: i64) -> Vec<(PathBuf, (u64, u64))> {
    // 和 AI 采集共用同一套目录枚举（一次拿齐元数据，见 ai::walk_files）
    super::ai::walk_files(root, suffix, modified_after_ms)
}

fn parse_dsh(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(reader) = super::ai::ZstdFrames::open(path) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    let mut pending_start = 0_i64;

    for_each_line(BufReader::new(reader), |line| {
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
            id: 0,
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
            // 带 id：同一个 messageId 会被 WorkBuddy 写进多个会话文件
            // （分叉 / 续写 / 子代理），跨文件去重全靠它
            id: hash_id(message_id),
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

/// Claude Code 的会话转录（`~/.claude/projects/**/*.jsonl`）。
///
/// 同一条消息流式写入多次（用量递增），按 `message.id` 合并取最大值。
fn parse_claude(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let mut map: HashMap<String, UsageRecord> = HashMap::new();
    let mut last_ts = 0_i64;

    for_each_line(BufReader::new(file), |line| {
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

        let Some(timestamp) = value["timestamp"].as_str().and_then(super::ai::parse_iso_ms) else {
            return;
        };

        let id = value["message"]["id"]
            .as_str()
            .or_else(|| value["requestId"].as_str())
            .unwrap_or("");
        if id.is_empty() {
            return;
        }

        let model = value["message"]["model"].as_str().unwrap_or("").to_string();
        let entry = map.entry(id.to_string()).or_insert_with(|| UsageRecord {
            day: super::ai::day_index(timestamp),
            hour: super::ai::local_hour(timestamp),
            source,
            model: model.clone(),
            output: 0,
            input: 0,
            cache_read: 0,
            duration_ms: 0,
            id: hash_id(id),
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
        if !model.is_empty() {
            entry.model = model;
        }

        entry.day = super::ai::day_index(timestamp);
        entry.hour = super::ai::local_hour(timestamp);
        if last_ts > 0 && timestamp > last_ts && timestamp - last_ts < 120_000 {
            entry.duration_ms = timestamp - last_ts;
        }
        if timestamp > last_ts {
            last_ts = timestamp;
        }
    });

    map.into_values().collect()
}

/// Codex 的会话转录（`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`）。
///
/// 每轮结束写一条 `token_count`，`info.last_token_usage` 是这一轮的增量。
fn parse_codex(path: &Path, source: &'static str) -> Vec<UsageRecord> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    let mut last_ts = 0_i64;

    for_each_line(BufReader::new(file), |line| {
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
        let cache_read = usage["cached_input_tokens"]
            .as_i64()
            .or_else(|| usage["cache_read_input_tokens"].as_i64())
            .unwrap_or(0);
        let input = usage["input_tokens"].as_i64().unwrap_or(0);
        if output <= 0 && input <= 0 {
            return;
        }

        let Some(timestamp) = value["timestamp"].as_str().and_then(super::ai::parse_iso_ms) else {
            return;
        };

        let model = payload["info"]["model"]
            .as_str()
            .or_else(|| payload["model"].as_str())
            .unwrap_or("")
            .to_string();

        records.push(UsageRecord {
            day: super::ai::day_index(timestamp),
            hour: super::ai::local_hour(timestamp),
            source,
            model,
            output,
            input,
            cache_read,
            duration_ms: if last_ts > 0 && timestamp > last_ts && timestamp - last_ts < 300_000 {
                timestamp - last_ts
            } else {
                0
            },
            id: 0,
        });

        last_ts = timestamp;
    });

    records
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
    summarize_filtered(records, from_day, to_day, &all_sources())
}

/// 同上，但只统计 `sources` 里的来源（传 `all_sources()` 就是全部）。
pub fn summarize_filtered(
    records: &[UsageRecord],
    from_day: i64,
    to_day: i64,
    sources: &[&'static str],
) -> Vec<DailyRow> {
    let mut map: HashMap<i64, DailyRow> = HashMap::new();
    let mut models: HashMap<i64, HashMap<String, i64>> = HashMap::new();

    for record in records {
        if record.day < from_day || record.day > to_day {
            continue;
        }

        if !sources.contains(&record.source) {
            continue;
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
    sources: &[&'static str],
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
        if !sources.contains(&record.source) {
            continue;
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
            id: 0,
        }
    }

    #[test]
    fn summarize_respects_source_subset() {
        let records = vec![
            record(5, SOURCE_OPENCODE, "a", 100),
            record(5, SOURCE_WORKBUDDY_AI, "b", 50),
            record(6, SOURCE_DSH, "c", 7),
        ];

        let only_ai = [SOURCE_WORKBUDDY_AI];
        assert_eq!(summarize_filtered(&records, 5, 6, &only_ai)[0].output, 50);
        assert_eq!(summarize_filtered(&records, 5, 6, &only_ai)[1].output, 0);

        let two = [SOURCE_OPENCODE, SOURCE_DSH];
        let rows = summarize_filtered(&records, 5, 6, &two);
        assert_eq!(rows[0].output, 100);
        assert_eq!(rows[1].output, 7);

        let all = all_sources();
        assert_eq!(summarize_filtered(&records, 5, 6, &all)[0].output, 150);

        // 小时视图同一套语义
        let hours = summarize_hours(&records, 5, &only_ai);
        assert_eq!(hours[12].output, 50);
    }

    #[test]
    fn claude_file_parses_into_records() {
        let dir = std::env::temp_dir().join("nst-claude-parse");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("session.jsonl");
        let body = concat!(
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"claude-x\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"cache_read_input_tokens\":40}},\"timestamp\":\"2026-11-15T10:00:00.000Z\"}\n",
            "{\"type\":\"user\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"claude-x\",\"usage\":{\"input_tokens\":100,\"output_tokens\":55,\"cache_read_input_tokens\":40}},\"timestamp\":\"2026-11-15T10:00:03.000Z\"}\n"
        );
        std::fs::write(&path, body).unwrap();

        let records = parse_claude(&path, SOURCE_CLAUDE);
        assert_eq!(records.len(), 1, "同一条消息合并成一条");
        assert_eq!(records[0].output, 55);
        assert_eq!(records[0].input, 140);
        assert_eq!(records[0].cache_read, 40);
        assert_eq!(records[0].model, "claude-x");
        assert_eq!(records[0].duration_ms, 3000);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn codex_file_parses_into_records() {
        let dir = std::env::temp_dir().join("nst-codex-parse");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollout.jsonl");
        let body = concat!(
            "{\"timestamp\":\"2026-11-15T10:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":200,\"cached_input_tokens\":50,\"output_tokens\":11}}}}\n",
            "{\"timestamp\":\"2026-11-15T10:00:04.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":10,\"output_tokens\":7}}}}\n"
        );
        std::fs::write(&path, body).unwrap();

        let records = parse_codex(&path, SOURCE_CODEX);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].output, 11);
        assert_eq!(records[0].cache_read, 50);
        assert_eq!(records[1].duration_ms, 4000);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dsh_reads_every_appended_frame() {
        let dir = std::env::temp_dir().join("nst-dsh-frames");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("session.v4.jsonl.zstd");
        let frame = |line: &str| {
            ruzstd::encoding::compress_to_vec(
                format!("{line}\n").as_bytes(),
                ruzstd::encoding::CompressionLevel::Uncompressed,
            )
        };
        let message = |seq: i64, time: i64, output: i64| {
            format!(
                r#"{{"type":"assistant/message","seq":{seq},"time":{time},"data":{{"usage":{{"inputTokens":10,"outputTokens":{output}}},"message":{{"source":{{"model":"m"}}}}}}}}"#
            )
        };
        let mut bytes = frame(&message(1, 1_000, 10));
        bytes.extend(frame(&message(2, 2_000, 20)));
        std::fs::write(&path, &bytes).unwrap();

        let records = parse_dsh(&path, SOURCE_DSH);
        assert_eq!(records.len(), 2, "追加的第二帧也要读到");
        assert_eq!(records[0].output, 10);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn workbuddy_message_ids_dedupe_across_files() {
        let dir = std::env::temp_dir().join("nst-wb-dedupe");
        let _ = std::fs::create_dir_all(&dir);
        let line = |id: &str, input: i64, output: i64| {
            serde_json::json!({
                "providerData": {
                    "messageId": id,
                    "model": "m",
                    "rawUsage": {"prompt_tokens": input, "completion_tokens": output}
                },
                "timestamp": 1_763_000_000_000_i64
            })
            .to_string()
        };
        let first = dir.join("a.jsonl");
        let second = dir.join("b.jsonl");
        // 会话分叉 / 续写时，同一批 messageId 会被写进多个文件
        std::fs::write(&first, format!("{}\n{}\n", line("m1", 100, 10), line("m2", 200, 20)))
            .unwrap();
        std::fs::write(&second, format!("{}\n", line("m1", 100, 10))).unwrap();

        let mut records = parse_workbuddy(&first, SOURCE_WORKBUDDY_AI);
        records.extend(parse_workbuddy(&second, SOURCE_WORKBUDDY_AI));
        assert!(
            records.iter().all(|record| record.id != 0),
            "WorkBuddy 记录要带 messageId，否则跨文件没法去重"
        );

        let merged = dedupe_records(records);
        assert_eq!(merged.len(), 2, "跨文件重复的消息只算一次");
        assert_eq!(merged.iter().map(|record| record.input).sum::<i64>(), 300);
        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
    }

    #[test]
    fn duplicate_message_ids_are_merged() {        let mut first = record(5, SOURCE_CLAUDE, "claude-x", 100);
        first.id = 42;
        let mut same = first.clone();
        same.output = 120;
        let unique = record(5, SOURCE_CLAUDE, "claude-x", 7);

        let out = dedupe_records(vec![first, same, unique]);
        assert_eq!(out.len(), 2, "同 id 只留一条，不同 id 都保留");
        // 重复的那一份可能是流式写到一半的旧值，留用量更大的那条
        assert_eq!(out[0].output, 120);
        assert_eq!(out[1].output, 7);
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

        let (version, read_hash, loaded) = read_index(&path).unwrap();
        assert_eq!(version, INDEX_VERSION);
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
