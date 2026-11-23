//! Rust 版任务栏网速插件（阶段一：面板 + 网速/内存 + 指示条 + 右键菜单）。
//!
//! 和 C# 版共用 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，可以并行试跑。

#![windows_subsystem = "windows"]

mod aa;
mod dialog;
mod dxgi;
mod gifrec;
mod history_ui;
mod hint;
mod autostart;
mod menu;
mod ocr;
mod ocr_ui;
mod translate;
mod panel;
mod settings_ui;
mod scroll;
mod shot;
mod theme;
mod tray;
mod ui;
mod update;

use netspeed_core::settings::{parse_hotkey, Settings};
use panel::Panel;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, TrackMouseEvent, UnregisterHotKey, TME_LEAVE,
    TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use theme::wide;

const WM_APP_TICK: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW_MENU: u32 = WM_APP + 3;
pub(crate) const WM_APP_SHOT_FIRE: u32 = WM_APP + 4;
const WM_APP_OCR_READY: u32 = WM_APP + 5;
const WM_APP_TRANSLATE_READY: u32 = WM_APP + 11;
pub(crate) const WM_APP_GIF_DONE: u32 = WM_APP + 9;
pub(crate) const WM_APP_SCROLL_DONE: u32 = WM_APP + 10;
const WM_APP_ANIM: u32 = WM_APP + 6;
/// 另一个实例启动时叫醒已有面板（单实例互斥体挡住重复启动，但得给点反馈）
const WM_APP_REVEAL: u32 = WM_APP + 7;
/// 冒烟测试用：模拟「换到另一块屏」的 DPI 变化。
/// （WM_DPICHANGED 是系统消息，跨进程 PostMessage 会被系统挡掉，测不了）
const WM_APP_DPI_TEST: u32 = WM_APP + 8;

/// windows-sys 0.59 没导出 WM_MOUSELEAVE，自己补一个
/// （不能省：漏了会变成变量绑定模式，把后面的消息分支全吃掉）
const WM_MOUSELEAVE: u32 = 0x02A3;
const HOTKEY_SHOT: i32 = 1;
const HOTKEY_OCR: i32 = 2;
const HOTKEY_GIF: i32 = 3;
const TRAY_ID: u32 = 1;

thread_local! {
    /// 上一次写进托盘的提示文本
    static LAST_TIP: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}

/// 任务栏句柄：用来发现 Explorer 重启（我们的子窗口会被连带销毁）
static mut TASKBAR: HWND = std::ptr::null_mut();

/// 用户从菜单里「隐藏面板」后不再自动显示（只有双击托盘才恢复）
static mut HIDDEN_BY_USER: bool = false;

/// 指示条动画是否在跑（ticker 线程看它决定推不推帧）
static ANIM_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 叫醒 ticker 线程用（它空闲时会睡到下一个采样点）
static TICK_WAKE: std::sync::OnceLock<(std::sync::Mutex<()>, std::sync::Condvar)> =
    std::sync::OnceLock::new();
/// 是否已经向系统登记了 WM_MOUSELEAVE
static mut TRACKING_LEAVE: bool = false;

fn tick_wake() -> &'static (std::sync::Mutex<()>, std::sync::Condvar) {
    TICK_WAKE.get_or_init(|| (std::sync::Mutex::new(()), std::sync::Condvar::new()))
}

pub(crate) fn request_anim(active: bool) {
    let was = ANIM_ACTIVE.swap(active, std::sync::atomic::Ordering::Relaxed);
    if active && !was {
        // ticker 空闲时睡到下一个采样点（最长 500ms）：这里直接叫醒它，
        // 否则动画要白等半秒才起步
        let (lock, wake) = tick_wake();
        if let Ok(_guard) = lock.lock() {
            wake.notify_all();
        }
    }
}

// 拖动面板（左键按住移动）：记起点和窗口原位，超过 3px 才算拖动
static mut DRAG_ACTIVE: bool = false;
static mut DRAG_MOVED: bool = false;
static mut DRAG_FROM: POINT = POINT { x: 0, y: 0 };
static mut DRAG_WINDOW: (i32, i32) = (0, 0);

/// 托盘图标句柄（进程内唯一）
static mut TRAY_ICON: windows_sys::Win32::UI::WindowsAndMessaging::HICON = std::ptr::null_mut();

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Panel";
const WINDOW_TITLE: &str = "NetSpeedTaskbar";

const ID_VIEW_CYCLE: usize = 1306;
const ID_SPEED: usize = 1001;
const ID_MEMORY: usize = 1002;
const ID_TEMPERATURE: usize = 1003;
const ID_AI: usize = 1004;
const ID_GAUGE_MEMORY: usize = 1103;
const ID_GAUGE_AI: usize = 1105;
const ID_GAUGE_NONE: usize = 1106;
const ID_HISTORY: usize = 1300;
const ID_SETTINGS: usize = 1301;
const ID_PANEL_HIDE: usize = 1200;
const ID_PANEL_SNAP: usize = 1201;
const ID_SHOT: usize = 1302;
const ID_OCR: usize = 1303;
const ID_GIF: usize = 1304;
const ID_QUIT: usize = 1099;

