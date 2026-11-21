//! 自绘的提示框 / 确认框：跟设置页、编辑窗一套风格。
//!
//! 原生 `MessageBoxW` 是 Win32 老圆角 + 灰底，和整个界面不搭，这里用同一套配色和圆角重画。

use std::cell::Cell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetStockObject, GetDC, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DT_CALCRECT, HGDIOBJ,
    NULL_BRUSH, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::{ui, wide};
use crate::ui::s;

const CLASS: &str = "NetSpeedTaskbarRs.Dialog";

struct Dialog {
    title: String,
    text: String,
    confirm: bool,
    /// 点了确定
    accepted: Cell<bool>,
    /// 窗口已关闭
    closed: Cell<bool>,
}

thread_local! {
    static STATE: Cell<*mut Dialog> = const { Cell::new(std::ptr::null_mut()) };
}

/// 只有一个「确定」的提示框
pub fn info(owner: HWND, title: &str, text: &str) {
    show(owner, title, text, false);
}

/// 确定 / 取消，返回是否点了确定
pub fn confirm(owner: HWND, title: &str, text: &str) -> bool {
    show(owner, title, text, true)
}

fn show(owner: HWND, title: &str, text: &str, confirm: bool) -> bool {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(proc_),
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

        // 先按文字量出窗口大小
        let scale = crate::screen_scale();
        let pad = s(24.0, scale);
        let screen = GetDC(std::ptr::null_mut());
        let font_body = CreateFontW(
            -((10.5 * scale * 96.0 / 72.0).round() as i32),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1,
            0,
            0,
            5,
            0,
            wide("Microsoft YaHei UI").as_ptr(),
        );
        let old = SelectObject(screen, font_body as HGDIOBJ);
        let mut measure = RECT {
            left: 0,
            top: 0,
            right: s(380.0, scale),
            bottom: 0,
        };
        DrawTextW(
            screen,
            wide(text).as_ptr(),
            -1,
            &mut measure,
            DT_LEFT | DT_WORDBREAK | DT_CALCRECT,
        );
        SelectObject(screen, old);
        DeleteObject(font_body as *mut _);
        ReleaseDC(std::ptr::null_mut(), screen);

        let text_w = measure.right.max(s(220.0, scale));
        let text_h = measure.bottom.max(s(40.0, scale));
        let width = text_w + pad * 2;
        let button_h = s(32.0, scale);
        let footer = button_h + s(20.0, scale);
        let height = s(20.0, scale) + text_h + s(36.0, scale) + footer;

        let mut owner_rect: RECT = std::mem::zeroed();
        GetWindowRect(owner, &mut owner_rect);
        let mut x = owner_rect.left + ((owner_rect.right - owner_rect.left) - width) / 2;
        let mut y = owner_rect.top + ((owner_rect.bottom - owner_rect.top) - height) / 2;
        // 主窗口可能很小（任务栏面板）或者贴边，别把提示框怼到屏幕外
        let mut point = POINT {
            x: owner_rect.left,
            y: owner_rect.top,
        };
        if owner.is_null() {
            GetCursorPos(&mut point);
        }
        let work = crate::work_area_at(point);
        if x + width > work.right {
            x = work.right - width - s(8.0, scale);
        }
        if y + height > work.bottom {
            y = work.bottom - height - s(8.0, scale);
        }
        if x < work.left {
            x = work.left + s(8.0, scale);
        }
        if y < work.top {
            y = work.top + s(8.0, scale);
        }

        let state = Box::into_raw(Box::new(Dialog {
            title: title.to_string(),
            text: text.to_string(),
            confirm,
            accepted: Cell::new(false),
            closed: Cell::new(false),
        }));

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide(title).as_ptr(),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            width,
            height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            drop(Box::from_raw(state));
            return false;
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        crate::ui::round_corners(hwnd);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);

        // 简易模态：自己跑消息循环，直到窗口关掉
        let mut message: MSG = std::mem::zeroed();
        while !(*state).closed.get() {
            let got = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
            if got <= 0 {
                break;
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let accepted = (*state).accepted.get();
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        drop(Box::from_raw(state));
        accepted
    }
}

fn buttons(hwnd: HWND) -> (RECT, RECT) {
    let scale = crate::screen_scale();
    let mut client: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        GetClientRect(hwnd, &mut client);
    }
    let w = s(84.0, scale);
    let h = s(32.0, scale);
    let bottom = client.bottom - s(20.0, scale);
    let ok = RECT {
        left: client.right - s(24.0, scale) - w,
        top: bottom - h,
        right: client.right - s(24.0, scale),
        bottom,
    };
    let cancel = RECT {
        left: ok.left - s(10.0, scale) - w,
        top: ok.top,
        right: ok.left - s(10.0, scale),
        bottom: ok.bottom,
    };
    (ok, cancel)
}

