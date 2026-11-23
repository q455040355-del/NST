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
    BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::{ui_face, wide};
use netspeed_core::settings::{
    format_hotkey, parse_hotkey, Settings, HOTKEY_MOD_ALT, HOTKEY_MOD_CONTROL, HOTKEY_MOD_SHIFT,
    HOTKEY_MOD_WIN,
};

const CLASS_NAME: &str = "NetSpeedTaskbarRs.Settings";
const POPUP_CLASS: &str = "NetSpeedTaskbarRs.Settings.Popup";

/// 下拉里选完，子窗口用这个消息告诉父窗口
const WM_APP_PICK: u32 = WM_APP + 0x31;
/// 动效的一帧（后台线程投递）
const WM_APP_ANIM: u32 = WM_APP + 0x36;

const DT_NOPREFIX: u32 = 0x0800;


#[allow(dead_code)]
const ID_WIDTH_LABEL: usize = 2001;
#[allow(dead_code)]
const ID_GAUGE_LABEL: usize = 2004;
#[allow(dead_code)]
const ID_INTERFACE_LABEL: usize = 2007;
#[allow(dead_code)]
const ID_REFRESH_LABEL: usize = 2009;
#[allow(dead_code)]
const ID_FONT_LABEL: usize = 2011;

/// 动画里的开关顺序：AI 统计 / 截图 / 取字 / 录 MP4 / 翻译 / 开机自启
const SW_AI: usize = 0;
const SW_SHOT: usize = 1;
const SW_OCR: usize = 2;
const SW_GIF: usize = 3;
const SW_TRANSLATE: usize = 4;
const SW_AUTOSTART: usize = 5;

static ANIM_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 上一帧还没被画完（消息队列里只允许排一帧，排多了会几帧挤在一起跳）
static ANIM_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ANIM_THREAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

// 两列：左列「显示 + AI 统计」，右列「截图与取字 + 翻译」。
// 列宽 = (窗口宽 - 左右留白 - 列间距) / 2；标签列固定 100 宽，控件从列左缘 +100 起。
const COL_GUTTER: f32 = 28.0;
const COL_LABEL_W: f32 = 100.0;

// 左列：AI 统计（5 个基础行下面）
const AI_TITLE_TOP: f32 = 410.0; // 「AI 统计」小标题
const AI_TOGGLE_TOP: f32 = 434.0; // 总开关行
const AI_SOURCES_TOP: f32 = 482.0; // 来源格子起点（两列三行）
const AI_ROW_H: f32 = 42.0;
/// 来源格子的行高（两列三行，比整行矮一点，正好铺满）
const AI_SOURCE_ROW_H: f32 = 34.0;
const AI_SOURCE_LABELS: [&str; 6] = [
    "OpenCode",
    "dsh",
    "WorkBuddy",
    "WorkBuddy AI",
    "Claude Code",
    "Codex",
];

// 右列：截图 / 取字（右列顶部，上面就是页头，不再画分隔线）
const SHOT_TITLE_TOP: f32 = 84.0; // 「截图与取字」小标题
const SHOT_FIRST_TOP: f32 = 106.0; // 截图行
const SHOT_OCR_TOP: f32 = 152.0; // 取字行
const SHOT_GIF_TOP: f32 = 198.0; // 录 MP4 行
const SHOT_REC_TOP: f32 = 244.0; // 录制参数行（画质 / 时长 / 帧率）
const SHOT_ROW_H: f32 = 46.0;

// 右列：翻译（截图 / 取字下面）—— 表单式：标签在控件上方，控件铺满整列
const TR_TITLE_TOP: f32 = 312.0; // 「翻译」小标题
const TR_ROW1_TOP: f32 = 352.0;
const TR_ROW2_TOP: f32 = 406.0;
const TR_ROW3_TOP: f32 = 460.0;
const TR_HINT_TOP: f32 = 492.0; // API Key 下面那行说明
const TR_ROW4_TOP: f32 = 530.0;
/// 表单输入框的高度（比老版矮一点，一行行排下来正好铺满）
const TR_BOX_H: f32 = 30.0;

/// 可滚动内容区：分隔线往下到页脚分割线往上
const CONTENT_TOP: f32 = 65.0;
/// 内容总高度（最后一行 + 一点留白）；窗口比这个矮就出现滚动
const CONTENT_BOTTOM: f32 = 600.0;
/// 窗口尺寸（逻辑像素）：两列铺开，1080p 上不用滚动就能看全
const WINDOW_W: f32 = 760.0;
const WINDOW_H: f32 = 676.0;
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
    ShotToggle,
    OcrToggle,
    ShotHotkey,
    OcrHotkey,
    GifToggle,
    GifFps,
    GifQuality,
    GifSeconds,
    GifHotkey,
    Scrollbar,
    AutoStart,
    Update,
    /// 「翻译」区块右上角的总开关
    TrToggle,
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
            ui_face().as_ptr(),
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
    /// GDI+ 画布：圆角 / 圆 / 斜线走它才有抗锯齿（建不出来就退回 GDI 硬边）
    aa: Option<crate::aa::Canvas>,
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
        if let Some(aa) = &self.aa {
            aa.round(left, top, right, bottom, radius, fill, border);
            return;
        }
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
        if let Some(aa) = &self.aa {
            aa.ring(left, top, right, bottom, radius, color, width);
            return;
        }
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
        if let Some(aa) = &self.aa {
            aa.line(x1, y1, x2, y2, color, width);
            return;
        }
        let pen = CreatePen(0, width.max(1), color);
        let old = SelectObject(self.dc, pen as HGDIOBJ);
        MoveToEx(self.dc, x1, y1, std::ptr::null_mut());
        LineTo(self.dc, x2, y2);
        SelectObject(self.dc, old);
        DeleteObject(pen as *mut _);
    }

    unsafe fn circle(&self, cx: i32, cy: i32, radius: i32, fill: u32, border: u32) {
        if let Some(aa) = &self.aa {
            aa.circle(cx, cy, radius, fill, border);
            return;
        }
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
    shot_enabled: bool,
    ocr_enabled: bool,
    gif_enabled: bool,
    gif_fps: i32,
    gif_quality: i32,
    gif_seconds: i32,
    shot_hotkey: String,
    ocr_hotkey: String,
    gif_hotkey: String,
    /// 翻译总开关（和 AI 统计一样：关着的时候整块收起来）
    translate_enabled: bool,
    /// 翻译：服务商 0 = ChatGPT，1 = DeepSeek
    tr_provider: usize,
    tr_key: String,
    tr_model: String,
    tr_base: String,
    /// 正在编辑哪个输入框（None = 没在编辑）
    tr_edit: Option<TrField>,
    /// API Key 点了「显示」：不打码
    tr_show_key: bool,
    /// 光标位置（按 char 计）
    tr_caret: usize,
    /// 正在录制快捷键：0 = 没在录，1 = 截图，2 = 取字，3 = 录 GIF
    capture: usize,
    fonts: Fonts,
    scale: f32,
    hover: Option<Hit>,
    /// 键盘焦点（Tab 走一圈）；鼠标一动就清掉，避免两个「当前项」打架
    focus: Option<Hit>,
    /// 最近一次操作是键盘：只有键盘操作才画焦点框
    keyboard: bool,
    slider_drag: bool,
    /// 常驻双缓冲：拖动滑杆时每秒要重画几十次，每帧新建 DIB 会明显卡（660×1308 约 3.4 MB）
    back: Option<(HDC, HBITMAP)>,
    back_size: (i32, i32),
    /// 正在拖滚动条滑块
    scroll_drag: bool,
    /// 内容区滚动位置（像素，0 = 顶部）
    scroll: i32,
    /// 自己这个窗口（动效线程要往里投消息）
    hwnd: HWND,
    /// 开关滑块位置（0 = 关，1 = 开）；按下标是 SW_* 那一套
    sw: [f32; 6],
    /// 翻译区块的「展开度」：0 = 收起，1 = 完全展开
    tr_open: f32,
    /// AI 来源那六格的手风琴展开度
    ai_open: f32,
    /// 上一帧动效的时间
    anim_at: Option<std::time::Instant>,
    popup: HWND,
    /// 弹出的下拉是第几行（0 = 没开）——只高亮那一行，别让四个下拉一起变灰
    popup_row: usize,
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
    shot_toggle: RECT,
    ocr_toggle: RECT,
    gif_toggle: RECT,
    /// 「录制」行三颗胶囊：画质 / 时长 / 帧率
    gif_quality: RECT,
    gif_seconds: RECT,
    gif_fps: RECT,
    shot_hotkey: RECT,
    ocr_hotkey: RECT,
    gif_hotkey: RECT,
    /// 页脚左边的「开机自启」整行热区
    auto_start: RECT,
    /// 页脚「检查更新」按钮
    update: RECT,
    /// 翻译区块：两颗服务商胶囊 + 三个输入框
    tr_chatgpt: RECT,
    tr_deepseek: RECT,
    tr_key: RECT,
    tr_model: RECT,
    tr_base: RECT,
    /// API Key 输入框右边的「显示 / 隐藏」热区
    tr_reveal: RECT,
    /// 「翻译」小标题右边那颗总开关
    tr_toggle: RECT,
}