fn main() {
    install_panic_hook();
    unsafe {
        // 必须在任何窗口/坐标计算之前：否则进程是 DPI 不感知的，
        // 系统会把坐标和尺寸按 150% 缩放虚拟化（面板位置/大小全错）。
        // Win10 1703+ 才有 SetProcessDpiAwarenessContext，老系统动态取，取不到退回系统 DPI 感知。
        let user32 = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(wide("user32.dll").as_ptr());
        let proc = windows_sys::Win32::System::LibraryLoader::GetProcAddress(
            user32,
            b"SetProcessDpiAwarenessContext\0".as_ptr(),
        );
        let mut applied = false;
        if let Some(function) = proc {
            let function: unsafe extern "system" fn(isize) -> i32 = std::mem::transmute(function);
            // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4
            applied = function(-4) != 0;
        }
        if !applied {
            windows_sys::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
        }

        // 调试用：在给定区域上跑一遍「抓帧 + 拼接」，把长图存 PNG（滚动由外部脚本驱动）
        if let Some(spec) = std::env::var_os("NST_SCROLL_TEST") {
            let ok = scroll::selftest(&spec.to_string_lossy());
            crate::debug_log(&format!("scroll test -> {ok}"));
            return;
        }

        // 调试用：录一段 mp4 直接落盘，验证编码输出（画面/方向）
        if let Some(spec) = std::env::var_os("NST_REC_TEST") {
            let ok = gifrec::selftest(&spec.to_string_lossy());
            crate::debug_log(&format!("rec test -> {ok}"));
            return;
        }

        // 调试用：把现成 mp4 的首帧解成 PNG，用来查录制画面对不对（歪没歪、颜色正不正）
        if let Some(spec) = std::env::var_os("NST_REC_PROBE") {
            let ok = gifrec::probe_to_png(&spec.to_string_lossy());
            crate::debug_log(&format!("rec probe -> {ok}"));
            return;
        }

        // 调试用：直接按 NV12 解首帧（不经过视频处理器），用来判断码流本身歪没歪
        if let Some(spec) = std::env::var_os("NST_REC_PROBE_NV12") {
            let ok = gifrec::probe_nv12_to_png(&spec.to_string_lossy());
            crate::debug_log(&format!("nv12 probe -> {ok}"));
            return;
        }

        // 调试用：抓一帧原始画面（不走编码器），判断「歪」是采集还是编码的锅
        if let Some(spec) = std::env::var_os("NST_REC_RAW") {
            let ok = gifrec::raw_frame_to_png(&spec.to_string_lossy());
            crate::debug_log(&format!("raw frame -> {ok}"));
            return;
        }

        // 调试用：直接把结果窗按示例内容打开（点按钮、试悬停时用）
        if std::env::var_os("NST_OCR_DEMO").is_some() {
            let source = "Latency budget\nThe panel samples the taskbar twice a second.\nIf sampling fails, the last good color is kept for 30 seconds before we retry,\nso the background never flickers between two close shades.";
            let translated = "延迟预算\n面板每秒采样任务栏两次。\n如果采样失败，会保留上一次成功的颜色 30 秒再重试，\n所以背景不会在两种接近的色值之间来回跳。";
            ocr_ui::open_translate(std::ptr::null_mut(), source, translated, None);
        }

        // 调试用：把取字 / 翻译结果窗离屏画成 PNG（改这块布局时做像素比对）
        if let Some(path) = std::env::var_os("NST_OCR_PREVIEW") {
            let view = std::env::var("NST_OCR_PREVIEW_VIEW").unwrap_or_else(|_| "lines".into());
            let ok = ocr_ui::preview_png(&path.to_string_lossy(), &view);
            crate::debug_log(&format!("ocr preview -> {ok}"));
            return;
        }

        // 调试用：把用量历史窗离屏画成 PNG（改图表布局时做像素比对）
        if let Some(path) = std::env::var_os("NST_HISTORY_PREVIEW") {
            let ok = history_ui::preview_png(&path.to_string_lossy());
            crate::debug_log(&format!(
                "history preview -> {} ({})",
                path.to_string_lossy(),
                if ok { "ok" } else { "failed" }
            ));
            return;
        }

        // 调试用：把设置窗离屏画成 PNG（改设置布局时做像素比对）
        if let Some(path) = std::env::var_os("NST_SETTINGS_PREVIEW") {
            let ok = settings_ui::preview_png(&path.to_string_lossy());
            crate::debug_log(&format!(
                "settings preview -> {} ({})",
                path.to_string_lossy(),
                if ok { "ok" } else { "failed" }
            ));
            return;
        }

        // 调试用：把工具栏每个图标导出成独立 PNG + 一张总表（拿去重设计用）
        if let Some(dir) = std::env::var_os("NST_ICONS_EXPORT") {
            let ok = shot::export_icons(&dir.to_string_lossy());
            crate::debug_log(&format!("icons export -> {ok}"));
            return;
        }

        // 调试用：把截图编辑窗（含一条底栏）离屏画成 PNG，不弹窗、不进托盘。
        // 改工具栏 / 图标时用：NST_EDITOR_PREVIEW=D:\tmp\editor.png netspeed.exe
        if let Some(path) = std::env::var_os("NST_EDITOR_PREVIEW") {
            let ok = shot::preview_png(&path.to_string_lossy());
            crate::debug_log(&format!(
                "editor preview -> {} ({})",
                path.to_string_lossy(),
                if ok { "ok" } else { "failed" }
            ));
            return;
        }

        // 调试用：看一眼「存完文件」的确认框（NST_DIALOG_PREVIEW=C:\\tmp\\x.png）
        if let Some(path) = std::env::var_os("NST_DIALOG_PREVIEW") {
            let open = dialog::saved_prompt(
                std::ptr::null_mut(),
                "截图已保存",
                &path.to_string_lossy(),
            );
            crate::debug_log(&format!("dialog preview -> open={open}"));
            return;
        }

        // 通用对话框（另存为）需要 COM 单元，不开的话 GetSaveFileNameW 会直接失败
        windows_sys::Win32::System::Com::CoInitializeEx(
            std::ptr::null(),
            windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32,
        );

        let mutex_name = wide("Local\\NetSpeedTaskbar.Rust");
        let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
        if mutex.is_null() || GetLastError() == 183 {
            // 已经在跑了：把已有面板叫出来，别让用户双击了没反应
            reveal_existing();
            return;
        }

        let instance = GetModuleHandleW(std::ptr::null());

        let class_name = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
        if taskbar.is_null() {
            return;
        }
        TASKBAR = taskbar;

        let settings = Settings::load();
        // 自启项对一遍：exe 挪过位置的话这里会把注册表里的路径修正
        autostart::sync(settings.auto_start);
        let width = scale_for_dpi(settings.panel_width);
        let height = scale_for_dpi(40);

        // 顶层 + 分层：任务栏会不断重绘自己的子窗口区域，挂在下面会被刷掉；
        // 分层窗口自己负责呈现（UpdateLayeredWindow），和 AgentSpeed / C# 稳定版一致
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class_name.as_ptr(),
            wide(WINDOW_TITLE).as_ptr(),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            width,
            height,
            taskbar, // owner = 任务栏：拥有窗口永远排在 owner 之上，Explorer 重绘也盖不住
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );

        if hwnd.is_null() {
            return;
        }

        position_panel(hwnd, taskbar, width, height);
        keep_on_top(hwnd);
        add_tray(hwnd);

        // 注意：本进程里的 SetTimer / WM_TIMER 完全不可靠（实测 set 成功但永不触发，
        // 回调形式也一样），所以周期刷新用一个后台线程投递消息，保证一定能到。
        start_ticker(hwnd);

        // 全局热键按设置注册（截图 / 取字都有开关，关掉的不占组合键）
        shot::set_ocr_enabled(settings.ocr_enabled);
        shot::set_translate_enabled(settings.translate_enabled);
        register_hotkeys(hwnd);

        // 调试/快捷入口：netspeed.exe --settings 直接打开设置窗口
        if std::env::args().any(|arg| arg == "--settings") {
            settings_ui::open(hwnd, &settings);
        }
        if std::env::args().any(|arg| arg == "--history") {
            history_ui::open(hwnd);
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 临时调试日志（排查 DPI / 主题时用）。
pub fn debug_log(message: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("netspeed-rs.log");

    // 日志只在诊断时用，长时间跑也别长到几十 MB：超过 1 MB 就从空的重新开始
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1_000_000 {
            let _ = std::fs::remove_file(&path);
        }
    }

    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

/// 崩溃现场：GUI 程序挂掉时托盘图标直接消失，不落盘就完全查不到原因
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "?".to_string());
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "?".to_string()
        };
        debug_log(&format!("PANIC {location}: {message}"));
        default(info);
    }));
}

fn scale_for_dpi(value: i32) -> i32 {
    let dpi = unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dpi = GetDeviceCaps(screen, 88) as u32; // LOGPIXELSX
        ReleaseDC(std::ptr::null_mut(), screen);
        dpi.max(96)
    };
    panel::scale(dpi, value)
}

