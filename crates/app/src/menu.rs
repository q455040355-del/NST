//! 自绘右键菜单（替换系统 TrackPopupMenu）。
//!
//! 一行一个条目，支持：分组标题、分隔线、缩进、勾选、右侧快捷键；
//! 悬停高亮、↑↓ 移动、Enter 选中、Esc / 点外面关闭。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GetDC, InvalidateRect, ReleaseDC, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;
use crate::ui::{self, font, Painter, ACCENT, BORDER, CHIP, DT_LEFT, DT_RIGHT, SURFACE, TEXT, TEXT2, TEXT3};

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Menu";

pub const WM_APP_MENU_PICK: u32 = WM_APP + 0x41;

pub enum Kind {
    Separator,
    Header(&'static str),
    Item {
        id: usize,
        label: &'static str,
        shortcut: &'static str,
        checked: bool,
        indent: bool,
    },
}

pub struct Entry {
    pub kind: Kind,
}

impl Entry {
    pub fn item(id: usize, label: &'static str) -> Self {
        Self {
            kind: Kind::Item {
                id,
                label,
                shortcut: "",
                checked: false,
                indent: false,
            },
        }
    }

    pub fn checked(id: usize, label: &'static str, checked: bool) -> Self {
        Self {
            kind: Kind::Item {
                id,
                label,
                shortcut: "",
                checked,
                indent: false,
            },
        }
    }

    pub fn indented(id: usize, label: &'static str, checked: bool) -> Self {
        Self {
            kind: Kind::Item {
                id,
                label,
                shortcut: "",
                checked,
                indent: true,
            },
        }
    }

    pub fn hotkey(id: usize, label: &'static str, shortcut: &'static str) -> Self {
        Self {
            kind: Kind::Item {
                id,
                label,
                shortcut,
                checked: false,
                indent: false,
            },
        }
    }

    pub fn header(label: &'static str) -> Self {
        Self {
            kind: Kind::Header(label),
        }
    }

    pub fn separator() -> Self {
        Self {
            kind: Kind::Separator,
        }
    }
}

struct State {
    owner: HWND,
    rows: Vec<Entry>,
    hover: Option<usize>,
    width: i32,
    height: i32,
    scale: f32,
    /// 每个可点条目的矩形（index 是 rows 下标）
    hit: Vec<(usize, RECT)>,
}

const ITEM_H: f32 = 32.0;
const HEADER_H: f32 = 28.0;
const SEP_H: f32 = 9.0;
const PAD: f32 = 6.0;
const CHECK_COL: f32 = 20.0;

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

fn item_id(entry: &Entry) -> Option<usize> {
    match &entry.kind {
        Kind::Item { id, .. } => Some(*id),
        _ => None,
    }
}

unsafe fn measure(rows: &[Entry], scale: f32) -> (i32, i32, Vec<(usize, RECT)>) {
    let screen = GetDC(std::ptr::null_mut());
    let p = Painter::new(screen, scale);
    let label_font = font(10.0, 400, scale);
    let header_font = font(9.0, 600, scale);
    let shortcut_font = font(9.0, 400, scale);

    let mut label_w = 0;
    let mut shortcut_w = 0;
    let mut header_w = 0;
    for entry in rows {
        match &entry.kind {
            Kind::Item { label, shortcut, .. } => {
                label_w = label_w.max(p.text_width(label, label_font));
                if !shortcut.is_empty() {
                    shortcut_w = shortcut_w.max(p.text_width(shortcut, shortcut_font));
                }
            }
            Kind::Header(text) => header_w = header_w.max(p.text_width(text, header_font)),
            Kind::Separator => {}
        }
    }
    ReleaseDC(std::ptr::null_mut(), screen);

    let check_w = ui::s(CHECK_COL + 6.0, scale);
    let side = ui::s(14.0, scale);
    let gap = if shortcut_w > 0 {
        ui::s(28.0, scale)
    } else {
        0
    };
    let width = (check_w + label_w.max(header_w) + shortcut_w + gap + side * 2)
        .max(ui::s(196.0, scale));

    let mut y = ui::s(PAD, scale);
    let mut hit = Vec::new();
    for (index, entry) in rows.iter().enumerate() {
        let height = match &entry.kind {
            Kind::Item { .. } => ui::s(ITEM_H, scale),
            Kind::Header(_) => ui::s(HEADER_H, scale),
            Kind::Separator => ui::s(SEP_H, scale),
        };
        if item_id(entry).is_some() {
            hit.push((
                index,
                RECT {
                    left: ui::s(PAD - 2.0, scale),
                    top: y,
                    right: width - ui::s(PAD - 2.0, scale),
                    bottom: y + height,
                },
            ));
        }
        y += height;
    }
    let height = y + ui::s(PAD, scale);
    (width, height, hit)
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
        p.s(10.0),
        SURFACE,
        BORDER,
    );

