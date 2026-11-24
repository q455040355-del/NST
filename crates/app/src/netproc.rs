//! 面板这一侧的「进程明细」：拉起提权助手、读命名管道、给悬停气泡供数。
//!
//! 背景：每个进程用了多少网速只能从 ETW 内核网络事件里拿，而开那个会话
//! 需要管理员权限（普通权限直接 `Access is denied`）。这个插件是个常驻的
//! 任务栏小工具，让整个程序提权代价太大，所以拆成两半：
//!
//! * 面板（普通权限）：建命名管道服务端，收到什么就画什么；
//! * 助手（`netspeed.exe --netproc-helper`，用户点「进程明细」时按需 UAC 拉起）：
//!   跑 `netspeed_core::netproc` 的 ETW 采集，每秒往管道里写一行 JSON。
//!
//! 助手一退出（写管道失败）就跟着结束，不会在后台赖着。面板退出时管道断开，
//! 助手也随之收摊。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use netspeed_core::netproc::{NetProcessWatcher, WireReport};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::theme::wide;

/// 面板和助手之间的管道名（同机器、同用户可见）。
const PIPE_NAME: &str = r"\\.\pipe\NetSpeedTaskbar.NetProc";

/// 助手启动参数：同一个 exe 换个身份跑。
pub const HELPER_ARG: &str = "--netproc-helper";

/// 调试开关：让面板不弹 UAC，由外部手动起一个普通权限的助手喂假数据。
pub const MOCK_ENV: &str = "NST_NETPROC_MOCK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 功能关着（设置里没开）
    Off,
    /// 已经拉起助手，等它连上来
    Starting,
    /// 有数据在流
    Running,
    /// 用户在 UAC 上点了「否」
    Denied,
    /// 出错了（原因在 `note` 里）
    Failed,
}

struct State {
    status: Status,
    note: String,
    report: Option<WireReport>,
    at: Option<Instant>,
    active: bool,
    /// 下一次建好管道后要不要再拉一次助手（首次启用 / 手动重试）
    launch_pending: bool,
    /// 服务端管道句柄（0 = 没有）：关功能时用它把助手踢掉。
    /// 存成 isize 而不是 HANDLE，才能放进 `Mutex` 里跨线程共享。
    pipe: isize,
}

/// 面板里同时只有一个「进程明细」链路；用 Mutex 让 UI 线程和管道线程共享。
static STATE: Mutex<State> = Mutex::new(State {
    status: Status::Off,
    note: String::new(),
    report: None,
    at: None,
    active: false,
    launch_pending: false,
    pipe: 0,
});

static WORKER: AtomicBool = AtomicBool::new(false);

fn state() -> MutexGuard<'static, State> {
    // 管道线程 panic 过也别让面板跟着 panic：拿回里面还在的数据继续用
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// 启动时调用：设置里开着才真正去提权拉助手。
pub fn init(enabled: bool) {
    if enabled || mock() {
        enable();
    }
}

pub fn mock() -> bool {
    std::env::var_os(MOCK_ENV).is_some()
}

/// 打开功能：起管道线程 + 拉一次提权助手。
pub fn enable() {
    {
        let mut shared = state();
        shared.active = true;
        shared.launch_pending = true;
        if shared.status != Status::Running {
            shared.status = Status::Starting;
            shared.note.clear();
        }
    }
    ensure_worker();
}

/// 关掉功能：断开管道（助手写不进去就自己退出了）。
pub fn disable() {
    let handle = {
        let mut shared = state();
        shared.active = false;
        shared.status = Status::Off;
        shared.note.clear();
        shared.report = None;
        shared.at = None;
        std::mem::replace(&mut shared.pipe, 0)
    };
    if handle != 0 {
        unsafe {
            // 先断开让阻塞里的 ReadFile 立刻返回；句柄由管道线程自己关，避免重复关
            DisconnectNamedPipe(handle as HANDLE);
        }
    }
}

/// 手动重试（右键菜单里点第二次 / 之前取消了 UAC）。
pub fn retry() {
    {
        let mut shared = state();
        shared.active = true;
        shared.launch_pending = true;
        shared.status = Status::Starting;
        shared.note.clear();
    }
    ensure_worker();
}

pub fn status() -> (Status, String) {
    let shared = state();
    (shared.status, shared.note.clone())
}

/// 最近一次报文；超过 4 秒没更新就当成「助手没在跑」（助手每秒都会写一行）。
pub fn latest() -> Option<(WireReport, Instant)> {
    let shared = state();
    let (Some(report), Some(at)) = (shared.report.as_ref(), shared.at) else {
        return None;
    };
    if at.elapsed() > Duration::from_secs(4) {
        return None;
    }
    Some((report.clone(), at))
}

fn ensure_worker() {
    if WORKER.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("netproc-link".to_string())
        .spawn(worker)
        .ok();
}

fn worker() {
    // ShellExecuteW 走「以管理员身份运行」要 COM；失败也不致命
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
    }

    loop {
        if !state().active {
            std::thread::sleep(Duration::from_millis(120));
            continue;
        }

        let pipe = create_pipe();
        if pipe.is_null() {
            crate::debug_log(&format!(
                "netproc: 建管道失败（{}）",
                unsafe { GetLastError() }
            ));
            std::thread::sleep(Duration::from_millis(800));
            continue;
        }
        state().pipe = pipe as isize;

        let launch = {
            let mut shared = state();
            std::mem::replace(&mut shared.launch_pending, false)
        };
        if launch {
            launch_helper();
        }

        unsafe {
            // 等助手连上来；助手没起来就一直等着（不占 CPU）
            ConnectNamedPipe(pipe, std::ptr::null_mut());
        }
        read_pipe(pipe);

        unsafe {
            CloseHandle(pipe);
        }
        {
            let mut shared = state();
            if shared.pipe == pipe as isize {
                shared.pipe = 0;
            }
            // 断线：数据作废，但别自动再弹一次 UAC，让用户自己决定要不要重试
            if shared.status == Status::Running {
                shared.status = Status::Starting;
                shared.note = "助手已退出".to_string();
            }
            shared.report = None;
            shared.at = None;
        }

        std::thread::sleep(Duration::from_millis(150));
    }
}

