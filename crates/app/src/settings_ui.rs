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
    InvalidateRect, IntersectClipRect, LineTo, MoveToEx, ReleaseDC, RestoreDC, RoundRect, SaveDC,
    ScreenToClient, SelectObject, SetBkMode, SetTextColor, SetViewportOrgEx, UpdateWindow,
    BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_LEFT, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;
use netspeed_core::settings::Settings;

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Settings";
const POPUP_CLASS: &str = "NetSpeedTaskbarRs.Settings.Popup";

/// 下拉里选完，子窗口用这个消息告诉父窗口
const WM_APP_PICK: u32 = WM_APP + 0x31;

const DT_NOPREFIX: u32 = 0x0800;


const ID_WIDTH_LABEL: usize = 2001;
const ID_GAUGE_LABEL: usize = 2004;
const ID_INTERFACE_LABEL: usize = 2007;
const ID_REFRESH_LABEL: usize = 2009;
const ID_FONT_LABEL: usize = 2011;

/// 面板字号（显示文案, 百分比）
const FONT_OPTIONS: [(&str, i32); 4] = [("小", 90), ("标准", 100), ("大", 110), ("特大", 125)];

const GAUGE_OPTIONS: [(&str, &str); 3] = [
    ("内存占用", "mem"),
    ("Token 速度", "ai"),
    ("关闭", "none"),
];

/// 面板刷新间隔（显示文案, 毫秒）
const REFRESH_OPTIONS: [(&str, i32); 3] = [("0.5 秒", 500), ("1 秒", 1000), ("2 秒", 2000)];

const ROW_COUNT: usize = 5;
const ROW_FIRST: f32 = 84.0;
const ROW_STEP: f32 = 62.0;
const CONTROL_X: f32 = 178.0;

// AI 区块（在 5 个基础行下面）
const AI_HEAD_TOP: f32 = 396.0; // 分隔线
const AI_TITLE_TOP: f32 = 402.0; // 「AI 统计」小标题
const AI_TOGGLE_TOP: f32 = 428.0; // 总开关行
const AI_SOURCES_TOP: f32 = 470.0; // 来源格子起点（两列三行）
const AI_ROW_H: f32 = 42.0;
/// 来源格子的行高（两列三行，比整行矮一点，正好铺满）
const AI_SOURCE_ROW_H: f32 = 36.0;
const AI_SOURCE_LABELS: [&str; 6] = [
    "OpenCode",
    "dsh",
    "WorkBuddy",
    "WorkBuddy AI",
    "Claude Code",
    "Codex",
];

/// 可滚动内容区：分隔线往下到页脚分割线往上
const CONTENT_TOP: f32 = 65.0;
/// 内容总高度（最后一个来源行 + 一点留白）；窗口比这个矮就出现滚动
const CONTENT_BOTTOM: f32 = 582.0;
/// 页脚（分割线 + 完成按钮）高度
const FOOTER_H: f32 = 64.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Close,
    Done,
    Slider,
    Select(usize),
    AiToggle,
    AiSource(usize),
    Scrollbar,
    AutoStart,
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

/// 加一档对比度的前景色：accent 上的圆钮、勾和滑块都用它（深色下也一样亮）
const KNOB: u32 = 0x00FF_FFFF;

