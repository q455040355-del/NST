//! 取字结果窗（自绘卡片 + 无边框多行编辑框）。
//!
//! 文本区还是原生 EDIT（要输入法、要选择、要 Ctrl+C），但去掉了系统边框，
//! 背景和卡片同色，外围的标题 / 按钮 / 状态全是自己画的。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect, ScreenToClient,
    SetBkColor, SetTextColor, HBRUSH, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;
use crate::ui::{self, font, Painter, DT_CENTER, DT_LEFT};

const CLASS_NAME: &str = "NetSpeedTaskbarRs.OcrResult";
const ID_EDIT: usize = 2001;

const EM_SETMARGINS: u32 = 0x00D3;
const EC_LEFTMARGIN: usize = 0x0001;
const EC_RIGHTMARGIN: usize = 0x0002;
const EM_SETSEL: u32 = 0x00B1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Close,
    Copy,
}

struct State {
    edit: HWND,
    status: String,
    fonts: Fonts,
    scale: f32,
    hover: Option<Hit>,
    /// EDIT 的底色画刷（WM_CTLCOLOREDIT 必须返回一个常驻句柄，不能每次新建）
    edit_brush: HBRUSH,
    /// 这个画刷对应的颜色：主题切了就重建
    edit_color: u32,
}

struct Fonts {
    title: ::windows_sys::Win32::Graphics::Gdi::HFONT,
    body: ::windows_sys::Win32::Graphics::Gdi::HFONT,
    small: ::windows_sys::Win32::Graphics::Gdi::HFONT,
}

impl Fonts {
    fn create(scale: f32) -> Self {
        Self {
            title: font(12.5, 650, scale),
            body: font(10.0, 400, scale),
            small: font(9.0, 400, scale),
        }
    }
}

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

struct Layout {
    close: RECT,
    copy: RECT,
    edit: RECT,
}

fn layout(state: &State, client: RECT) -> Layout {
    let scale = state.scale;
    let pad = ui::s(16.0, scale);

    let close_size = ui::s(26.0, scale);
    let close = RECT {
        left: client.right - pad - close_size,
        top: ui::s(14.0, scale),
        right: client.right - pad,
        bottom: ui::s(14.0, scale) + close_size,
    };

    let button_w = ui::s(84.0, scale);
    let button_h = ui::s(32.0, scale);
    let copy = RECT {
        left: client.right - pad - button_w,
        top: client.bottom - pad - button_h,
        right: client.right - pad,
        bottom: client.bottom - pad,
    };

    let edit = RECT {
        left: pad,
        top: ui::s(54.0, scale),
        right: client.right - pad,
        bottom: copy.top - ui::s(12.0, scale),
    };

    Layout { close, copy, edit }
}

fn contains(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

unsafe fn paint(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;

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
    let l = layout(state, client);
    let pad = p.s(16.0);
    let c = crate::theme::ui();

    p.fill(0, 0, client.right, client.bottom, c.bg);
    p.round(
        0,
        0,
        client.right,
        client.bottom,
        p.s(16.0),
        c.surface,
        c.border,
    );

    // 标题
    p.text(
        "取字结果",
        state.fonts.title,
        c.text,
        pad + p.s(2.0),
        p.s(14.0),
        l.close.left - p.s(8.0),
        p.s(44.0),
        DT_LEFT,
    );

    // 关闭 ×
    let close_hover = state.hover == Some(Hit::Close);
    if close_hover {
        p.round(
            l.close.left,
            l.close.top,
            l.close.right,
            l.close.bottom,
            p.s(8.0),
            c.chip,
            c.chip,
        );
    }
    let cx = (l.close.left + l.close.right) / 2;
    let cy = (l.close.top + l.close.bottom) / 2;
    let arm = p.s(5.0);
    let ink = if close_hover { c.text } else { c.text2 };
    p.line(cx - arm, cy - arm, cx + arm, cy + arm, ink, p.s(1.4));
    p.line(cx - arm, cy + arm, cx + arm, cy - arm, ink, p.s(1.4));

    // 文本区底板（EDIT 就盖在这块上，靠 WM_CTLCOLOREDIT 同色）
    p.round(
        l.edit.left - p.s(8.0),
        l.edit.top - p.s(8.0),
        l.edit.right + p.s(8.0),
        l.edit.bottom + p.s(8.0),
        p.s(10.0),
        c.surface,
        c.border,
    );

    // 底部
    p.text(
        &state.status,
        state.fonts.small,
        c.text3,
        pad + p.s(2.0),
        l.copy.top,
        l.copy.left - p.s(10.0),
        l.copy.bottom,
        DT_LEFT,
    );

    let hover = state.hover == Some(Hit::Copy);
    let fill = if hover { c.accent_pressed } else { c.accent };
    p.round(
        l.copy.left,
        l.copy.top,
        l.copy.right,
        l.copy.bottom,
        p.s(9.0),
        fill,
        fill,
    );
    p.text(
        "复制",
        state.fonts.body,
        0x00FF_FFFF,
        l.copy.left,
        l.copy.top,
        l.copy.right,
        l.copy.bottom,
        DT_CENTER,
    );

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, client.right, client.bottom, dib.dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);
}

unsafe fn layout_edit(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    GetClientRect(hwnd, &mut client);
    let l = layout(state, client);
    if !state.edit.is_null() {
        MoveWindow(
            state.edit,
            l.edit.left,
            l.edit.top,
            l.edit.right - l.edit.left,
            l.edit.bottom - l.edit.top,
            1,
        );
        SendMessageW(
            state.edit,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as WPARAM,
            (ui::s(8.0, state.scale) as isize) | ((ui::s(8.0, state.scale) as isize) << 16),
        );
    }
}

