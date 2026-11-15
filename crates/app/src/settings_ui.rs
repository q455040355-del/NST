//! 设置窗口：面板宽度滑块 + 底部指示条下拉。
//!
//! 用原生 Win32 控件（STATIC / msctls_trackbar32 / ComboBox / BUTTON），
//! 改动即时生效并写回 settings.ini。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, UpdateWindow, DEFAULT_GUI_FONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// windows-sys 没在默认 feature 里导出这些常量，按 SDK 定义写死
const COLOR_BTNFACE: u32 = 15;
const SS_RIGHT: u32 = 0x0002;
const BS_PUSHBUTTON: u32 = 0x0000;
const CBS_DROPDOWNLIST: u32 = 0x0003;
const TBS_HORZ: u32 = 0x0000;
const TBS_AUTOTICKS: u32 = 0x0008;
const TBM_GETPOS: u32 = WM_USER;
const TBM_SETPOS: u32 = WM_USER + 5;
const TBM_SETRANGE: u32 = WM_USER + 6;
const TBM_SETTICFREQ: u32 = WM_USER + 20;
const CB_ADDSTRING: u32 = 0x0143;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;
const CBN_SELCHANGE: usize = 1;

use crate::theme::wide;

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Settings";

const ID_WIDTH_LABEL: usize = 2001;
const ID_WIDTH_VALUE: usize = 2002;
const ID_WIDTH_TRACK: usize = 2003;
const ID_GAUGE_LABEL: usize = 2004;
const ID_GAUGE_COMBO: usize = 2005;
const ID_CLOSE: usize = 2006;

const GAUGE_OPTIONS: [(&str, &str); 6] = [
    ("跟随当前视图", "view"),
    ("网速", "speed"),
    ("内存占用", "mem"),
    ("CPU 温度", "cpu"),
    ("AI 速度", "ai"),
    ("关闭", "none"),
];

struct DialogState {
    parent: HWND,
    track: HWND,
    combo: HWND,
    value_label: HWND,
}

pub fn open(parent: HWND, width: i32, gauge: &str) {
    unsafe {
        // 已经开着就提到前面（同一时间只允许一个）
        let class = wide(CLASS_NAME);
        let mut found: HWND = std::ptr::null_mut();
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(enum_settings),
            &mut found as *mut HWND as isize,
        );
        if !found.is_null() {
            SetForegroundWindow(found);
            return;
        }

        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(CLASS_NAME);

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dialog_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as *mut _,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // 用屏幕 DC 的 LOGPIXELSX：面板本身也是这么算的，两处必须一致
        // （GetDpiForWindow 对挂在任务栏下的子窗口会返回 96，偏小）
        let dpi = crate::screen_scale();
        let width_px = (460.0 * dpi) as i32;
        let height_px = (300.0 * dpi) as i32;

        let mut screen = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut screen as *mut _ as *mut _, 0);
        let left = screen.left + ((screen.right - screen.left) - width_px) / 2;
        let top = screen.top + ((screen.bottom - screen.top) - height_px) / 3;

        let state = Box::into_raw(Box::new(DialogState {
            parent,
            track: std::ptr::null_mut(),
            combo: std::ptr::null_mut(),
            value_label: std::ptr::null_mut(),
        }));

        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
            class_name.as_ptr(),
            wide("网速显示设置").as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            left,
            top,
            width_px,
            height_px,
            parent,
            std::ptr::null_mut(),
            instance,
            state as *const _,
        );

        if hwnd.is_null() {
            let _ = Box::from_raw(state);
            return;
        }

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        create_children(hwnd, width, gauge, dpi);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
    }
}

unsafe extern "system" fn enum_settings(hwnd: HWND, lparam: LPARAM) -> i32 {
    let mut class = [0u16; 128];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if length > 0 {
        let name = String::from_utf16_lossy(&class[..length as usize]);
        if name == CLASS_NAME && IsWindowVisible(hwnd) != 0 {
            *(lparam as *mut HWND) = hwnd;
            return 0;
        }
    }

    1
}