/// 位图 → 偏白（t=0 原色，t=1 纯白）
fn mix_white(color: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| -> u32 {
        let value = ((color >> shift) & 0xFF) as f32;
        (value + (255.0 - value) * t).round() as u32
    };
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
        let rect = RECT {
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

    /// 空心圆角框（键盘焦点框）：只画边，不盖住里面的内容
    unsafe fn ring(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
        color: u32,
        width: i32,
    ) {
        let pen = CreatePen(0, width.max(1), color);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        let old_brush = SelectObject(
            self.dc,
            windows_sys::Win32::Graphics::Gdi::GetStockObject(
                windows_sys::Win32::Graphics::Gdi::NULL_BRUSH,
            ),
        );
        RoundRect(self.dc, left, top, right, bottom, radius, radius);
        SelectObject(self.dc, old_brush);
        SelectObject(self.dc, old_pen);
        DeleteObject(pen as *mut _);
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


}

// ---------------------------------------------------------------- 状态

struct State {
    parent: HWND,
    interfaces: Vec<String>,
    width: i32,
    gauge: usize,
    interface: usize,
    refresh: usize,
    font: usize,
    ai_enabled: bool,
    ai_sources: [bool; 6],
    auto_start: bool,
    fonts: Fonts,
    scale: f32,
    hover: Option<Hit>,
    /// 键盘焦点（Tab 走一圈）；鼠标一动就清掉，避免两个「当前项」打架
    focus: Option<Hit>,
    /// 最近一次操作是键盘：只有键盘操作才画焦点框
    keyboard: bool,
    slider_drag: bool,
    /// 正在拖滚动条滑块
    scroll_drag: bool,
    /// 内容区滚动位置（像素，0 = 顶部）
    scroll: i32,
    popup: HWND,
}

struct Layout {
    close: RECT,
    done: RECT,
    /// 滑杆的点击区域（比视觉上的轨道高，好点）
    slider: RECT,
    /// 四个下拉：0 = 字号、1 = 指示条、2 = 网卡、3 = 刷新（行号 = 下标 + 1）
    selects: [RECT; 4],
    ai_toggle: RECT,
    ai_sources: [RECT; 6],
    /// 页脚左边的「开机自启」整行热区
    auto_start: RECT,
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

    // 第 0 行是字号（下拉），滑杆停在原来的位置（第 1 行）
    let slider = RECT {
        left: control_left,
        top: row_center(1) - s(14.0, scale),
        right: right - s(96.0, scale),
        bottom: row_center(1) + s(14.0, scale),
    };

    let mut selects = [RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }; 4];
    // 字号占第 0 行，其余三个接着往下摆（跳过滑杆那一行）
    let rows = [0usize, 2, 3, 4];
    let height = s(32.0, scale);
    for (slot, row) in selects.iter_mut().zip(rows) {
        let center = row_center(row);
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
        ai_toggle: RECT {
            left: s(12.0, scale),
            top: s(AI_TOGGLE_TOP, scale),
            right,
            bottom: s(AI_TOGGLE_TOP + AI_ROW_H, scale),
        },
        ai_sources: ai_source_rects(scale, right),
        // 页脚左边：标签 + 开关都在这个热区里
        auto_start: RECT {
            left: pad + s(4.0, scale),
            top: done.top,
            right: (pad + s(4.0, scale) + s(170.0, scale)).min(done.left - s(12.0, scale)),
            bottom: done.bottom,
        },
    }
}

fn ai_source_rects(scale: f32, right: i32) -> [RECT; 6] {
    // 两列三行：一列六行会把窗口顶出滚动条
    let left = s(12.0, scale);
    let gutter = s(12.0, scale);
    let column = (right - left - gutter) / 2;
    let top0 = s(AI_SOURCES_TOP, scale);
    let row_h = s(AI_SOURCE_ROW_H, scale);

    let mut rects = [RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }; 6];
    for (index, slot) in rects.iter_mut().enumerate() {
        let column_index = (index / 3) as i32;
        let row = (index % 3) as i32;
        let left = left + column_index * (column + gutter);
        let top = top0 + row * row_h;
        *slot = RECT {
            left,
            top,
            right: left + column,
            bottom: top + row_h,
        };
    }
    rects
}

