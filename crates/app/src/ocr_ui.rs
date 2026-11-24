//! 取字 / 翻译结果窗。
//!
//! 三种看法，同一个窗口：
//! - **逐句**（翻译默认）：一句一行，原文小灰字在上、译文大字在下；悬停露出「复制这句」
//! - **整段**：原文 / 译文各一块，带小标题，适合读长文
//! - **只取字**：纯 OCR 结果，还是原生 EDIT（输入法、选择、Ctrl+C 都靠它）
//!
//! 逐句和整段是自绘的（只读），所以不需要 EDIT；EDIT 在这两种视图里会被藏起来。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW,
    EndPaint, GetDC, InvalidateRect, ReleaseDC, RestoreDC, SaveDC, ScreenToClient, SelectObject,
    IntersectClipRect, SetBkColor, SetTextColor, DT_CALCRECT, DT_LEFT, DT_NOPREFIX,
    DT_WORDBREAK, HBRUSH,
    HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;
use crate::ui::{self, font, Painter, DT_CENTER};

const CLASS_NAME: &str = "NetSpeedTaskbarRs.OcrResult";

/// windows-sys 0.59 没导出 WM_MOUSELEAVE（不自己定义的话它会变成通配绑定，后面的分支全被吃掉）
const WM_MOUSELEAVE: u32 = 0x02A3;
const ID_EDIT: usize = 2001;

const EM_SETMARGINS: u32 = 0x00D3;
const EC_LEFTMARGIN: usize = 0x0001;
const EC_RIGHTMARGIN: usize = 0x0002;
const EM_SETSEL: u32 = 0x00B1;

/// 窗口逻辑尺寸（比原来宽一点：逐句要并排放下「原文 + 译文」还不挤）
/// 上次用的看法：0 逐句 / 1 整段（只取字不记，那是临时看一眼）
static LAST_VIEW: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

const WINDOW_W: f32 = 720.0;
const WINDOW_H: f32 = 520.0;

const HEAD_H: f32 = 56.0;
const FOOT_H: f32 = 66.0;
const PAD: f32 = 22.0;
/// 行内左右留白（正文不贴边）
const ROW_PAD_X: f32 = 24.0;
const ROW_PAD_Y: f32 = 13.0;
const ROW_GAP: f32 = 5.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Close,
    Copy,
    CopySource,
    ViewLines,
    ViewProse,
    ViewOcr,
    Row(usize),
    RowCopy(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Lines,
    Prose,
    Ocr,
}

struct Row {
    source: String,
    target: String,
    top: i32,
    height: i32,
    source_h: i32,
    target_h: i32,
}

struct State {
    edit: HWND,
    status: String,
    error: Option<String>,
    fonts: Fonts,
    scale: f32,
    hover: Option<Hit>,
    edit_brush: HBRUSH,
    edit_color: u32,
    view: View,
    source: String,
    translated: String,
    rows: Vec<Row>,
    scroll: i32,
    max_scroll: i32,
    /// 正文区可视高度（重排时算 max_scroll 用）
    viewport_h: i32,
    selected: Option<usize>,
    measured_width: i32,
    /// 已经在等 WM_MOUSELEAVE 了
    tracking_leave: bool,
}

struct Fonts {
    title: HFONT,
    body: HFONT,
    source: HFONT,
    small: HFONT,
    label: HFONT,
}

impl Fonts {
    fn create(scale: f32) -> Self {
        Self {
            title: font(14.5, 600, scale),
            body: font(15.5, 400, scale),
            source: font(12.5, 400, scale),
            small: font(12.0, 400, scale),
            label: font(11.0, 500, scale),
        }
    }
}

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

struct Layout {
    close: RECT,
    view_lines: RECT,
    view_prose: RECT,
    view_ocr: RECT,
    copy: RECT,
    copy_source: RECT,
    rows: RECT,
}