unsafe fn copy_from_edit(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    if state.edit.is_null() {
        return;
    }
    let length = GetWindowTextLengthW(state.edit);
    let mut buffer = vec![0u16; (length + 1) as usize];
    GetWindowTextW(state.edit, buffer.as_mut_ptr(), buffer.len() as i32);
    let text = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
    crate::ocr::copy_text(&text);
    state.status = format!("已复制到剪贴板 · {} 字", text.chars().count());
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

pub fn open(_parent: HWND, text: &str, error: Option<&str>) {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::tray::shared_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        let scale = crate::screen_scale();
        let width = ui::s(560.0, scale);
        let height = ui::s(440.0, scale);

        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
        let x = work.left + ((work.right - work.left) - width) / 2;
        let y = work.top + ((work.bottom - work.top) - height) / 3;

        let status = match error {
            Some(message) => message.to_string(),
            None => format!("已自动复制 · {} 字", text.chars().count()),
        };

        let state = Box::into_raw(Box::new(State {
            edit: std::ptr::null_mut(),
            status,
            fonts: Fonts::create(scale),
            scale,
            hover: None,
            edit_brush: std::ptr::null_mut(),
            edit_color: 0,
        }));

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide("取字结果").as_ptr(),
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
            let _ = Box::from_raw(state);
            return;
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);

        // 文本区：原生 EDIT，但去掉边框（外观交给父窗口画）
        let edit = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_TABSTOP
                | 0x0004  // ES_MULTILINE
                | 0x0040  // ES_AUTOVSCROLL
                | 0x0080, // ES_AUTOHSCROLL
            0,
            0,
            0,
            0,
            hwnd,
            ID_EDIT as usize as *mut _,
            instance,
            std::ptr::null(),
        );

        let body = (*state).fonts.body;
        SendMessageW(edit, WM_SETFONT, body as WPARAM, 1);
        (*state).edit = edit;

        if error.is_none() {
            crate::ocr::copy_text(text);
        }

        ui::round_corners(hwnd);
        layout_edit(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(edit);
        // 全选，方便直接 Ctrl+C
        SendMessageW(edit, EM_SETSEL, 0, -1);
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

unsafe extern "system" fn window_proc(
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
        WM_SIZE => {
            layout_edit(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = wparam as ::windows_sys::Win32::Graphics::Gdi::HDC;
            let c = crate::theme::ui();
            SetBkColor(dc, c.surface);
            SetTextColor(dc, c.text);
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            // 系统换成深色后底色变了：旧画刷要删掉重做
            let state = &mut *state;
            if state.edit_brush.is_null() || state.edit_color != c.surface {
                if !state.edit_brush.is_null() {
                    DeleteObject(state.edit_brush as *mut _);
                }
                state.edit_brush = CreateSolidBrush(c.surface);
                state.edit_color = c.surface;
            }
            state.edit_brush as LRESULT
        }
        WM_NCHITTEST => {
            let state = state_of(hwnd);
            if state.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let mut point = POINT {
                x: (lparam as i32) & 0xFFFF,
                y: ((lparam as i32) >> 16) & 0xFFFF,
            };
            ScreenToClient(hwnd, &mut point);
            let mut client = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetClientRect(hwnd, &mut client);
            let l = layout(&*state, client);
            if point.y < ui::s(54.0, (*state).scale) && !contains(&l.close, point) {
                return HTCAPTION as LRESULT;
            }
            HTCLIENT as LRESULT
        }
        WM_MOUSEMOVE => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            let point = POINT {
                x: (lparam as i32) & 0xFFFF,
                y: ((lparam as i32) >> 16) & 0xFFFF,
            };
            let mut client = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetClientRect(hwnd, &mut client);
            let l = layout(state, client);
            let hover = if contains(&l.close, point) {
                Some(Hit::Close)
            } else if contains(&l.copy, point) {
                Some(Hit::Copy)
            } else {
                None
            };
            if state.hover != hover {
                state.hover = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let point = POINT {
                x: (lparam as i32) & 0xFFFF,
                y: ((lparam as i32) >> 16) & 0xFFFF,
            };
            let mut client = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetClientRect(hwnd, &mut client);
            let l = layout(&*state, client);
            if contains(&l.close, point) {
                DestroyWindow(hwnd);
            } else if contains(&l.copy, point) {
                copy_from_edit(hwnd);
            }
            0
        }
        WM_SETCURSOR => {
            let state = state_of(hwnd);
            if !state.is_null() {
                let mut point = POINT { x: 0, y: 0 };
                GetCursorPos(&mut point);
                ScreenToClient(hwnd, &mut point);
                let mut client = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetClientRect(hwnd, &mut client);
                let l = layout(&*state, client);
                if contains(&l.close, point) || contains(&l.copy, point) {
                    ui::set_hand_cursor();
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_KEYDOWN => {
            if wparam as u32 == 0x1B {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        // 系统切明暗主题：整窗重画（EDIT 的底色由 WM_CTLCOLOREDIT 自己更新）
        WM_SETTINGCHANGE => {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_DESTROY => {
            let state = state_of(hwnd);
            if !state.is_null() {
                let state = Box::from_raw(state);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !state.edit_brush.is_null() {
                    DeleteObject(state.edit_brush as *mut _);
                }
                drop(state);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