/// 翻译区块里可编辑的输入框
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrField {
    Key,
    Model,
    Base,
}

/// 翻译输入框的几何：一行高 30，宽度铺满右列
fn input_rect(top: i32, left: i32, right: i32, height: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom: top + height,
    }
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

    // 两列：左列 pad .. left_right，右列 right_left .. right
    let col_gutter = s(COL_GUTTER, scale);
    let col_w = ((right - pad - col_gutter) / 2).max(s(200.0, scale));
    let left_right = pad + col_w;
    let right_left = left_right + col_gutter;
    // 标签列固定宽：控件从列左缘往右 100 起，两列的控制区一样宽
    let control_left = pad + s(COL_LABEL_W, scale);
    let right_control = right_left + s(COL_LABEL_W, scale);

    let row_center = |index: usize| -> i32 {
        s(ROW_FIRST + ROW_STEP * index as f32, scale) + s(ROW_STEP * 0.5 - 6.0, scale)
    };

    // 第 0 行是字号（下拉），滑杆停在原来的位置（第 1 行）
    let slider = RECT {
        left: control_left,
        top: row_center(1) - s(14.0, scale),
        right: left_right - s(88.0, scale),
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
            right: left_right,
            bottom: center + height / 2,
        };
    }

    let row_h = s(SHOT_ROW_H, scale);
    let shot_top = s(SHOT_FIRST_TOP, scale);
    let ocr_top = s(SHOT_OCR_TOP, scale);
    let gif_top = s(SHOT_GIF_TOP, scale);
    let switcher_w = s(40.0, scale);
    let hotkey_w = s(116.0, scale);
    let hotkey_h = s(26.0, scale);
    let hotkey_right = right - switcher_w - s(10.0, scale);
    let hotkey_left = hotkey_right - hotkey_w;
    // 「录制」行的三颗胶囊（画质 / 时长 / 帧率），单独占一行，右缘和开关那一列对齐
    let rec_top = s(SHOT_REC_TOP, scale);
    let pill_w = s(74.0, scale);
    let pill_gap = s(8.0, scale);
    let pill_h = s(26.0, scale);
    let pill_x = right - (pill_w * 3 + pill_gap * 2);
    let pill_y = rec_top + (row_h - pill_h) / 2;
    let pill = |index: i32| RECT {
        left: pill_x + index * (pill_w + pill_gap),
        right: pill_x + index * (pill_w + pill_gap) + pill_w,
        top: pill_y,
        bottom: pill_y + pill_h,
    };
    let (quality_pill, seconds_pill, fps_pill) = (pill(0), pill(1), pill(2));

    Layout {
        close,
        done,
        slider,
        selects,
        ai_toggle: RECT {
            left: pad + s(4.0, scale),
            top: s(AI_TOGGLE_TOP, scale),
            right: left_right,
            bottom: s(AI_TOGGLE_TOP + AI_ROW_H, scale),
        },
        ai_sources: ai_source_rects(scale, pad + s(4.0, scale), left_right),
        shot_toggle: RECT {
            left: right_left,
            top: shot_top,
            right,
            bottom: shot_top + row_h,
        },
        ocr_toggle: RECT {
            left: right_left,
            top: ocr_top,
            right,
            bottom: ocr_top + row_h,
        },
        shot_hotkey: RECT {
            left: hotkey_left,
            top: shot_top + (row_h - hotkey_h) / 2,
            right: hotkey_right,
            bottom: shot_top + (row_h - hotkey_h) / 2 + hotkey_h,
        },
        ocr_hotkey: RECT {
            left: hotkey_left,
            top: ocr_top + (row_h - hotkey_h) / 2,
            right: hotkey_right,
            bottom: ocr_top + (row_h - hotkey_h) / 2 + hotkey_h,
        },
        gif_toggle: RECT {
            left: right_left,
            top: gif_top,
            right,
            bottom: gif_top + row_h,
        },
        gif_fps: fps_pill,
        gif_quality: quality_pill,
        gif_seconds: seconds_pill,
        gif_hotkey: RECT {
            left: hotkey_left,
            top: gif_top + (row_h - hotkey_h) / 2,
            right: hotkey_right,
            bottom: gif_top + (row_h - hotkey_h) / 2 + hotkey_h,
        },
        // 页脚左边：标签 + 开关都在这个热区里
        auto_start: RECT {
            left: pad + s(4.0, scale),
            top: done.top,
            right: (pad + s(4.0, scale) + s(170.0, scale)).min(done.left - s(12.0, scale)),
            bottom: done.bottom,
        },
        // 翻译是表单：标签在控件上方，控件铺满整列
        tr_chatgpt: RECT {
            left: right_control,
            top: s(TR_ROW1_TOP, scale),
            right: right_control + (right - right_control - s(6.0, scale)) / 2,
            bottom: s(TR_ROW1_TOP + TR_BOX_H, scale),
        },
        tr_deepseek: RECT {
            left: right_control + (right - right_control - s(6.0, scale)) / 2 + s(6.0, scale),
            top: s(TR_ROW1_TOP, scale),
            right,
            bottom: s(TR_ROW1_TOP + TR_BOX_H, scale),
        },
        tr_model: input_rect(
            s(TR_ROW2_TOP, scale),
            right_control,
            right,
            s(TR_BOX_H, scale),
        ),
        tr_key: input_rect(
            s(TR_ROW3_TOP, scale),
            right_control,
            right,
            s(TR_BOX_H, scale),
        ),
        tr_base: input_rect(
            s(TR_ROW4_TOP, scale),
            right_control,
            right,
            s(TR_BOX_H, scale),
        ),
        // 「翻译」标题右边那颗开关：关掉以后整块像手风琴一样收起来
        tr_toggle: RECT {
            left: right - s(34.0, scale),
            top: s(TR_TITLE_TOP + 1.5, scale),
            right,
            bottom: s(TR_TITLE_TOP + 1.5 + 18.0, scale),
        },
        // API Key 输入框右边的「显示 / 隐藏」
        tr_reveal: RECT {
            left: right - s(48.0, scale),
            top: s(TR_ROW3_TOP + 2.0, scale),
            right: right - s(8.0, scale),
            bottom: s(TR_ROW3_TOP + TR_BOX_H - 2.0, scale),
        },
        // 页脚右侧：检查更新，紧挨着「完成」
        update: RECT {
            left: done.left - s(12.0, scale) - s(104.0, scale),
            top: done.top,
            right: done.left - s(12.0, scale),
            bottom: done.bottom,
        },
    }
}

fn ai_source_rects(scale: f32, left: i32, right: i32) -> [RECT; 6] {
    // 两列三行：一列六行会把这一列顶得比左边高
    let gutter = s(12.0, scale);
    let column = ((right - left - gutter) / 2).max(s(80.0, scale));
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

/// 每一行的标题。加/改设置先动这张表，绘制按行号查它。
const ROW_TITLES: [&str; ROW_COUNT] = ["字号", "面板宽度", "底部指示条", "网卡", "刷新间隔"];

/// 某一行的标题（按行号查表，越界返回空串而不是 panic）
fn row_title(row: usize) -> &'static str {
    ROW_TITLES.get(row).copied().unwrap_or("")
}


// ---------------------------------------------------------------- 绘画

unsafe fn paint(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
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
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    paint_into(hdc, &mut *state, client);
    EndPaint(hwnd, &mut ps);
}