/// 面板所在显示器上的任务栏。
///
/// 主屏是 `Shell_TrayWnd`，副屏是各自的 `Shell_SecondaryTrayWnd`——以前一律拿主任务栏，
/// 面板拖到副屏后位置会按主屏的托盘算，跑偏。
fn taskbar_for(hwnd: HWND) -> HWND {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let primary = FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
        if !primary.is_null() && MonitorFromWindow(primary, MONITOR_DEFAULTTONEAREST) == monitor {
            return primary;
        }

        let mut query = TaskbarQuery {
            monitor,
            found: std::ptr::null_mut(),
        };
        EnumWindows(Some(find_taskbar), &mut query as *mut TaskbarQuery as isize);
        if query.found.is_null() {
            primary
        } else {
            query.found
        }
    }
}

struct TaskbarQuery {
    monitor: HMONITOR,
    found: HWND,
}

unsafe extern "system" fn find_taskbar(hwnd: HWND, lparam: LPARAM) -> i32 {
    let query = &mut *(lparam as *mut TaskbarQuery);
    let mut class = [0u16; 64];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    let name = String::from_utf16_lossy(&class[..length.max(0) as usize]);
    if name != "Shell_TrayWnd" && name != "Shell_SecondaryTrayWnd" {
        return 1;
    }
    if MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) != query.monitor {
        return 1;
    }
    query.found = hwnd;
    0
}

/// 鼠标所在显示器的可用区域：多屏时窗口开在用户正在用的那块屏上
pub(crate) fn work_area() -> RECT {
    unsafe {
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        work_area_at(cursor)
    }
}

/// 某个点所在显示器的可用区域
pub(crate) fn work_area_at(point: POINT) -> RECT {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);

        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            return info.rcWork;
        }

        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
        work
    }
}

/// 某块显示器当前的 DPI（per-monitor 生效值；失败就按 96 算）
pub(crate) fn dpi_for_monitor(monitor: HMONITOR) -> u32 {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe {
        let mut x = 96_u32;
        let mut y = 96_u32;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) == 0 {
            x.max(96)
        } else {
            96
        }
    }
}

/// 鼠标所在显示器的缩放（1.0 = 100%）
pub(crate) fn cursor_scale() -> f32 {
    unsafe {
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        dpi_for_monitor(monitor) as f32 / 96.0
    }
}

/// 屏幕坐标附近那块屏的缩放（气泡/菜单贴着面板或鼠标出现时用）
pub(crate) fn scale_at(point: POINT) -> f32 {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        dpi_for_monitor(monitor) as f32 / 96.0
    }
}


/// 按新 DPI 重排面板：字体、尺寸、缓冲区全部重建，位置尽量不动。
/// `dpi == 0` 表示「回到这块屏真正的 DPI」（测试通道用）。
fn apply_dpi(hwnd: HWND, dpi: u32, suggested: *const RECT) {
    unsafe {
        let dpi = if dpi == 0 {
            crate::dpi_for_window(hwnd)
        } else {
            dpi
        };

        if let Some(panel) = panel_mut(hwnd) {
            panel.set_dpi(dpi.max(96));
            let width = panel.desired_width();
            let height = panel.height();

            // 建议矩形给了就用它定位（跨屏拖过来时正好落在边界内），
            // 没给就保持原来的左上角——尺寸一定要按新 DPI 重设，不然字和框对不上
            let (left, top) = if suggested.is_null() {
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetWindowRect(hwnd, &mut rect);
                (rect.left, rect.top)
            } else {
                ((*suggested).left, (*suggested).top)
            };

            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                left,
                top,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

/// 顶层置顶窗口不用改 z 序，这里只负责跟随任务栏位置（任务栏可能移动/改尺寸）。
fn keep_on_top(hwnd: HWND) {
    // 每 500ms 的 tick 都会进来：完整的 FindWindow + 两次 GetWindowRect 不必这么勤，
    // 2 秒对一次就够跟上任务栏移动/DPI 变化
    if !place_due() {
        return;
    }

    unsafe {
        // 没手动拖过（PanelX = -1）才跟着托盘自动对齐；拖过之后只做 TOPMOST
        let saved_x = panel_mut(hwnd).map(|panel| panel.settings.panel_x).unwrap_or(-1);

        if saved_x < 0 {
            let taskbar = taskbar_for(hwnd);
            if taskbar.is_null() {
                return;
            }

            let mut bar = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetWindowRect(taskbar, &mut bar);

            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetWindowRect(hwnd, &mut rect);
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;

            let tray = FindWindowExW(
                taskbar,
                std::ptr::null_mut(),
                wide("TrayNotifyWnd").as_ptr(),
                std::ptr::null(),
            );
            let gap = scale_for_dpi(8);

            let mut tray_rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if !tray.is_null() {
                GetWindowRect(tray, &mut tray_rect);
            }

            // 竖排任务栏（Win10 侧边）：面板是横条，塞不进 48px 宽的竖栏，
            // 就贴在栏的外侧、纵向对齐托盘——总比被夹回栏里盖住图标强
            let vertical = (bar.bottom - bar.top) > (bar.right - bar.left) * 2;
            let (left, top) = if vertical {
                let work = work_area_at(POINT {
                    x: bar.left,
                    y: bar.top,
                });
                let on_left = bar.left <= work.left + 2;
                let left = if on_left {
                    bar.right + gap
                } else {
                    bar.left - width - gap
                };
                let center = if !tray.is_null() {
                    (tray_rect.top + tray_rect.bottom) / 2
                } else {
                    (bar.top + bar.bottom) / 2
                };
                let top = (center - height / 2).clamp(work.top, (work.bottom - height).max(work.top));
                (left.clamp(work.left, (work.right - width).max(work.left)), top)
            } else {
                let left = if !tray.is_null() {
                    tray_rect.left - width - gap
                } else {
                    bar.right - width - scale_for_dpi(220)
                };
                let top = panel_top(bar.top, bar.bottom - bar.top, height);
                (
                    left.max(bar.left),
                    top.max(bar.top),
                )
            };

            if rect.left != left || rect.top != top {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    left,
                    top,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
                return;
            }
        }

        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 有第二个实例启动时：找到已经在跑的面板，让它自己亮一下。
///
/// 单实例互斥体挡住重复启动是对的，但以前是静默 `return`——用户双击
/// 快捷方式什么都没发生，会以为程序坏了。
fn reveal_existing() {
    unsafe {
        let mut found: HWND = std::ptr::null_mut();
        EnumWindows(Some(find_panel), &mut found as *mut HWND as isize);
        if found.is_null() {
            return;
        }
        // 允许前台化：这是用户主动双击触发的，Windows 会放行
        AllowSetForegroundWindow(0xFFFF_FFFF);
        PostMessageW(found, WM_APP_REVEAL, 0, 0);

        // `netspeed --history` / `--settings` 在已经跑着的时候也要能打开对应窗口，
        // 否则用户从快捷方式带参数启动会什么反应都没有
        for arg in std::env::args() {
            let id = match arg.as_str() {
                "--history" => ID_HISTORY,
                "--settings" => ID_SETTINGS,
                _ => continue,
            };
            PostMessageW(found, menu::WM_APP_MENU_PICK, id, 0);
            break;
        }
    }
}

unsafe extern "system" fn find_panel(hwnd: HWND, lparam: LPARAM) -> i32 {
    let mut class = [0u16; 64];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    let name = String::from_utf16_lossy(&class[..length.max(0) as usize]);
    if name == CLASS_NAME {
        *(lparam as *mut HWND) = hwnd;
        return 0;
    }
    1
}

/// 是否到了重新定位面板的时间（2 秒一次）
fn place_due() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    static START: OnceLock<std::time::Instant> = OnceLock::new();
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);

    let start = START.get_or_init(std::time::Instant::now);
    let now = start.elapsed().as_millis() as u64;
    let last = LAST.load(Ordering::Relaxed);
    if last != u64::MAX && now.saturating_sub(last) < 2000 {
        return false;
    }

    LAST.store(now, Ordering::Relaxed);
    true
}

/// 面板在任务栏里的纵向位置。
///
/// 面板高度通常就等于任务栏高度，这时必须贴着 bar.top，不能居中——
/// 差 1~2px 就会「浮」在任务栏上下，看起来就不像嵌进去了。
fn panel_top(bar_top: i32, bar_height: i32, height: i32) -> i32 {
    if height >= bar_height - 1 {
        bar_top
    } else {
        bar_top + (bar_height - height) / 2
    }
}

fn position_panel(hwnd: HWND, taskbar: HWND, width: i32, height: i32) {
    unsafe {
        let mut bar = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(taskbar, &mut bar);

        let tray = FindWindowExW(
            taskbar,
            std::ptr::null_mut(),
            wide("TrayNotifyWnd").as_ptr(),
            std::ptr::null(),
        );

        let gap = scale_for_dpi(8);
        let default_left = if !tray.is_null() {
            let mut tray_rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetWindowRect(tray, &mut tray_rect);
            tray_rect.left - width - gap
        } else {
            bar.right - width - scale_for_dpi(220)
        };

        let default_top = panel_top(bar.top, bar.bottom - bar.top, height);

        // 用户拖过就按拖到的位置摆（PanelX / PanelY，-1 = 自动）
        let (saved_x, saved_y) = panel_mut(hwnd)
            .map(|panel| (panel.settings.panel_x, panel.settings.panel_y))
            .unwrap_or((-1, -1));

        let left = if saved_x >= 0 { saved_x } else { default_left };
        let top = if saved_y >= 0 { saved_y } else { default_top };

        // 夹在任务栏范围内，别拖到屏幕外找不回来
        let left = left.clamp(bar.left, (bar.right - width).max(bar.left));
        let top = top.clamp(bar.top, (bar.bottom - height).max(bar.top));

        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            left,
            top,
            width,
            height,
            SWP_NOACTIVATE,
        );
    }
}

/// 已知的远程控制客户端进程名（小写、去掉 .exe）。
///
/// 前台是这些窗口时，本机只是「控制端」：快捷键会被远控软件同时转发给被控端，
/// 两边都注册了全局热键就会两台机器一起弹截图。这里让控制端自己不触发，
/// 由被控端（前台不是远控窗口）去截图。
const REMOTE_CLIENTS: [&str; 11] = [
    "gameviewer", // 网易 UU 远程
    "uuremote",   // UU 远程的新版 exe 名
    "todesk",
    "sunloginclient", // 向日葵
    "anydesk",
    "teamviewer",
    "teamviewer_service",
    "rustdesk",
    "parsecd", // Parsec
    "mstsc",   // Windows 远程桌面
    "moonlight",
];

fn is_remote_client_exe(path: &str) -> bool {
    let name = path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    REMOTE_CLIENTS.contains(&name)
}

/// 当前前台窗口是不是远程控制软件（UU 远程 / ToDesk / 向日葵…）。
fn foreground_is_remote_client() -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return false;
        }

        let mut pid = 0_u32;
        GetWindowThreadProcessId(foreground, &mut pid);
        if pid == 0 {
            return false;
        }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }

        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 {
            return false;
        }

        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        is_remote_client_exe(&path)
    }
}

