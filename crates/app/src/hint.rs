//! 悬浮说明气泡（自绘小卡片）。
//!
//! 两种用法：
//! * `show_text`：一行字，指示条那种说明；
//! * `show`：标题 + 若干「左标签 / 右数值」行，网速悬停时的进程明细用。
//!
//! 卡片贴着面板上沿弹出，跟着面板所在那块屏的 DPI 和工作区走。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GetDC, InvalidateRect, ReleaseDC, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;
use crate::ui::{self, font, Painter, DT_LEFT, DT_RIGHT};

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Hint";

const PAD_X: f32 = 9.0;
const PAD_Y: f32 = 7.0;
/// 标题行
const TITLE_H: f32 = 20.0;
/// 标签 / 数值行
const ROW_H: f32 = 18.0;
/// 标签和数值之间的最小间距
const PAIR_GAP: f32 = 22.0;
const GAP: f32 = 6.0;

/// 卡片里的一行。
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// 标题（略粗、主文字色）
    Title(String),
    /// 左标签 / 右数值
    Pair(String, String),
    /// 整行说明（次要文字色）
    Text(String),
}

/// 一张悬浮卡片。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hint {
    pub lines: Vec<Line>,
}

impl Hint {
    pub fn new() -> Self {
        Self { lines: Vec::new() }
    }

    pub fn title(mut self, text: impl Into<String>) -> Self {
        self.lines.push(Line::Title(text.into()));
        self
    }

    pub fn pair(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.lines.push(Line::Pair(label.into(), value.into()));
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.lines.push(Line::Text(text.into()));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 缓存键：内容 + 缩放，一样就不用重算尺寸
    fn key(&self, scale: f32) -> String {
        let mut key = format!("{scale:.3}|");
        for line in &self.lines {
            match line {
                Line::Title(text) => {
                    key.push_str("\u{1}T");
                    key.push_str(text);
                }
                Line::Pair(label, value) => {
                    key.push_str("\u{1}P");
                    key.push_str(label);
                    key.push('\u{2}');
                    key.push_str(value);
                }
                Line::Text(text) => {
                    key.push_str("\u{1}X");
                    key.push_str(text);
                }
            }
        }
        key
    }
}

struct State {
    hint: Hint,
    scale: f32,
}

/// 当前气泡窗口（同一时间只有一个）
static mut HINT: HWND = std::ptr::null_mut();

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

/// 逐行画（尺寸也是同一套算法量出来的，画和量不会对不上）
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
    let c = crate::theme::ui();

    p.fill(0, 0, client.right, client.bottom, c.surface);
    p.round(
        0,
        0,
        client.right,
        client.bottom,
        p.s(7.0),
        c.surface,
        c.border,
    );

    let title_font = font(9.5, 700, state.scale);
    let label_font = font(9.5, 400, state.scale);
    let value_font = font(9.5, 600, state.scale);

    let left = p.s(PAD_X);
    let right = (client.right - p.s(PAD_X)).max(left + 1);

    // 单行说明：维持以前那种 24px 的小气泡（整块高度里垂直居中）
    if state.hint.lines.len() == 1 {
        if let Some(Line::Text(text)) = state.hint.lines.first() {
            p.text(text, label_font, c.text, left, 0, right, client.bottom, DT_LEFT);
            blit(hwnd, &dib, client);
            return;
        }
    }

    let mut y = p.s(PAD_Y);
    for line in &state.hint.lines {
        let height = match line {
            Line::Title(_) => p.s(TITLE_H),
            _ => p.s(ROW_H),
        };
        match line {
            Line::Title(text) => {
                p.text(
                    text,
                    title_font,
                    c.text,
                    left,
                    y,
                    right,
                    y + height,
                    DT_LEFT,
                );
            }
            Line::Pair(label, value) => {
                p.text(
                    label,
                    label_font,
                    c.text2,
                    left,
                    y,
                    right,
                    y + height,
                    DT_LEFT,
                );
                p.text(
                    value,
                    value_font,
                    c.text,
                    left,
                    y,
                    right,
                    y + height,
                    DT_RIGHT,
                );
            }
            Line::Text(text) => {
                p.text(
                    text,
                    label_font,
                    c.text2,
                    left,
                    y,
                    right,
                    y + height,
                    DT_LEFT,
                );
            }
        }
        y += height;
    }

    blit(hwnd, &dib, client);
}

