//! 指示条的小悬浮说明（自绘气泡）。
//!
//! 鼠标停在面板底部指示条上时，在面板上方弹出一个小卡片，
//! 说明当前这条指示条代表什么、具体数值是多少。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GetDC, InvalidateRect, ReleaseDC, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;
use crate::ui::{self, font, Painter, BORDER, DT_LEFT, SURFACE, TEXT};

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Hint";

const PAD_X: f32 = 9.0;
const HEIGHT: f32 = 24.0;
const GAP: f32 = 6.0;

struct State {
    text: String,
    scale: f32,
}

/// 当前气泡窗口（同一时间只有一个）
static mut HINT: HWND = std::ptr::null_mut();

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

unsafe fn paint(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &*state;

    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    GetClientRect(hwnd, &mut client);
    if client.right <= 0 || client.bottom <= 0 {
        return;
    }

    let Some(dib) = ui::Dib::new(client.right, client.bottom) else {
        return;
    };
    let p = Painter::new(dib.dc, state.scale);

    p.fill(0, 0, client.right, client.bottom, SURFACE);
    p.round(
        0,
        0,
        client.right,
        client.bottom,
        p.s(7.0),
        SURFACE,
        BORDER,
    );
    p.text(
        &state.text,
        font(9.5, 400, state.scale),
        TEXT,
        p.s(PAD_X),
        0,
        client.right - p.s(PAD_X),
        client.bottom,
        DT_LEFT,
    );

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, client.right, client.bottom, dib.dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);
}

unsafe extern "system" fn hint_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            let state = state_of(hwnd);
            if !state.is_null() {
                drop(Box::from_raw(state));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            if HINT == hwnd {
                HINT = std::ptr::null_mut();
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn visible() -> bool {
    unsafe { !HINT.is_null() && IsWindow(HINT) != 0 }
}

fn measure(text: &str, scale: f32) -> (i32, i32) {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let p = Painter::new(screen, scale);
        let width = p.text_width(text, font(9.5, 400, scale)) + ui::s(PAD_X * 2.0, scale);
        ReleaseDC(std::ptr::null_mut(), screen);
        (width.max(ui::s(40.0, scale)), ui::s(HEIGHT, scale))
    }
}

/// 在 `anchor`（屏幕坐标，一般传面板的窗口矩形）上方显示气泡；重复调用只更新内容。
pub unsafe fn show(anchor: RECT, text: &str) {
    if text.is_empty() {
        hide();
        return;
    }

    let scale = crate::screen_scale();
    let (width, height) = measure(text, scale);

    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);

    // 横向贴着面板居中，纵向放在面板上方（面板本身在任务栏里，放下面会出屏）
    let left = ((anchor.left + anchor.right) / 2 - width / 2)
        .max(work.left)
        .min((work.right - width).max(work.left));
    let mut top = anchor.top - height - ui::s(GAP, scale);
    if top < work.top {
        top = (anchor.bottom + ui::s(GAP, scale)).min(work.bottom - height);
    }

    if !HINT.is_null() && IsWindow(HINT) != 0 {
        let state = state_of(HINT);
        if !state.is_null() {
            (*state).text = text.to_string();
        }
        SetWindowPos(
            HINT,
            HWND_TOPMOST,
            left,
            top,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        InvalidateRect(HINT, std::ptr::null(), 0);
        return;
    }

    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(CLASS_NAME);
    let wc = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(hint_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);

    let state = Box::into_raw(Box::new(State {
        text: text.to_string(),
        scale,
    }));

    let hwnd = CreateWindowExW(
        // 不吃鼠标：气泡盖在面板上也不影响拖动/右键
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        class.as_ptr(),
        wide("").as_ptr(),
        WS_POPUP,
        left,
        top,
        width,
        height,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

    if hwnd.is_null() {
        let _ = Box::from_raw(state);
        return;
    }

    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    ui::round_corners(hwnd);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    InvalidateRect(hwnd, std::ptr::null(), 0);
    HINT = hwnd;
}

pub fn hide() {
    unsafe {
        if !HINT.is_null() && IsWindow(HINT) != 0 {
            DestroyWindow(HINT);
        }
        HINT = std::ptr::null_mut();
    }
}

/// 当前鼠标位置（屏幕坐标）
pub fn cursor() -> POINT {
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        point
    }
}
