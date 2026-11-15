//! Rust 版任务栏网速插件（阶段一：面板 + 网速/内存 + 指示条 + 右键菜单）。
//!
//! 和 C# 版共用 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，可以并行试跑。

#![windows_subsystem = "windows"]

mod history_ui;
mod panel;
mod settings_ui;
mod shot;
mod theme;
mod tray;

use netspeed_core::settings::Settings;
use panel::Panel;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ShowWindow, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_CONTROL, MOD_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use theme::wide;

const WM_APP_TICK: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW_MENU: u32 = WM_APP + 3;
pub(crate) const WM_APP_SHOT_FIRE: u32 = WM_APP + 4;
const HOTKEY_SHOT: i32 = 1;
const TRAY_ID: u32 = 1;

thread_local! {
    /// 上一次写进托盘的提示文本
    static LAST_TIP: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}

/// 任务栏句柄：用来发现 Explorer 重启（我们的子窗口会被连带销毁）
static mut TASKBAR: HWND = std::ptr::null_mut();

/// 用户从菜单里「隐藏面板」后不再自动显示（只有双击托盘才恢复）
static mut HIDDEN_BY_USER: bool = false;

/// 隐藏的顶层窗口：TrackPopupMenu 要求调用线程在前台，
/// 子窗口没法变前台，只能借一个自己的顶层窗口来"占前台"
static mut MENU_HOST: HWND = std::ptr::null_mut();

/// 托盘图标句柄（进程内唯一）
static mut TRAY_ICON: windows_sys::Win32::UI::WindowsAndMessaging::HICON = std::ptr::null_mut();

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Panel";
const WINDOW_TITLE: &str = "NetSpeedTaskbar";

const ID_SPEED: usize = 1001;
const ID_MEMORY: usize = 1002;
const ID_TEMPERATURE: usize = 1003;
const ID_AI: usize = 1004;
const ID_GAUGE_VIEW: usize = 1101;
const ID_GAUGE_SPEED: usize = 1102;
const ID_GAUGE_MEMORY: usize = 1103;
const ID_GAUGE_CPU: usize = 1104;
const ID_GAUGE_AI: usize = 1105;
const ID_GAUGE_NONE: usize = 1106;
const ID_HISTORY: usize = 1300;
const ID_SETTINGS: usize = 1301;
const ID_PANEL_HIDE: usize = 1200;
const ID_SHOT: usize = 1302;
const ID_QUIT: usize = 1099;