/// 周期刷新：后台线程每 500ms 投递一次 WM_APP_TICK。
/// （本进程 SetTimer/WM_TIMER 实测完全收不到，改用线程投递最稳。）
/// 前台是不是一个占满整块显示器的窗口（游戏、视频全屏）。
/// 任务栏/桌面/有 owner 的弹窗都不算。
/// 前台是不是一块屏上的真全屏窗口（游戏/视频）。
///
/// 只在它和 `panel` 在**同一块屏**时才算数：主屏打游戏不该把副屏任务栏上的
/// 面板一起藏掉。
fn fullscreen_foreground(panel: HWND) -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return false;
        }

        let mut class = [0u16; 96];
        let length = GetClassNameW(foreground, class.as_mut_ptr(), class.len() as i32);
        let name = String::from_utf16_lossy(&class[..length.max(0) as usize]);
        if name == "Shell_TrayWnd"
            || name == "Shell_SecondaryTrayWnd"
            || name == "Progman"
            || name == "WorkerW"
        {
            return false;
        }

        // 有 owner 的窗口是对话框/弹出菜单，不算全屏
        if !GetWindow(foreground, GW_OWNER).is_null() {
            return false;
        }

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(foreground, &mut rect) == 0 {
            return false;
        }

        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return false;
        }

        // 全屏窗口和面板不在同一块屏：不藏
        if monitor != MonitorFromWindow(panel, MONITOR_DEFAULTTONEAREST) {
            return false;
        }

        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return false;
        }

        rect.left <= info.rcMonitor.left
            && rect.top <= info.rcMonitor.top
            && rect.right >= info.rcMonitor.right
            && rect.bottom >= info.rcMonitor.bottom
    }
}

/// 采样节拍（毫秒）：跟着设置里的刷新间隔走，2 秒档就不用每 500ms 醒一次
static TICK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(500);

pub(crate) fn set_tick_interval(ms: i32) {
    let ms = ms.clamp(50, 2000) as u64;
    TICK_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
    // 从 2 秒调回 0.5 秒时，线程可能正睡 2 秒：叫醒它按新间隔重算
    let (lock, wake) = tick_wake();
    if let Ok(_guard) = lock.lock() {
        wake.notify_all();
    }
}