fn layout(state: &State, client: RECT) -> Layout {
    let scale = state.scale;
    let pad = ui::s(PAD, scale);

    let close_size = ui::s(26.0, scale);
    let close = RECT {
        left: client.right - pad - close_size,
        top: ui::s(15.0, scale),
        right: client.right - pad,
        bottom: ui::s(15.0, scale) + close_size,
    };

    // 头部右侧的三个看法切换（从右往左排：只取字 / 整段 / 逐句）
    let chip_h = ui::s(26.0, scale);
    let chip_top = ui::s(15.0, scale);
    let mut chip_right = close.left - ui::s(18.0, scale);
    let mut chip = |label: &str| -> RECT {
        let w = ui::s(if label == "只取字" { 58.0 } else { 44.0 }, scale);
        let rect = RECT {
            left: chip_right - w,
            top: chip_top,
            right: chip_right,
            bottom: chip_top + chip_h,
        };
        chip_right = rect.left - ui::s(2.0, scale);
        rect
    };
    let view_ocr = chip("只取字");
    let view_prose = chip("整段");
    let view_lines = chip("逐句");

    let button_h = ui::s(34.0, scale);
    let copy = RECT {
        left: client.right - pad - ui::s(96.0, scale),
        top: client.bottom - ui::s(16.0, scale) - button_h,
        right: client.right - pad,
        bottom: client.bottom - ui::s(16.0, scale),
    };
    let copy_source = RECT {
        left: copy.left - ui::s(10.0, scale) - ui::s(88.0, scale),
        top: copy.top,
        right: copy.left - ui::s(10.0, scale),
        bottom: copy.bottom,
    };

    let rows = RECT {
        left: pad,
        top: ui::s(HEAD_H, scale),
        right: client.right - pad,
        bottom: client.bottom - ui::s(FOOT_H, scale),
    };

    Layout {
        close,
        view_lines,
        view_prose,
        view_ocr,
        copy,
        copy_source,
        rows,
    }
}

fn contains(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

/// 量一段文字在给定宽度下要占多高（和画的时候用同一套 flag，高度才准）
unsafe fn measure(dc: HDC, font: HFONT, text: &str, width: i32) -> i32 {
    let old = SelectObject(dc, font as HGDIOBJ);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width.max(1),
        bottom: 0,
    };
    let units = wide(text);
    DrawTextW(
        dc,
        units.as_ptr(),
        (units.len() - 1) as i32,
        &mut rect,
        DT_LEFT | DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX,
    );
    SelectObject(dc, old);
    (rect.bottom - rect.top).max(1)
}