fn main() {
    unsafe {
        // 必须在任何窗口/坐标计算之前：否则进程是 DPI 不感知的，
        // 系统会把坐标和尺寸按 150% 缩放虚拟化（面板位置/大小全错）
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );

        // 通用对话框（另存为）需要 COM 单元，不开的话 GetSaveFileNameW 会直接失败
        windows_sys::Win32::System::Com::CoInitializeEx(
            std::ptr::null(),
            windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32,
        );

        let mutex_name = wide("Local\\NetSpeedTaskbar.Rust");
        let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
        if mutex.is_null() || GetLastError() == 183 {
            // 已经在跑了
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

        // 菜单宿主：隐藏的顶层窗口
        let host_class = wide("NetSpeedTaskbarRs.MenuHost");
        let host_wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: host_class.as_ptr(),
        };
        RegisterClassW(&host_wc);
        // 必须是个“可见”的顶层窗口才能拿到前台（隐藏窗口不行），放到屏幕外 1x1
        MENU_HOST = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            host_class.as_ptr(),
            wide("NetSpeedTaskbarRs.MenuHost").as_ptr(),
            WS_POPUP,
            -200,
            -200,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        ShowWindow(MENU_HOST, SW_SHOWNOACTIVATE);

        position_panel(hwnd, taskbar, width, height);
        keep_on_top(hwnd);
        add_tray(hwnd);

        // 注意：本进程里的 SetTimer / WM_TIMER 完全不可靠（实测 set 成功但永不触发，
        // 回调形式也一样），所以周期刷新用一个后台线程投递消息，保证一定能到。
        start_ticker(hwnd);

        // Ctrl+Shift+A 截图
        RegisterHotKey(hwnd, HOTKEY_SHOT, MOD_CONTROL | MOD_SHIFT, 0x41);

        // 调试/快捷入口：netspeed.exe --settings 直接打开设置窗口
        if std::env::args().any(|arg| arg == "--settings") {
            settings_ui::open(hwnd, settings.panel_width, &settings.gauge_source);
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
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
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


/// 顶层置顶窗口不用改 z 序，这里只负责跟随任务栏位置（任务栏可能移动/改尺寸）。
fn keep_on_top(hwnd: HWND) {
    unsafe {
        let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
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
        let left = if !tray.is_null() {
            let mut tray_rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetWindowRect(tray, &mut tray_rect);
            tray_rect.left - width - gap
        } else {
            bar.right - width - scale_for_dpi(220)
        };
        let top = bar.top + ((bar.bottom - bar.top) - height) / 2;

        if rect.left != left || rect.top != top {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                left.max(bar.left),
                top.max(bar.top),
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
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
        let left = if !tray.is_null() {
            let mut tray_rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetWindowRect(tray, &mut tray_rect);
            tray_rect.left - width - gap
        } else {
            bar.right - width - scale_for_dpi(220)
        };

        let top = bar.top + ((bar.bottom - bar.top) - height) / 2;

        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            left.max(bar.left),
            top.max(bar.top),
            width,
            height,
            SWP_NOACTIVATE,
        );
    }
}

/// 周期刷新：后台线程每 500ms 投递一次 WM_APP_TICK。
/// （本进程 SetTimer/WM_TIMER 实测完全收不到，改用线程投递最稳。）
/// 前台是不是一个占满整块显示器的窗口（游戏、视频全屏）。
/// 任务栏/桌面/有 owner 的弹窗都不算。
fn fullscreen_foreground() -> bool {
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

fn start_ticker(hwnd: HWND) {
    let handle = hwnd as usize;
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            let target = handle as HWND;
            if IsWindow(target) == 0 {
                break;
            }
            if PostMessageW(target, WM_APP_TICK, 0, 0) == 0 {
                break;
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
        WM_APP_TICK => {
            // Explorer 重启后我们的窗口会跟着销毁，这时直接退出进程，
            // 否则会留下「没有窗口但占着互斥体」的僵尸进程，导致再也打不开。
            if !TASKBAR.is_null() && IsWindow(TASKBAR) == 0 {
                std::process::exit(0);
            }

            keep_on_top(hwnd);

            // 全屏程序（游戏/视频）在前台时把自己藏起来，否则一个 TOPMOST 面板会糊在画面上
            let fullscreen = fullscreen_foreground();
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
            0
        }
        WM_APP_TRAY => {
            let event = (lparam as u32) & 0xFFFF;
            match event {
                WM_LBUTTONUP | WM_RBUTTONUP => show_menu(hwnd),
                WM_LBUTTONDBLCLK => {
                    HIDDEN_BY_USER = false;
                    ShowWindow(hwnd, SW_SHOW);
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
            0
        }
        WM_PRINTCLIENT => {
            if let Some(panel) = panel_mut(hwnd) {
                panel.render(wparam as windows_sys::Win32::Graphics::Gdi::HDC);
            }
            0
        }
        WM_LBUTTONUP => {
            if let Some(panel) = panel_mut(hwnd) {
                panel.cycle_view();
            }
            0
        }
        WM_RBUTTONDOWN => {

            PostMessageW(hwnd, WM_APP_SHOW_MENU, 0, 0);
            0
        }

        WM_APP_SHOW_MENU => {

            show_menu(hwnd);
            0
        }
        WM_APP_SHOT_FIRE => {
            shot::begin(hwnd);
            0
        }
        WM_HOTKEY => {
            if wparam as i32 == HOTKEY_SHOT {
                shot::request(hwnd);
            }
            0
        }
        WM_DESTROY => {
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

fn show_menu(hwnd: HWND) {
    unsafe {
        let Some(panel) = panel_mut(hwnd) else {
            return;
        };

        let menu = CreatePopupMenu();
        let view = panel.view().to_string();
        let gauge = panel.gauge_source().to_string();

        append(menu, ID_SPEED, "显示网速", view == "speed");
        append(menu, ID_MEMORY, "显示内存", view == "memory");
        append(menu, ID_TEMPERATURE, "显示温度", view == "temperature");
        append(menu, ID_AI, "显示 AI 速度", view == "ai");
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

        let gauge_menu = CreatePopupMenu();
        append(gauge_menu, ID_GAUGE_VIEW, "跟随当前视图", gauge == "view");
        append(gauge_menu, ID_GAUGE_SPEED, "网速", gauge == "speed");
        append(gauge_menu, ID_GAUGE_MEMORY, "内存占用", gauge == "mem");
        append(gauge_menu, ID_GAUGE_CPU, "CPU 温度", gauge == "cpu");
        append(gauge_menu, ID_GAUGE_AI, "AI 速度", gauge == "ai");
        append(gauge_menu, ID_GAUGE_NONE, "关闭", gauge == "none");
        AppendMenuW(
            menu,
            MF_POPUP,
            gauge_menu as usize,
            wide("底部指示条").as_ptr(),
        );

        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, ID_HISTORY, wide("Token 用量历史…").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_SETTINGS, wide("设置…").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_SHOT, wide("截图…\tCtrl+Shift+A").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_PANEL_HIDE, wide("隐藏面板（双击托盘恢复）").as_ptr());
        AppendMenuW(
            menu,
            MF_STRING,
            ID_QUIT,
            wide("退出 (Rust 版)").as_ptr(),
        );

        let mut point: windows_sys::Win32::Foundation::POINT = std::mem::zeroed();
        GetCursorPos(&mut point);

        // 关键：先把自己（隐藏顶层窗口）设为前台，否则 TrackPopupMenu 会立刻返回、菜单根本不显示
        let owner = if MENU_HOST.is_null() { hwnd } else { MENU_HOST };
        let fg = SetForegroundWindow(owner);
        let _ = fg;

        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            point.x,
            point.y,
            0,
            owner,
            std::ptr::null(),
        ) as usize;


        PostMessageW(owner, WM_NULL, 0, 0);

        DestroyMenu(menu);

        let Some(panel) = panel_mut(hwnd) else {
            return;
        };

        match command {
            ID_SPEED => panel.set_view("speed"),
            ID_MEMORY => panel.set_view("memory"),
            ID_TEMPERATURE => panel.set_view("temperature"),
            ID_AI => panel.set_view("ai"),
            ID_GAUGE_VIEW => panel.set_gauge("view"),
            ID_GAUGE_SPEED => panel.set_gauge("speed"),
            ID_GAUGE_MEMORY => panel.set_gauge("mem"),
            ID_GAUGE_CPU => panel.set_gauge("cpu"),
            ID_GAUGE_AI => panel.set_gauge("ai"),
            ID_GAUGE_NONE => panel.set_gauge("none"),
            ID_HISTORY => {
                history_ui::open(hwnd);
            }
            ID_SETTINGS => {
                let width = panel.settings.panel_width;
                let gauge = panel.settings.gauge_source.clone();
                settings_ui::open(hwnd, width, &gauge);
            }
            ID_SHOT => {
                shot::request(hwnd);
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

fn append(menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU, id: usize, text: &str, checked: bool) {
    unsafe {
        let flags = MF_STRING | if checked { MF_CHECKED } else { 0 };
        AppendMenuW(menu, flags, id, wide(text).as_ptr());
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
        let icon = tray::create_icon();
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

pub(crate) fn apply_panel_width(parent: HWND, width: i32) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_panel_width(width);
        let taskbar = unsafe { FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null()) };
        position_panel(parent, taskbar, panel.desired_width(), panel.height());
    }
}

pub(crate) fn apply_gauge(parent: HWND, source: &str) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_gauge(source);
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