fn contains(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

/// 内容区在窗口里的可见范围（页头分隔线和页脚分割线之间）
fn content_viewport(state: &State, client: RECT) -> (i32, i32) {
    let top = s(CONTENT_TOP, state.scale);
    let bottom = (client.bottom - s(FOOTER_H, state.scale)).max(top);
    (top, bottom)
}

/// 最多能往下滚多少像素
fn max_scroll(state: &State, client: RECT) -> i32 {
    let (top, bottom) = content_viewport(state, client);
    let content = s(CONTENT_BOTTOM - CONTENT_TOP, state.scale);
    let overflow = content - (bottom - top);
    // 留 2px 容差：默认高度下正好铺满，别因为取整差 1px 就冒出一条没用的滚动条
    if overflow <= s(2.0, state.scale) {
        0
    } else {
        overflow
    }
}

/// 竖向滚动条的几何：绘制和拖动共用同一套算法
struct ScrollBar {
    x: i32,
    track_w: i32,
    track_top: i32,
    track_bottom: i32,
    thumb_h: i32,
    travel: i32,
}

impl ScrollBar {
    fn thumb_top(&self, scroll: i32, max: i32) -> i32 {
        if self.travel <= 0 || max <= 0 {
            return self.track_top;
        }
        let ratio = scroll.clamp(0, max) as f32 / max as f32;
        self.track_top + (self.travel as f32 * ratio).round() as i32
    }

    /// 鼠标 y → 新的 scroll（让滑块中心跟着鼠标走）
    fn scroll_at(&self, y: i32, max: i32) -> i32 {
        if self.travel <= 0 || max <= 0 {
            return 0;
        }
        let offset = y - self.track_top - self.thumb_h / 2;
        let ratio = offset as f32 / self.travel as f32;
        (ratio * max as f32).round().clamp(0.0, max as f32) as i32
    }

    /// 命中区比画出来的 4px 宽一点，不然不好点
    fn hot(&self, scale: f32) -> RECT {
        let pad = s(4.0, scale);
        RECT {
            left: self.x - pad,
            top: self.track_top,
            right: self.x + self.track_w + pad,
            bottom: self.track_bottom,
        }
    }
}

fn scroll_bar(state: &State, client: RECT) -> ScrollBar {
    let scale = state.scale;
    let (view_top, view_bottom) = content_viewport(state, client);
    let track_w = s(4.0, scale);
    let x = client.right - s(9.0, scale);
    let track_top = view_top + s(4.0, scale);
    let track_bottom = view_bottom - s(4.0, scale);
    let track_h = (track_bottom - track_top).max(1);
    let span = s(CONTENT_BOTTOM - CONTENT_TOP, scale).max(1);
    let thumb_h = ((track_h as f32) * (track_h as f32) / (span as f32)).round() as i32;
    let thumb_h = thumb_h.clamp(s(24.0, scale), track_h);
    ScrollBar {
        x,
        track_w,
        track_top,
        track_bottom,
        thumb_h,
        travel: (track_h - thumb_h).max(0),
    }
}

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

fn text_of_row(state: &State, row: usize) -> String {
    match row {
        1 => FONT_OPTIONS
            .get(state.font)
            .map(|(label, _)| *label)
            .unwrap_or("标准")
            .to_string(),
        2 => GAUGE_OPTIONS
            .get(state.gauge)
            .map(|(label, _)| *label)
            .unwrap_or("内存占用")
            .to_string(),
        3 => {
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
        ID_FONT_LABEL => "字号",
        ID_WIDTH_LABEL => "面板宽度",
        ID_GAUGE_LABEL => "底部指示条",
        ID_INTERFACE_LABEL => "网卡",
        _ => "刷新间隔",
    }
}

// ---------------------------------------------------------------- 绘画

unsafe fn paint(hwnd: HWND) {
    let c = crate::theme::ui();
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

    let pad = p.s(20.0);
    let content_right = client.right - pad;

    // 标题
    p.text(
        "显示设置",
        state.fonts.title,
        c.text,
        pad + p.s(4.0),
        p.s(16.0),
        content_right,
        p.s(46.0),
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

    // 顶部分割线
    p.fill(0, p.s(64.0), client.right, p.s(65.0), c.border);

    // ---------------------------------------------------------------- 可滚动内容
    //
    // 窗口比内容矮时（小屏 / 高缩放）只滚中间这一段：页头（标题、×）和
    // 页脚（完成按钮）固定不动。用 GDI 的视口原点做平移，画图函数里的
    // 坐标还是「未滚动」的绝对值，裁剪区负责把超出部分切掉。
    let (view_top, view_bottom) = content_viewport(state, client);
    let max = max_scroll(state, client);
    if state.scroll > max {
        state.scroll = max;
    }
    if state.scroll < 0 {
        state.scroll = 0;
    }
    let scroll = state.scroll;

    let saved = SaveDC(dc);
    if saved != 0 {
        IntersectClipRect(dc, 0, view_top, client.right, view_bottom);
        SetViewportOrgEx(dc, 0, -scroll, std::ptr::null_mut());
    }

    // 行
    for row in 0..ROW_COUNT {
        let top = p.s(ROW_FIRST + ROW_STEP * row as f32);
        let bottom = p.s(ROW_FIRST + ROW_STEP * (row + 1) as f32);
        let center = (top + bottom) / 2 - p.s(6.0);

        let label = match row {
            0 => row_label(ID_FONT_LABEL),
            1 => row_label(ID_WIDTH_LABEL),
            2 => row_label(ID_GAUGE_LABEL),
            3 => row_label(ID_INTERFACE_LABEL),
            _ => row_label(ID_REFRESH_LABEL),
        };
        p.text(
            label,
            state.fonts.label,
            c.text,
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
                mix_white(c.border, 0.45),
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
            c.chip,
            c.chip,
        );
        p.round(
            track.left,
            mid - track_h / 2,
            knob_x.max(track.left + track_h),
            mid + track_h / 2,
            track_h,
            c.accent,
            c.accent,
        );
        p.circle(knob_x, mid, knob_r, KNOB, mix_white(c.border, 0.1));
        p.circle(knob_x, mid, knob_r - p.s(3.0), KNOB, KNOB);

        // 数值（固定在右侧，不跟着圆钮跑，避免文字框被挤成负数）
        p.text(
            &format!("{} px", state.width),
            state.fonts.value,
            c.text2,
            content_right - p.s(80.0),
            mid - p.s(10.0),
            content_right,
            mid + p.s(10.0),
            DT_RIGHT,
        );
    }

    // 四个下拉（字号 / 指示条 / 网卡 / 刷新）
    for (index, slot) in l.selects.iter().enumerate() {
        let rect = *slot;
        let row = index + 1;
        let hovered = state.hover == Some(Hit::Select(row));
        let open = state.popup != std::ptr::null_mut();
        let fill = if hovered || open {
            c.chip_hover
        } else {
            c.chip
        };
        let border = if open { c.accent } else { fill };
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
            c.text,
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
        p.line(ax, ay, ax + step, ay + step, c.text2, p.s(1.4));
        p.line(ax + step, ay + step, ax + step * 2, ay, c.text2, p.s(1.4));
    }

    // ---------------------------------------------------------------- AI 统计
    p.fill(
        0,
        p.s(AI_HEAD_TOP),
        client.right,
        p.s(AI_HEAD_TOP) + 1,
        c.border,
    );
    p.text(
        "AI 统计",
        state.fonts.small,
        c.text3,
        pad + p.s(4.0),
        p.s(AI_TITLE_TOP),
        content_right,
        p.s(AI_TITLE_TOP + 20.0),
        DT_LEFT,
    );

    let toggle = l.ai_toggle;
    let mid = (toggle.top + toggle.bottom) / 2;
    if state.hover == Some(Hit::AiToggle) {
        p.round(
            toggle.left,
            toggle.top + p.s(1.0),
            content_right,
            toggle.bottom - p.s(1.0),
            p.s(8.0),
            c.chip,
            c.chip,
        );
    }
    p.text(
        "启用 AI 统计",
        state.fonts.label,
        c.text,
        pad + p.s(4.0),
        toggle.top,
        toggle.right - p.s(70.0),
        toggle.top + p.s(22.0),
        DT_LEFT,
    );
    p.text(
        if state.ai_enabled {
            "常驻采集：停在别的视图也在累计"
        } else {
            "关闭时不起线程、不读任何会话文件"
        },
        state.fonts.small,
        c.text3,
        pad + p.s(4.0),
        mid,
        toggle.right - p.s(70.0),
        toggle.bottom,
        DT_LEFT,
    );

    // 开关本体
    let sw_w = p.s(40.0);
    let sw_h = p.s(22.0);
    let sx = content_right - sw_w;
    let sy = mid - sw_h / 2;
    if state.ai_enabled {
        p.round(sx, sy, sx + sw_w, sy + sw_h, sw_h, c.accent, c.accent);
        p.circle(
            sx + sw_w - sw_h / 2,
            mid,
            sw_h / 2 - p.s(3.0),
            KNOB,
            KNOB,
        );
    } else {
        p.round(
            sx,
            sy,
            sx + sw_w,
            sy + sw_h,
            sw_h,
            c.chip,
            mix_white(c.border, 0.05),
        );
        p.circle(
            sx + sw_h / 2,
            mid,
            sw_h / 2 - p.s(3.0),
            KNOB,
            mix_white(c.border, 0.05),
        );
    }

    // 六个来源：排成两列三行（一列六行会顶出窗口）
    for (index, slot) in l.ai_sources.iter().enumerate() {
        let rect = *slot;
        let right = rect.right;
        let on = state.ai_sources[index];
        if state.ai_enabled && state.hover == Some(Hit::AiSource(index)) {
            p.round(
                rect.left,
                rect.top + p.s(1.0),
                right,
                rect.bottom - p.s(1.0),
                p.s(7.0),
                c.chip,
                c.chip,
            );
        }
        p.text(
            AI_SOURCE_LABELS[index],
            state.fonts.value,
            if state.ai_enabled { c.text } else { c.text3 },
            rect.left + p.s(16.0),
            rect.top,
            right - p.s(30.0),
            rect.bottom,
            DT_LEFT,
        );

        let size = p.s(16.0);
        let bx = right - size - p.s(6.0);
        let by = (rect.top + rect.bottom) / 2 - size / 2;
        let (fill, border) = if !state.ai_enabled {
            (c.surface, mix_white(c.border, 0.25))
        } else if on {
            (c.accent, c.accent)
        } else {
            (c.surface, mix_white(c.border, 0.05))
        };
        p.round(bx, by, bx + size, by + size, p.s(4.0), fill, border);
        if on && state.ai_enabled {
            p.line(
                bx + p.s(4.0),
                by + p.s(8.0),
                bx + p.s(7.0),
                by + p.s(11.0),
                KNOB,
                p.s(1.6),
            );
            p.line(
                bx + p.s(7.0),
                by + p.s(11.0),
                bx + p.s(12.0),
                by + p.s(5.0),
                KNOB,
                p.s(1.6),
            );
        }
    }

    // 键盘焦点框：内容区的控件画在滚动坐标系里（还在裁剪区内）
    if state.keyboard {
        if let Some(hit) = state.focus {
            if !matches!(hit, Hit::Done | Hit::AutoStart) {
                if let Some(rect) = focus_rect(state, client, hit) {
                    draw_focus(&p, rect);
                }
            }
        }
    }

    // 内容区结束：恢复裁剪和原点，页脚固定在底部
    if saved != 0 {
        RestoreDC(dc, saved);
    }

    // 页脚控件的焦点框画在正常坐标系里
    if state.keyboard {
        if let Some(hit @ (Hit::Done | Hit::AutoStart)) = state.focus {
            if let Some(rect) = focus_rect(state, client, hit) {
                draw_focus(&p, rect);
            }
        }
    }

    // 滚动条（只有在内容放不下时才出现）
    if max > 0 {
        let bar = scroll_bar(state, client);
        let track_w = bar.track_w;
        p.round(
            bar.x,
            bar.track_top,
            bar.x + track_w,
            bar.track_bottom,
            track_w,
            mix_white(c.border, 0.35),
            mix_white(c.border, 0.35),
        );

        // 拖动中把滑块画深一点，和悬停的其他控件一个反馈
        let active = if state.scroll_drag || state.hover == Some(Hit::Scrollbar) {
            c.text2
        } else {
            c.text3
        };
        let thumb_top = bar.thumb_top(scroll, max);
        p.round(
            bar.x,
            thumb_top,
            bar.x + track_w,
            thumb_top + bar.thumb_h,
            track_w,
            active,
            active,
        );
    }

    // 底部分割线
    let foot_top = client.bottom - p.s(64.0);    p.fill(0, foot_top, client.right, foot_top + 1, c.border);

    let done_hover = state.hover == Some(Hit::Done);
    let done_fill = if done_hover {
        c.accent_pressed
    } else {
        c.accent
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

    // 页脚左边：开机自启（放在这里就不用让内容区再长一行、也不会多出滚动条）
    {
        let hovered = state.hover == Some(Hit::AutoStart);
        let sw_w = p.s(38.0);
        let sw_h = p.s(22.0);
        let area = l.auto_start;
        let sx = area.right - sw_w;
        let mid = (area.top + area.bottom) / 2;
        let sy = mid - sw_h / 2;

        p.text(
            "开机自启",
            state.fonts.value,
            if hovered { c.text } else { c.text2 },
            area.left,
            area.top,
            sx - p.s(10.0),
            area.bottom,
            DT_LEFT,
        );

        if state.auto_start {
            p.round(sx, sy, sx + sw_w, sy + sw_h, sw_h, c.accent, c.accent);
            p.circle(
                sx + sw_w - sw_h / 2,
                mid,
                sw_h / 2 - p.s(3.0),
                KNOB,
                KNOB,
            );
        } else {
            p.round(
                sx,
                sy,
                sx + sw_w,
                sy + sw_h,
                sw_h,
                c.chip,
                mix_white(c.border, 0.05),
            );
            p.circle(
                sx + sw_h / 2,
                mid,
                sw_h / 2 - p.s(3.0),
                KNOB,
                KNOB,
            );
        }
    }

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
    let c = crate::theme::ui();
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

    p.fill(0, 0, client.right, client.bottom, c.surface);
    p.round(
        0,
        0,
        client.right,
        client.bottom,
        p.s(10.0),
        c.surface,
        c.border,
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
                c.chip,
                c.chip,
            );
        }

        p.text(
            item,
            if selected { f_selected } else { f },
            if selected { c.text } else { c.text },
            pad + p.s(10.0),
            top,
            client.right - p.s(34.0),
            bottom,
            DT_LEFT,
        );

        if selected {
            let ax = client.right - p.s(24.0);
            let ay = (top + bottom) / 2;
            p.line(ax, ay, ax + p.s(4.0), ay + p.s(4.0), c.accent, p.s(1.8));
            p.line(
                ax + p.s(4.0),
                ay + p.s(4.0),
                ax + p.s(11.0),
                ay - p.s(5.0),
                c.accent,
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
    let mut anchor = l.selects[row - 1];
    // 下拉挂在控件下方：控件坐标是「未滚动」的，这里换成窗口坐标
    anchor.top -= state.scroll;
    anchor.bottom -= state.scroll;

    let items: Vec<String> = match row {
        1 => FONT_OPTIONS.iter().map(|(label, _)| label.to_string()).collect(),
        2 => GAUGE_OPTIONS.iter().map(|(label, _)| label.to_string()).collect(),
        3 => {
            let mut list = vec!["自动（全部合计）".to_string()];
            list.extend(state.interfaces.iter().cloned());
            list
        }
        _ => REFRESH_OPTIONS.iter().map(|(label, _)| label.to_string()).collect(),
    };
    let selected = match row {
        1 => state.font,
        2 => state.gauge,
        3 => state.interface,
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

pub fn open(parent: HWND, settings: &Settings) {
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
            hIcon: crate::tray::shared_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // 窗口开在鼠标那块屏上：缩放按那块屏算（多屏缩放不同也对）
        let scale = crate::cursor_scale();
        let width_px = s(440.0, scale);
        let mut height_px = s(710.0, scale);

        let screen = crate::work_area();

        // 小屏 / 高缩放（比如 1366×768 @125%）下 710px 会顶出工作区，
        // 压到工作区里，内容靠滚轮看
        let available = ((screen.bottom - screen.top) - s(48.0, scale)).max(s(360.0, scale));
        if height_px > available {
            height_px = available;
        }

        let left = screen.left + ((screen.right - screen.left) - width_px).max(0) / 2;
        let top = (screen.top + ((screen.bottom - screen.top) - height_px) / 3).max(screen.top);

        let interfaces: Vec<String> = netspeed_core::net::adapters()
            .into_iter()
            .map(|adapter| adapter.alias)
            .collect();

        let gauge = GAUGE_OPTIONS
            .iter()
            .position(|(_, value)| value.eq_ignore_ascii_case(&settings.gauge_source))
            .unwrap_or(0);
        let interface = if settings.network_interface.trim().is_empty()
            || settings.network_interface.eq_ignore_ascii_case("auto")
        {
            0
        } else {
            interfaces
                .iter()
                .position(|alias| alias.eq_ignore_ascii_case(&settings.network_interface))
                .map(|index| index + 1)
                .unwrap_or(0)
        };
        let refresh = REFRESH_OPTIONS
            .iter()
            .position(|(_, ms)| *ms == settings.refresh_ms)
            .unwrap_or(0);
        let font = FONT_OPTIONS
            .iter()
            .position(|(_, percent)| *percent == settings.font_scale)
            .unwrap_or(1);

        let state = Box::into_raw(Box::new(State {
            parent,
            interfaces,
            width: settings.panel_width.clamp(72, 360),
            gauge,
            interface,
            refresh,
            font,
            ai_enabled: settings.ai_enabled,
            ai_sources: [
                settings.ai_opencode,
                settings.ai_dsh,
                settings.ai_workbuddy,
                settings.ai_workbuddy_ai,
                settings.ai_claude,
                settings.ai_codex,
            ],
            auto_start: settings.auto_start,
            fonts: Fonts::create(scale),
            scale,
            hover: None,
            // 打开时焦点在第一项（字号），按 Tab 就能一路走过去
            focus: Some(Hit::Select(1)),
            keyboard: false,
            slider_drag: false,
            scroll_drag: false,
            scroll: 0,
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
    if contains(&l.auto_start, point) {
        return Some(Hit::AutoStart);
    }

    // 滚动条比内容区窄，得先判：否则会被下面「内容区偏移」的逻辑吃掉
    if max_scroll(state, client) > 0 {
        let bar = scroll_bar(state, client);
        if contains(&bar.hot(state.scale), point) {
            return Some(Hit::Scrollbar);
        }
    }

    // 滚出可见区的控件既不该响应悬停也不该响应点击
    let (top, bottom) = content_viewport(state, client);
    if point.y < top || point.y >= bottom {
        return None;
    }
    let point = POINT {
        x: point.x,
        y: point.y + state.scroll,
    };

    if contains(&l.slider, point) {
        return Some(Hit::Slider);
    }
    for (index, rect) in l.selects.iter().enumerate() {
        if contains(rect, point) {
            return Some(Hit::Select(index + 1));
        }
    }
    if contains(&l.ai_toggle, point) {
        return Some(Hit::AiToggle);
    }
    for (index, rect) in l.ai_sources.iter().enumerate() {
        if contains(rect, point) {
            return Some(Hit::AiSource(index));
        }
    }
    None
}

/// 键盘能走到的控件（顺序 = Tab 顺序）。AI 关着时那几行不参与。
fn nav_items(state: &State) -> Vec<Hit> {
    let mut items = vec![
        Hit::Select(1),
        Hit::Slider,
        Hit::Select(2),
        Hit::Select(3),
        Hit::Select(4),
    ];
    if state.ai_enabled {
        items.push(Hit::AiToggle);
        for index in 0..4 {
            items.push(Hit::AiSource(index));
        }
    }
    items.push(Hit::AutoStart);
    items.push(Hit::Done);
    items
}

/// 某个控件的矩形（焦点框用）
fn focus_rect(state: &State, client: RECT, hit: Hit) -> Option<RECT> {
    let l = layout(state, client);
    Some(match hit {
        Hit::Slider => l.slider,
        Hit::Select(row) => *l.selects.get(row.checked_sub(1)?)?,
        Hit::AiToggle => l.ai_toggle,
        Hit::AiSource(index) => *l.ai_sources.get(index)?,
        Hit::AutoStart => l.auto_start,
        Hit::Done => l.done,
        _ => return None,
    })
}

/// 下拉选中某一项：鼠标点选和键盘改值共用
fn choose(state: &mut State, row: usize, index: usize) {
    match row {
        1 => {
            if let Some((_, percent)) = FONT_OPTIONS.get(index) {
                state.font = index;
                crate::apply_font_scale(state.parent, *percent);
            }
        }
        2 => {
            if let Some((_, value)) = GAUGE_OPTIONS.get(index) {
                state.gauge = index;
                crate::apply_gauge(state.parent, value);
            }
        }
        3 => {
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
}

/// 键盘左右/上下改值：在当前项附近循环
fn shift_choice(state: &mut State, row: usize, delta: i32) {
    let (current, len) = match row {
        1 => (state.font, FONT_OPTIONS.len()),
        2 => (state.gauge, GAUGE_OPTIONS.len()),
        3 => (state.interface, state.interfaces.len() + 1),
        _ => (state.refresh, REFRESH_OPTIONS.len()),
    };
    let len = len.max(1) as i32;
    let next = (current as i32 + delta).rem_euclid(len) as usize;
    choose(state, row, next);
}

fn toggle_ai(state: &mut State) {
    let enabled = !state.ai_enabled;
    // 打开总开关时如果来源全关着，默认全开，否则什么也采不到
    if enabled && !state.ai_sources.iter().any(|flag| *flag) {
        state.ai_sources = [true; 6];
        crate::apply_ai_sources(state.parent, state.ai_sources);
    }
    state.ai_enabled = enabled;
    crate::apply_ai_enabled(state.parent, enabled);
}

fn toggle_source(state: &mut State, index: usize) {
    if !state.ai_enabled {
        return;
    }
    state.ai_sources[index] = !state.ai_sources[index];
    crate::apply_ai_sources(state.parent, state.ai_sources);
}

fn toggle_auto_start(state: &mut State) {
    state.auto_start = !state.auto_start;
    crate::apply_auto_start(state.parent, state.auto_start);
}

/// 键盘焦点框：比控件大 3px 的空心圆角框，不挡住里面的内容
fn draw_focus(p: &Painter, rect: RECT) {
    let pad = p.s(3.0);
    unsafe {
        p.ring(
            rect.left - pad,
            rect.top - pad,
            rect.right + pad,
            rect.bottom + pad,
            p.s(10.0),
            crate::theme::ui().accent,
            p.s(2.0),
        );
    }
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
        // 系统切了明暗主题：整套颜色会变，重画一次就够了（内部按 2 秒缓存读注册表）
        WM_SETTINGCHANGE => {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
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

            if state.scroll_drag {
                let max = max_scroll(state, client);
                let bar = scroll_bar(state, client);
                let next = bar.scroll_at(point.y, max);
                if next != state.scroll {
                    state.scroll = next;
                    state.hover = None;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
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

            // 鼠标点谁，焦点就落在谁身上（焦点框只在键盘操作时显示）
            let hit = hit_test(state, client, point);
            if let Some(hit) = hit {
                state.focus = Some(hit);
                state.keyboard = false;
            }

            match hit {
                Some(Hit::Close) | Some(Hit::Done) => {
                    DestroyWindow(hwnd);
                }
                Some(Hit::Slider) => {
                    state.slider_drag = true;
                    SetCapture(hwnd);
                    apply_width(hwnd, state, point.x, client);
                }
                Some(Hit::Scrollbar) => {
                    // 按下就跳到鼠标位置（和系统滚动条一样），然后跟着拖
                    let max = max_scroll(state, client);
                    let bar = scroll_bar(state, client);
                    state.scroll_drag = true;
                    SetCapture(hwnd);
                    state.scroll = bar.scroll_at(point.y, max);
                    state.hover = Some(Hit::Scrollbar);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::Select(row)) => {
                    open_popup(hwnd, row);
                }
                Some(Hit::AiToggle) => {
                    toggle_ai(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::AiSource(index)) => {
                    toggle_source(state, index);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::AutoStart) => {
                    toggle_auto_start(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
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
                (*state).scroll_drag = false;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSEWHEEL => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            let mut client = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetClientRect(hwnd, &mut client);

            let max = max_scroll(state, client);
            if max == 0 {
                return 0;
            }

            // 一格滚轮 120，一次滚 3 行（约 66px）
            let delta = ((wparam >> 16) as i16) as i32;
            let step = s(66.0, state.scale).max(1);
            let next = (state.scroll - delta / 120 * step).clamp(0, max);
            if next != state.scroll {
                state.scroll = next;
                state.hover = None;
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

            choose(state, row, index);

            state.popup = std::ptr::null_mut();
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        // 设置窗可以拖到另一块屏：缩放跟着那块屏走（字体/尺寸/布局全部重算）
        WM_DPICHANGED => {
            let dpi = ((wparam >> 16) & 0xFFFF) as u32;
            let scale = (dpi.max(96) as f32) / 96.0;
            let state = state_of(hwnd);
            if !state.is_null() {
                (*state).scale = scale;
                (*state).fonts = Fonts::create(scale);
                (*state).scroll = 0;

                let width = s(440.0, scale);
                let mut height = s(710.0, scale);
                let suggested = lparam as *const RECT;
                let (left, top) = if suggested.is_null() {
                    let mut rect = RECT {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    };
                    GetWindowRect(hwnd, &mut rect);
                    (rect.left, rect.top)
                } else {
                    ((*suggested).left, (*suggested).top)
                };

                let work = crate::work_area_at(POINT { x: left, y: top });
                let available = ((work.bottom - work.top) - s(48.0, scale)).max(s(360.0, scale));
                height = height.min(available);

                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    left,
                    top,
                    width,
                    height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_KEYDOWN => {
            crate::debug_log(&format!("key {wparam}"));
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            if wparam as u32 == 0x1B {
                if !state.popup.is_null() {
                    DestroyWindow(state.popup);
                } else {
                    DestroyWindow(hwnd);
                }
                return 0;
            }

            // Tab / Shift+Tab：在控件之间走一圈
            if wparam as u32 == 0x09 {
                let shift = unsafe { GetKeyState(0x10) } < 0;
                let items = nav_items(state);
                if items.is_empty() {
                    return 0;
                }
                let current = state
                    .focus
                    .and_then(|hit| items.iter().position(|item| *item == hit));
                let next = match (current, shift) {
                    (Some(index), true) => (index + items.len() - 1) % items.len(),
                    (Some(index), false) => (index + 1) % items.len(),
                    (None, _) => 0,
                };
                state.focus = Some(items[next]);
                state.keyboard = true;
                InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }

            let Some(hit) = state.focus else {
                return 0;
            };
            state.keyboard = true;

            match wparam as u32 {
                // ↑↓←→：下拉改值、滑杆调宽度
                0x26 | 0x25 | 0x28 | 0x27 => {
                    let delta = if (wparam as u32) == 0x26 || (wparam as u32) == 0x25 {
                        -1
                    } else {
                        1
                    };
                    match hit {
                        Hit::Select(row) => shift_choice(state, row, delta),
                        Hit::Slider => {
                            let width = (state.width + delta * 4).clamp(72, 360);
                            if width != state.width {
                                state.width = width;
                                crate::apply_panel_width(state.parent, width);
                            }
                        }
                        _ => {}
                    }
                }
                // 空格 / 回车：开关、按钮
                0x20 | 0x0D => match hit {
                    Hit::AiToggle => toggle_ai(state),
                    Hit::AiSource(index) => toggle_source(state, index),
                    Hit::AutoStart => toggle_auto_start(state),
                    Hit::Done => {
                        DestroyWindow(hwnd);
                        return 0;
                    }
                    _ => {}
                },
                _ => return 0,
            }

            InvalidateRect(hwnd, std::ptr::null(), 0);
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