/// 逐句的行高：源文 + 译文 + 行内留白
unsafe fn relayout_rows(state: &mut State, width: i32) {
    if state.measured_width == width && !state.rows.is_empty() {
        return;
    }
    state.measured_width = width;
    if state.rows.is_empty() {
        state.max_scroll = 0;
        return;
    }
    let screen = GetDC(std::ptr::null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(std::ptr::null_mut(), screen);
    if dc.is_null() {
        return;
    }
    let scale = state.scale;
    let pad_y = ui::s(ROW_PAD_Y, scale);
    let gap = ui::s(ROW_GAP, scale);
    let mut top = 0;
    for row in state.rows.iter_mut() {
        row.source_h = if row.source.is_empty() {
            0
        } else {
            measure(dc, state.fonts.source, &row.source, width)
        };
        row.target_h = measure(dc, state.fonts.body, &row.target, width);
        row.top = top;
        row.height = pad_y * 2
            + row.source_h
            + if row.source_h > 0 { gap } else { 0 }
            + row.target_h;
        top += row.height;
    }
    DeleteDC(dc);
    let total: i32 = state.rows.iter().map(|row| row.height).sum();
    state.max_scroll = (total - state.viewport_h).max(0);
    state.scroll = state.scroll.clamp(0, state.max_scroll);
}

/// 鼠标落在哪一行
fn hit_row(state: &State, l: &Layout, point: POINT) -> Option<usize> {
    if point.x < l.rows.left || point.x >= l.rows.right + ui::s(12.0, state.scale) {
        return None;
    }
    if point.y < l.rows.top || point.y >= l.rows.bottom {
        return None;
    }
    let y = point.y - l.rows.top + state.scroll;
    state
        .rows
        .iter()
        .position(|row| y >= row.top && y < row.top + row.height)
}

unsafe fn paint_into(hwnd: HWND, target: HDC) {
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
    let c = crate::theme::ui();
    let pad = ui::s(PAD, state.scale);

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

    // ---------------- 头部 ----------------
    let title = if state.has_translation() {
        "翻译结果"
    } else {
        "取字结果"
    };
    p.text(
        title,
        state.fonts.title,
        c.text,
        pad + p.s(2.0),
        p.s(15.0),
        l.view_lines.left - p.s(12.0),
        p.s(41.0),
        DT_LEFT,
    );

    // 标题右边一句说明：几句 / 多少字
    let meta = if state.has_translation() {
        format!("{} 句 · {} 字", state.rows.len(), state.unit_count())
    } else {
        format!("{} 字", state.unit_count())
    };
    let title_w = p.text_width(title, state.fonts.title);
    let meta_left = pad + p.s(2.0) + title_w + p.s(12.0);
    p.text(
        &meta,
        state.fonts.small,
        c.text3,
        meta_left,
        p.s(15.0),
        meta_left + p.s(220.0),
        p.s(41.0),
        DT_LEFT,
    );

    if state.has_translation() {
        for (rect, view, label, hit) in [
            (l.view_lines, View::Lines, "逐句", Hit::ViewLines),
            (l.view_prose, View::Prose, "整段", Hit::ViewProse),
            (l.view_ocr, View::Ocr, "只取字", Hit::ViewOcr),
        ] {
            let active = state.view == view;
            let hovered = state.hover == Some(hit);
            if active || hovered {
                p.round(
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    p.s(7.0),
                    if active { c.chip } else { c.row_tint },
                    if active { c.chip } else { c.row_tint },
                );
            }
            p.text(
                label,
                state.fonts.small,
                if active { c.accent } else { c.text2 },
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                DT_CENTER,
            );
        }
    }

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

    // 头部下面一条细线（分组靠线，不靠盒子）
    p.fill(pad, l.rows.top - p.s(1.0), client.right - pad, l.rows.top, c.border);

    // ---------------- 正文 ----------------
    match state.view {
        View::Lines => paint_lines(&p, state, &l, c),
        View::Prose => paint_prose(&p, state, &l, c),
        View::Ocr => {}
    }

    // ---------------- 底部 ----------------
    p.fill(
        pad,
        client.bottom - ui::s(FOOT_H, state.scale) + p.s(1.0),
        client.right - pad,
        client.bottom - ui::s(FOOT_H, state.scale) + p.s(2.0),
        c.border,
    );
    let status_color = if state.error.is_some() { c.accent } else { c.text3 };
    p.text(
        &state.status,
        state.fonts.small,
        status_color,
        pad + p.s(2.0),
        l.copy.top,
        l.copy_source.left - p.s(12.0),
        l.copy.bottom,
        DT_LEFT,
    );

    if state.has_translation() && state.view != View::Ocr {
        let hovered = state.hover == Some(Hit::CopySource);
        p.round(
            l.copy_source.left,
            l.copy_source.top,
            l.copy_source.right,
            l.copy_source.bottom,
            p.s(9.0),
            c.surface,
            if hovered { c.text3 } else { c.border },
        );
        p.text(
            "复制原文",
            state.fonts.small,
            if hovered { c.text } else { c.text2 },
            l.copy_source.left,
            l.copy_source.top,
            l.copy_source.right,
            l.copy_source.bottom,
            DT_CENTER,
        );
    }

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
        if state.view == View::Ocr { "复制" } else { "复制全部" },
        state.fonts.small,
        0x00FF_FFFF,
        l.copy.left,
        l.copy.top,
        l.copy.right,
        l.copy.bottom,
        DT_CENTER,
    );

    BitBlt(target, 0, 0, client.right, client.bottom, dib.dc, 0, 0, SRCCOPY);
}