unsafe fn blit(hwnd: HWND, dib: &ui::Dib, client: RECT) {
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
        // 系统切明暗主题：气泡也要换色
        WM_SETTINGCHANGE => {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
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

/// 量尺寸：返回（宽, 高）。
fn measure(hint: &Hint, scale: f32) -> (i32, i32) {
    use std::sync::{Mutex, OnceLock};

    // 缓存键要带缩放：面板挪到另一块缩放不同的屏后，同样的文字尺寸不一样
    static CACHE: OnceLock<Mutex<Option<(String, i32, i32)>>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let key = hint.key(scale);

    if let Ok(guard) = cell.lock() {
        if let Some((cached, width, height)) = guard.as_ref() {
            if *cached == key {
                return (*width, *height);
            }
        }
    }

    let size = unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let p = Painter::new(screen, scale);
        let title_font = font(9.5, 700, scale);
        let label_font = font(9.5, 400, scale);
        let value_font = font(9.5, 600, scale);

        let mut widest = 0;
        for line in &hint.lines {
            let width = match line {
                Line::Title(text) => p.text_width(text, title_font),
                Line::Pair(label, value) => {
                    p.text_width(label, label_font)
                        + ui::s(PAIR_GAP, scale)
                        + p.text_width(value, value_font)
                }
                Line::Text(text) => p.text_width(text, label_font),
            };
            widest = widest.max(width);
        }

        let width = (widest + ui::s(PAD_X * 2.0, scale)).max(ui::s(46.0, scale));
        let height = if hint.lines.len() == 1 && matches!(hint.lines[0], Line::Text(_)) {
            ui::s(24.0, scale)
        } else {
            let rows: i32 = hint
                .lines
                .iter()
                .map(|line| match line {
                    Line::Title(_) => ui::s(TITLE_H, scale),
                    _ => ui::s(ROW_H, scale),
                })
                .sum();
            rows + ui::s(PAD_Y * 2.0, scale)
        };
        ReleaseDC(std::ptr::null_mut(), screen);
        (width, height)
    };

    if let Ok(mut guard) = cell.lock() {
        *guard = Some((key, size.0, size.1));
    }
    size
}

/// 一行字的说明气泡（指示条用）。
pub unsafe fn show_text(anchor: RECT, text: &str) {
    if text.is_empty() {
        hide();
        return;
    }
    show(anchor, &Hint::new().text(text));
}

/// 在 `anchor`（屏幕坐标，一般传面板的窗口矩形）上方显示卡片；重复调用只更新内容。
pub unsafe fn show(anchor: RECT, hint: &Hint) {
    if hint.is_empty() {
        hide();
        return;
    }

    // 面板在哪块屏，气泡就用那块屏的缩放和工作区（多屏缩放不同也不会跑偏）
    let center = POINT {
        x: (anchor.left + anchor.right) / 2,
        y: (anchor.top + anchor.bottom) / 2,
    };
    let scale = crate::scale_at(center);
    let (width, height) = measure(hint, scale);
    let work = crate::work_area_at(center);

    // 横向贴着面板居中，纵向放在面板上方（面板本身在任务栏里，放下面会出屏）
    let left = ((anchor.left + anchor.right) / 2 - width / 2)
        .max(work.left)
        .min((work.right - width).max(work.left));
    let mut top = anchor.top - height - ui::s(GAP, scale);
    if top < work.top {
        top = (anchor.bottom + ui::s(GAP, scale)).min(work.bottom - height);
    }

    if !HINT.is_null() && IsWindow(HINT) != 0 {
        // 鼠标在面板上每移动一像素都会调进来：内容和位置都没变就别折腾窗口
        let mut current = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetWindowRect(HINT, &mut current);
        let state = state_of(HINT);
        let same_hint = !state.is_null() && (*state).hint == *hint;
        if same_hint
            && current.left == left
            && current.top == top
            && current.right - current.left == width
            && current.bottom - current.top == height
        {
            return;
        }

        if !state.is_null() {
            (*state).hint = hint.clone();
            (*state).scale = scale;
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
        hint: hint.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_separates_lines() {
        let a = Hint::new().title("上传 1.2 M/s").pair("chrome", "800 K/s");
        let b = Hint::new().title("上传 1.2 M/s").pair("chrome", "801 K/s");
        assert_ne!(a.key(1.0), b.key(1.0));
        // 标签和数值的拼接不能撞车
        let c = Hint::new().pair("a", "bc");
        let d = Hint::new().pair("ab", "c");
        assert_ne!(c.key(1.0), d.key(1.0));
        // 缩放不同也要分开缓存
        assert_ne!(a.key(1.0), a.key(1.25));
    }
}

