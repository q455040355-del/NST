//! 设置窗口（全自绘，纯 Win32/GDI）。
//!
//! 视觉和历史窗口一致：浅色卡片 + 圆角 + 1px 描边，
//! 控件全部自己画（滑杆 / 下拉 / 按钮），所以没有系统控件的土味边框。
//!
//! 改任何一项都立刻生效并落盘。

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    InvalidateRect, LineTo, MoveToEx, ReleaseDC, RoundRect, ScreenToClient, SelectObject, SetBkMode,
    SetTextColor, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Settings";
const POPUP_CLASS: &str = "NetSpeedTaskbarRs.Settings.Popup";

/// 下拉里选完，子窗口用这个消息告诉父窗口
const WM_APP_PICK: u32 = WM_APP + 0x31;

const DT_NOPREFIX: u32 = 0x0800;
const DT_CALCRECT: u32 = 0x0400;

// 和 history_ui 一套色（浅色）
const BG: u32 = 0x00F8_F6F5; // #F5F6F8
const SURFACE: u32 = 0x00FF_FFFF;
const BORDER: u32 = 0x00EF_EAE8; // #E8EAEF
const TEXT: u32 = 0x001A_1614; // #14161A
const TEXT2: u32 = 0x0080_726B; // #6B7280
const CHIP: u32 = 0x00F3_F1EF;
const CHIP_HOVER: u32 = 0x00FE_F7F2; // 悬停时略暖一点
const ACCENT: u32 = 0x00E4_6F_2F; // #2F6FE4 蓝（COLORREF 倒序）
const ACCENT_PRESSED: u32 = 0x00C2_5A_1F;

const ID_WIDTH_LABEL: usize = 2001;
const ID_GAUGE_LABEL: usize = 2004;
const ID_INTERFACE_LABEL: usize = 2007;
const ID_REFRESH_LABEL: usize = 2009;

const GAUGE_OPTIONS: [(&str, &str); 6] = [
    ("跟随当前视图", "view"),
    ("网速", "speed"),
    ("内存占用", "mem"),
    ("CPU 温度", "cpu"),
    ("AI 速度", "ai"),
    ("关闭", "none"),
];

/// 面板刷新间隔（显示文案, 毫秒）
const REFRESH_OPTIONS: [(&str, i32); 3] = [("0.5 秒", 500), ("1 秒", 1000), ("2 秒", 2000)];

const ROW_COUNT: usize = 4;
const ROW_FIRST: f32 = 84.0;
const ROW_STEP: f32 = 62.0;
const CONTROL_X: f32 = 178.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Close,
    Done,
    Slider,
    Select(usize),
}

struct Fonts {
    title: HFONT,
    label: HFONT,
    value: HFONT,
    small: HFONT,
}

impl Fonts {
    fn create(scale: f32) -> Self {
        Self {
            title: font(12.5, 650, scale),
            label: font(10.5, 600, scale),
            value: font(10.5, 400, scale),
            small: font(9.5, 400, scale),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            for handle in [self.title, self.label, self.value, self.small] {
                DeleteObject(handle as *mut _);
            }
        }
    }
}

fn font(points: f32, weight: i32, scale: f32) -> HFONT {
    let height = -((points * scale * 96.0 / 72.0).round() as i32);
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Microsoft YaHei UI").as_ptr(),
        )
    }
}

fn s(value: f32, scale: f32) -> i32 {
    (value * scale).round() as i32
}

/// 位图 → 偏白（t=0 原色，t=1 纯白）
fn mix_white(color: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| -> u32 {
        let value = ((color >> shift) & 0xFF) as f32;
        (value + (255.0 - value) * t).round() as u32
    };
    (ch(0)) | (ch(8) << 8) | (ch(16) << 16)
}

/// 位图 → 偏黑（t=0 原色，t=1 纯黑）
fn mix_black(color: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| -> u32 { (((color >> shift) & 0xFF) as f32 * (1.0 - t)).round() as u32 };
    (ch(0)) | (ch(8) << 8) | (ch(16) << 16)
}