unsafe fn paint_lines(p: &Painter, state: &State, l: &Layout, c: crate::theme::Ui) {
    let saved = SaveDC(p.dc);
    if saved != 0 {
        IntersectClipRect(p.dc, l.rows.left, l.rows.top, l.rows.right, l.rows.bottom);
    }

    let pad_x = ui::s(ROW_PAD_X, state.scale);
    let text_left = l.rows.left + pad_x;
    let width = (l.rows.right - pad_x) - text_left;

    for (index, row) in state.rows.iter().enumerate() {
        let top = l.rows.top + row.top - state.scroll;
        let bottom = top + row.height;
        if bottom < l.rows.top - 2 || top > l.rows.bottom + 2 {
            continue;
        }
        let hovered =
            state.hover == Some(Hit::Row(index)) || state.hover == Some(Hit::RowCopy(index));
        let selected = state.selected == Some(index);

        if selected {
            p.fill(l.rows.left, top, l.rows.right, bottom, c.row_tint);
            p.fill(l.rows.left, top, l.rows.left + p.s(2.0), bottom, c.accent);
        } else if hovered {
            p.fill(l.rows.left, top, l.rows.right, bottom, 0x00FA_FBFC);
        }

        let mut y = top + ui::s(ROW_PAD_Y, state.scale);
        if !row.source.is_empty() {
            p.text_block(
                &row.source,
                state.fonts.source,
                c.text3,
                text_left,
                y,
                text_left + width,
                y + row.source_h,
            );
            y += row.source_h + ui::s(ROW_GAP, state.scale);
        }
        p.text_block(
            &row.target,
            state.fonts.body,
            c.text,
            text_left,
            y,
            text_left + width,
            y + row.target_h,
        );

        // 行尾的「复制这句」：鼠标在这一行才出现
        if hovered {
            let bw = p.s(72.0);
            let bh = p.s(26.0);
            let right = l.rows.right - ui::s(10.0, state.scale);
            let mid = (top + bottom) / 2;
            let rect = RECT {
                left: right - bw,
                top: mid - bh / 2,
                right,
                bottom: mid + bh / 2,
            };
            let active = state.hover == Some(Hit::RowCopy(index));
            p.round(
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                p.s(8.0),
                c.surface,
                if active { c.accent } else { c.border },
            );
            p.text(
                "复制这句",
                state.fonts.small,
                if active { c.accent } else { c.text2 },
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                DT_CENTER,
            );
        }

        // 句与句之间一条极细的线（只在译文块下方，不切在两行中间）
        p.fill(
            text_left,
            bottom - 1,
            l.rows.right - ui::s(10.0, state.scale),
            bottom,
            0x00F1_F3F6,
        );
    }

    if saved != 0 {
        RestoreDC(p.dc, saved);
    }

    // 滚动条：细长一条，贴右边缘
    if state.max_scroll > 0 {
        let track_top = l.rows.top + ui::s(6.0, state.scale);
        let track_bottom = l.rows.bottom - ui::s(6.0, state.scale);
        let total = state.rows.iter().map(|row| row.height).sum::<i32>().max(1);
        let visible = (l.rows.bottom - l.rows.top).max(1);
        let thumb_h = ((visible as f32 / total as f32) * (track_bottom - track_top) as f32)
            .max(p.s(28.0) as f32) as i32;
        let travel = (track_bottom - track_top - thumb_h).max(1);
        let offset = (state.scroll as f32 / state.max_scroll as f32 * travel as f32) as i32;
        let x = l.rows.right + ui::s(4.0, state.scale);
        p.round(
            x,
            track_top + offset,
            x + p.s(3.0),
            track_top + offset + thumb_h,
            p.s(3.0),
            c.border,
            c.border,
        );
    }
}