unsafe fn paint(hwnd: HWND, state: &Dialog) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let ui_colors = ui();
    let scale = crate::screen_scale();
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);

    let bg = CreateSolidBrush(ui_colors.bg);
    FillRect(dc, &client, bg);
    DeleteObject(bg as *mut _);

    SetBkMode(dc, TRANSPARENT as i32);

    // 标题
    let title_font = CreateFontW(
        -((12.0 * scale * 96.0 / 72.0).round() as i32),
        0,
        0,
        0,
        700,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        wide("Microsoft YaHei UI").as_ptr(),
    );
    let old = SelectObject(dc, title_font as HGDIOBJ);
    SetTextColor(dc, ui_colors.text);
    let pad = s(24.0, scale);
    let mut rect = RECT {
        left: pad,
        top: s(20.0, scale),
        right: client.right - pad,
        bottom: s(48.0, scale),
    };
    DrawTextW(
        dc,
        wide(&state.title).as_ptr(),
        -1,
        &mut rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    SelectObject(dc, old);
    DeleteObject(title_font as *mut _);

    // 正文
    let body_font = CreateFontW(
        -((10.5 * scale * 96.0 / 72.0).round() as i32),
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        wide("Microsoft YaHei UI").as_ptr(),
    );
    let old = SelectObject(dc, body_font as HGDIOBJ);
    SetTextColor(dc, ui_colors.text2);
    let mut body = RECT {
        left: pad,
        top: s(52.0, scale),
        right: client.right - pad,
        bottom: client.bottom - s(60.0, scale),
    };
    DrawTextW(
        dc,
        wide(&state.text).as_ptr(),
        -1,
        &mut body,
        DT_LEFT | DT_WORDBREAK,
    );
    SelectObject(dc, old);
    DeleteObject(body_font as *mut _);

    // 按钮
    let (ok, cancel) = buttons(hwnd);
    let brush = CreateSolidBrush(ui_colors.accent);
    let pen = CreateSolidBrush(ui_colors.accent);
    let old_b = SelectObject(dc, brush as HGDIOBJ);
    let old_p = SelectObject(dc, pen as HGDIOBJ);
    RoundRect(dc, ok.left, ok.top, ok.right, ok.bottom, s(9.0, scale), s(9.0, scale));
    SelectObject(dc, old_p);
    SelectObject(dc, old_b);
    DeleteObject(brush as *mut _);
    DeleteObject(pen as *mut _);

    let button_font = CreateFontW(
        -((10.0 * scale * 96.0 / 72.0).round() as i32),
        0,
        0,
        0,
        500,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        wide("Microsoft YaHei UI").as_ptr(),
    );
    let old = SelectObject(dc, button_font as HGDIOBJ);
    SetTextColor(dc, 0x00FF_FFFF);
    let mut label = ok;
    DrawTextW(
        dc,
        wide("确定").as_ptr(),
        -1,
        &mut label,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    if state.confirm {
        let cbg = CreateSolidBrush(ui_colors.chip);
        let cpen = CreateSolidBrush(ui_colors.chip);
        let ob = SelectObject(dc, cbg as HGDIOBJ);
        let op = SelectObject(dc, cpen as HGDIOBJ);
        RoundRect(
            dc,
            cancel.left,
            cancel.top,
            cancel.right,
            cancel.bottom,
            s(9.0, scale),
            s(9.0, scale),
        );
        SelectObject(dc, op);
        SelectObject(dc, ob);
        DeleteObject(cbg as *mut _);
        DeleteObject(cpen as *mut _);
        SetTextColor(dc, ui_colors.text2);
        let mut cl = cancel;
        DrawTextW(
            dc,
            wide("取消").as_ptr(),
            -1,
            &mut cl,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
    SelectObject(dc, old);
    DeleteObject(button_font as *mut _);

    let _ = GetStockObject(NULL_BRUSH);
    EndPaint(hwnd, &mut ps);
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dialog;
    match msg {
        WM_PAINT => {
            if !state.is_null() {
                paint(hwnd, &*state);
            }
            0
        }
        WM_LBUTTONUP => {
            if state.is_null() {
                return 0;
            }
            let mut point = POINT {
                x: (lparam & 0xFFFF) as i16 as i32,
                y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = &mut point;
            let (ok, cancel) = buttons(hwnd);
            if contains(&ok, point) {
                (*state).accepted.set(true);
                DestroyWindow(hwnd);
            } else if (*state).confirm && contains(&cancel, point) {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_KEYDOWN => {
            if !state.is_null() && (wparam == 0x0D || wparam == 0x1B) {
                (*state).accepted.set(wparam == 0x0D);
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            if !state.is_null() {
                (*state).closed.set(true);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn contains(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}