/// 把界面画到任意 DC（窗口的 WM_PAINT、WM_PRINTCLIENT、离屏预览共用）
unsafe fn paint_into(hdc: HDC, state: &mut State, client: RECT) {
    let c = crate::theme::ui();

    // 尺寸没变就复用上一帧的缓冲；变了才重建
    let size = (client.right, client.bottom);
    if state.back.is_none() || state.back_size != size {
        if let Some((old_dc, old_dib)) = state.back.take() {
            DeleteObject(old_dib as *mut _);
            DeleteDC(old_dc);
        }
        let (dc, dib, _bits) = make_dib(size.0, size.1);
        if dc.is_null() || dib.is_null() {
            return;
        }
        state.back = Some((dc, dib));
        state.back_size = size;
    }
    let (dc, _dib) = match state.back {
        Some((dc, dib)) => (dc, dib),
        None => return,
    };

    let p = Painter {
        dc,
        scale: state.scale,
        aa: crate::aa::Canvas::new(dc),
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
    // 两列的边界（和 layout() 里的算法必须一致，否则点击和画面会错位）
    let col_gutter = p.s(COL_GUTTER);
    let col_w = ((content_right - pad - col_gutter) / 2).max(p.s(200.0));
    let left_right = pad + col_w;
    let right_left = left_right + col_gutter;
    let control_left = pad + p.s(COL_LABEL_W);

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
    // GDI+ 不认视口原点和 GDI 裁剪区，滚动那点位移和裁剪区得自己补上，
    // 否则滚起来以后「抗锯齿的那一半」会原地不动 / 画到页头上去
    if let Some(aa) = &p.aa {
        aa.translate(0, -scroll);
        // 可见带是窗口的 view_top..view_bottom，换算回「没滚动」的内容坐标
        aa.clip_rect(
            0,
            view_top + scroll,
            client.right,
            view_bottom + scroll,
        );
    }

    // 两个颜色按 t 混合（开关轨道、手风琴淡入淡出都用它）
    let mix = |a: u32, b: u32, t: f32| -> u32 {
        let t = t.clamp(0.0, 1.0);
        let ch = |shift: u32| -> u32 {
            let av = ((a >> shift) & 0xFF) as f32;
            let bv = ((b >> shift) & 0xFF) as f32;
            ((av + (bv - av) * t).round() as u32) & 0xFF
        };
        ch(0) | (ch(8) << 8) | (ch(16) << 16)
    };

    // 开关：轨道颜色和滑块位置都跟着 t（0 = 关，1 = 开）走，动效就是在改 t
    let switch = |left: i32, top: i32, width: i32, height: i32, t: f32| {
        let t = t.clamp(0.0, 1.0);
        let track = mix(c.chip, c.accent, t);
        p.round(
            left,
            top,
            left + width,
            top + height,
            height,
            track,
            track,
        );
        let pad = p.s(3.0);
        let radius = (height / 2 - pad).max(p.s(2.0));
        let cx0 = left + height / 2;
        let cx1 = left + width - height / 2;
        let cx = cx0 + ((cx1 - cx0) as f32 * t).round() as i32;
        p.circle(cx, top + height / 2, radius, KNOB, KNOB);
    };

    // 区块标题：小号灰字 + 后面拖一条细线（方向 A 的分组语言）
    let section = |text: &str, top: i32, left: i32, rule_right: i32| {
        let width = crate::shot::text_width(p.dc, text, state.fonts.small);
        p.text(
            text,
            state.fonts.small,
            c.text3,
            left,
            top,
            rule_right,
            top + p.s(15.0),
            DT_LEFT,
        );
        let y = top + p.s(7.0);
        p.fill(
            left + width + p.s(10.0),
            y,
            rule_right,
            y + 1,
            mix_white(c.border, 0.25),
        );
    };

    // 行
    for row in 0..ROW_COUNT {
        let top = p.s(ROW_FIRST + ROW_STEP * row as f32);
        let bottom = p.s(ROW_FIRST + ROW_STEP * (row + 1) as f32);
        let center = (top + bottom) / 2 - p.s(6.0);

        let label = row_title(row);
        p.text(
            label,
            state.fonts.label,
            c.text,
            pad + p.s(4.0),
            center - p.s(11.0),
            control_left - p.s(12.0),
            center + p.s(11.0),
            DT_LEFT,
        );

        if row < ROW_COUNT - 1 {
            // 行间很淡的细线（只铺左列）
            p.fill(
                pad + p.s(4.0),
                bottom,
                left_right,
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

        // 数值（固定在列右侧，不跟着圆钮跑，避免文字框被挤成负数）
        p.text(
            &format!("{} px", state.width),
            state.fonts.value,
            c.text2,
            left_right - p.s(80.0),
            mid - p.s(10.0),
            left_right,
            mid + p.s(10.0),
            DT_RIGHT,
        );
    }

    // 四个下拉（字号 / 指示条 / 网卡 / 刷新）
    //
    // 方向 A 的语言：平时不套盒子，就是「值 + 箭头」右对齐；悬停 / 展开才给一层很淡的底。
    for (index, slot) in l.selects.iter().enumerate() {
        let rect = *slot;
        let row = index + 1;
        let hovered = state.hover == Some(Hit::Select(row));
        let open = state.popup != std::ptr::null_mut() && state.popup_row == row;
        if hovered || open {
            p.round(
                rect.left - p.s(8.0),
                rect.top,
                rect.right + p.s(4.0),
                rect.bottom,
                p.s(7.0),
                c.chip,
                c.chip,
            );
        }
        p.text(
            &text_of_row(state, row),
            state.fonts.value,
            c.text,
            rect.left,
            rect.top,
            rect.right - p.s(22.0),
            rect.bottom,
            DT_RIGHT,
        );

        // chevron
        let ax = rect.right - p.s(15.0);
        let ay = (rect.top + rect.bottom) / 2 - p.s(2.0);
        let step = p.s(4.5);
        p.line(ax, ay, ax + step, ay + step, c.text2, p.s(1.4));
        p.line(ax + step, ay + step, ax + step * 2, ay, c.text2, p.s(1.4));
    }

    // ---------------------------------------------------------------- AI 统计
    section("AI 统计", p.s(AI_TITLE_TOP), pad + p.s(4.0), left_right);

    let toggle = l.ai_toggle;
    let mid = (toggle.top + toggle.bottom) / 2;
    if state.hover == Some(Hit::AiToggle) {
        p.round(
            toggle.left,
            toggle.top + p.s(1.0),
            left_right,
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
    switch(left_right - sw_w, mid - sw_h / 2, sw_w, sw_h, state.sw[SW_AI]);

    // 六个来源：排成两列三行（一列六行会顶出窗口）；
    // AI 关掉时这一块也要像手风琴一样收起来
    let ai_shade_open = state.ai_open > 0.01;
    let mut ai_shade_saved: i32 = 0;
    if ai_shade_open {
        let shade_top = p.s(AI_SOURCES_TOP - 4.0);
        let shade_full = p.s(AI_SOURCE_ROW_H * 3.0 + 8.0);
        let shade_bottom = shade_top + (shade_full as f32 * state.ai_open).round() as i32;
        let slide = (((1.0 - state.ai_open) * -8.0) * state.scale).round() as i32;
        ai_shade_saved = SaveDC(dc);
        if ai_shade_saved != 0 {
            IntersectClipRect(dc, 0, shade_top, client.right, shade_bottom);
            SetViewportOrgEx(dc, 0, -scroll + slide, std::ptr::null_mut());
        }
        if let Some(aa) = &p.aa {
            aa.clip_intersect(0, shade_top, client.right, shade_bottom);
            aa.translate(0, slide);
        }
    }
    if ai_shade_open {
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
                mix(c.text3, c.text, state.sw[SW_AI]),
                rect.left + p.s(16.0),
                rect.top,
                right - p.s(30.0),
                rect.bottom,
                DT_LEFT,
            );

            let size = p.s(16.0);
            // 复选框紧跟在文字后面（原来甩在格子最右边，和标签断开了）
            let text_w = crate::shot::text_width(p.dc, AI_SOURCE_LABELS[index], state.fonts.value);
            let bx = (rect.left + p.s(16.0) + text_w + p.s(10.0))
                .min(right - size - p.s(6.0));
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
    }
    if ai_shade_saved != 0 {
        RestoreDC(dc, ai_shade_saved);
    }
    if ai_shade_open {
        if let Some(aa) = &p.aa {
            aa.translate(0, -(((1.0 - state.ai_open) * -8.0) * state.scale).round() as i32);
            aa.clip_rect(0, view_top + scroll, client.right, view_bottom + scroll);
        }
    }

    // ---------------------------------------------------------------- 截图 / 取字
    section("截图与取字", p.s(SHOT_TITLE_TOP), right_left + p.s(4.0), content_right);

    let shot_rows = [
        (
            l.shot_toggle,
            l.shot_hotkey,
            Hit::ShotToggle,
            Hit::ShotHotkey,
            "截图",
            "截图后标注、贴图、另存为",
            state.shot_enabled,
            state.shot_hotkey.as_str(),
            state.capture == 1,
            SW_SHOT,
        ),
        (
            l.ocr_toggle,
            l.ocr_hotkey,
            Hit::OcrToggle,
            Hit::OcrHotkey,
            "取字（OCR）",
            "只管菜单项和取字热键",
            state.ocr_enabled,
            state.ocr_hotkey.as_str(),
            state.capture == 2,
            SW_OCR,
        ),
    ];

    for (row, hotkey_rect, toggle_hit, hotkey_hit, title, subtitle, on, value, capturing, sw_index) in
        shot_rows
    {
        let mid = (row.top + row.bottom) / 2;
        let hovered = state.hover == Some(toggle_hit) || state.hover == Some(hotkey_hit);
        if hovered {
            p.round(
                row.left,
                row.top + p.s(1.0),
                row.right,
                row.bottom - p.s(1.0),
                p.s(8.0),
                c.chip,
                c.chip,
            );
        }

        p.text(
            title,
            state.fonts.label,
            if on { c.text } else { c.text2 },
            row.left + p.s(4.0),
            row.top + p.s(2.0),
            hotkey_rect.left - p.s(10.0),
            row.top + p.s(24.0),
            DT_LEFT,
        );
        p.text(
            subtitle,
            state.fonts.small,
            c.text3,
            row.left + p.s(4.0),
            mid,
            hotkey_rect.left - p.s(10.0),
            row.bottom,
            DT_LEFT,
        );

        // 快捷键胶囊：点一下进入录制，按 Esc 取消（描边款，平时是白底）
        let fill = if capturing {
            c.chip_hover
        } else if state.hover == Some(hotkey_hit) {
            c.chip
        } else {
            c.surface
        };
        let border = if capturing { c.accent } else { c.border };
        p.round(
            hotkey_rect.left,
            hotkey_rect.top,
            hotkey_rect.right,
            hotkey_rect.bottom,
            p.s(8.0),
            fill,
            border,
        );
        p.text(
            if capturing { "按下组合键…" } else { value },
            state.fonts.small,
            if capturing {
                c.text
            } else if on {
                c.text2
            } else {
                c.text3
            },
            hotkey_rect.left + p.s(5.0),
            hotkey_rect.top,
            hotkey_rect.right - p.s(5.0),
            hotkey_rect.bottom,
            DT_CENTER,
        );

        // 开关本体
        let sw_w = p.s(40.0);
        let sw_h = p.s(22.0);
        switch(
            row.right - sw_w,
            mid - sw_h / 2,
            sw_w,
            sw_h,
            state.sw[sw_index],
        );
    }

    // ---------------------------------------------------------------- 截区域录 GIF
    {
        let row = l.gif_toggle;
        let mid = (row.top + row.bottom) / 2;
        let hovered = matches!(
            state.hover,
            Some(Hit::GifToggle) | Some(Hit::GifHotkey)
        );
        if hovered {
            p.round(
                row.left,
                row.top + p.s(1.0),
                row.right,
                row.bottom - p.s(1.0),
                p.s(8.0),
                c.chip,
                c.chip,
            );
        }

        p.text(
            "截区域录 MP4",
            state.fonts.label,
            if state.gif_enabled { c.text } else { c.text2 },
            row.left + p.s(4.0),
            row.top + p.s(2.0),
            l.gif_hotkey.left - p.s(10.0),
            row.top + p.s(24.0),
            DT_LEFT,
        );
        p.text(
            "框一块区域录成视频",
            state.fonts.small,
            c.text3,
            row.left + p.s(4.0),
            mid,
            l.gif_hotkey.left - p.s(10.0),
            row.bottom,
            DT_LEFT,
        );

        // 帧率胶囊搬到了下一行（和画质、时长排一起），这里只保留热键 + 开关

        // 热键胶囊
        let hotkey = l.gif_hotkey;
        let capturing = state.capture == 3;
        let fill = if capturing {
            c.chip_hover
        } else if state.hover == Some(Hit::GifHotkey) {
            c.chip
        } else {
            c.surface
        };
        let border = if capturing { c.accent } else { c.border };
        p.round(
            hotkey.left,
            hotkey.top,
            hotkey.right,
            hotkey.bottom,
            p.s(8.0),
            fill,
            border,
        );
        p.text(
            if capturing {
                "按下组合键…"
            } else {
                state.gif_hotkey.as_str()
            },
            state.fonts.small,
            if capturing {
                c.text
            } else if state.gif_enabled {
                c.text2
            } else {
                c.text3
            },
            hotkey.left + p.s(5.0),
            hotkey.top,
            hotkey.right - p.s(5.0),
            hotkey.bottom,
            DT_CENTER,
        );

        // 开关本体
        let sw_w = p.s(40.0);
        let sw_h = p.s(22.0);
        switch(
            row.right - sw_w,
            mid - sw_h / 2,
            sw_w,
            sw_h,
            state.sw[SW_GIF],
        );

        // 「录制」行：画质 / 时长 / 帧率，三颗胶囊各自循环
        let rec_pills: [(RECT, Hit, String); 3] = [
            (l.gif_quality, Hit::GifQuality, format!("画质 {}", state.gif_quality)),
            (l.gif_seconds, Hit::GifSeconds, format!("{} 秒", state.gif_seconds)),
            (
                l.gif_fps,
                Hit::GifFps,
                if state.gif_fps == 0 {
                    "帧率 自动".to_string()
                } else {
                    format!("帧率 {}", state.gif_fps)
                },
            ),
        ];
        p.text(
            "录制",
            state.fonts.small,
            c.text3,
            row.left + p.s(4.0),
            rec_pills[0].0.top,
            rec_pills[0].0.left - p.s(6.0),
            rec_pills[0].0.bottom,
            DT_LEFT,
        );
        for (rect, hit, label) in rec_pills {
            let fill = if state.hover == Some(hit) {
                c.chip_hover
            } else {
                c.chip
            };
            p.round(rect.left, rect.top, rect.right, rect.bottom, p.s(8.0), fill, fill);
            p.text(
                &label,
                state.fonts.small,
                if state.gif_enabled { c.text2 } else { c.text3 },
                rect.left + p.s(4.0),
                rect.top,
                rect.right - p.s(4.0),
                rect.bottom,
                DT_CENTER,
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

    // ---------------------------------------------------------------- 翻译
    {
        section(
            "翻译",
            p.s(TR_TITLE_TOP),
            right_left + p.s(4.0),
            l.tr_toggle.left - p.s(10.0),
        );
        // 总开关：关掉的时候下面四条整个收起来（手风琴）
        let toggle = l.tr_toggle;
        let toggle_mid = (toggle.top + toggle.bottom) / 2;
        let sw_h = toggle.bottom - toggle.top;
        if state.hover == Some(Hit::TrToggle) {
            p.round(
                toggle.left - p.s(6.0),
                toggle.top - p.s(4.0),
                toggle.right + p.s(6.0),
                toggle.bottom + p.s(4.0),
                p.s(7.0),
                c.chip,
                c.chip,
            );
        }
        switch(
            toggle.left,
            toggle.top,
            toggle.right - toggle.left,
            sw_h,
            state.sw[SW_TRANSLATE],
        );
        let _ = toggle_mid;
        {
            // 收起来的提示：只在整个收完之后露出来。
            // 它和「服务商」那一行占的是同一条横带，提前淡入会和正在收的字段叠在一起。
            let hint = ((0.03 - state.tr_open) / 0.03).clamp(0.0, 1.0);
            if hint > 0.02 {
                p.text(
                    "翻译关着：勾上右上角的开关就会出现服务商和 Key",
                    state.fonts.small,
                    mix(c.surface, c.text3, hint),
                    right_left + p.s(4.0),
                    p.s(TR_TITLE_TOP + 22.0),
                    content_right,
                    p.s(TR_TITLE_TOP + 40.0),
                    DT_LEFT,
                );
            }
        }

        // 表单式（方向 C 的输入框）：标签在控件上方，控件铺满整列；
        // 平时是浅灰填充块，聚焦时反白 + 蓝边 + 一圈很淡的外发光。
        // 标签跟控件左缘对齐（标签是「字段的标签」，不是「整列的标签」）
        let form_label = |text: &str, rect: &RECT| {
            p.text(
                text,
                state.fonts.small,
                c.text3,
                rect.left + p.s(2.0),
                rect.top - p.s(14.0),
                rect.right,
                rect.top - p.s(2.0),
                DT_LEFT,
            );
        };
        let glow = mix_white(c.accent, 0.86);
        let field_box = |rect: RECT, editing: bool| {
            if editing {
                p.round(
                    rect.left - p.s(3.0),
                    rect.top - p.s(3.0),
                    rect.right + p.s(3.0),
                    rect.bottom + p.s(3.0),
                    p.s(11.0),
                    glow,
                    glow,
                );
            }
            let fill = if editing { c.surface } else { c.chip };
            p.round(
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                p.s(8.0),
                fill,
                fill,
            );
            if editing {
                p.ring(
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    p.s(8.0),
                    c.accent,
                    p.s(1.5).max(1),
                );
            }
        };

        // 手风琴：展开度就是一块「百叶窗」，从标签上方一路拉到 Base URL 下面；
        // 里面的内容同时从上面滑下来（拉开时往下落 10px，收起时往上抽走），有「抖开」的感觉
        let shade_open = state.tr_open > 0.01;
        let mut shade_saved: i32 = 0;
        if shade_open {
            let shade_top = p.s(TR_ROW1_TOP - 14.0);
            let shade_full = p.s(TR_ROW4_TOP + TR_BOX_H + 6.0) - shade_top;
            let shade_bottom = shade_top + (shade_full as f32 * state.tr_open).round() as i32;
            let slide = (((1.0 - state.tr_open) * -10.0) * state.scale).round() as i32;
            shade_saved = SaveDC(dc);
            if shade_saved != 0 {
                IntersectClipRect(dc, 0, shade_top, client.right, shade_bottom);
                // 裁剪区已经定在设备坐标上，再挪原点只动内容
                SetViewportOrgEx(dc, 0, -scroll + slide, std::ptr::null_mut());
            }
            if let Some(aa) = &p.aa {
                aa.clip_intersect(0, shade_top, client.right, shade_bottom);
                aa.translate(0, slide);
            }
            // 服务商：两个等宽按钮，选中的那颗是实心强调色
            form_label("服务商", &l.tr_chatgpt);
            for (rect, name, index) in [
                (l.tr_chatgpt, "ChatGPT", 0usize),
                (l.tr_deepseek, "DeepSeek", 1usize),
            ] {
                let active = state.tr_provider == index;
                let (fill, ink) = if active {
                    (c.accent, c.bg)
                } else {
                    (c.chip, c.text2)
                };
                p.round(
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    p.s(8.0),
                    fill,
                    fill,
                );
                p.text(
                    name,
                    state.fonts.label,
                    ink,
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    DT_CENTER,
                );
            }

            // 三个输入框
            let fields: [(RECT, TrField, &str, &str); 3] = [
                (l.tr_model, TrField::Model, "模型", "留空用默认"),
                (l.tr_key, TrField::Key, "API Key", "sk-…"),
                (l.tr_base, TrField::Base, "Base URL", "留空用官方地址"),
            ];
            for (rect, field, title, placeholder) in fields {
                form_label(title, &rect);
                let editing = state.tr_edit == Some(field);
                field_box(rect, editing);
                let value = match field {
                    TrField::Key => state.tr_key.as_str(),
                    TrField::Model => state.tr_model.as_str(),
                    TrField::Base => state.tr_base.as_str(),
                };
                let masked = field == TrField::Key && !value.is_empty() && !editing && !state.tr_show_key;
                let shown = if masked {
                    // 没在编辑、也没点「显示」时把 Key 打码，肩窥也不怕
                    let tail: String = value
                        .chars()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!("••••••{tail}")
                } else {
                    value.to_string()
                };
                let (text, color) = if !shown.is_empty() {
                    (shown, c.text)
                } else if editing {
                    // 空着又正在编辑：不画占位符，否则光标会压在第一个字上
                    (String::new(), c.text3)
                } else {
                    (placeholder.to_string(), c.text3)
                };
                // API Key 右边要留出「显示 / 隐藏」
                let suffix_w = if field == TrField::Key {
                    rect.right - l.tr_reveal.left + p.s(4.0)
                } else {
                    p.s(10.0)
                };
                let text_left = rect.left + p.s(10.0);
                p.text(
                    &text,
                    state.fonts.label,
                    color,
                    text_left,
                    rect.top,
                    rect.right - suffix_w,
                    rect.bottom,
                    DT_LEFT,
                );
                if field == TrField::Key {
                    let reveal = l.tr_reveal;
                    p.text(
                        if state.tr_show_key { "隐藏" } else { "显示" },
                        state.fonts.small,
                        c.accent,
                        reveal.left,
                        reveal.top,
                        reveal.right,
                        reveal.bottom,
                        DT_RIGHT,
                    );
                    // 说明文案：存在本机，不上传
                    p.text(
                        "存在本机设置文件里，不上传",
                        state.fonts.small,
                        c.text3,
                        rect.left + p.s(2.0),
                        p.s(TR_HINT_TOP),
                        content_right,
                        p.s(TR_HINT_TOP) + p.s(14.0),
                        DT_LEFT,
                    );
                }
                // 光标：按字符宽度粗略定位（等宽字体不需要，这里用平均宽度）
                if editing {
                    let prefix: String = value.chars().take(state.tr_caret).collect();
                    let caret_x =
                        text_left + crate::shot::text_width(p.dc, &prefix, state.fonts.label);
                    let caret_w = p.s(2.0).max(1);
                    p.fill(
                        caret_x,
                        rect.top + p.s(6.0),
                        caret_x + caret_w,
                        rect.bottom - p.s(6.0),
                        c.accent,
                    );
                }
            }
        }
        if shade_saved != 0 {
            RestoreDC(dc, shade_saved);
        }
        if shade_open {
            if let Some(aa) = &p.aa {
                // 先把平移还回去，再恢复成滚动那一刀（视口范围）
                aa.translate(0, -(((1.0 - state.tr_open) * -10.0) * state.scale).round() as i32);
                aa.clip_rect(0, view_top + scroll, client.right, view_bottom + scroll);
            }
        }
    }

    // 内容区结束：恢复裁剪和原点，页脚固定在底部
    if saved != 0 {
        RestoreDC(dc, saved);
    }
    // 页脚画在正常坐标系里，GDI+ 的位移和裁剪要一起复位
    if let Some(aa) = &p.aa {
        aa.reset();
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
        // 检查更新（页脚右侧，紧挨「完成」）
        let area = l.update;
        let hovered = state.hover == Some(Hit::Update);
        let (fill, border) = if hovered {
            (c.chip_hover, c.chip_hover)
        } else {
            (c.chip, mix_white(c.border, 0.05))
        };
        p.round(
            area.left,
            area.top + p.s(4.0),
            area.right,
            area.bottom - p.s(4.0),
            p.s(8.0),
            fill,
            border,
        );
        p.text(
            "检查更新",
            state.fonts.value,
            if hovered { c.text } else { c.text2 },
            area.left,
            area.top,
            area.right,
            area.bottom,
            DT_CENTER_ALIGN,
        );

        // 当前版本号，放在页脚最左边
        let auto = l.auto_start;
        p.text(
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            state.fonts.small,
            c.text3,
            auto.left,
            auto.top,
            auto.left + p.s(66.0),
            auto.bottom,
            DT_LEFT,
        );

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
            area.left + p.s(66.0),
            area.top,
            sx - p.s(10.0),
            area.bottom,
            DT_LEFT,
        );

        switch(sx, sy, sw_w, sw_h, state.sw[SW_AUTOSTART]);
        let _ = mid;
    }

    // 提交（缓冲是常驻的，由 state.back 管，这里不能删）
    BitBlt(hdc, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
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
        aa: crate::aa::Canvas::new(dc),
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
        WM_CHAR => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            if let Some(field) = state.tr_edit {
                let ch = wparam as u32;
                if ch >= 0x20 && ch != 0x7F {
                    if let Some(c) = char::from_u32(ch) {
                        let buffer = match field {
                            TrField::Key => &mut state.tr_key,
                            TrField::Model => &mut state.tr_model,
                            TrField::Base => &mut state.tr_base,
                        };
                        let mut chars: Vec<char> = buffer.chars().collect();
                        let caret = state.tr_caret.min(chars.len());
                        chars.insert(caret, c);
                        *buffer = chars.into_iter().collect();
                        state.tr_caret = caret + 1;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
            }
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
                        (*settings).popup_row = 0;
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

    state.popup_row = row;

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
        // 两列：左列「显示 + AI 统计」，右列「截图与取字 + 翻译」
        let width_px = s(WINDOW_W, scale);
        let mut height_px = s(WINDOW_H, scale);

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
            shot_enabled: settings.shot_enabled,
            ocr_enabled: settings.ocr_enabled,
            gif_enabled: settings.gif_enabled,
            gif_fps: settings.gif_fps,
            gif_quality: settings.gif_quality,
            gif_seconds: settings.gif_seconds,
            shot_hotkey: settings.shot_hotkey.clone(),
            ocr_hotkey: settings.ocr_hotkey.clone(),
            gif_hotkey: settings.gif_hotkey.clone(),
            translate_enabled: settings.translate_enabled,
            tr_provider: if settings.translate_provider.eq_ignore_ascii_case("deepseek") {
                1
            } else {
                0
            },
            tr_key: settings.translate_key.clone(),
            tr_model: settings.translate_model.clone(),
            tr_base: settings.translate_base.clone(),
            tr_edit: None,
            tr_show_key: false,
            tr_caret: 0,
            capture: 0,
            fonts: Fonts::create(scale),
            scale,
            hover: None,
            // 打开时焦点在第一项（字号），按 Tab 就能一路走过去
            focus: Some(Hit::Select(1)),
            keyboard: false,
            slider_drag: false,
            back: None,
            back_size: (0, 0),
            scroll_drag: false,
            scroll: 0,
            hwnd: std::ptr::null_mut(),
            sw: [
                if settings.ai_enabled { 1.0 } else { 0.0 },
                if settings.shot_enabled { 1.0 } else { 0.0 },
                if settings.ocr_enabled { 1.0 } else { 0.0 },
                if settings.gif_enabled { 1.0 } else { 0.0 },
                if settings.translate_enabled { 1.0 } else { 0.0 },
                if settings.auto_start { 1.0 } else { 0.0 },
            ],
            tr_open: if settings.translate_enabled { 1.0 } else { 0.0 },
            ai_open: if settings.ai_enabled { 1.0 } else { 0.0 },
            anim_at: None,
            popup: std::ptr::null_mut(),
            popup_row: 0,
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
        (*state).hwnd = hwnd;
        LAST_HWND.with(|cell| cell.set(hwnd as isize));
        round_corners(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        UpdateWindow(hwnd);
    }
}

thread_local! {
    /// 最近一次打开的设置窗（离屏预览用）
    static LAST_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// 最近一次打开的设置窗句柄
pub fn last_hwnd() -> HWND {
    LAST_HWND.with(|cell| cell.get() as HWND)
}

/// 离屏渲染设置窗到 PNG（改布局时用来做像素比对）
pub unsafe fn preview_png(path: &str) -> bool {
    let settings = Settings::load();
    open(std::ptr::null_mut(), &settings);
    let hwnd = last_hwnd();
    if hwnd.is_null() {
        return false;
    }
    // 让它把第一帧画完
    let mut message: MSG = std::mem::zeroed();
    for _ in 0..6 {
        while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    let width = (client.right - client.left).max(1);
    let height = (client.bottom - client.top).max(1);

    let (dc, dib, bits) = make_dib(width, height);
    if dc.is_null() || bits.is_null() {
        DestroyWindow(hwnd);
        return false;
    }
    // 走 WM_PRINTCLIENT：和真实绘制同一条代码路径，不依赖 PrintWindow
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
    // 「检查更新」压在「开机自启」的热区范围里，必须先判它
    if contains(&l.update, point) {
        return Some(Hit::Update);
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
    if contains(&l.tr_toggle, point) {
        return Some(Hit::TrToggle);
    }
    // 快捷键胶囊比整行矮，得先判
    if contains(&l.shot_hotkey, point) {
        return Some(Hit::ShotHotkey);
    }
    if contains(&l.ocr_hotkey, point) {
        return Some(Hit::OcrHotkey);
    }
    if contains(&l.gif_hotkey, point) {
        return Some(Hit::GifHotkey);
    }
    if contains(&l.gif_fps, point) {
        return Some(Hit::GifFps);
    }
    if contains(&l.shot_toggle, point) {
        return Some(Hit::ShotToggle);
    }
    if contains(&l.ocr_toggle, point) {
        return Some(Hit::OcrToggle);
    }
    if contains(&l.gif_toggle, point) {
        return Some(Hit::GifToggle);
    }
    // 「录制」行三颗胶囊在 gif 行下沿，先判它们再判整行热区
    for (rect, hit) in [
        (l.gif_quality, Hit::GifQuality),
        (l.gif_seconds, Hit::GifSeconds),
        (l.gif_fps, Hit::GifFps),
    ] {
        if contains(&rect, point) {
            return Some(hit);
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
        for index in 0..6 {
            items.push(Hit::AiSource(index));
        }
    }
    items.push(Hit::ShotToggle);
    items.push(Hit::ShotHotkey);
    items.push(Hit::OcrToggle);
    items.push(Hit::OcrHotkey);
    items.push(Hit::GifToggle);
    items.push(Hit::GifQuality);
    items.push(Hit::GifSeconds);
    items.push(Hit::GifFps);
    items.push(Hit::GifHotkey);
    items.push(Hit::TrToggle);
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
        Hit::ShotToggle => l.shot_toggle,
        Hit::OcrToggle => l.ocr_toggle,
        Hit::ShotHotkey => l.shot_hotkey,
        Hit::OcrHotkey => l.ocr_hotkey,
        Hit::GifToggle => l.gif_toggle,
        Hit::GifQuality => l.gif_quality,
        Hit::GifSeconds => l.gif_seconds,
        Hit::GifFps => l.gif_fps,
        Hit::GifHotkey => l.gif_hotkey,
        Hit::TrToggle => l.tr_toggle,
        Hit::AutoStart => l.auto_start,
        Hit::Done => l.done,
        _ => return None,
    })
}

/// 下拉选中某一项：鼠标点选和键盘改值共用
/// 把翻译配置写回设置（点服务商、编辑结束、关窗时调）
fn commit_translate(state: &State) {
    let provider = if state.tr_provider == 1 { "deepseek" } else { "chatgpt" };
    crate::apply_translate_settings(
        state.parent,
        provider,
        &state.tr_key,
        &state.tr_model,
        &state.tr_base,
    );
}

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
        // 第 0 项是「自动（全部合计）」，所以要多一个
        3 => (state.interface, state.interfaces.len() + 1),
        _ => (state.refresh, REFRESH_OPTIONS.len()),
    };
    let next = step_choice(current, len, delta);
    choose(state, row, next);
}

/// 在 [0, len) 里按 delta 走一步，走到头折回去。len 为 0 时返回 0。
fn step_choice(current: usize, len: usize, delta: i32) -> usize {
    let len_i = len.max(1) as i32;
    (current as i32 + delta).rem_euclid(len_i) as usize
}

fn toggle_ai(state: &mut State) {
    let (enabled, sources) = ai_toggle_plan(state.ai_enabled, state.ai_sources);
    if let Some(sources) = sources {
        state.ai_sources = sources;
        crate::apply_ai_sources(state.parent, state.ai_sources);
    }
    state.ai_enabled = enabled;
    crate::apply_ai_enabled(state.parent, enabled);
    start_anim(state.hwnd);
}

/// 点总开关之后应该变成什么：(新的总开关, 需要一并改动的来源列表)。
///
/// 规则：**打开**总开关时如果来源全关着，就默认全开 —— 否则开关看着是开的、
/// 实际什么也采不到，这种"静默失效"最难查。
fn ai_toggle_plan(enabled: bool, sources: [bool; 6]) -> (bool, Option<[bool; 6]>) {
    let next = !enabled;
    if next && !sources.iter().any(|flag| *flag) {
        (next, Some([true; 6]))
    } else {
        (next, None)
    }
}

fn toggle_source(state: &mut State, index: usize) {
    if !state.ai_enabled {
        return;
    }
    state.ai_sources[index] = !state.ai_sources[index];
    crate::apply_ai_sources(state.parent, state.ai_sources);
}

/// 起一轮动效：后台线程按 16ms 往窗口里投 WM_APP_ANIM，直到所有值都到位
fn start_anim(hwnd: HWND) {
    use std::sync::atomic::Ordering;
    if hwnd.is_null() {
        return;
    }
    ANIM_ACTIVE.store(true, Ordering::Relaxed);
    if ANIM_THREAD.swap(true, Ordering::Relaxed) {
        return;
    }
    let raw = hwnd as isize;
    std::thread::spawn(move || {
        use std::time::{Duration, Instant};
        let frame = Duration::from_micros(16_000);
        let mut next = Instant::now();
        loop {
            if !ANIM_ACTIVE.load(Ordering::Relaxed) {
                break;
            }
            // 等上一帧画完再发下一帧：消息队列里永远只排一帧，节奏才是匀的
            let deadline = Instant::now() + Duration::from_millis(80);
            while ANIM_PENDING.load(Ordering::Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }

            next += frame;
            let now = Instant::now();
            if now > next + frame * 3 {
                // 落后太多就不追了，重新对齐（免得一口气补好几帧）
                next = now;
            }
            // Windows 默认 Sleep 精度是 15.6ms，睡到 16ms 会变成 31ms 一顿一顿的：
            // 所以大头用 sleep，最后 2ms 自旋，保证真的 60fps
            loop {
                let now = Instant::now();
                if now >= next {
                    break;
                }
                let left = next - now;
                if left > Duration::from_millis(2) {
                    std::thread::sleep(left - Duration::from_millis(2));
                } else {
                    std::hint::spin_loop();
                }
            }

            ANIM_PENDING.store(true, Ordering::Relaxed);
            unsafe {
                if PostMessageW(raw as HWND, WM_APP_ANIM, 0, 0) == 0 {
                    break;
                }
            }
        }
        ANIM_PENDING.store(false, Ordering::Relaxed);
        ANIM_THREAD.store(false, Ordering::Relaxed);
    });
}

/// 推一帧动效：所有值都朝目标走，全部到位就停下来
fn advance_anim(state: &mut State) {
    use std::sync::atomic::Ordering;
    let now = std::time::Instant::now();
    let dt = state
        .anim_at
        .map(|last| now.duration_since(last).as_secs_f32())
        .unwrap_or(0.016)
        .clamp(0.001, 0.05);
    state.anim_at = Some(now);

    let targets = [
        state.ai_enabled,
        state.shot_enabled,
        state.ocr_enabled,
        state.gif_enabled,
        state.translate_enabled,
        state.auto_start,
    ];
    let mut busy = false;
    let k = 1.0 - (-dt * 22.0).exp();
    for (slot, target) in state.sw.iter_mut().zip(targets) {
        let goal = if target { 1.0 } else { 0.0 };
        if (*slot - goal).abs() > 0.004 {
            *slot += (goal - *slot) * k;
            busy = true;
        } else {
            *slot = goal;
        }
    }
    let goal = if state.translate_enabled { 1.0 } else { 0.0 };
    if (state.tr_open - goal).abs() > 0.004 {
        state.tr_open += (goal - state.tr_open) * (1.0 - (-dt * 12.0).exp());
        busy = true;
    } else {
        state.tr_open = goal;
    }
    let goal = if state.ai_enabled { 1.0 } else { 0.0 };
    if (state.ai_open - goal).abs() > 0.004 {
        state.ai_open += (goal - state.ai_open) * (1.0 - (-dt * 12.0).exp());
        busy = true;
    } else {
        state.ai_open = goal;
    }

    ANIM_ACTIVE.store(busy, Ordering::Relaxed);
    unsafe {
        InvalidateRect(state.hwnd, std::ptr::null(), 0);
    }
}

fn toggle_translate(state: &mut State) {
    state.translate_enabled = !state.translate_enabled;
    if !state.translate_enabled {
        // 收起之前先把正在编辑的那一格收尾
        if state.tr_edit.is_some() {
            commit_translate(state);
            state.tr_edit = None;
        }
        state.tr_show_key = false;
    }
    crate::apply_translate_enabled(state.parent, state.translate_enabled);
    start_anim(state.hwnd);
}

fn toggle_auto_start(state: &mut State) {
    state.auto_start = !state.auto_start;
    crate::apply_auto_start(state.parent, state.auto_start);
    start_anim(state.hwnd);
}

fn toggle_shot(state: &mut State) {
    state.shot_enabled = !state.shot_enabled;
    crate::apply_shot_enabled(state.parent, state.shot_enabled);
    start_anim(state.hwnd);
}

fn toggle_ocr(state: &mut State) {
    state.ocr_enabled = !state.ocr_enabled;
    crate::apply_ocr_enabled(state.parent, state.ocr_enabled);
    start_anim(state.hwnd);
}

fn toggle_gif(state: &mut State) {
    state.gif_enabled = !state.gif_enabled;
    crate::apply_gif_enabled(state.parent, state.gif_enabled);
    start_anim(state.hwnd);
}

/// 帧率胶囊：30 → 60 → 120 → 自动 → 30
fn cycle_gif_fps(state: &mut State) {
    state.gif_fps = match state.gif_fps {
        0 => 30,
        30 => 60,
        60 => 120,
        _ => 0,
    };
    crate::apply_gif_fps(state.parent, state.gif_fps);
}

/// 画质胶囊：50 → 65 → 80 → 50
fn cycle_gif_quality(state: &mut State) {
    state.gif_quality = match state.gif_quality {
        50 => 65,
        65 => 80,
        _ => 50,
    };
    crate::apply_gif_quality(state.parent, state.gif_quality);
}

/// 时长胶囊：10 → 20 → 30 → 10 秒
fn cycle_gif_seconds(state: &mut State) {
    state.gif_seconds = match state.gif_seconds {
        10 => 20,
        20 => 30,
        _ => 10,
    };
    crate::apply_gif_seconds(state.parent, state.gif_seconds);
}

/// 把当前按下的组合键写进对应那一行
fn accept_hotkey(state: &mut State, vk: u32) {
    let mut mods = 0_u32;
    unsafe {
        if GetKeyState(0x11) < 0 {
            mods |= HOTKEY_MOD_CONTROL;
        }
        if GetKeyState(0x10) < 0 {
            mods |= HOTKEY_MOD_SHIFT;
        }
        if GetKeyState(0x12) < 0 {
            mods |= HOTKEY_MOD_ALT;
        }
        if GetKeyState(0x5B) < 0 || GetKeyState(0x5C) < 0 {
            mods |= HOTKEY_MOD_WIN;
        }
    }

    let text = format_hotkey(mods, vk);
    if parse_hotkey(&text).is_none() {
        return;
    }

    if state.capture == 1 {
        state.shot_hotkey = text.clone();
        crate::apply_hotkey(state.parent, false, &text);
    } else if state.capture == 3 {
        state.gif_hotkey = text.clone();
        crate::apply_gif_hotkey(state.parent, &text);
    } else {
        state.ocr_hotkey = text.clone();
        crate::apply_hotkey(state.parent, true, &text);
    }
    state.capture = 0;
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
    // 拖动过程中只预览，松手（WM_LBUTTONUP）再落盘
    crate::preview_panel_width(state.parent, width);
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
        // 离屏渲染（NST_SETTINGS_PREVIEW / 系统打印对话框）走同一条绘制路径
        WM_PRINTCLIENT => {
            let state = state_of(hwnd);
            if !state.is_null() {
                let mut client: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut client);
                if client.right > 0 && client.bottom > 0 {
                    paint_into(wparam as HDC, &mut *state, client);
                }
            }
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

            // 翻译：API Key 右边的「显示 / 隐藏」（要排在别的命中判定前面）
            {
                let l = layout(state, client);
                let (view_top, view_bottom) = content_viewport(state, client);
                let y = point.y + state.scroll;
                let reveal = l.tr_reveal;
                if state.translate_enabled
                    && point.y >= view_top
                    && point.y < view_bottom
                    && point.x >= reveal.left
                    && point.x <= reveal.right
                    && y >= reveal.top
                    && y <= reveal.bottom
                {
                    state.tr_show_key = !state.tr_show_key;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
            }

            // 翻译区块：点一下才生效（鼠标扫过去什么都别做）
            {
                let l = layout(state, client);
                let (view_top, view_bottom) = content_viewport(state, client);
                let y = point.y + state.scroll;
                let inside = |rect: RECT| {
                    point.y >= view_top
                        && point.y < view_bottom
                        && point.x >= rect.left
                        && point.x <= rect.right
                        && y >= rect.top
                        && y <= rect.bottom
                };
                let provider = if !state.translate_enabled {
                    None
                } else if inside(l.tr_chatgpt) {
                    Some(0usize)
                } else if inside(l.tr_deepseek) {
                    Some(1usize)
                } else {
                    None
                };
                if let Some(provider) = provider {
                    state.tr_provider = provider;
                    commit_translate(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                let hit = if state.translate_enabled && inside(l.tr_key) && !inside(l.tr_reveal) {
                    Some(TrField::Key)
                } else if state.translate_enabled && inside(l.tr_model) {
                    Some(TrField::Model)
                } else if state.translate_enabled && inside(l.tr_base) {
                    Some(TrField::Base)
                } else {
                    None
                };
                if let Some(field) = hit {
                    let value = match field {
                        TrField::Key => &state.tr_key,
                        TrField::Model => &state.tr_model,
                        TrField::Base => &state.tr_base,
                    };
                    state.tr_caret = value.chars().count();
                    state.tr_edit = Some(field);
                    SetFocus(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if state.tr_edit.is_some() {
                    // 点到别处 = 结束编辑
                    commit_translate(state);
                    state.tr_edit = None;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }

            // 鼠标点谁，焦点就落在谁身上（焦点框只在键盘操作时显示）
            let hit = hit_test(state, client, point);
            if state.capture != 0
                && !matches!(
                    hit,
                    Some(Hit::ShotHotkey) | Some(Hit::OcrHotkey) | Some(Hit::GifHotkey)
                )
            {
                state.capture = 0;
            }
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
                Some(Hit::ShotToggle) => {
                    toggle_shot(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::OcrToggle) => {
                    toggle_ocr(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::ShotHotkey) => {
                    state.capture = 1;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::OcrHotkey) => {
                    state.capture = 2;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::GifToggle) => {
                    toggle_gif(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::GifFps) => {
                    cycle_gif_fps(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::GifQuality) => {
                    cycle_gif_quality(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::GifSeconds) => {
                    cycle_gif_seconds(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::GifHotkey) => {
                    state.capture = 3;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::TrToggle) => {
                    toggle_translate(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::AutoStart) => {
                    toggle_auto_start(state);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                Some(Hit::Update) => {
                    crate::update::check(state.parent);
                    return 0;
                }
                None => {}
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let state = state_of(hwnd);
            if !state.is_null() {
                // 拖完滑杆才落盘（拖动过程中只改面板宽度）
                if (*state).slider_drag {
                    crate::commit_panel_width((*state).parent);
                }
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
        WM_APP_ANIM => {
            std::sync::atomic::AtomicBool::store(&ANIM_PENDING, false, std::sync::atomic::Ordering::Relaxed);
            let state = state_of(hwnd);
            if !state.is_null() {
                advance_anim(&mut *state);
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
            state.popup_row = 0;
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

                let width = s(WINDOW_W, scale);
                let mut height = s(WINDOW_H, scale);
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
        WM_CHAR => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            if let Some(field) = state.tr_edit {
                let ch = wparam as u32;
                if ch >= 0x20 && ch != 0x7F {
                    if let Some(c) = char::from_u32(ch) {
                        let buffer = match field {
                            TrField::Key => &mut state.tr_key,
                            TrField::Model => &mut state.tr_model,
                            TrField::Base => &mut state.tr_base,
                        };
                        let mut chars: Vec<char> = buffer.chars().collect();
                        let caret = state.tr_caret.min(chars.len());
                        chars.insert(caret, c);
                        *buffer = chars.into_iter().collect();
                        state.tr_caret = caret + 1;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
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

            // 正在编辑翻译输入框：先处理编辑键
            if state.tr_edit.is_some() {
                let field = state.tr_edit.unwrap();
                let buffer = match field {
                    TrField::Key => &mut state.tr_key,
                    TrField::Model => &mut state.tr_model,
                    TrField::Base => &mut state.tr_base,
                };
                let count = buffer.chars().count();
                let caret = state.tr_caret.min(count);
                match wparam as u32 {
                    0x08 => {
                        // 退格
                        if caret > 0 {
                            let mut chars: Vec<char> = buffer.chars().collect();
                            chars.remove(caret - 1);
                            *buffer = chars.into_iter().collect();
                            state.tr_caret = caret - 1;
                        }
                    }
                    0x2E => {
                        // Delete
                        if caret < count {
                            let mut chars: Vec<char> = buffer.chars().collect();
                            chars.remove(caret);
                            *buffer = chars.into_iter().collect();
                        }
                    }
                    0x25 => state.tr_caret = caret.saturating_sub(1),
                    0x27 => state.tr_caret = (caret + 1).min(count),
                    0x24 => state.tr_caret = 0,
                    0x23 => state.tr_caret = count,
                    0x0D | 0x1B => {
                        // 回车 / Esc：结束编辑并落盘
                        commit_translate(state);
                        state.tr_edit = None;
                    }
                    _ => return 0,
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }

            // 正在录快捷键：这一下按键只用来组合，Esc 取消
            if state.capture != 0 {
                if wparam as u32 == 0x1B {
                    state.capture = 0;
                } else if !matches!(wparam as u32, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C) {
                    accept_hotkey(state, wparam as u32);
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }

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
                    Hit::ShotToggle => toggle_shot(state),
                    Hit::OcrToggle => toggle_ocr(state),
                    Hit::ShotHotkey => state.capture = 1,
                    Hit::OcrHotkey => state.capture = 2,
                    Hit::GifToggle => toggle_gif(state),
                    Hit::GifFps => cycle_gif_fps(state),
                    Hit::GifQuality => cycle_gif_quality(state),
                    Hit::GifSeconds => cycle_gif_seconds(state),
                    Hit::GifHotkey => state.capture = 3,
                    Hit::TrToggle => toggle_translate(state),
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

#[cfg(test)]
mod choice_tests {
    use super::*;

    #[test]
    fn step_choice_moves_and_wraps() {
        // 中间往前走
        assert_eq!(step_choice(0, 4, 1), 1);
        // 走到末尾折回开头
        assert_eq!(step_choice(3, 4, 1), 0);
        // 从开头往回走折到末尾
        assert_eq!(step_choice(0, 4, -1), 3);
        // 大步跨越也能正确取模
        assert_eq!(step_choice(1, 4, 7), 0);
        assert_eq!(step_choice(1, 4, -7), 2);
    }

    #[test]
    fn step_choice_handles_degenerate_lengths() {
        // 只有一项时怎么走都还在原地
        assert_eq!(step_choice(0, 1, 1), 0);
        assert_eq!(step_choice(0, 1, -5), 0);
        // len 为 0（比如网卡列表为空时）不能除零 panic
        assert_eq!(step_choice(0, 0, 1), 0);
        // 当前值越界（列表变短过）也不会 panic，会被折回范围内
        assert!(step_choice(9, 4, 0) < 4);
    }

    #[test]
    fn options_tables_are_non_empty_and_consistent() {
        // 下拉框的"标签/值"数量一致，否则 choose() 取到的 value 会对不上
        assert!(!FONT_OPTIONS.is_empty());
        assert!(!GAUGE_OPTIONS.is_empty());
        assert!(!REFRESH_OPTIONS.is_empty());
        // 刷新间隔必须递增，否则 shift_choice 的"下一档"看起来会乱跳
        let mut last = 0;
        for (_, ms) in REFRESH_OPTIONS.iter() {
            assert!(*ms > last, "刷新档位要递增");
            last = *ms;
        }
        // 字号档位也要递增
        let mut last = 0;
        for (_, percent) in FONT_OPTIONS.iter() {
            assert!(*percent > last, "字号档位要递增");
            last = *percent;
        }
    }
}

#[cfg(test)]
mod ai_toggle_tests {
    use super::*;

    #[test]
    fn turning_on_with_all_sources_off_enables_everything() {
        let (enabled, sources) = ai_toggle_plan(false, [false; 6]);
        assert!(enabled, "总开关要打开");
        assert_eq!(sources, Some([true; 6]), "来源全关时必须一起打开");
    }

    #[test]
    fn turning_on_with_some_sources_keeps_them() {
        let mut sources = [false; 6];
        sources[2] = true;
        let (enabled, changed) = ai_toggle_plan(false, sources);
        assert!(enabled);
        assert_eq!(changed, None, "已经有来源开着就不要再动用户的勾选");
    }

    #[test]
    fn turning_off_never_touches_sources() {
        for sources in [[true; 6], [false; 6]] {
            let (enabled, changed) = ai_toggle_plan(true, sources);
            assert!(!enabled);
            assert_eq!(changed, None, "关总开关不该改来源");
        }
    }

    #[test]
    fn all_off_detection_covers_every_position() {
        // 任意一个位置开着都不算"全关"
        for index in 0..6 {
            let mut sources = [false; 6];
            sources[index] = true;
            assert_eq!(ai_toggle_plan(false, sources).1, None, "第 {index} 项开着就不该覆盖");
        }
    }
}