unsafe fn paint_prose(p: &Painter, state: &State, l: &Layout, c: crate::theme::Ui) {
    let pad_x = p.s(ROW_PAD_X);
    let left = l.rows.left + pad_x;
    let right = l.rows.right - pad_x;
    let width = right - left;
    let saved = SaveDC(p.dc);
    if saved != 0 {
        IntersectClipRect(p.dc, l.rows.left, l.rows.top, l.rows.right, l.rows.bottom);
    }

    let mut y = l.rows.top + p.s(12.0) - state.scroll;
    if !state.source.is_empty() {
        p.text("原文", state.fonts.label, c.text3, left, y, right, y + p.s(15.0), DT_LEFT);
        y += p.s(19.0);
        let h = measure(p.dc, state.fonts.source, &state.source, width);
        p.text_block(
            &state.source,
            state.fonts.source,
            c.text2,
            left,
            y,
            left + width,
            y + h,
        );
        y += h + p.s(26.0);
    }
    if !state.translated.is_empty() {
        p.text("译文", state.fonts.label, c.text3, left, y, right, y + p.s(15.0), DT_LEFT);
        y += p.s(19.0);
        let h = measure(p.dc, state.fonts.body, &state.translated, width);
        p.text_block(
            &state.translated,
            state.fonts.body,
            c.text,
            left,
            y,
            left + width,
            y + h,
        );
    }
    if saved != 0 {
        RestoreDC(p.dc, saved);
    }
}

impl State {
    fn has_translation(&self) -> bool {
        self.error.is_none() && !self.translated.is_empty()
    }

    fn unit_count(&self) -> usize {
        let text = if self.translated.is_empty() {
            &self.source
        } else {
            &self.translated
        };
        text.chars().filter(|ch| !ch.is_whitespace()).count()
    }
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
    if state.edit.is_null() {
        return;
    }
    let show = state.view == View::Ocr;
    if show {
        MoveWindow(
            state.edit,
            l.rows.left,
            l.rows.top,
            l.rows.right - l.rows.left,
            l.rows.bottom - l.rows.top,
            1,
        );
        SendMessageW(
            state.edit,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as WPARAM,
            (ui::s(10.0, state.scale) as isize) | ((ui::s(10.0, state.scale) as isize) << 16),
        );
    }
    ShowWindow(state.edit, if show { SW_SHOW } else { SW_HIDE });

