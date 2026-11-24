//! 每个进程的实时网速：ETW 内核网络事件（`Microsoft-Windows-Kernel-Network`）。
//!
//! Windows 只在「内核网络事件」里把字节数和进程 ID 放在一起报，而开这个
//! ETW 会话需要管理员权限（普通权限 `StartTrace` 直接 `Access is denied`）。
//! 所以这里只做「采集 + 汇总」，正常使用时跑在同 exe 的提权助手进程里
//! （`netspeed.exe --netproc-helper`），面板侧通过命名管道读结果。
//!
//! 事件号和载荷布局来自该 provider 的清单（Windows 10 18990 版）：
//!
//! | 事件 | 含义 | 载荷开头 |
//! | ---- | ---- | -------- |
//! | 10   | TCP 发送（IPv4） | PID(u32) size(u32) … |
//! | 11   | TCP 接收（IPv4） | PID(u32) size(u32) … |
//! | 26   | TCP 发送（IPv6） | PID(u32) size(u32) … |
//! | 27   | TCP 接收（IPv6） | PID(u32) size(u32) … |
//! | 42   | UDP 发送（IPv4） | PID(u32) size(u32) … |
//! | 43   | UDP 接收（IPv4） | PID(u32) size(u32) … |
//! | 58   | UDP 发送（IPv6） | PID(u32) size(u32) … |
//! | 59   | UDP 接收（IPv6） | PID(u32) size(u32) … |
//!
//! 重传（14/30）和「协议代拷」事件（18/34）也带 size，但那是同一份流量，
//! 一起加会把数字放大，所以只认上面这八个。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS,
};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE,
    PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, WNODE_FLAG_TRACED_GUID,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};

use windows_sys::core::GUID;

/// 会话名：助手被强杀时会话可能还挂着，下次启动按这个名字先停掉再重建。
const SESSION_NAME: &str = "NetSpeedTaskbarNetProc";

/// `Microsoft-Windows-Kernel-Network` 的 provider GUID。
const KERNEL_NETWORK: GUID = GUID {
    data1: 0x7dd42a49,
    data2: 0x5329,
    data3: 0x4832,
    data4: [0x8d, 0xfd, 0x43, 0xd9, 0x79, 0x15, 0x3a, 0x88],
};

/// 管道里每个方向带几条（面板上的气泡只画得下三条 + 「其他」）。
pub const TOP: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

/// 一个进程在这个采样周期里的平均速率。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcTraffic {
    pub name: String,
    pub pid: u32,
    pub bytes_per_sec: f64,
}

/// 一次采样：上行 / 下行的前几名 + 全体合计（面板按合计算占比）。
#[derive(Debug, Clone, Default)]
pub struct NetProcessSnapshot {
    pub up: Vec<ProcTraffic>,
    pub down: Vec<ProcTraffic>,
    pub total_up: f64,
    pub total_down: f64,
}

/// 助手 → 面板的管道报文（一行一条 JSON）。
///
/// 只用「进程名 + 速率」：PID 会复用，跨进程传名字更稳；面板也不需要 PID。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WireReport {
    pub up: Vec<(String, f64)>,
    pub down: Vec<(String, f64)>,
    #[serde(default)]
    pub up_total: f64,
    #[serde(default)]
    pub down_total: f64,
    /// 助手起不来时把原因带给面板（悬停气泡照实说，别假装没数据）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WireReport {
    pub fn from_snapshot(snapshot: &NetProcessSnapshot) -> Self {
        let pairs = |list: &[ProcTraffic]| -> Vec<(String, f64)> {
            list.iter()
                .map(|item| (item.name.clone(), item.bytes_per_sec))
                .collect()
        };
        Self {
            up: pairs(&snapshot.up),
            down: pairs(&snapshot.down),
            up_total: snapshot.total_up,
            down_total: snapshot.total_down,
            error: None,
        }
    }

    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        line.push('\n');
        line
    }

    pub fn from_line(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
}

/// 事件号 → 方向。只认清单里的这八个（见模块头的表）。
fn direction_of(id: u16) -> Option<Direction> {
    match id {
        10 | 26 | 42 | 58 => Some(Direction::Up),
        11 | 27 | 43 | 59 => Some(Direction::Down),
        _ => None,
    }
}