fn start_ticker(hwnd: HWND) {
    let handle = hwnd as usize;
    std::thread::spawn(move || {
        // 采样节拍跟着 RefreshMs 走；指示条动画在跑时按 ~60fps 推帧。
        // （本进程 SetTimer/WM_TIMER 实测完全收不到，只能靠线程投递。）
        //
        // 空闲时直接等到下一个采样点（以前固定每 50ms 醒一次，
        // 一天白醒 170 万次，笔记本上纯属浪费电）。有动画要启动时
        // request_anim 会 notify 立刻把它叫醒。
        let (lock, wake) = tick_wake();
        let mut last_tick = std::time::Instant::now();
        loop {
            let animating = ANIM_ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
            let interval = std::time::Duration::from_millis(
                TICK_MS.load(std::sync::atomic::Ordering::Relaxed).max(50),
            );
            let timeout = if animating {
                std::time::Duration::from_millis(16)
            } else {
                interval
                    .saturating_sub(last_tick.elapsed())
                    .max(std::time::Duration::from_millis(20))
            };

            if let Ok(guard) = lock.lock() {
                let _ = wake.wait_timeout(guard, timeout);
            }

            unsafe {
                let target = handle as HWND;
                if IsWindow(target) == 0 {
                    break;
                }
                let animating = ANIM_ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
                if animating && PostMessageW(target, WM_APP_ANIM, 0, 0) == 0 {
                    break;
                }
                let interval = std::time::Duration::from_millis(
                    TICK_MS.load(std::sync::atomic::Ordering::Relaxed).max(50),
                );
                if last_tick.elapsed() >= interval {
                    last_tick = std::time::Instant::now();
                    if PostMessageW(target, WM_APP_TICK, 0, 0) == 0 {
                        break;
                    }
                }
            }
        }
    });
}
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let settings = Settings::load();
            let panel = Box::new(Panel::new(hwnd, settings));
            let ptr = Box::into_raw(panel);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            PostMessageW(hwnd, WM_APP_TICK, 0, 0);
            0
        }
        // 面板被拖到另一块显示器 / 那块屏的缩放被改了
        WM_DPICHANGED => {
            apply_dpi(hwnd, ((wparam >> 16) & 0xFFFF) as u32, lparam as *const RECT);
            0
        }
        // 测试通道：和 WM_DPICHANGED 走同一段逻辑
        WM_APP_DPI_TEST => {
            apply_dpi(hwnd, (wparam & 0xFFFF) as u32, std::ptr::null());
            0
        }
        // 分辨率 / 显示器布局变了：重新贴回任务栏
        WM_DISPLAYCHANGE => {
            keep_on_top(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_APP_TICK => {
            // Explorer 重启后我们的窗口会跟着销毁，这时直接退出进程，
            // 否则会留下「没有窗口但占着互斥体」的僵尸进程，导致再也打不开。
            if !TASKBAR.is_null() && IsWindow(TASKBAR) == 0 {
                std::process::exit(0);
            }

            keep_on_top(hwnd);

            // 全屏程序（游戏/视频）在前台时把自己藏起来，否则一个 TOPMOST 面板会糊在画面上
            let fullscreen = fullscreen_foreground(hwnd);
            let visible = IsWindowVisible(hwnd) != 0;
            // 截图/编辑过程中不要把自己弹出来盖住遮罩
            if !shot::active() && !fullscreen && !visible && !HIDDEN_BY_USER {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            if fullscreen && visible {
                ShowWindow(hwnd, SW_HIDE);
            }
            if fullscreen {
                return 0;
            }

            if let Some(panel) = panel_mut(hwnd) {
                panel.tick();
                let tip = panel.tip_text();
                // NIM_MODIFY 只在文字变了才发，否则托盘会频繁闪烁
                let changed = LAST_TIP.with(|cell| {
                    let mut value = cell.borrow_mut();
                    if *value == tip {
                        false
                    } else {
                        *value = tip.clone();
                        true
                    }
                });
                if changed {
                    update_tray(hwnd, &tip);
                }
            }

            // 悬浮说明开着就顺手刷一下数值（不动鼠标也能看到最新值）
            if hint::visible() {
                update_gauge_hint(hwnd);
            }
            0
        }
        WM_APP_REVEAL => {
            // 第二个实例被启动：把面板亮出来（用户双击了图标总得有点反应）
            HIDDEN_BY_USER = false;
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            keep_on_top(hwnd);
            if let Some(panel) = panel_mut(hwnd) {
                panel.tick();
            }
            SetForegroundWindow(hwnd);
            hint::hide();
            0
        }
        WM_APP_TRAY => {
            let event = (lparam as u32) & 0xFFFF;
            match event {
                WM_LBUTTONUP => {
                    if HIDDEN_BY_USER {
                        // 面板被手动隐藏时，左键直接唤回（不然左键只弹菜单，永远点不回来）
                        HIDDEN_BY_USER = false;
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        keep_on_top(hwnd);
                        if let Some(panel) = panel_mut(hwnd) {
                            panel.tick();
                        }
                    } else {
                        show_menu(hwnd);
                    }
                }
                WM_RBUTTONUP => show_menu(hwnd),
                WM_LBUTTONDBLCLK => {
                    HIDDEN_BY_USER = false;
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    keep_on_top(hwnd);
                    if let Some(panel) = panel_mut(hwnd) {
                        panel.tick();
                    }
                }
                _ => {}
            }
            0
        }
        WM_PAINT => {
            if let Some(panel) = panel_mut(hwnd) {
                panel.paint();
            }
            // 面板是分层窗口，内容用 UpdateLayeredWindow 直接提交，不需要 BeginPaint。
            // 但更新区必须自己清掉：不清的话 WM_PAINT 会一直重发（面板又是 TOPMOST，
            // 每次都排在前面），同线程的其它窗口就拿不到 WM_PAINT ——
            // 截图编辑器里拖箭头/马赛克卡顿就是这么来的。
            windows_sys::Win32::Graphics::Gdi::ValidateRect(hwnd, std::ptr::null());
            0
        }
        WM_PRINTCLIENT => {
            if let Some(panel) = panel_mut(hwnd) {
                panel.render(wparam as windows_sys::Win32::Graphics::Gdi::HDC);
            }
            0
        }
        // 系统主题 / 任务栏外观变了：让它重新采样底色（平时不每 2 秒重采，免得颜色自己抖）
        WM_SETTINGCHANGE => {
            if let Some(panel) = panel_mut(hwnd) {
                panel.forget_background();
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_LBUTTONDOWN => {
            hint::hide();
            // 拖面板挪位置：按下先记起点，移动超过 3px 才算拖动，否则松手还是切视图
            let mut cursor = POINT { x: 0, y: 0 };
            GetCursorPos(&mut cursor);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetWindowRect(hwnd, &mut rect);
            DRAG_FROM = cursor;
            DRAG_WINDOW = (rect.left, rect.top);
            DRAG_MOVED = false;
            DRAG_ACTIVE = true;
            SetCapture(hwnd);
            0
        }
        WM_MOUSEMOVE => {
            // 鼠标停在底部指示条上 → 弹个说明
            if !DRAG_ACTIVE {
                track_mouse_leave(hwnd);
                update_gauge_hint(hwnd);
                return 0;
            }
            update_gauge_hint(hwnd);
            let mut cursor = POINT { x: 0, y: 0 };
            GetCursorPos(&mut cursor);
            let dx = cursor.x - DRAG_FROM.x;
            let dy = cursor.y - DRAG_FROM.y;
            if !DRAG_MOVED && dx.abs() + dy.abs() < 3 {
                return 0;
            }
            DRAG_MOVED = true;
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                DRAG_WINDOW.0 + dx,
                DRAG_WINDOW.1 + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            );
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let moved = {
                // 单线程 UI，直接读+清标志；以前写 &mut DRAG_ACTIVE 属于可变静态引用
                let active = DRAG_ACTIVE;
                DRAG_ACTIVE = false;
                active && DRAG_MOVED
            };
            if moved {
                // 记住拖到的位置（夹在任务栏里），下次启动还在这儿
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetWindowRect(hwnd, &mut rect);
                let taskbar = taskbar_for(hwnd);
                let mut bar = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                if !taskbar.is_null() {
                    GetWindowRect(taskbar, &mut bar);
                }
                if let Some(panel) = panel_mut(hwnd) {
                    let width = rect.right - rect.left;
                    let height = rect.bottom - rect.top;
                    // 竖排任务栏装不下横条面板：拖到侧边栏时按整块工作区夹，
                    // 而不是夹进那条 48px 宽的竖栏
                    let vertical = (bar.bottom - bar.top) > (bar.right - bar.left) * 2;
                    let area = if vertical {
                        work_area_at(POINT {
                            x: bar.left,
                            y: bar.top,
                        })
                    } else {
                        bar
                    };
                    panel.settings.panel_x =
                        rect.left.clamp(area.left, (area.right - width).max(area.left));
                    panel.settings.panel_y =
                        rect.top.clamp(area.top, (area.bottom - height).max(area.top));
                    panel.settings.save();
                }
            } else if let Some(panel) = panel_mut(hwnd) {
                panel.cycle_view();
            }
            0
        }
        WM_MOUSELEAVE => {
            unsafe {
                TRACKING_LEAVE = false;
            }
            hint::hide();
            0
        }
        WM_RBUTTONDOWN => {
            hint::hide();
            PostMessageW(hwnd, WM_APP_SHOW_MENU, 0, 0);
            0
        }

        WM_APP_SHOW_MENU => {

            show_menu(hwnd);
            0
        }
        WM_APP_TRANSLATE_READY => {
            let pointer = wparam as *mut translate::TranslateOutcome;
            if !pointer.is_null() {
                let outcome = Box::from_raw(pointer);
                // 原文和译文分开给：结果窗自己按「逐句 / 整段」排
                ocr_ui::open_translate(
                    hwnd,
                    &outcome.source,
                    &outcome.translated,
                    outcome.error.as_deref(),
                );
            }
            0
        }
        WM_APP_ANIM => {
            let mut keep = false;
            if let Some(panel) = panel_mut(hwnd) {
                keep = panel.animate();
            }
            if !keep {
                request_anim(false);
            }
            0
        }
        msg if msg == menu::WM_APP_MENU_PICK => {
            menu::release();
            handle_command(hwnd, wparam);
            0
        }
        WM_APP_SHOT_FIRE => {
            shot::begin(hwnd);
            0
        }
        WM_HOTKEY => {
            let settings = panel_mut(hwnd)
                .map(|panel| panel.settings.clone())
                .unwrap_or_default();
            // 本机前台是远控窗口（UU 远程 / ToDesk / 向日葵…）：说明它只是控制端，
            // 这次按键会被远控转发到被控端，让那边去截图，本机不要再弹一份。
            let controller = foreground_is_remote_client();
            match wparam as i32 {
                HOTKEY_SHOT if settings.shot_enabled && !controller => shot::request(hwnd),
                HOTKEY_OCR if settings.ocr_enabled && !controller => shot::request_ocr(hwnd),
                HOTKEY_GIF if settings.gif_enabled && !controller => {
                    // 录着就停，没录就开始选区
                    if gifrec::is_recording() {
                        gifrec::stop();
                    } else {
                        shot::request_gif(
                            hwnd,
                            settings.gif_fps.max(0) as u32,
                            settings.gif_quality as u32,
                            settings.gif_seconds as u32,
                        );
                    }
                }
                _ => {}
            }
            0
        }
        WM_APP_GIF_DONE => {
            let pointer = wparam as *mut gifrec::RecOutput;
            if !pointer.is_null() {
                let output = *Box::from_raw(pointer);
                // 1 = 复制到剪贴板（默认），2 = 另存为
                if lparam == 2 {
                    gifrec::save_and_notify(hwnd, output);
                } else {
                    gifrec::copy_to_clipboard(hwnd, output);
                }
            }
            0
        }
        WM_APP_SCROLL_DONE => {
            let pointer = wparam as *mut (shot::Bitmap, RECT);
            if !pointer.is_null() {
                let (bitmap, rect) = *Box::from_raw(pointer);
                shot::open_editor(hwnd, bitmap, (rect.left, rect.top, rect.right, rect.bottom));
            }
            0
        }
        WM_APP_OCR_READY => {
            let pointer = wparam as *mut ocr::OcrOutcome;
            if !pointer.is_null() {
                let outcome = Box::from_raw(pointer);
                ocr_ui::open(hwnd, &outcome.text, outcome.error.as_deref());
            }
            0
        }
        WM_DESTROY => {
            hint::hide();
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Panel;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            remove_tray(hwnd);
            // 窗口没了就退出整个进程（包括托盘/AI 线程），避免留下僵尸占着互斥体
            std::process::exit(0);
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn panel_mut(hwnd: HWND) -> Option<&'static mut Panel> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Panel;
        ptr.as_mut()
    }
}

/// 登记 WM_MOUSELEAVE：鼠标移出面板时才能把悬浮说明收掉
fn track_mouse_leave(hwnd: HWND) {
    unsafe {
        if TRACKING_LEAVE {
            return;
        }
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        if TrackMouseEvent(&mut track) != 0 {
            TRACKING_LEAVE = true;
        }
    }
}

/// 光标在指示条上就弹说明，离开就收掉
fn update_gauge_hint(hwnd: HWND) {
    let Some(panel) = panel_mut(hwnd) else {
        return;
    };
    if unsafe { HIDDEN_BY_USER } || unsafe { IsWindowVisible(hwnd) } == 0 {
        hint::hide();
        return;
    }

    let Some(zone) = panel.gauge_hot_rect() else {
        hint::hide();
        return;
    };

    let cursor = hint::cursor();
    let inside = cursor.x >= zone.left
        && cursor.x < zone.right
        && cursor.y >= zone.top
        && cursor.y < zone.bottom;
    if !inside {
        hint::hide();
        return;
    }

    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetWindowRect(hwnd, &mut frame) };
    let text = panel.gauge_hint();
    unsafe { hint::show(frame, &text) };
}

fn show_menu(hwnd: HWND) {
    hint::hide();
    let Some(panel) = panel_mut(hwnd) else {
        return;
    };

    let view = panel.view().to_string();
    let gauge = panel.gauge_source().to_string();
    let ai = panel.settings.ai_enabled;

    // 面板显示合并成一行：点一下切到下一个视图（和左键点面板一样）
    let view_label = match view.as_str() {
        "speed" => "面板显示：网速",
        "memory" => "面板显示：内存",
        "temperature" => "面板显示：温度",
        _ => "面板显示：AI 速度",
    };
    let mut rows = vec![menu::Entry::item(ID_VIEW_CYCLE, view_label)];
    rows.push(menu::Entry::separator());
    rows.push(menu::Entry::header("底部指示条"));
    rows.push(menu::Entry::radio(ID_GAUGE_MEMORY, "内存占用", gauge == "mem"));
    if ai {
        rows.push(menu::Entry::radio(ID_GAUGE_AI, "Token 速度", gauge == "ai"));
    }
    rows.push(menu::Entry::radio(ID_GAUGE_NONE, "关闭", gauge == "none"));
    rows.push(menu::Entry::separator());
    if ai {
        rows.push(menu::Entry::item(ID_HISTORY, "Token 用量历史…"));
    }
    rows.push(menu::Entry::item(ID_SETTINGS, "设置…"));
    rows.push(menu::Entry::separator());
    if panel.settings.shot_enabled {
        rows.push(menu::Entry::hotkey(ID_SHOT, "截图…", panel.settings.shot_hotkey.clone()));
    }
    if panel.settings.ocr_enabled {
        rows.push(menu::Entry::hotkey(
            ID_OCR,
            "取字（OCR）…",
            panel.settings.ocr_hotkey.clone(),
        ));
    }

    if panel.settings.gif_enabled {
        rows.push(menu::Entry::hotkey(
            ID_GIF,
            if gifrec::is_recording() { "结束 MP4 录制" } else { "录制区域 MP4…" },
            panel.settings.gif_hotkey.clone(),
        ));
    }
    rows.push(menu::Entry::separator());
    rows.push(menu::Entry::item(ID_PANEL_SNAP, "吸附到任务栏"));
    rows.push(menu::Entry::item(ID_PANEL_HIDE, "隐藏面板"));
    rows.push(menu::Entry::item(ID_QUIT, "退出"));

    unsafe {
        menu::open(hwnd, rows);
    }
}

fn handle_command(hwnd: HWND, command: usize) {
    unsafe {
        let Some(panel) = panel_mut(hwnd) else {
            return;
        };

        match command {
            ID_VIEW_CYCLE => panel.cycle_view(),
            ID_SPEED => panel.set_view("speed"),
            ID_MEMORY => panel.set_view("memory"),
            ID_TEMPERATURE => panel.set_view("temperature"),
            ID_AI => panel.set_view("ai"),
            ID_GAUGE_MEMORY => panel.set_gauge("mem"),
            ID_GAUGE_AI => panel.set_gauge("ai"),
            ID_GAUGE_NONE => panel.set_gauge("none"),
            ID_HISTORY => {
                history_ui::open(hwnd);
            }
            ID_SETTINGS => {
                let settings = panel.settings.clone();
                settings_ui::open(hwnd, &settings);
            }
            ID_SHOT => {
                shot::request(hwnd);
            }
            ID_OCR => {
                shot::request_ocr(hwnd);
            }
            ID_GIF => {
                if gifrec::is_recording() {
                    gifrec::stop();
                } else {
                    let fps = panel.settings.gif_fps.max(0) as u32;
                    let quality = panel.settings.gif_quality as u32;
                    let seconds = panel.settings.gif_seconds as u32;
                    shot::request_gif(hwnd, fps, quality, seconds);
                }
            }
            ID_PANEL_SNAP => {
                panel.reset_anchor();
                let taskbar = taskbar_for(hwnd);
                position_panel(hwnd, taskbar, panel.desired_width(), panel.height());
                keep_on_top(hwnd);
            }
            ID_PANEL_HIDE => {
                HIDDEN_BY_USER = true;
                ShowWindow(hwnd, SW_HIDE);
            }
            ID_QUIT => {
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

#[allow(dead_code)]
fn unused(_: *mut c_void) {}

// ---------------------------------------------------------------- 托盘

fn copy_wide(target: &mut [u16], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().take(target.len() - 1).collect();
    for (index, unit) in encoded.iter().enumerate() {
        target[index] = *unit;
    }
    target[encoded.len()] = 0;
}

fn add_tray(hwnd: HWND) {
    unsafe {
        let icon = tray::create_tray_icon();
        TRAY_ICON = icon;

        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = icon;
        copy_wide(&mut data.szTip, "网速插件 (Rust)");

        Shell_NotifyIconW(NIM_ADD, &data);
    }
}

fn update_tray(hwnd: HWND, tip: &str) {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_TIP;
        copy_wide(&mut data.szTip, tip);
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn remove_tray(hwnd: HWND) {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        Shell_NotifyIconW(NIM_DELETE, &data);
        tray::destroy_icon(TRAY_ICON);
        TRAY_ICON = std::ptr::null_mut();
    }
}
// ---------------------------------------------------------------- 设置窗口回调

/// 设置面板宽度并落盘（键盘左右键、离散操作走这里）
pub(crate) fn apply_panel_width(parent: HWND, width: i32) {
    preview_panel_width(parent, width);
    commit_panel_width(parent);
}

/// 拖动滑杆时：只改宽度和位置，不写 ini（一次拖动几十档，落盘没必要）
pub(crate) fn preview_panel_width(parent: HWND, width: i32) {
    if let Some(panel) = panel_mut(parent) {
        panel.preview_panel_width(width);
        let taskbar = taskbar_for(parent);
        position_panel(parent, taskbar, panel.desired_width(), panel.height());
    }
}

/// 松开滑杆：把最终宽度写进 ini
pub(crate) fn commit_panel_width(parent: HWND) {
    if let Some(panel) = panel_mut(parent) {
        panel.commit_panel_width();
    }
}

/// 设置里改字号：重建面板字体，并按新宽度重新贴任务栏。
pub(crate) fn apply_font_scale(parent: HWND, percent: i32) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_font_scale(percent);
        let taskbar = unsafe { FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null()) };
        position_panel(parent, taskbar, panel.desired_width(), panel.height());
    }
}

/// 设置窗里改了翻译配置：立刻落盘（下次翻译就用新的）
pub(crate) fn apply_translate_settings(
    parent: HWND,
    provider: &str,
    key: &str,
    model: &str,
    base: &str,
    profiles: &[netspeed_core::settings::TranslateProfile],
) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.translate_provider = provider.to_string();
        panel.settings.translate_key = key.to_string();
        panel.settings.translate_model = model.to_string();
        panel.settings.translate_base = base.to_string();
        // 每家一份：切换服务商时互不覆盖
        for (slot, profile) in panel.settings.translate_profiles.iter_mut().zip(profiles) {
            *slot = profile.clone();
        }
        panel.settings.save();
        crate::shot::set_translate_enabled(panel.settings.translate_enabled);
    }
}

/// 设置窗里拨「启用翻译」：落盘 + 让工具栏的翻译按钮跟着出现/消失
pub(crate) fn apply_translate_enabled(parent: HWND, enabled: bool) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.translate_enabled = enabled;
        panel.settings.save();
        crate::shot::set_translate_enabled(enabled);
    }
}