    // 逐句的行高依赖正文宽度，宽度一变就重排
    let width = (l.rows.right - ui::s(ROW_PAD_X, state.scale))
        - (l.rows.left + ui::s(ROW_PAD_X, state.scale));
    state.viewport_h = l.rows.bottom - l.rows.top;
    relayout_rows(state, width);
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

/// 复制一句（译文）
unsafe fn copy_row(hwnd: HWND, index: usize) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    let Some(row) = state.rows.get(index) else {
        return;
    };
    let text = if row.target.is_empty() {
        row.source.clone()
    } else {
        row.target.clone()
    };
    crate::ocr::copy_text(&text);
    state.selected = Some(index);
    state.status = format!("已复制第 {} 句", index + 1);
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

/// 复制当前视图的全部内容
unsafe fn copy_all(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    if state.view == View::Ocr {
        copy_from_edit(hwnd);
        return;
    }
    let text = if state.translated.is_empty() {
        state.source.clone()
    } else {
        state.translated.clone()
    };
    crate::ocr::copy_text(&text);
    state.status = format!("已自动复制 · {} 字", text.chars().count());
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

/// 复制原文
unsafe fn copy_source(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    if state.source.is_empty() {
        return;
    }
    crate::ocr::copy_text(&state.source);
    state.status = "已复制原文".to_string();
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

fn open_inner(source: Option<&str>, translated: Option<&str>, error: Option<&str>) {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW | CS_DBLCLKS,
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
        let width = ui::s(WINDOW_W, scale);
        let height = ui::s(WINDOW_H, scale);

        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
        let x = work.left + ((work.right - work.left) - width) / 2;
        let y = work.top + ((work.bottom - work.top) - height) / 3;

        let source_text = source.unwrap_or("").to_string();
        let translated_text = translated.unwrap_or("").to_string();

        // 逐句：原文和译文按行配对；行数对不上（模型多拆/少拆了行）就退到整段
        let mut rows: Vec<Row> = Vec::new();
        let mut can_lines = false;
        if !translated_text.is_empty() {
            let source_lines: Vec<&str> = source_text
                .lines()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .collect();
            let target_lines: Vec<&str> = translated_text
                .lines()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .collect();
            if !source_lines.is_empty() && source_lines.len() == target_lines.len() {
                can_lines = true;
                for (a, b) in source_lines.iter().zip(target_lines.iter()) {
                    rows.push(Row {
                        source: a.to_string(),
                        target: b.to_string(),
                        top: 0,
                        height: 0,
                        source_h: 0,
                        target_h: 0,
                    });
                }
            } else if !target_lines.is_empty() {
                rows.push(Row {
                    source: source_lines.join(" "),
                    target: target_lines.join(" "),
                    top: 0,
                    height: 0,
                    source_h: 0,
                    target_h: 0,
                });
                can_lines = true;
            }
        }

        let view = if can_lines {
            // 记住上次看的姿势：偏爱「整段」的人不用每回都切
            match LAST_VIEW.load(std::sync::atomic::Ordering::Relaxed) {
                1 => View::Prose,
                _ => View::Lines,
            }
        } else {
            View::Ocr
        };
        let status = match error {
            Some(message) => message.to_string(),
            None => {
                let text = if translated_text.is_empty() {
                    &source_text
                } else {
                    &translated_text
                };
                format!(
                    "已自动复制 · {} 字",
                    text.chars().filter(|ch| !ch.is_whitespace()).count()
                )
            }
        };

        let state = Box::into_raw(Box::new(State {
            edit: std::ptr::null_mut(),
            status,
            error: error.map(|message| message.to_string()),
            fonts: Fonts::create(scale),
            scale,
            hover: None,
            edit_brush: std::ptr::null_mut(),
            edit_color: 0,
            view,
            source: source_text.clone(),
            translated: translated_text.clone(),
            rows,
            scroll: 0,
            max_scroll: 0,
            viewport_h: 0,
            selected: None,
            measured_width: -1,
            tracking_leave: false,
        }));

        let title = if translated.is_some() {
            "翻译结果"
        } else {
            "取字结果"
        };

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
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
            let _ = Box::from_raw(state);
            return;
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);

        // OCR 视图才用得上 EDIT（逐句 / 整段是自绘只读内容）
        // EDIT 里始终放 OCR 原文：「只取字」视图靠它显示，其余视图它是隐藏的。
        // 注意 EDIT 只认 \r\n，裸 \n 会被当成没换行，整段挤成一行
        let edit_text = source_text.replace('\r', "").replace('\n', "\r\n");
        let edit = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide(&edit_text).as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_TABSTOP
                | 0x0004  // ES_MULTILINE
                | 0x0040, // ES_AUTOVSCROLL（不要 ES_AUTOHSCROLL：长行要折，不要横向滚）
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
            let to_copy = if translated_text.is_empty() {
                source_text.as_str()
            } else {
                translated_text.as_str()
            };
            crate::ocr::copy_text(to_copy);
        }

        ui::round_corners(hwnd);
        layout_edit(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        if (*state).view == View::Ocr {
            SetFocus(edit);
            SendMessageW(edit, EM_SETSEL, 0, -1);
        } else {
            SetFocus(hwnd);
        }
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
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_into(hwnd, hdc);
            EndPaint(hwnd, &mut ps);
            0
        }
        // 离屏预览走这条：和真实绘制同一个函数
        WM_PRINTCLIENT => {
            paint_into(hwnd, wparam as HDC);
            0
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            layout_edit(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = wparam as HDC;
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
            if point.y < ui::s(HEAD_H, (*state).scale) && !contains(&l.close, point) {
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

            let mut hover = if contains(&l.close, point) {
                Some(Hit::Close)
            } else if contains(&l.copy, point) {
                Some(Hit::Copy)
            } else if state.has_translation() && contains(&l.copy_source, point) {
                Some(Hit::CopySource)
            } else if state.has_translation() && contains(&l.view_lines, point) {
                Some(Hit::ViewLines)
            } else if state.has_translation() && contains(&l.view_prose, point) {
                Some(Hit::ViewProse)
            } else if state.has_translation() && contains(&l.view_ocr, point) {
                Some(Hit::ViewOcr)
            } else {
                None
            };

            if hover.is_none() && state.view == View::Lines {
                if let Some(index) = hit_row(state, &l, point) {
                    // 行尾那块是「复制这句」，其余地方是选中这一行
                    let right = l.rows.right - ui::s(10.0, state.scale);
                    let near_copy = point.x >= right - ui::s(72.0, state.scale);
                    hover = Some(if near_copy {
                        Hit::RowCopy(index)
                    } else {
                        Hit::Row(index)
                    });
                }
            }

            if !state.tracking_leave {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if TrackMouseEvent(&mut track) != 0 {
                    state.tracking_leave = true;
                }
            }
            if state.hover != hover {
                state.hover = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            let state = state_of(hwnd);
            if !state.is_null() {
                (*state).tracking_leave = false;
                if (*state).hover.is_some() {
                    (*state).hover = None;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
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
                return 0;
            }
            if contains(&l.copy, point) {
                copy_all(hwnd);
                return 0;
            }
            if (*state).has_translation() {
                if contains(&l.copy_source, point) {
                    copy_source(hwnd);
                    return 0;
                }
                if contains(&l.view_lines, point) {
                    (*state).view = View::Lines;
                    LAST_VIEW.store(0, std::sync::atomic::Ordering::Relaxed);
                    (*state).scroll = 0;
                    layout_edit(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if contains(&l.view_prose, point) {
                    (*state).view = View::Prose;
                    LAST_VIEW.store(1, std::sync::atomic::Ordering::Relaxed);
                    (*state).scroll = 0;
                    layout_edit(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if contains(&l.view_ocr, point) {
                    (*state).view = View::Ocr;
                    (*state).scroll = 0;
                    layout_edit(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
            }
            if (*state).view == View::Lines {
                if let Some(index) = hit_row(&*state, &l, point) {
                    let right = l.rows.right - ui::s(10.0, (*state).scale);
                    if point.x >= right - ui::s(72.0, (*state).scale) {
                        copy_row(hwnd, index);
                    } else {
                        (*state).selected = Some(index);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
            }
            0
        }
        WM_LBUTTONDBLCLK => {
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
            if (*state).view == View::Lines {
                if let Some(index) = hit_row(&*state, &l, point) {
                    copy_row(hwnd, index);
                }
            }
            0
        }
        WM_MOUSEWHEEL => {
            let state = state_of(hwnd);
            if state.is_null() || (*state).view == View::Ocr {
                return 0;
            }
            let state = &mut *state;
            let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32;
            let step = ui::s(64.0, state.scale);
            state.scroll = (state.scroll - delta.signum() * step).clamp(0, state.max_scroll);
            InvalidateRect(hwnd, std::ptr::null(), 0);
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
                let interactive = contains(&l.close, point)
                    || contains(&l.copy, point)
                    || ((*state).has_translation() && contains(&l.copy_source, point))
                    || ((*state).has_translation()
                        && (contains(&l.view_lines, point)
                            || contains(&l.view_prose, point)
                            || contains(&l.view_ocr, point)))
                    || ((*state).view == View::Lines
                        && hit_row(&*state, &l, point).is_some());
                if interactive {
                    ui::set_hand_cursor();
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
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
                // ↑ / ↓：在句与句之间走
                0x26 | 0x28 => {
                    if state.view == View::Lines && !state.rows.is_empty() {
                        let count = state.rows.len();
                        let next = match (state.selected, wparam as u32) {
                            (Some(index), 0x28) => (index + 1).min(count - 1),
                            (Some(index), _) => index.saturating_sub(1),
                            (None, 0x28) => 0,
                            (None, _) => count - 1,
                        };
                        state.selected = Some(next);
                        // 选中行要滚进可视区
                        let row = &state.rows[next];
                        if row.top < state.scroll {
                            state.scroll = row.top;
                        } else if row.top + row.height > state.scroll + state.viewport_h {
                            state.scroll = row.top + row.height - state.viewport_h;
                        }
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                0x0D => {
                    if let Some(index) = state.selected {
                        copy_row(hwnd, index);
                    } else {
                        copy_all(hwnd);
                    }
                }
                // Ctrl+C
                0x43 => {
                    let ctrl = unsafe {
                        windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11)
                    } < 0;
                    if ctrl {
                        match state.selected {
                            Some(index) if state.view == View::Lines => copy_row(hwnd, index),
                            _ => copy_all(hwnd),
                        }
                    }
                }
                // 1 / 2 / 3：逐句 / 整段 / 只取字
                0x31 | 0x32 | 0x33 => {
                    if state.has_translation() {
                        let next = match wparam as u32 {
                            0x31 => View::Lines,
                            0x32 => View::Prose,
                            _ => View::Ocr,
                        };
                        state.view = next;
                        state.scroll = 0;
                        match next {
                            View::Lines => LAST_VIEW.store(0, std::sync::atomic::Ordering::Relaxed),
                            View::Prose => LAST_VIEW.store(1, std::sync::atomic::Ordering::Relaxed),
                            View::Ocr => {}
                        }
                        layout_edit(hwnd);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                0x21 => {
                    state.scroll = (state.scroll - state.viewport_h).max(0);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0x22 => {
                    state.scroll = (state.scroll + state.viewport_h).min(state.max_scroll);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                _ => {}
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

/// 调试用：把结果窗离屏画成 PNG（和真实绘制同一条代码路径）
pub unsafe fn preview_png(path: &str, view: &str) -> bool {
    let source = "Latency budget\nThe panel samples the taskbar twice a second.\nIf sampling fails, the last good color is kept for 30 seconds before we retry,\nso the background never flickers between two close shades.";
    let translated = "延迟预算\n面板每秒采样任务栏两次。\n如果采样失败，会保留上一次成功的颜色 30 秒再重试，\n所以背景不会在两种接近的色值之间来回跳。";
    open_translate(std::ptr::null_mut(), source, translated, None);

    // 找到刚创建的那个窗口，让它把第一帧画完
    let mut message: MSG = std::mem::zeroed();
    let mut hwnd: HWND = std::ptr::null_mut();
    for _ in 0..6 {
        while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut found: HWND = std::ptr::null_mut();
    EnumThreadWindows(
        windows_sys::Win32::System::Threading::GetCurrentThreadId(),
        Some(enum_preview),
        &mut found as *mut HWND as isize,
    );
    hwnd = found;
    if hwnd.is_null() {
        return false;
    }
    // 按名字切到指定视图再截
    {
        let state = state_of(hwnd);
        if !state.is_null() {
            (*state).view = match view {
                "prose" => View::Prose,
                "ocr" => View::Ocr,
                _ => View::Lines,
            };
            layout_edit(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    let width = (client.right - client.left).max(1);
    let height = (client.bottom - client.top).max(1);

    let (dc, dib, bits) = crate::ui::make_dib(width, height);
    if dc.is_null() || bits.is_null() {
        DestroyWindow(hwnd);
        return false;
    }
    SendMessageW(hwnd, WM_PRINTCLIENT, dc as usize, 0);

    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for index in 0..(width * height) as usize {
        let offset = index * 4;
        rgba.push(*bits.add(offset + 2));
        rgba.push(*bits.add(offset + 1));
        rgba.push(*bits.add(offset));
        rgba.push(255);
    }
    let saved = match image::RgbaImage::from_raw(width as u32, height as u32, rgba) {
        Some(buffer) => buffer.save(path).is_ok(),
        None => false,
    };
    DeleteObject(dib as *mut _);
    DeleteDC(dc);
    DestroyWindow(hwnd);
    saved
}

unsafe extern "system" fn enum_preview(hwnd: HWND, lparam: LPARAM) -> i32 {
    let class = wide(CLASS_NAME);
    let mut buffer = [0u16; 128];
    GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    if buffer[..class.len() - 1] == class[..class.len() - 1] {
        *(lparam as *mut HWND) = hwnd;
        return 0;
    }
    1
}

pub fn open(_parent: HWND, text: &str, error: Option<&str>) {
    open_inner(Some(text), None, error);
}

/// 翻译结果：原文 + 译文分开给，窗口自己决定怎么排
pub fn open_translate(_parent: HWND, source: &str, translated: &str, error: Option<&str>) {
    open_inner(Some(source), Some(translated), error);
}