    let label_font = font(10.0, 400, state.scale);
    let header_font = font(9.0, 600, state.scale);
    let shortcut_font = font(9.0, 400, state.scale);

    let mut y = p.s(PAD);
    for (index, entry) in state.rows.iter().enumerate() {
        match &entry.kind {
            Kind::Separator => {
                let mid = y + p.s(SEP_H) / 2;
                p.fill(
                    p.s(10.0),
                    mid,
                    client.right - p.s(10.0),
                    mid + 1,
                    ui::mix_white(BORDER, 0.25),
                );
                y += p.s(SEP_H);
            }
            Kind::Header(text) => {
                p.text(
                    text,
                    header_font,
                    TEXT3,
                    p.s(14.0),
                    y,
                    client.right - p.s(12.0),
                    y + p.s(HEADER_H),
                    DT_LEFT,
                );
                y += p.s(HEADER_H);
            }
            Kind::Item {
                label,
                shortcut,
                checked,
                indent,
                ..
            } => {
                let top = y + p.s(1.0);
                let bottom = y + p.s(ITEM_H) - p.s(1.0);

                if state.hover == Some(index) {
                    p.round(
                        p.s(4.0),
                        top,
                        client.right - p.s(4.0),
                        bottom,
                        p.s(7.0),
                        CHIP,
                        CHIP,
                    );
                }

                if *checked {
                    let ax = p.s(16.0);
                    let ay = (top + bottom) / 2;
                    p.line(ax, ay, ax + p.s(4.0), ay + p.s(4.0), ACCENT, p.s(1.8));
                    p.line(
                        ax + p.s(4.0),
                        ay + p.s(4.0),
                        ax + p.s(11.0),
                        ay - p.s(5.0),
                        ACCENT,
                        p.s(1.8),
                    );
                }

                let text_left = if *indent { p.s(16.0) } else { 0 };
                let left = p.s(32.0) + text_left;
                p.text(
                    label,
                    label_font,
                    TEXT,
                    left,
                    top,
                    client.right - p.s(12.0),
                    bottom,
                    DT_LEFT,
                );

                if !shortcut.is_empty() {
                    p.text(
                        shortcut,
                        shortcut_font,
                        TEXT3,
                        client.right - p.s(120.0),
                        top,
                        client.right - p.s(12.0),
                        bottom,
                        DT_RIGHT,
                    );
                }

                y += p.s(ITEM_H);
            }
        }
    }

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, client.right, client.bottom, dib.dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);

}