pub(crate) fn apply_network_interface(parent: HWND, interface: &str) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_network_interface(interface);
    }
}

/// 按面板当前设置注册 / 重注册全局热键：关掉的功能不注册，也不占着组合键。
fn register_hotkeys(hwnd: HWND) {
    let Some(panel) = panel_mut(hwnd) else {
        return;
    };
    let settings = &panel.settings;

    unsafe {
        UnregisterHotKey(hwnd, HOTKEY_SHOT);
        UnregisterHotKey(hwnd, HOTKEY_OCR);
        UnregisterHotKey(hwnd, HOTKEY_GIF);

        if settings.shot_enabled {
            if let Some((mods, vk)) = parse_hotkey(&settings.shot_hotkey) {
                RegisterHotKey(hwnd, HOTKEY_SHOT, mods, vk);
            }
        }
        if settings.ocr_enabled {
            if let Some((mods, vk)) = parse_hotkey(&settings.ocr_hotkey) {
                RegisterHotKey(hwnd, HOTKEY_OCR, mods, vk);
            }
        }
        if settings.gif_enabled {
            if let Some((mods, vk)) = parse_hotkey(&settings.gif_hotkey) {
                RegisterHotKey(hwnd, HOTKEY_GIF, mods, vk);
            }
        }
    }
}