fn make_dib(width: i32, height: i32) -> (HDC, HBITMAP, *mut u8) {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width.max(1);
        info.bmiHeader.biHeight = -height.max(1);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = 0;
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        if !dib.is_null() {
            SelectObject(dc, dib as HGDIOBJ);
        }
        ReleaseDC(std::ptr::null_mut(), screen);
        (dc, dib, bits as *mut u8)
    }
}

// ---------------------------------------------------------------- 画图小工具

struct Painter {
    dc: HDC,
    scale: f32,
}

impl Painter {
    fn s(&self, value: f32) -> i32 {
        s(value, self.scale)
    }

    unsafe fn fill(&self, left: i32, top: i32, right: i32, bottom: i32, color: u32) {
        let brush = CreateSolidBrush(color);
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        FillRect(self.dc, &rect, brush);
        DeleteObject(brush as *mut _);
    }

    unsafe fn round(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
        fill: u32,
        border: u32,
    ) {
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, border);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        RoundRect(self.dc, left, top, right, bottom, radius, radius);
        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }

    unsafe fn line(&self, x1: i32, y1: i32, x2: i32, y2: i32, color: u32, width: i32) {
        let pen = CreatePen(0, width.max(1), color);
        let old = SelectObject(self.dc, pen as HGDIOBJ);
        MoveToEx(self.dc, x1, y1, std::ptr::null_mut());
        LineTo(self.dc, x2, y2);
        SelectObject(self.dc, old);
        DeleteObject(pen as *mut _);
    }

    unsafe fn circle(&self, cx: i32, cy: i32, radius: i32, fill: u32, border: u32) {
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, border);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::Ellipse(
            self.dc,
            cx - radius,
            cy - radius,
            cx + radius,
            cy + radius,
        );
        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }

    unsafe fn text(
        &self,
        text: &str,
        font: HFONT,
        color: u32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        align: u32,
    ) {
        let old = SelectObject(self.dc, font as HGDIOBJ);
        SetBkMode(self.dc, 1);
        SetTextColor(self.dc, color);
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        let units = wide(text);
        DrawTextW(
            self.dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut rect,
            align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(self.dc, old);
    }

    unsafe fn text_width(&self, text: &str, font: HFONT) -> i32 {
        let old = SelectObject(self.dc, font as HGDIOBJ);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let units = wide(text);
        DrawTextW(
            self.dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut rect,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(self.dc, old);
        rect.right - rect.left
    }
}

// ---------------------------------------------------------------- 状态

struct State {
    parent: HWND,
    interfaces: Vec<String>,
    width: i32,
    gauge: usize,
    interface: usize,
    refresh: usize,
    fonts: Fonts,
    scale: f32,
    hover: Option<Hit>,
    slider_drag: bool,
    popup: HWND,
}

struct Layout {
    close: RECT,
    done: RECT,
    /// 滑杆的点击区域（比视觉上的轨道高，好点）
    slider: RECT,
    selects: [RECT; 3],
}

fn layout(state: &State, client: RECT) -> Layout {
    let scale = state.scale;
    let pad = s(20.0, scale);
    let right = client.right - pad;

    let close_size = s(26.0, scale);
    let close = RECT {
        left: right - close_size,
        top: s(16.0, scale),
        right,
        bottom: s(16.0, scale) + close_size,
    };

    let done_w = s(92.0, scale);
    let done_h = s(32.0, scale);
    let done = RECT {
        left: right - done_w,
        top: client.bottom - pad - done_h,
        right,
        bottom: client.bottom - pad,
    };

    let control_left = s(CONTROL_X, scale);
    let row_center = |index: usize| -> i32 {
        s(ROW_FIRST + ROW_STEP * index as f32, scale) + s(ROW_STEP * 0.5 - 6.0, scale)
    };

    let slider = RECT {
        left: control_left,
        top: row_center(0) - s(14.0, scale),
        right: right - s(96.0, scale),
        bottom: row_center(0) + s(14.0, scale),
    };

    let mut selects = [RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }; 3];
    let height = s(32.0, scale);
    for (index, slot) in selects.iter_mut().enumerate() {
        let center = row_center(index + 1);
        *slot = RECT {
            left: control_left,
            top: center - height / 2,
            right,
            bottom: center + height / 2,
        };
    }

    Layout {
        close,
        done,
        slider,
        selects,
    }
}