fn create_children(hwnd: HWND, width: i32, gauge: &str, dpi: f32) {
    unsafe {
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let pad = (18.0 * dpi) as i32;

        let label = |text: &str, y: i32, width: i32| {
            let handle = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE,
                pad,
                y,
                width,
                (22.0 * dpi) as i32,
                hwnd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            SendMessageW(handle, WM_SETFONT, font as usize, 1);
            handle
        };

        label("面板宽度", (20.0 * dpi) as i32, (100.0 * dpi) as i32);

        let value_label = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(&format!("{width} px")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_RIGHT,
            (330.0 * dpi) as i32,
            (20.0 * dpi) as i32,
            (110.0 * dpi) as i32,
            (22.0 * dpi) as i32,
            hwnd,
            ID_WIDTH_VALUE as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        SendMessageW(value_label, WM_SETFONT, font as usize, 1);

        let track = CreateWindowExW(
            0,
            wide("msctls_trackbar32").as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | TBS_HORZ | TBS_AUTOTICKS,
            (110.0 * dpi) as i32,
            (16.0 * dpi) as i32,
            (210.0 * dpi) as i32,
            (40.0 * dpi) as i32,
            hwnd,
            ID_WIDTH_TRACK as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        SendMessageW(track, WM_SETFONT, font as usize, 1);
        SendMessageW(track, TBM_SETRANGE, 1, make_long(72, 360));
        SendMessageW(track, TBM_SETPOS, 1, width as isize);
        SendMessageW(track, TBM_SETTICFREQ, 24, 0);

        label("底部指示条", (78.0 * dpi) as i32, (110.0 * dpi) as i32);

        let combo = CreateWindowExW(
            0,
            wide("ComboBox").as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | CBS_DROPDOWNLIST | WS_VSCROLL,
            (110.0 * dpi) as i32,
            (74.0 * dpi) as i32,
            (320.0 * dpi) as i32,
            (200.0 * dpi) as i32,
            hwnd,
            ID_GAUGE_COMBO as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        SendMessageW(combo, WM_SETFONT, font as usize, 1);

        for (index, (text, _)) in GAUGE_OPTIONS.iter().enumerate() {
            let item = wide(text);
            SendMessageW(combo, CB_ADDSTRING, 0, item.as_ptr() as isize);
            if *text != "" && GAUGE_OPTIONS[index].1 == gauge {
                SendMessageW(combo, CB_SETCURSEL, index, 0);
            }
        }

        let close = CreateWindowExW(
            0,
            wide("Button").as_ptr(),
            wide("关闭").as_ptr(),
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
            (338.0 * dpi) as i32,
            (220.0 * dpi) as i32,
            (90.0 * dpi) as i32,
            (30.0 * dpi) as i32,
            hwnd,
            ID_CLOSE as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        SendMessageW(close, WM_SETFONT, font as usize, 1);

        // 把控件句柄回填进状态（WM_HSCROLL / WM_COMMAND 要用）
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogState;
        if !state.is_null() {
            (*state).track = track;
            (*state).combo = combo;
            (*state).value_label = value_label;
        }

        let _ = ID_WIDTH_LABEL;
        let _ = ID_GAUGE_LABEL;
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => 0,
        WM_HSCROLL => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const DialogState;
            if state.is_null() {
                return 0;
            }

            let state = &*state;
            // 只有一个滑块，不必比对 lparam
            let width = SendMessageW(state.track, TBM_GETPOS, 0, 0) as i32;
            let text = wide(&format!("{width} px"));
            SetWindowTextW(state.value_label, text.as_ptr());
            crate::apply_panel_width(state.parent, width);
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let code = (wparam >> 16) & 0xFFFF;
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const DialogState;
            if state.is_null() {
                return 0;
            }

            let state = &*state;
            if id == ID_GAUGE_COMBO && code == CBN_SELCHANGE {
                let index = SendMessageW(state.combo, CB_GETCURSEL, 0, 0) as usize;
                if let Some((_, value)) = GAUGE_OPTIONS.get(index) {
                    crate::apply_gauge(state.parent, value);
                }
            } else if id == ID_CLOSE {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn make_long(low: i32, high: i32) -> isize {
    ((low as u32 & 0xFFFF) | ((high as u32 & 0xFFFF) << 16)) as isize
}