/// 事件载荷的前 8 字节：`{ PID: u32, size: u32 }`（IPv4 / IPv6 模板都一样）。
///
/// 顺手做两道保险：PID 为 0、size 为 0 或大得不像单个报文的，直接丢——
/// 万一哪天 provider 换了载荷布局，宁可少算也不要算成天文数字。
fn parse_packet(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    if pid == 0 || size == 0 || size > 1024 * 1024 {
        return None;
    }
    Some((pid, size))
}

/// IPv4 地址段里是不是回环（127.0.0.0/8）。
///
/// 网络序和整数序谁先谁后各家实现不一致，首/尾字节都认一下：
/// 真回环两种顺序都能认出来，误伤只可能落在「地址里有个 127 的字节」这种少数情况。
fn ipv4_loopback(address: &[u8]) -> bool {
    address.len() >= 4 && (address[0] == 127 || address[3] == 127)
}

/// IPv6 的回环：`::1` 或者 IPv4 映射过来的 `::ffff:127.x.x.x`。
fn ipv6_loopback(address: &[u8]) -> bool {
    if address.len() < 16 {
        return false;
    }
    let tail = &address[..16];
    if tail[..15].iter().all(|byte| *byte == 0) && tail[15] == 1 {
        return true;
    }
    tail[..10].iter().all(|byte| *byte == 0) && tail[10] == 0xff && tail[11] == 0xff && tail[12] == 127
}

/// 这条事件是不是本机内部回环流量。
///
/// 回环（本机开发服务器、Electron 各进程之间的 socket）也是网络事件，
/// 但网卡的字节计数里没有它——不排掉的话「进程明细」的合计会比面板上的
/// 网速大出好几倍，占比也会被本地进程带偏。
fn is_loopback(id: u16, data: &[u8]) -> bool {
    match id {
        // IPv4：PID, size, daddr(4), saddr(4)
        10 | 11 | 42 | 43 => {
            data.len() >= 16 && (ipv4_loopback(&data[8..12]) || ipv4_loopback(&data[12..16]))
        }
        // IPv6：PID, size, daddr(16), saddr(16)
        26 | 27 | 58 | 59 => {
            data.len() >= 40 && (ipv6_loopback(&data[8..24]) || ipv6_loopback(&data[24..40]))
        }
        _ => false,
    }
}

/// 全局累计计数器：ETW 回调线程写，采样侧读。
fn counters() -> &'static Mutex<HashMap<u32, (u64, u64)>> {
    static COUNTERS: OnceLock<Mutex<HashMap<u32, (u64, u64)>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// ETW 回调：每来一个报文就把 (PID, size) 累进对应方向。
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let record = &*record;
    let Some(direction) = direction_of(record.EventHeader.EventDescriptor.Id) else {
        return;
    };
    if record.UserData.is_null() || (record.UserDataLength as usize) < 8 {
        return;
    }
    let data = std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize);
    let Some((pid, size)) = parse_packet(data) else {
        return;
    };
    if is_loopback(record.EventHeader.EventDescriptor.Id, data) {
        return;
    }

    if let Ok(mut map) = counters().lock() {
        let entry = map.entry(pid).or_insert((0, 0));
        match direction {
            Direction::Up => entry.0 += size as u64,
            Direction::Down => entry.1 += size as u64,
        }
    }
}

/// 一块按 8 字节对齐的 `EVENT_TRACE_PROPERTIES` + 会话名缓冲。
///
/// `EVENT_TRACE_PROPERTIES` 里有 u64，`Vec<u8>` 只保证 1 字节对齐，
/// 直接当结构体写是未对齐访问，所以按 u64 开内存。
struct Props {
    words: Vec<u64>,
}

