//! Rust 版任务栏网速插件（阶段一：面板 + 网速/内存 + 指示条 + 右键菜单）。
//!
//! 和 C# 版共用 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，可以并行试跑。

#![windows_subsystem = "windows"]

mod history_ui;
mod menu;
mod ocr;
mod ocr_ui;
mod panel;
mod settings_ui;
mod shot;
mod theme;
mod tray;
mod ui;

use netspeed_core::settings::Settings;
use panel::Panel;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
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
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, MOD_CONTROL, MOD_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use theme::wide;

const WM_APP_TICK: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW_MENU: u32 = WM_APP + 3;
pub(crate) const WM_APP_SHOT_FIRE: u32 = WM_APP + 4;
const WM_APP_OCR_READY: u32 = WM_APP + 5;
const HOTKEY_SHOT: i32 = 1;
const HOTKEY_OCR: i32 = 2;
const TRAY_ID: u32 = 1;

thread_local! {
    /// 上一次写进托盘的提示文本
    static LAST_TIP: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}

/// 任务栏句柄：用来发现 Explorer 重启（我们的子窗口会被连带销毁）
static mut TASKBAR: HWND = std::ptr::null_mut();

/// 用户从菜单里「隐藏面板」后不再自动显示（只有双击托盘才恢复）
static mut HIDDEN_BY_USER: bool = false;

// 拖动面板（左键按住移动）：记起点和窗口原位，超过 3px 才算拖动
static mut DRAG_ACTIVE: bool = false;
static mut DRAG_MOVED: bool = false;
static mut DRAG_FROM: POINT = POINT { x: 0, y: 0 };
static mut DRAG_WINDOW: (i32, i32) = (0, 0);

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
const ID_OCR: usize = 1303;
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

        position_panel(hwnd, taskbar, width, height);
        keep_on_top(hwnd);
        add_tray(hwnd);

        // 注意：本进程里的 SetTimer / WM_TIMER 完全不可靠（实测 set 成功但永不触发，
        // 回调形式也一样），所以周期刷新用一个后台线程投递消息，保证一定能到。
        start_ticker(hwnd);

        // Ctrl+Shift+A 截图
        RegisterHotKey(hwnd, HOTKEY_SHOT, MOD_CONTROL | MOD_SHIFT, 0x41);
        RegisterHotKey(hwnd, HOTKEY_OCR, MOD_CONTROL | MOD_SHIFT, 0x4F);

        // 调试/快捷入口：netspeed.exe --settings 直接打开设置窗口
        if std::env::args().any(|arg| arg == "--settings") {
            settings_ui::open(
                hwnd,
                settings.panel_width,
                &settings.gauge_source,
                &settings.network_interface,
                settings.refresh_ms,
            );
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
        // 没手动拖过（PanelX = -1）才跟着托盘自动对齐；拖过之后只做 TOPMOST
        let saved_x = panel_mut(hwnd).map(|panel| panel.settings.panel_x).unwrap_or(-1);

        if saved_x < 0 {
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

        let default_top = bar.top + ((bar.bottom - bar.top) - height) / 2;

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
        WM_LBUTTONDOWN => {
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
            if !DRAG_ACTIVE {
                return 0;
            }
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
            let moved = std::mem::replace(&mut DRAG_ACTIVE, false) && DRAG_MOVED;
            if moved {
                // 记住拖到的位置（夹在任务栏里），下次启动还在这儿
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetWindowRect(hwnd, &mut rect);
                let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
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
                    panel.settings.panel_x =
                        rect.left.clamp(bar.left, (bar.right - width).max(bar.left));
                    panel.settings.panel_y =
                        rect.top.clamp(bar.top, (bar.bottom - height).max(bar.top));
                    panel.settings.save();
                }
            } else if let Some(panel) = panel_mut(hwnd) {
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
            match wparam as i32 {
                HOTKEY_SHOT => shot::request(hwnd),
                HOTKEY_OCR => shot::request_ocr(hwnd),
                _ => {}
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
    let Some(panel) = panel_mut(hwnd) else {
        return;
    };

    let view = panel.view().to_string();
    let gauge = panel.gauge_source().to_string();

    let rows = vec![
        menu::Entry::header("面板显示"),
        menu::Entry::checked(ID_SPEED, "网速", view == "speed"),
        menu::Entry::checked(ID_MEMORY, "内存", view == "memory"),
        menu::Entry::checked(ID_TEMPERATURE, "温度", view == "temperature"),
        menu::Entry::checked(ID_AI, "AI 速度", view == "ai"),
        menu::Entry::separator(),
        menu::Entry::header("底部指示条"),
        menu::Entry::indented(ID_GAUGE_VIEW, "跟随当前视图", gauge == "view"),
        menu::Entry::indented(ID_GAUGE_SPEED, "网速", gauge == "speed"),
        menu::Entry::indented(ID_GAUGE_MEMORY, "内存占用", gauge == "mem"),
        menu::Entry::indented(ID_GAUGE_CPU, "CPU 温度", gauge == "cpu"),
        menu::Entry::indented(ID_GAUGE_AI, "AI 速度", gauge == "ai"),
        menu::Entry::indented(ID_GAUGE_NONE, "关闭", gauge == "none"),
        menu::Entry::separator(),
        menu::Entry::item(ID_HISTORY, "Token 用量历史…"),
        menu::Entry::item(ID_SETTINGS, "设置…"),
        menu::Entry::separator(),
        menu::Entry::hotkey(ID_SHOT, "截图…", "Ctrl+Shift+A"),
        menu::Entry::hotkey(ID_OCR, "取字…", "Ctrl+Shift+O"),
        menu::Entry::separator(),
        menu::Entry::item(ID_PANEL_HIDE, "隐藏面板"),
        menu::Entry::item(ID_QUIT, "退出"),
    ];

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
                let interface = panel.settings.network_interface.clone();
                let refresh_ms = panel.settings.refresh_ms;
                settings_ui::open(hwnd, width, &gauge, &interface, refresh_ms);
            }
            ID_SHOT => {
                shot::request(hwnd);
            }
            ID_OCR => {
                shot::request_ocr(hwnd);
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

pub(crate) fn apply_network_interface(parent: HWND, interface: &str) {
    if let Some(panel) = panel_mut(parent) {
        panel.set_network_interface(interface);
    }
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