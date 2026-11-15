//! Rust 版任务栏网速插件（阶段一：面板 + 网速/内存 + 指示条 + 右键菜单）。
//!
//! 和 C# 版共用 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，可以并行试跑。

#![windows_subsystem = "windows"]

mod history_ui;
mod panel;
mod settings_ui;
mod theme;
mod tray;

use netspeed_core::settings::Settings;
use panel::Panel;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use theme::wide;

const WM_APP_TICK: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW_MENU: u32 = WM_APP + 3;
const TIMER_ID: usize = 1;
const TRAY_ID: u32 = 1;

thread_local! {
    /// 上一次写进托盘的提示文本
    static LAST_TIP: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}

/// 任务栏句柄：用来发现 Explorer 重启（我们的子窗口会被连带销毁）
static mut TASKBAR: HWND = std::ptr::null_mut();

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
const ID_QUIT: usize = 1099;

fn main() {
    unsafe {
        // 必须在任何窗口/坐标计算之前：否则进程是 DPI 不感知的，
        // 系统会把坐标和尺寸按 150% 缩放虚拟化（面板位置/大小全错）
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
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

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide(WINDOW_TITLE).as_ptr(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            width,
            height,
            taskbar,
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
        SetTimer(hwnd, TIMER_ID, 500, None);

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


/// 任务栏里的 MSTaskListWClass 会压在我们上面（点击都给它），必须周期性置顶。
fn keep_on_top(hwnd: HWND) {
    unsafe {
        SetWindowPos(
            hwnd,
            0 as HWND, // HWND_TOP
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
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

        // 子窗口的 SetWindowPos 用「父窗口客户区」坐标，要减去任务栏原点
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            left.max(bar.left) - bar.left,
            top.max(bar.top) - bar.top,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
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
            if let Some(panel) = panel_mut(hwnd) {
                panel.tick();
            }
            0
        }
        WM_TIMER => {
            // Explorer 重启会把我们的子窗口连带销毁：此时直接退出，
            // 否则进程会变成“没有窗口、还占着单实例互斥体”的僵尸（再次启动就闪退）
            if !TASKBAR.is_null() && IsWindow(TASKBAR) == 0 {
                std::process::exit(0);
            }

            keep_on_top(hwnd);
            if let Some(panel) = panel_mut(hwnd) {
                panel.tick();
                let tip = panel.tip_text();
                // NIM_MODIFY 很贵，文本没变就别调
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
        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Panel;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            KillTimer(hwnd, TIMER_ID);
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
            ID_PANEL_HIDE => {
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
        let dc = GetDC(hwnd);
        let dpi = GetDeviceCaps(dc, 88) as u32;
        ReleaseDC(hwnd, dc);
        dpi.max(96)
    }
}