fn contains(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

fn text_of_row(state: &State, row: usize) -> String {
    match row {
        0 => format!("{} px", state.width),
        1 => GAUGE_OPTIONS
            .get(state.gauge)
            .map(|(label, _)| *label)
            .unwrap_or("跟随当前视图")
            .to_string(),
        2 => {
            if state.interface == 0 {
                "自动（全部合计）".to_string()
            } else {
                state
                    .interfaces
                    .get(state.interface - 1)
                    .cloned()
                    .unwrap_or_else(|| "自动（全部合计）".to_string())
            }
        }
        _ => REFRESH_OPTIONS
            .get(state.refresh)
            .map(|(label, _)| *label)
            .unwrap_or("0.5 秒")
            .to_string(),
    }
}

fn row_label(row: usize) -> &'static str {
    match row {
        ID_WIDTH_LABEL => "面板宽度",
        ID_GAUGE_LABEL => "底部指示条",
        ID_INTERFACE_LABEL => "网卡",
        _ => "刷新间隔",
    }
}

// ---------------------------------------------------------------- 绘画

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

    let (dc, dib, _bits) = make_dib(client.right, client.bottom);
    if dc.is_null() || dib.is_null() {
        return;
    }

    let p = Painter {
        dc,
        scale: state.scale,
    };
    let l = layout(state, client);

    // 底色 + 卡片（卡片铺满客户区，圆角靠 DWM）
    p.fill(0, 0, client.right, client.bottom, BG);
    p.round(
        0,
        0,
        client.right,
        client.bottom,
        p.s(16.0),
        SURFACE,
        BORDER,
    );

    let pad = p.s(20.0);
    let content_right = client.right - pad;

    // 标题
    p.text(
        "显示设置",
        state.fonts.title,
        TEXT,
        pad + p.s(4.0),
        p.s(14.0),
        content_right,
        p.s(40.0),
        DT_LEFT,
    );
    p.text(
        "改动即时生效，自动写入 settings.ini",
        state.fonts.small,
        TEXT2,
        pad + p.s(4.0),
        p.s(38.0),
        content_right,
        p.s(58.0),
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
            CHIP,
            CHIP,
        );
    }
    let cx = (l.close.left + l.close.right) / 2;
    let cy = (l.close.top + l.close.bottom) / 2;
    let arm = p.s(5.0);
    let ink = if close_hover { TEXT } else { TEXT2 };
    p.line(cx - arm, cy - arm, cx + arm, cy + arm, ink, p.s(1.4));
    p.line(cx - arm, cy + arm, cx + arm, cy - arm, ink, p.s(1.4));

    // 顶部分割线
    p.fill(0, p.s(64.0), client.right, p.s(65.0), BORDER);

    // 行
    for row in 0..ROW_COUNT {
        let top = p.s(ROW_FIRST + ROW_STEP * row as f32);
        let bottom = p.s(ROW_FIRST + ROW_STEP * (row + 1) as f32);
        let center = (top + bottom) / 2 - p.s(6.0);

        let label = match row {
            0 => row_label(ID_WIDTH_LABEL),
            1 => row_label(ID_GAUGE_LABEL),
            2 => row_label(ID_INTERFACE_LABEL),
            _ => row_label(ID_REFRESH_LABEL),
        };
        p.text(
            label,
            state.fonts.label,
            TEXT,
            pad + p.s(4.0),
            center - p.s(11.0),
            s(CONTROL_X, state.scale) - p.s(12.0),
            center + p.s(11.0),
            DT_LEFT,
        );

        if row < ROW_COUNT - 1 {
            // 行间很淡的细线
            p.fill(
                pad + p.s(4.0),
                bottom,
                content_right,
                bottom + 1,
                mix_white(BORDER, 0.45),
            );
        }
    }

    // 滑杆
    {
        let track = l.slider;
        let mid = (track.top + track.bottom) / 2;
        let track_h = p.s(4.0);
        let knob_r = if state.slider_drag || state.hover == Some(Hit::Slider) {
            p.s(9.0)
        } else {
            p.s(8.0)
        };

        // 圆钮留出半径，别让它在两端半个身子探出轨道
        let travel_left = track.left + knob_r;
        let travel_right = (track.right - knob_r).max(travel_left + 1);
        let knob_x = travel_left
            + ((state.width - 72) as f32 / (360.0 - 72.0) * (travel_right - travel_left) as f32)
                .round() as i32;

        p.round(
            track.left,
            mid - track_h / 2,
            track.right,
            mid + track_h / 2,
            track_h,
            CHIP,
            CHIP,
        );
        p.round(
            track.left,
            mid - track_h / 2,
            knob_x.max(track.left + track_h),
            mid + track_h / 2,
            track_h,
            ACCENT,
            ACCENT,
        );
        p.circle(knob_x, mid, knob_r, SURFACE, mix_white(BORDER, 0.1));
        p.circle(knob_x, mid, knob_r - p.s(3.0), SURFACE, SURFACE);

        // 数值（固定在右侧，不跟着圆钮跑，避免文字框被挤成负数）
        p.text(
            &format!("{} px", state.width),
            state.fonts.value,
            TEXT2,
            content_right - p.s(80.0),
            mid - p.s(10.0),
            content_right,
            mid + p.s(10.0),
            DT_RIGHT,
        );
    }

    // 三个下拉
    for row in 1..ROW_COUNT {
        let rect = l.selects[row - 1];
        let hovered = state.hover == Some(Hit::Select(row));
        let open = state.popup != std::ptr::null_mut();
        let fill = if hovered || open {
            CHIP_HOVER
        } else {
            CHIP
        };
        let border = if open { ACCENT } else { fill };
        p.round(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            p.s(9.0),
            fill,
            border,
        );
        p.text(
            &text_of_row(state, row),
            state.fonts.value,
            TEXT,
            rect.left + p.s(12.0),
            rect.top,
            rect.right - p.s(34.0),
            rect.bottom,
            DT_LEFT,
        );

        // chevron
        let ax = rect.right - p.s(22.0);
        let ay = (rect.top + rect.bottom) / 2 - p.s(2.0);
        let step = p.s(4.5);
        p.line(ax, ay, ax + step, ay + step, TEXT2, p.s(1.4));
        p.line(ax + step, ay + step, ax + step * 2, ay, TEXT2, p.s(1.4));
    }

    // 底部分割线
    let foot_top = client.bottom - p.s(64.0);
    p.fill(0, foot_top, client.right, foot_top + 1, BORDER);

    p.text(
        "拖动滑块调宽度 · 点开下拉切换来源",
        state.fonts.small,
        TEXT2,
        pad + p.s(4.0),
        l.done.top,
        l.done.left - p.s(8.0),
        l.done.bottom,
        DT_LEFT,
    );

    let done_hover = state.hover == Some(Hit::Done);
    let done_fill = if done_hover {
        ACCENT_PRESSED
    } else {
        ACCENT
    };
    p.round(
        l.done.left,
        l.done.top,
        l.done.right,
        l.done.bottom,
        p.s(9.0),
        done_fill,
        done_fill,
    );
    p.text(
        "完成",
        state.fonts.value,
        0x00FF_FFFF,
        l.done.left,
        l.done.top,
        l.done.right,
        l.done.bottom,
        DT_CENTER_ALIGN,
    );

    // 提交
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);

    DeleteObject(dib as *mut _);
    DeleteDC(dc);
}