unsafe extern "system" fn menu_proc(
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
            let mut hover = None;
            for (index, rect) in &state.hit {
                if point.x >= rect.left
                    && point.x < rect.right
                    && point.y >= rect.top
                    && point.y < rect.bottom
                {
                    hover = Some(*index);
                    break;
                }
            }
            if state.hover != hover {
                state.hover = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let state = state_of(hwnd);
            if !state.is_null() {
                let state = &*state;
                let point = POINT {
                    x: (lparam as i32) & 0xFFFF,
                    y: ((lparam as i32) >> 16) & 0xFFFF,
                };
                let mut inside = false;
                let mut picked = None;
                for (index, rect) in &state.hit {
                    if point.x >= rect.left
                        && point.x < rect.right
                        && point.y >= rect.top
                        && point.y < rect.bottom
                    {
                        inside = true;
                        picked = item_id(&state.rows[*index]);
                        break;
                    }
                }
                let _ = inside;
                if let Some(id) = picked {
                    PostMessageW(state.owner, WM_APP_MENU_PICK, id, 0);
                }
            }
            DestroyWindow(hwnd);
            0
        }
        WM_KEYDOWN => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            match wparam as u32 {
                0x1B => {
                    DestroyWindow(hwnd);
                }
                0x0D => {
                    if let Some(index) = state.hover {
                        if let Some(id) = item_id(&state.rows[index]) {
                            PostMessageW(state.owner, WM_APP_MENU_PICK, id, 0);
                        }
                    }
                    DestroyWindow(hwnd);
                }
                0x28 | 0x26 => {
                    let forward = (wparam as u32) == 0x28;
                    let count = state.rows.len();
                    if count > 0 {
                        let mut cursor = state.hover.unwrap_or(if forward { usize::MAX } else { 0 });
                        for _ in 0..count {
                            cursor = if forward {
                                (cursor + 1) % count
                            } else {
                                (cursor + count - 1) % count
                            };
                            if item_id(&state.rows[cursor]).is_some() {
                                state.hover = Some(cursor);
                                InvalidateRect(hwnd, std::ptr::null(), 0);
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_ACTIVATE => {
            if (wparam as u32 & 0xFFFF) == 0 {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            let state = state_of(hwnd);
            if !state.is_null() {
                let state = Box::from_raw(state);
                if state.owner.is_null() {
                    // 没有 owner 就没什么要通知的
                }
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(state);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 在鼠标位置弹出菜单；选中后给 owner 投递 `WM_APP_MENU_PICK`。
pub unsafe fn open(owner: HWND, rows: Vec<Entry>) {
    if rows.is_empty() {
        return;
    }

    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(CLASS_NAME);
    let wc = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(menu_proc),
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

    let scale = crate::screen_scale();
    let (width, height, hit) = measure(&rows, scale);

    let mut point = POINT { x: 0, y: 0 };
    GetCursorPos(&mut point);

    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);

    let left = point.x.min(work.right - width).max(work.left);
    let top = if point.y + height > work.bottom {
        (point.y - height).max(work.top)
    } else {
        point.y
    };

    let state = Box::into_raw(Box::new(State {
        owner,
        rows,
        hover: None,
        width,
        height,
        scale,
        hit,
    }));

    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class.as_ptr(),
        wide("").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        left,
        top,
        width,
        height,
        owner,
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
    ShowWindow(hwnd, SW_SHOWNA);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    SetCapture(hwnd);
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

pub fn is_open() -> bool {
    unsafe {
        let mut found: HWND = std::ptr::null_mut();
        // 遍历当前线程的窗口找菜单
        EnumThreadWindows(
            windows_sys::Win32::System::Threading::GetCurrentThreadId(),
            Some(enum_menu),
            &mut found as *mut HWND as isize,
        );
        !found.is_null()
    }
}

unsafe extern "system" fn enum_menu(hwnd: HWND, lparam: LPARAM) -> i32 {
    let mut class = [0u16; 128];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if length > 0 && String::from_utf16_lossy(&class[..length as usize]) == CLASS_NAME {
        *(lparam as *mut HWND) = hwnd;
        return 0;
    }
    1
}

/// 关闭当前菜单（窗口失活、按 Esc 之类）
pub fn close_all() {
    unsafe {
        let mut found: HWND = std::ptr::null_mut();
        EnumThreadWindows(
            windows_sys::Win32::System::Threading::GetCurrentThreadId(),
            Some(enum_menu),
            &mut found as *mut HWND as isize,
        );
        if !found.is_null() {
            ReleaseCapture();
            DestroyWindow(found);
        }
    }
}

pub fn release() {
    unsafe {
        ReleaseCapture();
    }
}