fn create_pipe() -> HANDLE {
    let name = wide(PIPE_NAME);
    unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            0,
            64 * 1024,
            0,
            std::ptr::null(),
        )
    }
}

fn read_pipe(pipe: HANDLE) {
    let mut buffer = vec![0u8; 8192];
    let mut pending = String::new();

    loop {
        if !state().active {
            return;
        }

        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                pipe,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }

        pending.push_str(&String::from_utf8_lossy(&buffer[..read as usize]));
        // 一条报文一行；被切断的尾巴留到下一轮
        while let Some(index) = pending.find('\n') {
            let line: String = pending.drain(..=index).collect();
            let Some(report) = WireReport::from_line(&line) else {
                continue;
            };

            let mut shared = state();
            if let Some(error) = report.error.as_deref() {
                shared.status = Status::Failed;
                shared.note = error.to_string();
                shared.report = None;
                shared.at = None;
                continue;
            }
            shared.report = Some(report);
            shared.at = Some(Instant::now());
            shared.status = Status::Running;
            shared.note.clear();
        }

        // 万一对端一直不换行，别把内存吃光
        if pending.len() > 64 * 1024 {
            pending.clear();
        }
    }
}

/// 拉提权助手。失败原因直接反映到悬停气泡上，不弹任何自定义对话框。
fn launch_helper() {
    if mock() {
        // 调试模式：助手由外面手动起（普通权限就能喂假数据），面板不弹 UAC
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            set_failed(format!("找不到自己的路径：{error}"));
            return;
        }
    };

    let operation = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(HELPER_ARG);

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    // ShellExecuteW 的返回值 <= 32 都是错误码
    const ERROR_CANCELLED: isize = 1223;
    if result > 32 {
        return;
    }
    if result == ERROR_CANCELLED {
        let mut shared = state();
        shared.status = Status::Denied;
        shared.note = "已取消管理员授权".to_string();
        return;
    }
    set_failed(format!("提权启动失败（错误码 {result}）"));
}

fn set_failed(note: String) {
    crate::debug_log(&format!("netproc: {note}"));
    let mut shared = state();
    shared.status = Status::Failed;
    shared.note = note;
}

// ---------------------------------------------------------------- 助手侧

/// `--netproc-helper` 模式：采集 + 往管道里写，面板没了就退出。
pub fn run_helper() {
    if mock() {
        run_mock_helper();
        return;
    }

    let mut watcher = match NetProcessWatcher::start() {
        Ok(watcher) => watcher,
        Err(error) => {
            crate::debug_log(&format!("netproc helper: {error}"));
            // 面板还等着信儿：把失败原因送过去，悬停时能说清楚为什么没有数据
            if let Some(pipe) = connect_pipe() {
                let report = WireReport {
                    error: Some(error),
                    ..Default::default()
                };
                write_line(pipe, &report.to_line());
                unsafe { CloseHandle(pipe) };
            }
            return;
        }
    };

    let Some(pipe) = connect_pipe() else {
        return;
    };

    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let snapshot = watcher.sample();
        let line = WireReport::from_snapshot(&snapshot).to_line();
        if !write_line(pipe, &line) {
            break;
        }
    }

    unsafe {
        CloseHandle(pipe);
    }
}

/// 调试用助手：不碰 ETW，按合成数据喂管道，用来验证面板/气泡这一侧。
fn run_mock_helper() {
    let Some(pipe) = connect_pipe() else {
        crate::debug_log("netproc mock: 管道连不上（面板没开进程明细？）");
        return;
    };

    let mut tick = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        tick += 1;

        // 0.6 ~ 1.4 倍的缓慢起伏，数字不会一帧一个样
        let wave = 1.0 + (tick as f64 * 0.35).sin() * 0.4;
        let up = vec![
            ("chrome".to_string(), 920_000.0 * wave),
            ("Code".to_string(), 140_000.0 * wave),
            ("svchost".to_string(), 9_000.0),
            ("OneDrive".to_string(), 4_000.0),
        ];
        let down = vec![
            ("chrome".to_string(), 6_400_000.0 * wave),
            ("Steam".to_string(), 1_100_000.0),
            ("explorer".to_string(), 26_000.0),
        ];
        let report = WireReport {
            up_total: up.iter().map(|(_, rate)| rate).sum(),
            down_total: down.iter().map(|(_, rate)| rate).sum(),
            up,
            down,
            error: None,
        };
        if !write_line(pipe, &report.to_line()) {
            break;
        }
    }

    unsafe {
        CloseHandle(pipe);
    }
}

fn connect_pipe() -> Option<HANDLE> {
    let name = wide(PIPE_NAME);
    // 面板可能比助手先起/后起：给 15 秒的耐心
    for _ in 0..75 {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Some(handle);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

fn write_line(pipe: HANDLE, line: &str) -> bool {
    let mut remaining = line.as_bytes();
    while !remaining.is_empty() {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                pipe,
                remaining.as_ptr() as *const _,
                remaining.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_BROKEN_PIPE {
                crate::debug_log(&format!("netproc: 写管道失败（{code}）"));
            }
            return false;
        }
        remaining = &remaining[written as usize..];
    }
    true
}