const DT_CENTER_ALIGN: u32 = 0x0001;

// ---------------------------------------------------------------- 下拉弹窗

struct Popup {
    parent: HWND,
    row: usize,
    items: Vec<String>,
    selected: usize,
    hover: Option<usize>,
    scale: f32,
}

fn popup_state(hwnd: HWND) -> *mut Popup {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Popup }
}

const POPUP_PAD: f32 = 6.0;
const POPUP_ITEM: f32 = 36.0;

unsafe fn popup_paint(hwnd: HWND) {
    let state = popup_state(hwnd);
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

    let (dc, dib, _bits) = make_dib(client.right, client.bottom);
    if dc.is_null() || dib.is_null() {
        return;
    }
    let p = Painter {
        dc,
        scale: state.scale,
    };

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

    let item_h = p.s(POPUP_ITEM);
    let pad = p.s(POPUP_PAD);
    let f = font(10.5, 400, state.scale);
    let f_selected = font(10.5, 600, state.scale);

    for (index, item) in state.items.iter().enumerate() {
        let top = pad + item_h * index as i32;
        let bottom = top + item_h;
        let hovered = state.hover == Some(index);
        let selected = index == state.selected;

        if hovered {
            p.round(
                pad,
                top,
                client.right - pad,
                bottom,
                p.s(7.0),
                CHIP,
                CHIP,
            );
        }

        p.text(
            item,
            if selected { f_selected } else { f },
            if selected { TEXT } else { TEXT },
            pad + p.s(10.0),
            top,
            client.right - p.s(34.0),
            bottom,
            DT_LEFT,
        );

        if selected {
            let ax = client.right - p.s(24.0);
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
    }

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);

    DeleteObject(f as *mut _);
    DeleteObject(f_selected as *mut _);
    DeleteObject(dib as *mut _);
    DeleteDC(dc);
}

unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            popup_paint(hwnd);
            0
        }
        WM_MOUSEMOVE => {
            let state = popup_state(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            let pad = s(POPUP_PAD, state.scale);
            let item_h = s(POPUP_ITEM, state.scale);
            let y = ((lparam as i32) >> 16) & 0xFFFF;
            let index = if y < pad {
                None
            } else {
                Some(((y - pad) / item_h) as usize)
            };
            let index = index.filter(|index| *index < state.items.len());
            if state.hover != index {
                state.hover = index;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let state = popup_state(hwnd);
            if !state.is_null() {
                let state = &*state;
                if let Some(index) = state.hover {
                    PostMessageW(state.parent, WM_APP_PICK, state.row, index as isize);
                }
            }
            DestroyWindow(hwnd);
            0
        }
        WM_KEYDOWN => {
            match wparam as u32 {
                0x1B => {
                    DestroyWindow(hwnd);
                }
                0x0D => {
                    let state = popup_state(hwnd);
                    if !state.is_null() {
                        let state = &*state;
                        PostMessageW(
                            state.parent,
                            WM_APP_PICK,
                            state.row,
                            state.selected as isize,
                        );
                    }
                    DestroyWindow(hwnd);
                }
                0x28 | 0x26 => {
                    let state = popup_state(hwnd);
                    if !state.is_null() {
                        let state = &mut *state;
                        let count = state.items.len();
                        if count > 0 {
                            let next = if (wparam as u32) == 0x28 {
                                (state.selected + 1) % count
                            } else {
                                (state.selected + count - 1) % count
                            };
                            state.selected = next;
                            state.hover = Some(next);
                            InvalidateRect(hwnd, std::ptr::null(), 0);
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
        WM_ERASEBKGND => 1,
        WM_DESTROY => {
            let state = popup_state(hwnd);
            if !state.is_null() {
                let state = Box::from_raw(state);
                let parent = state.parent;
                drop(state);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !parent.is_null() {
                    let settings = state_of(parent);
                    if !settings.is_null() {
                        (*settings).popup = std::ptr::null_mut();
                        InvalidateRect(parent, std::ptr::null(), 0);
                    }
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn open_popup(settings: HWND, row: usize) {
    let state = state_of(settings);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    if !state.popup.is_null() {
        DestroyWindow(state.popup);
        state.popup = std::ptr::null_mut();
    }

    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    GetClientRect(settings, &mut client);
    let l = layout(state, client);
    let anchor = l.selects[row - 1];

    let items: Vec<String> = match row {
        1 => GAUGE_OPTIONS.iter().map(|(label, _)| label.to_string()).collect(),
        2 => {
            let mut list = vec!["自动（全部合计）".to_string()];
            list.extend(state.interfaces.iter().cloned());
            list
        }
        _ => REFRESH_OPTIONS.iter().map(|(label, _)| label.to_string()).collect(),
    };
    let selected = match row {
        1 => state.gauge,
        2 => state.interface,
        _ => state.refresh,
    };

    let scale = state.scale;
    let width = anchor.right - anchor.left;
    let height = (s(POPUP_PAD * 2.0, scale) + s(POPUP_ITEM, scale) * items.len() as i32).max(1);

    let mut point = POINT {
        x: anchor.left,
        y: anchor.bottom + s(6.0, scale),
    };
    ClientToScreen(settings, &mut point);

    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
    let mut top = point.y;
    if top + height > work.bottom {
        // 下面放不下就翻到上方
        let mut above = POINT {
            x: anchor.left,
            y: anchor.top - s(6.0, scale),
        };
        ClientToScreen(settings, &mut above);
        top = (above.y - height).max(work.top);
    }

    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(POPUP_CLASS);
    let wc = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(popup_proc),
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

    let popup_state = Box::into_raw(Box::new(Popup {
        parent: settings,
        row,
        items,
        selected,
        hover: None,
        scale,
    }));

    let popup = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class.as_ptr(),
        wide("").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        point.x,
        top,
        width,
        height,
        settings,
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

    if popup.is_null() {
        let _ = Box::from_raw(popup_state);
        return;
    }

    SetWindowLongPtrW(popup, GWLP_USERDATA, popup_state as isize);
    round_corners(popup);
    ShowWindow(popup, SW_SHOW);
    SetForegroundWindow(popup);
    SetFocus(popup);
    InvalidateRect(popup, std::ptr::null(), 0);
    state.popup = popup;
    InvalidateRect(settings, std::ptr::null(), 0);
}

// ---------------------------------------------------------------- 主窗口

pub fn open(parent: HWND, width: i32, gauge: &str, interface: &str, refresh_ms: i32) {
    unsafe {
        // 已经开着就提到前面（同一时间只允许一个）
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
            style: CS_DROPSHADOW | CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dialog_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::tray::create_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let scale = crate::screen_scale();
        let width_px = s(440.0, scale);
        let height_px = s(396.0, scale);

        let mut screen = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut screen as *mut _ as *mut _, 0);
        let left = screen.left + ((screen.right - screen.left) - width_px) / 2;
        let top = screen.top + ((screen.bottom - screen.top) - height_px) / 3;

        let interfaces: Vec<String> = netspeed_core::net::adapters()
            .into_iter()
            .map(|adapter| adapter.alias)
            .collect();

        let gauge = GAUGE_OPTIONS
            .iter()
            .position(|(_, value)| value.eq_ignore_ascii_case(gauge))
            .unwrap_or(0);
        let interface = if interface.trim().is_empty() || interface.eq_ignore_ascii_case("auto") {
            0
        } else {
            interfaces
                .iter()
                .position(|alias| alias.eq_ignore_ascii_case(interface))
                .map(|index| index + 1)
                .unwrap_or(0)
        };
        let refresh = REFRESH_OPTIONS
            .iter()
            .position(|(_, ms)| *ms == refresh_ms)
            .unwrap_or(0);

        let state = Box::into_raw(Box::new(State {
            parent,
            interfaces,
            width: width.clamp(72, 360),
            gauge,
            interface,
            refresh,
            fonts: Fonts::create(scale),
            scale,
            hover: None,
            slider_drag: false,
            popup: std::ptr::null_mut(),
        }));

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            wide("网速显示设置").as_ptr(),
            WS_POPUP | WS_VISIBLE,
            left,
            top,
            width_px,
            height_px,
            // 不挂面板当 owner：面板会带 TOPMOST，设置窗会跟着置顶压住别的窗口
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
        round_corners(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        UpdateWindow(hwnd);
    }
}

/// Win11 圆角（Win10 上会失败，无所谓）
unsafe fn round_corners(hwnd: HWND) {
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_ROUND: i32 = 2;
    let value = DWMWCP_ROUND;
    DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &value as *const i32 as *const _,
        std::mem::size_of::<i32>() as u32,
    );
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

unsafe fn hit_test(state: &State, client: RECT, point: POINT) -> Option<Hit> {
    let l = layout(state, client);
    if contains(&l.close, point) {
        return Some(Hit::Close);
    }
    if contains(&l.done, point) {
        return Some(Hit::Done);
    }
    if contains(&l.slider, point) {
        return Some(Hit::Slider);
    }
    for (index, rect) in l.selects.iter().enumerate() {
        if contains(rect, point) {
            return Some(Hit::Select(index + 1));
        }
    }
    None
}

unsafe fn apply_width(hwnd: HWND, state: *mut State, x: i32, client: RECT) {
    let l = layout(&*state, client);
    let span = (l.slider.right - l.slider.left).max(1) as f32;
    let t = ((x - l.slider.left) as f32 / span).clamp(0.0, 1.0);
    let width = (72.0 + t * (360.0 - 72.0)).round() as i32;
    let width = (width / 4 * 4).clamp(72, 360); // 4px 一档，拖着不抖

    let state = &mut *state;
    if state.width == width {
        return;
    }
    state.width = width;
    crate::apply_panel_width(state.parent, width);
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

unsafe extern "system" fn dialog_proc(
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
            // 顶部空白 + 标题区域都能拖窗口（关闭按钮除外）
            if point.y < s(64.0, (*state).scale) && !contains(&l.close, point) {
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

            if state.slider_drag {
                apply_width(hwnd, state, point.x, client);
                return 0;
            }

            let hover = hit_test(state, client, point);
            if state.hover != hover {
                state.hover = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONDOWN => {
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

            match hit_test(state, client, point) {
                Some(Hit::Close) | Some(Hit::Done) => {
                    DestroyWindow(hwnd);
                }
                Some(Hit::Slider) => {
                    state.slider_drag = true;
                    SetCapture(hwnd);
                    apply_width(hwnd, state, point.x, client);
                }
                Some(Hit::Select(row)) => {
                    open_popup(hwnd, row);
                }
                None => {}
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let state = state_of(hwnd);
            if !state.is_null() {
                (*state).slider_drag = false;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_APP_PICK => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            let row = wparam;
            let index = lparam as usize;

            match row {
                1 => {
                    if let Some((_, value)) = GAUGE_OPTIONS.get(index) {
                        state.gauge = index;
                        crate::apply_gauge(state.parent, value);
                    }
                }
                2 => {
                    state.interface = index;
                    let alias = if index == 0 {
                        "auto".to_string()
                    } else {
                        state
                            .interfaces
                            .get(index - 1)
                            .cloned()
                            .unwrap_or_else(|| "auto".to_string())
                    };
                    crate::apply_network_interface(state.parent, &alias);
                }
                _ => {
                    if let Some((_, ms)) = REFRESH_OPTIONS.get(index) {
                        state.refresh = index;
                        crate::apply_refresh_ms(state.parent, *ms);
                    }
                }
            }

            state.popup = std::ptr::null_mut();
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_KEYDOWN => {
            if wparam as u32 == 0x1B {
                let state = state_of(hwnd);
                if !state.is_null() && !(*state).popup.is_null() {
                    DestroyWindow((*state).popup);
                } else {
                    DestroyWindow(hwnd);
                }
            }
            0
        }
        WM_SETCURSOR => {
            let state = state_of(hwnd);
            if state.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
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
            let cursor = match hit_test(&*state, client, point) {
                Some(Hit::Slider) => IDC_SIZEWE,
                Some(_) => IDC_HAND,
                None => IDC_ARROW,
            };
            SetCursor(LoadCursorW(std::ptr::null_mut(), cursor));
            1
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            let state = state_of(hwnd);
            if !state.is_null() {
                let state = Box::from_raw(state);
                if !state.popup.is_null() {
                    DestroyWindow(state.popup);
                }
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(state);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