impl Props {
    fn new(name: &[u16], log_mode: u32, buffer_kb: u32, flush_seconds: u32) -> Self {
        let base = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let total = base + name.len() * 2;
        let mut words = vec![0u64; total.div_ceil(8)];

        unsafe {
            let bytes = words.as_mut_ptr() as *mut u8;
            let props = bytes as *mut EVENT_TRACE_PROPERTIES;
            (*props).Wnode.BufferSize = total as u32;
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            // 1 = QPC 时间戳
            (*props).Wnode.ClientContext = 1;
            (*props).LogFileMode = log_mode;
            // 实时会话必须攒够一点才投递；1 秒够用，再慢气泡就明显滞后
            (*props).FlushTimer = flush_seconds;
            (*props).BufferSize = buffer_kb;
            (*props).LoggerNameOffset = base as u32;
            std::ptr::copy_nonoverlapping(name.as_ptr(), bytes.add(base) as *mut u16, name.len());
        }

        Self { words }
    }

    fn as_ptr(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.words.as_mut_ptr() as *mut _
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn stop_session(session: CONTROLTRACE_HANDLE, name: &[u16]) {
    let mut props = Props::new(name, 0, 0, 0);
    unsafe {
        // 有会话句柄就按句柄停；没有（上一次助手崩了）就按名字停
        ControlTraceW(session, name.as_ptr(), props.as_ptr(), EVENT_TRACE_CONTROL_STOP);
    }
}

fn describe(code: u32) -> String {
    match code {
        ERROR_ACCESS_DENIED => "需要管理员权限".to_string(),
        2 => "系统里找不到网络事件源".to_string(),
        _ => format!("网络事件会话启动失败（错误码 {code}）"),
    }
}

/// 进程名（去掉路径和 `.exe`，任务管理器那种叫法）。
fn process_name(pid: u32) -> String {
    if pid == 4 {
        return "System".to_string();
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return format!("PID {pid}");
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 {
            return format!("PID {pid}");
        }
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        let file = path.rsplit(['\\', '/']).next().unwrap_or("").trim();
        let stem = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE"));
        let name = stem.unwrap_or(file);
        if name.is_empty() {
            format!("PID {pid}")
        } else {
            name.to_string()
        }
    }
}

/// 提权助手侧的采集器：开一个实时 ETW 会话，按固定间隔给出每个进程的速率。
pub struct NetProcessWatcher {
    session: CONTROLTRACE_HANDLE,
    name: Vec<u16>,
    trace: Option<PROCESSTRACE_HANDLE>,
    thread: Option<std::thread::JoinHandle<()>>,
    previous: HashMap<u32, (u64, u64)>,
    last: Instant,
    names: HashMap<u32, String>,
}

impl NetProcessWatcher {
    /// 开会话并开始收事件。普通权限返回 `Err("需要管理员权限")`。
    pub fn start() -> Result<Self, String> {
        let name = wide(SESSION_NAME);

        let mut props = Props::new(&name, EVENT_TRACE_REAL_TIME_MODE, 64, 1);
        let mut session = CONTROLTRACE_HANDLE { Value: 0 };
        let mut status = unsafe { StartTraceW(&mut session, name.as_ptr(), props.as_ptr()) };

        if status == ERROR_ALREADY_EXISTS {
            // 上一次助手是崩溃退出的：会话还挂在系统里，先收掉再重开
            stop_session(CONTROLTRACE_HANDLE { Value: 0 }, &name);
            let mut retry = Props::new(&name, EVENT_TRACE_REAL_TIME_MODE, 64, 1);
            status = unsafe { StartTraceW(&mut session, name.as_ptr(), retry.as_ptr()) };
        }

        if status != ERROR_SUCCESS {
            return Err(describe(status));
        }

        let enabled = unsafe {
            EnableTraceEx2(
                session,
                &KERNEL_NETWORK,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                // 5 = Verbose：这些事件本身是 Informational，别被级别过滤掉
                5,
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        if enabled != ERROR_SUCCESS {
            stop_session(session, &name);
            return Err(describe(enabled));
        }

        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        logfile.LoggerName = name.as_ptr() as *mut u16;
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);

        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == 0 || trace.Value == u64::MAX {
            stop_session(session, &name);
            return Err("网络事件会话打不开".to_string());
        }

        // ProcessTrace 一直阻塞到会话被停掉，只能自己占一条线程
        let handles = [trace];
        let thread = match std::thread::Builder::new()
            .name("netproc-etw".to_string())
            .spawn(move || unsafe {
                ProcessTrace(handles.as_ptr(), 1, std::ptr::null(), std::ptr::null());
            })
        {
            Ok(thread) => thread,
            Err(error) => {
                stop_session(session, &name);
                unsafe { CloseTrace(trace) };
                return Err(format!("接收线程起不来：{error}"));
            }
        };

        // 上一轮残留的计数器（同进程里重启采集时）清掉
        if let Ok(mut map) = counters().lock() {
            map.clear();
        }

        Ok(Self {
            session,
            name,
            trace: Some(trace),
            thread: Some(thread),
            previous: HashMap::new(),
            last: Instant::now(),
            names: HashMap::new(),
        })
    }

    /// 采一次：把累计计数器换成「距上次采样这段时间的平均速率」。
    pub fn sample(&mut self) -> NetProcessSnapshot {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64().clamp(0.05, 10.0);
        self.last = now;

        let totals: HashMap<u32, (u64, u64)> = match counters().lock() {
            Ok(map) => map.iter().map(|(pid, value)| (*pid, *value)).collect(),
            Err(_) => return NetProcessSnapshot::default(),
        };

        let mut up: Vec<(u32, f64)> = Vec::new();
        let mut down: Vec<(u32, f64)> = Vec::new();
        let mut total_up = 0.0;
        let mut total_down = 0.0;

        for (pid, (bytes_up, bytes_down)) in &totals {
            let (previous_up, previous_down) = self.previous.get(pid).copied().unwrap_or((0, 0));
            // 计数器理论上只增不减；真回绕（休眠/换网卡）就按 0 算，别冒出负数
            let up_rate = bytes_up.saturating_sub(previous_up) as f64 / elapsed;
            let down_rate = bytes_down.saturating_sub(previous_down) as f64 / elapsed;
            total_up += up_rate;
            total_down += down_rate;
            if up_rate >= 1.0 {
                up.push((*pid, up_rate));
            }
            if down_rate >= 1.0 {
                down.push((*pid, down_rate));
            }
        }
        self.previous = totals;

        up.sort_by(|a, b| b.1.total_cmp(&a.1));
        down.sort_by(|a, b| b.1.total_cmp(&a.1));
        up.truncate(TOP);
        down.truncate(TOP);

        let mut snapshot = NetProcessSnapshot {
            total_up,
            total_down,
            ..Default::default()
        };
        snapshot.up = up
            .into_iter()
            .map(|(pid, rate)| ProcTraffic {
                name: self.name_of(pid),
                pid,
                bytes_per_sec: rate,
            })
            .collect();
        snapshot.down = down
            .into_iter()
            .map(|(pid, rate)| ProcTraffic {
                name: self.name_of(pid),
                pid,
                bytes_per_sec: rate,
            })
            .collect();

        // PID 会复用：同一时刻最多几十个进程在跑，缓存超过 512 条就整个作废
        if self.names.len() > 512 {
            self.names.clear();
        }
        snapshot
    }

    fn name_of(&mut self, pid: u32) -> String {
        if let Some(name) = self.names.get(&pid) {
            return name.clone();
        }
        let name = process_name(pid);
        self.names.insert(pid, name.clone());
        name
    }
}

impl Drop for NetProcessWatcher {
    fn drop(&mut self) {
        // 先停会话：ProcessTrace 才会返回，然后才能 CloseTrace
        stop_session(self.session, &self.name);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(trace) = self.trace.take() {
            if trace.Value != 0 && trace.Value != u64::MAX {
                unsafe { CloseTrace(trace) };
            }
        }
        if let Ok(mut map) = counters().lock() {
            map.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u32, size: u32) -> [u8; 8] {
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&pid.to_le_bytes());
        data[4..].copy_from_slice(&size.to_le_bytes());
        data
    }

    #[test]
    fn directions_match_manifest() {
        // TCP / UDP × IPv4 / IPv6 的「发」「收」
        assert_eq!(direction_of(10), Some(Direction::Up));
        assert_eq!(direction_of(26), Some(Direction::Up));
        assert_eq!(direction_of(42), Some(Direction::Up));
        assert_eq!(direction_of(58), Some(Direction::Up));
        assert_eq!(direction_of(11), Some(Direction::Down));
        assert_eq!(direction_of(27), Some(Direction::Down));
        assert_eq!(direction_of(43), Some(Direction::Down));
        assert_eq!(direction_of(59), Some(Direction::Down));

        // 连接 / 断开 / 重传 / 协议代拷：不带「这次传了多少字节」的语义，不能统计
        for id in [12u16, 13, 14, 15, 16, 17, 18, 30, 34, 49] {
            assert_eq!(direction_of(id), None, "事件 {id} 不该被统计");
        }
    }

    #[test]
    fn packet_header_parsing() {
        assert_eq!(parse_packet(&packet(1234, 1460)), Some((1234, 1460)));
        // 载荷不够长、PID 为 0、size 为 0、size 大得不像报文：一律丢掉
        assert_eq!(parse_packet(&[0u8; 7]), None);
        assert_eq!(parse_packet(&packet(0, 100)), None);
        assert_eq!(parse_packet(&packet(9, 0)), None);
        assert_eq!(parse_packet(&packet(9, 4 * 1024 * 1024)), None);
    }

    #[test]
    fn wire_roundtrip() {
        let snapshot = NetProcessSnapshot {
            up: vec![ProcTraffic {
                name: "chrome".to_string(),
                pid: 100,
                bytes_per_sec: 1234.5,
            }],
            down: vec![ProcTraffic {
                name: "系统".to_string(),
                pid: 4,
                bytes_per_sec: 1024.0,
            }],
            total_up: 2000.0,
            total_down: 3000.0,
        };
        let line = WireReport::from_snapshot(&snapshot).to_line();
        assert!(line.ends_with('\n'));

        let parsed = WireReport::from_line(&line).expect("报文要能解回来");
        assert_eq!(parsed.up, vec![("chrome".to_string(), 1234.5)]);
        assert_eq!(parsed.down, vec![("系统".to_string(), 1024.0)]);
        assert_eq!(parsed.up_total, 2000.0);
        assert_eq!(parsed.down_total, 3000.0);
    }

    #[test]
    fn loopback_traffic_is_ignored() {
        // IPv4 事件：PID, size, daddr(4), saddr(4)
        let mut ipv4 = [0u8; 16];
        ipv4[..8].copy_from_slice(&packet(100, 60));
        ipv4[8..12].copy_from_slice(&[127, 0, 0, 1]);
        ipv4[12..16].copy_from_slice(&[127, 0, 0, 1]);
        assert!(is_loopback(10, &ipv4));
        assert!(is_loopback(43, &ipv4));

        // 内网直连不是回环
        ipv4[8..12].copy_from_slice(&[192, 168, 1, 10]);
        ipv4[12..16].copy_from_slice(&[192, 168, 1, 20]);
        assert!(!is_loopback(10, &ipv4));

        // IPv6 事件：PID, size, daddr(16), saddr(16)
        let mut ipv6 = [0u8; 40];
        ipv6[..8].copy_from_slice(&packet(100, 60));
        ipv6[23] = 1; // daddr = ::1
        assert!(is_loopback(26, &ipv6));

        // IPv4 映射到 IPv6 的回环（::ffff:127.0.0.1）也算：10 个 0 + ff ff + 127.x
        ipv6 = [0u8; 40];
        ipv6[..8].copy_from_slice(&packet(100, 60));
        ipv6[18] = 0xff;
        ipv6[19] = 0xff;
        ipv6[20] = 127;
        assert!(is_loopback(58, &ipv6));

        // 公网 v6 地址
        ipv6 = [0u8; 40];
        ipv6[..8].copy_from_slice(&packet(100, 60));
        ipv6[8] = 0x20;
        ipv6[9] = 0x01;
        assert!(!is_loopback(26, &ipv6));
    }

    #[test]
    fn junk_wire_line_is_ignored() {
        assert!(WireReport::from_line("not json").is_none());
        assert!(WireReport::from_line("").is_none());
        // 老版本没带合计字段也能用，缺的按 0 算
        let report = WireReport::from_line(r#"{"up":[["a",10.0]],"down":[]}"#).unwrap();
        assert_eq!(report.up_total, 0.0);
        assert_eq!(report.down_total, 0.0);
    }
}