/// 设置里打开/关闭截图：菜单项和全局热键一起跟着开关。
pub(crate) fn apply_shot_enabled(parent: HWND, enabled: bool) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.shot_enabled = enabled;
        panel.settings.save();
    }
    register_hotkeys(parent);
}

/// 设置里打开/关闭取字（OCR）：截图工具栏里的 OCR 按钮也一起隐藏。
pub(crate) fn apply_ocr_enabled(parent: HWND, enabled: bool) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.ocr_enabled = enabled;
        panel.settings.save();
    }
    shot::set_ocr_enabled(enabled);
    register_hotkeys(parent);
}

/// 设置里改截图 / 取字快捷键（`ocr` = true 时改取字）：立刻重注册。
pub(crate) fn apply_hotkey(parent: HWND, ocr: bool, hotkey: &str) {
    if let Some(panel) = panel_mut(parent) {
        if ocr {
            panel.settings.ocr_hotkey = hotkey.to_string();
        } else {
            panel.settings.shot_hotkey = hotkey.to_string();
        }
        panel.settings.save();
    }
    register_hotkeys(parent);
}

/// 设置里开关 GIF 录制：菜单项和热键一起跟着开关。
pub(crate) fn apply_gif_enabled(parent: HWND, enabled: bool) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.gif_enabled = enabled;
        panel.settings.save();
    }
    register_hotkeys(parent);
}

/// 设置里改录制帧率（自动 / 30 / 60 / 120）：下一次录制生效。
pub(crate) fn apply_gif_fps(parent: HWND, fps: i32) {
    if let Some(panel) = panel_mut(parent) {
        // 以前这里写死成 `if fps >= 60 { 60 } else { 30 }`，设置界面的 120 存不进去
        panel.settings.gif_fps = netspeed_core::settings::normalize_fps(fps);
        panel.settings.save();
    }
}

/// 设置里改录制画质（50 / 65 / 80）：下一次录制生效。
pub(crate) fn apply_gif_quality(parent: HWND, quality: i32) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.gif_quality = netspeed_core::settings::normalize_quality(quality);
        panel.settings.save();
    }
}

/// 设置里改录制时长上限（10 / 20 / 30 秒）：下一次录制生效。
pub(crate) fn apply_gif_seconds(parent: HWND, seconds: i32) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.gif_seconds = netspeed_core::settings::normalize_seconds(seconds);
        panel.settings.save();
    }
}

/// 编辑窗上次是不是置顶（记住用户的选择）
pub(crate) fn editor_topmost(parent: HWND) -> bool {
    panel_mut(parent)
        .map(|panel| panel.settings.shot_topmost)
        .unwrap_or(false)
}

/// 编辑窗上点了置顶：存下来，下次沿用
pub(crate) fn set_editor_topmost(parent: HWND, on: bool) {
    if parent.is_null() {
        return;
    }
    if let Some(panel) = panel_mut(parent) {
        panel.settings.shot_topmost = on;
        panel.settings.save();
    }
}

/// 设置里改 GIF 热键：立刻重注册。
pub(crate) fn apply_gif_hotkey(parent: HWND, hotkey: &str) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.gif_hotkey = hotkey.to_string();
        panel.settings.save();
    }
    register_hotkeys(parent);
}

/// 设置里改刷新间隔：立刻按新节奏采样重绘。
pub(crate) fn apply_refresh_ms(parent: HWND, ms: i32) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_refresh_ms(ms);
    }
}

pub(crate) fn apply_gauge(parent: HWND, source: &str) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_gauge(source);
    }
}

/// 设置里打开/关闭 AI 统计：开了常驻采集，关了立刻停线程。
pub(crate) fn apply_ai_enabled(parent: HWND, enabled: bool) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_ai_enabled(enabled);
    }
}

/// 设置窗里勾/取消「开机自启」：落盘 + 写注册表
pub(crate) fn apply_auto_start(parent: HWND, enabled: bool) {
    if let Some(panel) = panel_mut(parent) {
        panel.settings.auto_start = enabled;
        panel.settings.save();
    }
    autostart::set(enabled);
}

/// 设置里勾选的 AI 来源（OpenCode / dsh / WorkBuddy / WorkBuddy AI）。
pub(crate) fn apply_ai_sources(parent: HWND, flags: [bool; 6]) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_ai_sources(flags);
    }
}

/// 屏幕缩放比例（DPI/96），和启动时那套一致。
pub(crate) fn screen_scale() -> f32 {
    let dpi = unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dpi = GetDeviceCaps(screen, 88) as u32;
        ReleaseDC(std::ptr::null_mut(), screen);
        dpi.max(96)
    };
    dpi as f32 / 96.0
}

pub(crate) fn dpi_for_window(hwnd: HWND) -> u32 {
    unsafe {
        // 分层窗口刚创建、还没显示时 GetDC(hwnd)+LOGPIXELSX 会返回 96（虚拟化值），
        // 会让面板算出 109x40 这种未缩放尺寸。优先用 GetDpiForWindow。
        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
        if dpi >= 96 {
            return dpi;
        }

        let dc = GetDC(hwnd);
        let dpi = GetDeviceCaps(dc, 88) as u32;
        ReleaseDC(hwnd, dc);
        dpi.max(96)
    }
}

#[cfg(test)]
mod tests {
    use super::is_remote_client_exe;

    #[test]
    fn remote_client_exe_names() {
        assert!(is_remote_client_exe(r"F:\uu\GameViewer\bin\GameViewer.exe"));
        assert!(is_remote_client_exe(
            r"C:\Program Files\UU Remote\UURemote.exe"
        ));
        assert!(is_remote_client_exe(r"C:\Program Files\ToDesk\ToDesk.exe"));
        assert!(is_remote_client_exe(r"C:\Program Files\Sunlogin\SunloginClient.exe"));

        // 名字里只是包含关键词不算
        assert!(!is_remote_client_exe(r"C:\Windows\explorer.exe"));
        assert!(!is_remote_client_exe(r"C:\Tools\GameViewerHelper\foo.exe"));
        assert!(!is_remote_client_exe(""));
    }
}