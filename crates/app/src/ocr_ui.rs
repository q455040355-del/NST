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

/// 语言下拉：选中某一项
const WM_APP_LANG_PICK: u32 = WM_APP + 0x44;
/// 把选中的目标语言告诉面板（它负责落盘）。注意要和 main.rs 里同一个号。
const WM_APP_OCR_TARGET: u32 = WM_APP + 13;
const LANG_CLASS: &str = "NetSpeedTaskbarRs.OcrLang";

/// 打字机的一帧
const WM_APP_REVEAL: u32 = WM_APP + 0x41;
/// 结果回来了（lparam 是 Box<AsyncUpdate>）
const WM_APP_RESULT: u32 = WM_APP + 0x42;

/// windows-sys 0.59 没导出 WM_MOUSELEAVE（不自己定义的话它会变成通配绑定，后面的分支全被吃掉）
const WM_MOUSELEAVE: u32 = 0x02A3;
const ID_EDIT: usize = 2001;

const EM_SETMARGINS: u32 = 0x00D3;
const EC_LEFTMARGIN: usize = 0x0001;
const EC_RIGHTMARGIN: usize = 0x0002;
const EM_SETSEL: u32 = 0x00B1;

/// windows-sys 0.59 没导出 DT_SINGLELINE，量单行宽度要用
const DT_SINGLELINE: u32 = 0x0020;

/// 窗口逻辑尺寸（比原来宽一点：逐句要并排放下「原文 + 译文」还不挤）
/// 上次用的看法：0 逐句 / 1 整段（只取字不记，那是临时看一眼）
/// 上次用的看法：0 逐句 / 1 整段。默认整段（逐句要自己点）
static LAST_VIEW: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

const WINDOW_W: f32 = 720.0;
const WINDOW_H: f32 = 520.0;

const HEAD_H: f32 = 56.0;
const FOOT_H: f32 = 66.0;
const PAD: f32 = 22.0;
/// 行内左右留白（正文不贴边）
const ROW_PAD_X: f32 = 24.0;
const ROW_PAD_Y: f32 = 13.0;
const ROW_GAP: f32 = 5.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Close,
    Copy,
    /// 按表格复制（只在认出表格时出现，占「复制原文」那个位置）
    CopyTable,
    /// 翻失败时，底栏那个按钮变成「重试」
    Retry,
    CopySource,
    ViewLines,
    ViewProse,
    ViewOcr,
    /// 认出表格时头部多出来的「表格」胶囊
    ViewTable,
    Target,
    Row(usize),
    RowCopy(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Lines,
    Prose,
    Ocr,
    /// 认出表格时的默认视图：按行列画成真表格
    Table,
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
    /// 目标语言下标（settings_ui::TARGETS）
    target: usize,
    /// 正在等什么：0 空闲 / 1 等整段译文 / 2 等按句对齐
    pending: u8,
    /// 现在这组 rows 是不是按标点对过齐的（整段翻出来的不算）
    aligned: bool,
    /// 语言下拉窗（同一时刻只开一个）
    lang_popup: HWND,
    /// 当前视图的内容总高（滚动条和滚轮都用它算）
    content_h: i32,
    /// 正在拖滚动条：按下时手指在滑块里的偏移
    scroll_drag: Option<i32>,
    /// 打字机：已显示多少个目标字
    reveal: usize,
    reveal_total: usize,
    /// 已经在等 WM_MOUSELEAVE 了
    tracking_leave: bool,
    /// 几何上认出来的表格（没有就是普通文字）；「复制表格」按钮只在有时出现
    table: Option<netspeed_core::table::Table>,
    /// 表格视图的排版缓存（正文宽度 / 缩放变了就重算）
    table_layout: Option<TableLayout>,
}

/// 表格视图的排版结果：列宽、每行高度、总高
struct TableLayout {
    width: i32,
    scale: f32,
    cols: Vec<i32>,
    rows: Vec<i32>,
    height: i32,
}

struct Fonts {
    title: HFONT,
    body: HFONT,
    source: HFONT,
    small: HFONT,
    label: HFONT,
    /// 表头用（比正文稍粗）
    table_head: HFONT,
}

impl Fonts {
    fn create(scale: f32) -> Self {
        Self {
            title: font(14.5, 600, scale),
            body: font(15.5, 400, scale),
            source: font(12.5, 400, scale),
            small: font(12.0, 400, scale),
            label: font(11.0, 500, scale),
            table_head: font(12.5, 600, scale),
        }
    }
}

/// 设置里存的目标语言 → TARGETS 下标
fn settings_target_index() -> usize {
    let target = netspeed_core::settings::Settings::load().translate_target;
    crate::settings_ui::TARGETS
        .iter()
        .position(|(_, key)| key.eq_ignore_ascii_case(target.trim()))
        .unwrap_or(0)
}

/// 打字机：为一个窗口起一条 16ms 的推帧线程，打完自己退出。
/// 不能用全局标志（多个结果窗会互相把闸门占住，后来的窗永远不打字）。
fn start_reveal(hwnd: HWND, total: usize) {
    if hwnd.is_null() || total == 0 {
        return;
    }
    unsafe {
        let state = state_of(hwnd);
        if !state.is_null() {
            (*state).reveal = 0;
            (*state).reveal_total = total;
        }
    }
    let step = (total / 56).max(2);
    let frames = (total + step - 1) / step + 1;
    let raw = hwnd as isize;
    std::thread::spawn(move || {
        for _ in 0..frames {
            std::thread::sleep(std::time::Duration::from_millis(16));
            unsafe {
                let hwnd = raw as HWND;
                if IsWindow(hwnd) == 0 || PostMessageW(hwnd, WM_APP_REVEAL, 0, 0) == 0 {
                    break;
                }
            }
        }
    });
}

/// 语言下拉的状态
struct LangPopup {
    parent: HWND,
    hover: Option<usize>,
    current: usize,
    scale: f32,
}

fn lang_state(hwnd: HWND) -> *mut LangPopup {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut LangPopup }
}

const LANG_ITEM_H: f32 = 32.0;
const LANG_PAD: f32 = 6.0;

unsafe fn lang_paint(hwnd: HWND) {
    let state = lang_state(hwnd);
    if state.is_null() {
        return;
    }
    let state = &*state;
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    if client.right <= 0 || client.bottom <= 0 {
        return;
    }
    let Some(dib) = ui::Dib::new(client.right, client.bottom) else {
        return;
    };
    let p = Painter::new(dib.dc, state.scale);
    let c = crate::theme::ui();
    p.round(
        0,
        0,
        client.right,
        client.bottom,
        p.s(10.0),
        c.surface,
        c.border,
    );

    let item_h = ui::s(LANG_ITEM_H, state.scale);
    let pad = ui::s(LANG_PAD, state.scale);
    for (index, (label, _)) in crate::settings_ui::TARGETS.iter().enumerate() {
        let top = pad + item_h * index as i32;
        let bottom = top + item_h;
        if state.hover == Some(index) {
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
            label,
            ui::font(12.0, 400, state.scale),
            if state.current == index { c.accent } else { c.text },
            pad + p.s(10.0),
            top,
            client.right - p.s(28.0),
            bottom,
            DT_LEFT,
        );
        if state.current == index {
            let ax = client.right - p.s(22.0);
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
    BitBlt(hdc, 0, 0, client.right, client.bottom, dib.dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);
}

unsafe extern "system" fn lang_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            lang_paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let state = lang_state(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            let pad = ui::s(LANG_PAD, state.scale);
            let item_h = ui::s(LANG_ITEM_H, state.scale);
            let y = ((lparam as i32) >> 16) & 0xFFFF;
            let index = if y < pad {
                None
            } else {
                Some(((y - pad) / item_h) as usize)
            };
            let index = index.filter(|index| *index < crate::settings_ui::TARGETS.len());
            if state.hover != index {
                state.hover = index;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let state = lang_state(hwnd);
            if !state.is_null() {
                let state = &*state;
                if let Some(index) = state.hover {
                    PostMessageW(state.parent, WM_APP_LANG_PICK, index, 0);
                }
            }
            DestroyWindow(hwnd);
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
            let state = lang_state(hwnd);
            if !state.is_null() {
                drop(Box::from_raw(state));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 在胶囊下面弹出语言列表
unsafe fn open_lang_popup(hwnd: HWND, state: &State, client: RECT) -> HWND {
    let l = layout(state, client);
    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(LANG_CLASS);
    let scale = state.scale;
    let wc = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(lang_proc),
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

    let width = l.target.right - l.target.left;
    let height = ui::s(LANG_PAD * 2.0, scale)
        + ui::s(LANG_ITEM_H, scale) * crate::settings_ui::TARGETS.len() as i32;
    let mut point = POINT {
        x: l.target.left,
        y: l.target.bottom + ui::s(6.0, scale),
    };
    windows_sys::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut point);

    let popup = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class.as_ptr(),
        wide("").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        point.x,
        point.y,
        width,
        height,
        hwnd,
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if popup.is_null() {
        return std::ptr::null_mut();
    }
    let popup_state = Box::into_raw(Box::new(LangPopup {
        parent: hwnd,
        hover: None,
        current: state.target.min(crate::settings_ui::TARGETS.len() - 1),
        scale,
    }));
    SetWindowLongPtrW(popup, GWLP_USERDATA, popup_state as isize);
    ui::round_corners(popup);
    ShowWindow(popup, SW_SHOW);
    SetForegroundWindow(popup);
    SetFocus(popup);
    InvalidateRect(popup, std::ptr::null(), 0);
    popup
}

/// 面板窗口（按类名找）：结果窗里换了语言要通知它落盘
unsafe fn find_panel() -> HWND {
    let mut found: HWND = std::ptr::null_mut();
    EnumWindows(Some(find_panel_win), &mut found as *mut HWND as isize);
    found
}

unsafe extern "system" fn find_panel_win(hwnd: HWND, lparam: LPARAM) -> i32 {
    let mut class = [0u16; 64];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    let name = String::from_utf16_lossy(&class[..length.max(0) as usize]);
    if name == "NetSpeedTaskbarRs.Panel" {
        *(lparam as *mut HWND) = hwnd;
        return 0;
    }
    1
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
    target: RECT,
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

    // 「目标 · Srpski (lat.)」这类长名字要放得下
    let target = RECT {
        left: view_lines.left - ui::s(12.0, scale) - ui::s(146.0, scale),
        top: chip_top,
        right: view_lines.left - ui::s(12.0, scale),
        bottom: chip_top + chip_h,
    };

    Layout {
        close,
        view_lines,
        view_prose,
        view_ocr,
        copy,
        copy_source,
        rows,
        target,
    }
}

fn contains(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

/// 量一段文字在给定宽度下要占多高（和画的时候用同一套 flag，高度才准）
/// 40 字符还没有空格就硬插一个换行（只影响显示，复制出去还是原文）
fn soft_wrap(text: &str) -> std::borrow::Cow<'_, str> {
    const MAX_RUN: usize = 40;
    let mut run = 0usize;
    let mut needed = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            run = 0;
            continue;
        }
        run += 1;
        if run > MAX_RUN {
            needed = true;
            break;
        }
    }
    if !needed {
        return std::borrow::Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len() + text.len() / MAX_RUN + 4);
    run = 0;
    for ch in text.chars() {
        if ch.is_whitespace() {
            run = 0;
            out.push(ch);
            continue;
        }
        if run == MAX_RUN {
            out.push('\n');
            run = 0;
        }
        out.push(ch);
        run += 1;
    }
    std::borrow::Cow::Owned(out)
}

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
            measure(dc, state.fonts.source, &soft_wrap(&row.source), width)
        };
        row.target_h = measure(dc, state.fonts.body, &soft_wrap(&row.target), width);
        row.top = top;
        row.height = pad_y * 2
            + row.source_h
            + if row.source_h > 0 { gap } else { 0 }
            + row.target_h;
        top += row.height;
    }
    DeleteDC(dc);
    state.content_h = state.rows.iter().map(|row| row.height).sum();
    state.max_scroll = (state.content_h - state.viewport_h).max(0);
    state.scroll = state.scroll.clamp(0, state.max_scroll);
}

/// 滚动条几何：返回 (轨道顶, 轨道底, 滑块高, 可走距离, x)
fn scrollbar_geometry(state: &State, l: &Layout) -> Option<(i32, i32, i32, i32, i32)> {
    if state.max_scroll <= 0 || state.content_h <= 0 {
        return None;
    }
    let track_top = l.rows.top + ui::s(6.0, state.scale);
    let track_bottom = l.rows.bottom - ui::s(6.0, state.scale);
    if track_bottom <= track_top {
        return None;
    }
    let visible = (l.rows.bottom - l.rows.top).max(1);
    let thumb_h = ((visible as f32 / state.content_h as f32) * (track_bottom - track_top) as f32)
        .max(ui::s(28.0, state.scale) as f32) as i32;
    let thumb_h = thumb_h.min(track_bottom - track_top);
    let travel = (track_bottom - track_top - thumb_h).max(1);
    Some((
        track_top,
        track_bottom,
        thumb_h,
        travel,
        l.rows.right + ui::s(4.0, state.scale),
    ))
}

/// 鼠标落在哪个控件上（纯函数：布局 + 命中，方便测试）
fn hit_test(state: &State, l: &Layout, point: POINT) -> Option<Hit> {
    if contains(&l.close, point) {
        return Some(Hit::Close);
    }
    if contains(&l.copy, point) {
        return Some(if state.error.is_some() {
            Hit::Retry
        } else {
            Hit::Copy
        });
    }
    if state.has_translation() {
        if contains(&l.copy_source, point) {
            return Some(Hit::CopySource);
        }
        if contains(&l.view_lines, point) {
            return Some(Hit::ViewLines);
        }
        if contains(&l.view_prose, point) {
            return Some(Hit::ViewProse);
        }
        if contains(&l.view_ocr, point) {
            return Some(Hit::ViewOcr);
        }
        if contains(&l.target, point) {
            return Some(Hit::Target);
        }
    }
    if state.table.is_some() && state.view != View::Table && contains(&l.copy_source, point) {
        return Some(Hit::CopyTable);
    }
    if !state.has_translation() && state.table.is_some() {
        if contains(&l.view_prose, point) {
            return Some(Hit::ViewTable);
        }
        if contains(&l.view_ocr, point) {
            return Some(Hit::ViewOcr);
        }
    }
    if state.view == View::Lines {
        if let Some(index) = hit_row(state, l, point) {
            // 行尾那块是「复制这句」，其余地方是选中这一行
            let right = l.rows.right - ui::s(10.0, state.scale);
            let near_copy = point.x >= right - ui::s(72.0, state.scale);
            return Some(if near_copy {
                Hit::RowCopy(index)
            } else {
                Hit::Row(index)
            });
        }
    }
    None
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

/// 头部的小胶囊：选中填 chip 色，悬停给一层淡底
unsafe fn draw_chip(
    p: &Painter,
    state: &State,
    c: crate::theme::Ui,
    rect: RECT,
    label: &str,
    active: bool,
    hovered: bool,
) {
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
        let sentences = if state.aligned {
            state.rows.len()
        } else {
            crate::translate::split_units(&state.source).len().max(1)
        };
        format!("{sentences} 句 · {} 字", state.unit_count())
    } else if state.view == View::Table {
        match state.table.as_ref() {
            Some(table) => format!(
                "{} 列 × {} 行 · {} 字",
                table.cols(),
                table.rows(),
                state.unit_count()
            ),
            None => format!("{} 字", state.unit_count()),
        }
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
        // 目标语言：点一下换语言并重译
        let label = format!(
            "目标 · {}  ▾",
            crate::settings_ui::TARGETS[state.target.min(crate::settings_ui::TARGETS.len() - 1)].0
        );
        let hovered = state.hover == Some(Hit::Target);
        p.round(
            l.target.left,
            l.target.top,
            l.target.right,
            l.target.bottom,
            p.s(7.0),
            if hovered { c.chip } else { c.surface },
            if hovered { c.chip } else { c.border },
        );
        p.text(
            &label,
            state.fonts.small,
            if hovered { c.text } else { c.text2 },
            l.target.left,
            l.target.top,
            l.target.right,
            l.target.bottom,
            DT_CENTER,
        );

        for (rect, view, label, hit) in [
            (l.view_lines, View::Lines, "逐句", Hit::ViewLines),
            (l.view_prose, View::Prose, "整段", Hit::ViewProse),
            (l.view_ocr, View::Ocr, "只取字", Hit::ViewOcr),
        ] {
            draw_chip(
                &p,
                state,
                c,
                rect,
                label,
                state.view == view,
                state.hover == Some(hit),
            );
        }
    }

    // 取字窗口认出表格时，头部换成「表格 / 文字」两粒（逐句、整段用不上）
    if !state.has_translation() && state.table.is_some() {
        for (rect, view, label, hit) in [
            (l.view_prose, View::Table, "表格", Hit::ViewTable),
            (l.view_ocr, View::Ocr, "文字", Hit::ViewOcr),
        ] {
            draw_chip(
                &p,
                state,
                c,
                rect,
                label,
                state.view == view,
                state.hover == Some(hit),
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
        View::Table => paint_table(&p, &mut *state, &l, c),
        View::Ocr => {}
    }

    // 滚动条：细长一条，贴右边缘（逐句和整段都有）
    if let Some((track_top, _bottom, thumb_h, travel, x)) = scrollbar_geometry(state, &l) {
        let offset = if state.max_scroll > 0 {
            (state.scroll as f32 / state.max_scroll as f32 * travel as f32) as i32
        } else {
            0
        };
        let dragging = state.scroll_drag.is_some();
        let color = if dragging { c.text3 } else { c.border };
        p.round(
            x,
            track_top + offset,
            x + p.s(4.0),
            track_top + offset + thumb_h,
            p.s(3.0),
            color,
            color,
        );
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

    if state.table.is_some() && !state.has_translation() && state.view != View::Table {
        // 认出表格时，「复制表格」占住「复制原文」的位置（原来那个位置在纯取字窗里是空的）
        let hovered = state.hover == Some(Hit::CopyTable);
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
            "复制表格",
            state.fonts.small,
            if hovered { c.text } else { c.text2 },
            l.copy_source.left,
            l.copy_source.top,
            l.copy_source.right,
            l.copy_source.bottom,
            DT_CENTER,
        );
    }

    let hover = state.hover == Some(Hit::Copy) || state.hover == Some(Hit::Retry);
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
    let primary = if state.error.is_some() {
        "重试"
    } else if state.view == View::Ocr {
        "复制"
    } else if state.view == View::Table {
        "复制表格"
    } else {
        "复制全部"
    };
    p.text(
        primary,
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
    let typer = state.reveal < state.reveal_total;
    let mut offset = 0usize;

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
                &soft_wrap(&row.source),
                state.fonts.source,
                c.text3,
                text_left,
                y,
                text_left + width,
                y + row.source_h,
            );
            y += row.source_h + ui::s(ROW_GAP, state.scale);
        }
        let target_text = if typer {
            let count = row.target.chars().count();
            let visible = state.reveal.saturating_sub(offset).min(count);
            row.target.chars().take(visible).collect::<String>()
        } else {
            row.target.clone()
        };
        offset += row.target.chars().count();
        p.text_block(
            &soft_wrap(&target_text),
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
        let h = measure(p.dc, state.fonts.source, &soft_wrap(&state.source), width);
        p.text_block(
            &soft_wrap(&state.source),
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
        let h = measure(p.dc, state.fonts.body, &soft_wrap(&state.translated), width);
        let shown = if state.reveal < state.reveal_total {
            state
                .translated
                .chars()
                .take(state.reveal)
                .collect::<String>()
        } else {
            state.translated.clone()
        };
        p.text_block(&soft_wrap(&shown), state.fonts.body, c.text, left, y, left + width, y + h);
    }
    if saved != 0 {
        RestoreDC(p.dc, saved);
    }
}

// ---------------------------------------------------------------- 表格视图

/// 表格正文区：左右各留 8px，返回 (左边, 可用宽度)
fn table_viewport(state: &State, l: &Layout) -> (i32, i32) {
    let pad = ui::s(8.0, state.scale);
    (l.rows.left + pad, ((l.rows.right - l.rows.left) - pad * 2).max(1))
}

unsafe fn text_width(dc: HDC, font: HFONT, text: &str) -> i32 {
    let old = SelectObject(dc, font as HGDIOBJ);
    let mut rect: RECT = std::mem::zeroed();
    let units = wide(text);
    DrawTextW(
        dc,
        units.as_ptr(),
        (units.len() - 1) as i32,
        &mut rect,
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(dc, old);
    rect.right - rect.left
}

/// 量一张表：列宽先按内容给，太宽就从最宽的那列往回收；再按列宽折行算行高
unsafe fn measure_table(
    dc: HDC,
    table: &netspeed_core::table::Table,
    fonts: &Fonts,
    avail: i32,
    scale: f32,
) -> TableLayout {
    let cols = table.cols();
    let mut layout = TableLayout {
        width: avail,
        scale,
        cols: Vec::new(),
        rows: Vec::new(),
        height: 0,
    };
    if cols == 0 || table.rows() == 0 {
        return layout;
    }

    let pad_x = ui::s(10.0, scale);
    let pad_y = ui::s(8.0, scale);
    let min_col = (avail / 2).min(ui::s(56.0, scale)).max(ui::s(36.0, scale));

    let mut widths = vec![min_col; cols];
    for (col, width) in widths.iter_mut().enumerate() {
        for row in 0..table.rows() {
            let text = table.cell(row, col);
            if text.is_empty() {
                continue;
            }
            let font = if row == 0 { fonts.table_head } else { fonts.source };
            *width = (*width).max(text_width(dc, font, text) + pad_x * 2);
        }
    }

    // 太宽：每次压最宽的那列，压到 min_col 为止；还超就让它出界被裁（列实在太多了）
    let mut total: i32 = widths.iter().sum();
    while total > avail {
        let mut widest = 0usize;
        for col in 1..cols {
            if widths[col] > widths[widest] {
                widest = col;
            }
        }
        if widths[widest] <= min_col {
            break;
        }
        let take = (total - avail).min(widths[widest] - min_col).max(1);
        widths[widest] -= take;
        total -= take;
    }
    // 没撑满：多出来的宽度平分给各列，表看起来才像表
    if total < avail {
        let extra = avail - total;
        let each = extra / cols as i32;
        for width in &mut widths {
            *width += each;
        }
        widths[0] += extra - each * cols as i32;
    }

    let mut rows = Vec::with_capacity(table.rows());
    for row in 0..table.rows() {
        let mut height = 0;
        for (col, width) in widths.iter().enumerate() {
            let text = table.cell(row, col);
            if text.is_empty() {
                continue;
            }
            let font = if row == 0 { fonts.table_head } else { fonts.source };
            let inner = (*width - pad_x * 2).max(1);
            height = height.max(measure(dc, font, &soft_wrap(text), inner));
        }
        rows.push((height + pad_y * 2).max(ui::s(26.0, scale)));
    }

    layout.height = rows.iter().sum();
    layout.cols = widths;
    layout.rows = rows;
    layout
}

/// 宽度或缩放变了才重新量；量完把总高/滚动范围一起更新
unsafe fn table_relayout(state: &mut State, dc: HDC, width: i32) {
    if state.table.is_none() {
        state.table_layout = None;
        state.content_h = 0;
        state.max_scroll = 0;
        state.scroll = 0;
        return;
    }
    let fresh = state
        .table_layout
        .as_ref()
        .is_some_and(|cached| cached.width == width && cached.scale == state.scale);
    if !fresh {
        let Some(table) = state.table.as_ref() else {
            return;
        };
        let measured = measure_table(dc, table, &state.fonts, width, state.scale);
        state.table_layout = Some(measured);
    }
    let height = state.table_layout.as_ref().map_or(0, |cached| cached.height);
    state.content_h = height;
    state.max_scroll = (height - state.viewport_h).max(0);
    state.scroll = state.scroll.clamp(0, state.max_scroll);
}

unsafe fn paint_table(p: &Painter, state: &mut State, l: &Layout, c: crate::theme::Ui) {
    let (left, width) = table_viewport(state, l);
    table_relayout(state, p.dc, width);

    let Some(table) = state.table.as_ref() else {
        return;
    };
    let Some(layout) = state.table_layout.as_ref() else {
        return;
    };
    if layout.cols.is_empty() || layout.rows.is_empty() {
        return;
    }

    let pad_x = p.s(10.0);
    let pad_y = p.s(8.0);
    let table_w: i32 = layout.cols.iter().sum();
    let top = l.rows.top + p.s(8.0) - state.scroll;

    let saved = SaveDC(p.dc);
    if saved != 0 {
        IntersectClipRect(p.dc, l.rows.left, l.rows.top, l.rows.right, l.rows.bottom);
    }

    // 表头一层淡淡的底色，和正文分开
    if let Some(header_h) = layout.rows.first() {
        if top + header_h > l.rows.top && top < l.rows.bottom {
            p.fill(left, top, left + table_w, top + header_h, c.row_tint);
        }
    }

    let mut y = top;
    for (row, row_h) in layout.rows.iter().enumerate() {
        if y + row_h < l.rows.top {
            y += row_h;
            continue;
        }
        if y > l.rows.bottom {
            break;
        }
        let mut x = left;
        for (col, col_w) in layout.cols.iter().enumerate() {
            // 竖线（第一列左边靠外框，不重复画）
            if col > 0 {
                p.fill(x, y, x + 1, y + row_h, c.border);
            }
            let text = table.cell(row, col);
            if !text.is_empty() {
                let font = if row == 0 {
                    state.fonts.table_head
                } else {
                    state.fonts.source
                };
                p.text_block(
                    &soft_wrap(text),
                    font,
                    c.text,
                    x + pad_x,
                    y + pad_y,
                    x + col_w - pad_x,
                    y + row_h - pad_y,
                );
            }
            x += col_w;
        }
        // 行底：表头那根重一点
        let ink = if row == 0 { c.text3 } else { c.border };
        p.fill(left, y + row_h, left + table_w, y + row_h + 1, ink);
        y += row_h;
    }

    // 外框（顶 + 左右）
    let bottom = y.min(l.rows.bottom);
    if top < l.rows.bottom {
        p.fill(left, top, left + table_w, top + 1, c.border);
        p.fill(left, top, left + 1, bottom, c.border);
        p.fill(left + table_w - 1, top, left + table_w, bottom, c.border);
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
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    let l = layout(state, client);

    // 先更新视口和行布局（EDIT 建失败也要有行高，否则所有行会叠在顶端）
    let width = (l.rows.right - ui::s(ROW_PAD_X, state.scale))
        - (l.rows.left + ui::s(ROW_PAD_X, state.scale));
    state.viewport_h = l.rows.bottom - l.rows.top;
    relayout_rows(state, width);
    if state.view == View::Prose {
        state.content_h = prose_height(state, width);
        state.max_scroll = (state.content_h - state.viewport_h).max(0);
        state.scroll = state.scroll.clamp(0, state.max_scroll);
    }
    if state.view == View::Table {
        // 滚动范围先在这里算好，否则滚轮第一次推不动
        let (_, table_width) = table_viewport(state, &l);
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        if !dc.is_null() {
            table_relayout(state, dc, table_width);
            DeleteDC(dc);
        }
    }

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

    // 焦点跟着视图走：EDIT 藏起来以后键盘不能死在半空
    if show {
        SetFocus(state.edit);
    } else {
        SetFocus(hwnd);
    }
}

/// 整段视图的高度（换行按实际宽度算）
unsafe fn prose_height(state: &State, width: i32) -> i32 {
    let screen = GetDC(std::ptr::null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(std::ptr::null_mut(), screen);
    if dc.is_null() {
        return 0;
    }
    let mut height = 12;
    if !state.source.is_empty() {
        height += 19 + measure(dc, state.fonts.source, &soft_wrap(&state.source), width) + 26;
    }
    if !state.translated.is_empty() {
        height += 19 + measure(dc, state.fonts.body, &soft_wrap(&state.translated), width);
    }
    height += 12;
    DeleteDC(dc);
    height
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
    if crate::ocr::copy_text(&text) {
        state.status = format!("已复制到剪贴板 · {} 字", text.chars().count());
    } else {
        state.status = "剪贴板被别的程序占着，请再试一次".to_string();
    }
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
    if crate::ocr::copy_text(&text) {
        state.selected = Some(index);
        state.status = format!("已复制第 {} 句", index + 1);
    } else {
        state.status = "剪贴板被别的程序占着，请再试一次".to_string();
    }
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
    if state.view == View::Table {
        copy_table_state(state);
        InvalidateRect(hwnd, std::ptr::null(), 0);
        return;
    }
    let text = if state.translated.is_empty() {
        state.source.clone()
    } else {
        state.translated.clone()
    };
    if crate::ocr::copy_text(&text) {
        state.status = format!("已自动复制 · {} 字", text.chars().count());
    } else {
        state.status = "剪贴板被别的程序占着，请再试一次".to_string();
    }
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

/// 按表格复制：TSV 走文本格式，另附一份 HTML 表格，Excel / WPS / Word 都能直接落格
fn copy_table_state(state: &mut State) -> bool {
    let Some(table) = state.table.as_ref() else {
        return false;
    };
    let shape = format!("{} 列 × {} 行", table.cols(), table.rows());
    let copied = crate::ocr::copy_table(table);
    state.status = if copied {
        format!("已按 {shape} 表格复制 · 可直接粘贴进 Excel")
    } else {
        "剪贴板被别的程序占着，请再试一次".to_string()
    };
    copied
}

unsafe fn copy_as_table(hwnd: HWND) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    copy_table_state(state);
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
    if crate::ocr::copy_text(&state.source) {
        state.status = "已复制原文".to_string();
    } else {
        state.status = "剪贴板被别的程序占着，请再试一次".to_string();
    }
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// 造一个没有窗口的状态：只测矩形，不碰 GDI 句柄
    fn state(scale: f32) -> State {
        State {
            edit: std::ptr::null_mut(),
            status: String::new(),
            error: None,
            fonts: Fonts::create(scale),
            scale,
            hover: None,
            edit_brush: std::ptr::null_mut(),
            edit_color: 0,
            view: View::Lines,
            source: "Latency budget. The panel samples twice a second.".to_string(),
            translated: "延迟预算。面板每秒采样两次。".to_string(),
            rows: Vec::new(),
            scroll: 0,
            max_scroll: 0,
            viewport_h: 0,
            selected: None,
            measured_width: -1,
            target: 0,
            pending: 0,
            aligned: true,
            lang_popup: std::ptr::null_mut(),
            content_h: 0,
            scroll_drag: None,
            reveal: usize::MAX,
            reveal_total: 0,
            tracking_leave: false,
            table: None,
            table_layout: None,
        }
    }

    fn rect(state: &State, client: RECT) -> Layout {
        layout(state, client)
    }

    fn inside(r: RECT, client: RECT) -> bool {
        r.left >= client.left
            && r.top >= client.top
            && r.right <= client.right
            && r.bottom <= client.bottom
            && r.right > r.left
            && r.bottom > r.top
    }

    #[test]
    fn every_control_is_inside_a_normal_window() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let st = state(scale);
            let client = RECT {
                left: 0,
                top: 0,
                right: ui::s(WINDOW_W, scale),
                bottom: ui::s(WINDOW_H, scale),
            };
            let l = rect(&st, client);
            for (name, r) in [
                ("close", l.close),
                ("copy", l.copy),
                ("copy_source", l.copy_source),
                ("view_lines", l.view_lines),
                ("view_prose", l.view_prose),
                ("view_ocr", l.view_ocr),
                ("target", l.target),
                ("rows", l.rows),
            ] {
                assert!(
                    inside(r, client),
                    "scale {scale}: {name} 出界了：({},{})-({},{}) 客户区 ({},{})-({},{})",
                    r.left,
                    r.top,
                    r.right,
                    r.bottom,
                    client.left,
                    client.top,
                    client.right,
                    client.bottom,
                );
            }
        }
    }

    #[test]
    fn header_controls_do_not_overlap() {
        let st = state(1.0);
        let client = RECT {
            left: 0,
            top: 0,
            right: ui::s(WINDOW_W, 1.0),
            bottom: ui::s(WINDOW_H, 1.0),
        };
        let l = rect(&st, client);
        // 从左到右：目标 < 逐句 < 整段 < 只取字 < 关闭
        assert!(l.target.right <= l.view_lines.left, "目标压到逐句了");
        assert!(l.view_lines.right <= l.view_prose.left, "逐句压到整段了");
        assert!(l.view_prose.right <= l.view_ocr.left, "整段压到只取字了");
        assert!(l.view_ocr.right <= l.close.left, "只取字压到关闭了");
    }

    #[test]
    fn small_window_keeps_rows_positive() {
        let st = state(1.0);
        let client = RECT {
            left: 0,
            top: 0,
            right: 420,
            bottom: 260,
        };
        let l = rect(&st, client);
        assert!(l.rows.bottom > l.rows.top, "内容区被挤没了");
        assert!(l.copy.bottom <= client.bottom, "底栏按钮出界");
    }

    #[test]
    fn hit_test_matches_the_drawn_rects() {
        let st = state(1.0);
        let client = RECT {
            left: 0,
            top: 0,
            right: ui::s(WINDOW_W, 1.0),
            bottom: ui::s(WINDOW_H, 1.0),
        };
        let l = rect(&st, client);
        let center = |r: RECT| POINT {
            x: (r.left + r.right) / 2,
            y: (r.top + r.bottom) / 2,
        };
        assert_eq!(hit_test(&st, &l, center(l.close)), Some(Hit::Close));
        assert_eq!(hit_test(&st, &l, center(l.target)), Some(Hit::Target));
        assert_eq!(
            hit_test(&st, &l, center(l.view_lines)),
            Some(Hit::ViewLines)
        );
    }
}

#[cfg(test)]
mod soft_wrap_tests {
    use super::soft_wrap;

    #[test]
    fn short_words_are_untouched() {
        let text = "Latency budget. The panel samples twice a second.";
        assert!(matches!(soft_wrap(text), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn long_url_gets_line_breaks() {
        let url = "https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let wrapped = soft_wrap(url);
        assert!(wrapped.contains('\n'), "长串要断行：{wrapped:?}");
        // 断行只是插了换行，字符顺序不能变
        let stripped: String = wrapped.chars().filter(|ch| *ch != '\n').collect();
        assert_eq!(stripped, url);
    }

    #[test]
    fn text_round_trips_when_breaks_are_removed() {
        let text = "a b 0123456789012345678901234567890123456789012345678901234567890 end";
        let wrapped = soft_wrap(text);
        let stripped: String = wrapped.chars().filter(|ch| *ch != '\n').collect();
        assert_eq!(stripped, text);
    }
}

/// 后台翻译回来的东西
struct AsyncUpdate {
    source: String,
    translated: String,
    error: Option<String>,
    pairs: Option<Vec<(String, String)>>,
    /// pairs 来自「按句编号翻译」才算对齐
    aligned: bool,
}

/// 按指定语言重译整段（点语言胶囊时调）
fn request_retranslate(hwnd: HWND, target: usize) {
    let source = unsafe {
        let state = state_of(hwnd);
        if state.is_null() {
            return;
        }
        let state = &mut *state;
        // 上一次还没回来就别再发：连点两下语言会同时跑两个请求、白花钱
        if state.pending != 0 {
            return;
        }
        state.pending = 1;
        state.status = format!(
            "正在译成 {}…",
            crate::settings_ui::TARGETS[target.min(crate::settings_ui::TARGETS.len() - 1)].0
        );
        InvalidateRect(hwnd, std::ptr::null(), 0);
        (*state).source.clone()
    };
    let mut settings = netspeed_core::settings::Settings::load();
    settings.translate_target =
        crate::settings_ui::TARGETS[target.min(crate::settings_ui::TARGETS.len() - 1)]
            .1
            .to_string();
    let raw = hwnd as isize;
    std::thread::spawn(move || {
        let update = match crate::translate::translate_pairs(&settings, &source) {
            Ok(pairs) => AsyncUpdate {
                aligned: true,
                source: source.clone(),
                translated: pairs
                    .iter()
                    .map(|(_, target)| target.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
                error: None,
                pairs: Some(pairs),
            },
            Err(error) => AsyncUpdate {
                aligned: false,
                source: source.clone(),
                translated: String::new(),
                error: Some(error),
                pairs: None,
            },
        };
        let payload = Box::into_raw(Box::new(update));
        unsafe {
            if PostMessageW(raw as HWND, WM_APP_RESULT, 0, payload as isize) == 0 {
                drop(Box::from_raw(payload));
            }
        }
    });
}

/// 切「逐句」时把整段按句拆开，再对照翻一次（保证一句对一句）
fn request_align(hwnd: HWND) {
    unsafe {
        let state = state_of(hwnd);
        if state.is_null() {
            return;
        }
        let state = &mut *state;
        if state.pending != 0 {
            return;
        }
        state.pending = 2;
        state.status = "正在按句拆分并对照翻译…".to_string();
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
    let target = unsafe { (*state_of(hwnd)).target };
    request_retranslate(hwnd, target);
}

/// 把一次后台结果装进窗口
unsafe fn apply_update(hwnd: HWND, update: AsyncUpdate) {
    let state = state_of(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    state.pending = 0;
    if !update.source.is_empty() {
        state.source = update.source.clone();
    }
    match update.error {
        Some(error) => {
            state.error = Some(error.clone());
            state.status = error;
            state.reveal = usize::MAX;
            state.reveal_total = 0;
            // 旧译文不能再留在屏幕上（否则「复制全部」拷的是上一次的结果）
            state.translated.clear();
            state.rows.clear();
            state.selected = None;
            state.measured_width = -1;
            state.view = View::Ocr;
        }
        None => {
            state.error = None;
            state.aligned = update.aligned;
            state.translated = update.translated.clone();
            state.rows.clear();
            if let Some(pairs) = update.pairs {
                for (source, target) in pairs {
                    state.rows.push(Row {
                        source,
                        target,
                        top: 0,
                        height: 0,
                        source_h: 0,
                        target_h: 0,
                    });
                }
            }
            state.measured_width = -1;
            state.scroll = 0;
            state.selected = None;
            if state.view == View::Ocr && !state.rows.is_empty() {
                state.view = match LAST_VIEW.load(std::sync::atomic::Ordering::Relaxed) {
                    1 => View::Prose,
                    _ => View::Lines,
                };
            }
            state.status = format!("已自动复制 · {} 字", state.unit_count());
            if !state.translated.is_empty() {
                crate::ocr::copy_text(&state.translated);
            }
            let total = state.translated.chars().count();
            start_reveal(hwnd, total);
        }
    }
    layout_edit(hwnd);
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

fn open_inner(
    source: Option<&str>,
    translated: Option<&str>,
    table: Option<&netspeed_core::table::Table>,
    error: Option<&str>,
) {
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

        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
        let max_w = (work.right - work.left - ui::s(24.0, scale)).max(ui::s(420.0, scale));
        let max_h = (work.bottom - work.top - ui::s(24.0, scale)).max(ui::s(320.0, scale));
        let width = ui::s(WINDOW_W, scale).min(max_w);
        let height = ui::s(WINDOW_H, scale).min(max_h);
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
        } else if table.is_some() {
            // 认出表格就直接按表格排，别让人再看一遍挤在一起的文字
            View::Table
        } else {
            View::Ocr
        };
        let status = match error {
            Some(message) => message.to_string(),
            None => match (table, translated_text.is_empty()) {
                // 三列以上基本不可能是分栏正文，直接按表格进剪贴板
                (Some(grid), _) if grid.cols() >= 3 => format!(
                    "已按 {} 列 × {} 行表格复制 · 可直接粘贴进 Excel",
                    grid.cols(),
                    grid.rows()
                ),
                // 两列可能是并排的两栏文字，不擅自改默认粘贴：给按钮，让人自己点
                (Some(grid), _) => format!(
                    "识别到 {} 列 × {} 行 · 点「复制表格」粘贴",
                    grid.cols(),
                    grid.rows()
                ),
                (None, true) => format!(
                    "已自动复制 · {} 字",
                    source_text.chars().filter(|ch| !ch.is_whitespace()).count()
                ),
                (None, false) => format!(
                    "已自动复制 · {} 字",
                    translated_text
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .count()
                ),
            },
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
            target: settings_target_index(),
            pending: 0,
            aligned: false,
            lang_popup: std::ptr::null_mut(),
            content_h: 0,
            scroll_drag: None,
            reveal: usize::MAX,
            reveal_total: 0,
            tracking_leave: false,
            table: table.cloned(),
            table_layout: None,
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
        LAST_HWND.with(|cell| cell.set(hwnd as isize));

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
                | 0x0040  // ES_AUTOVSCROLL（不要 ES_AUTOHSCROLL：长行要折，不要横向滚）
                | 0x0800, // ES_READONLY：结果窗的原文不该被改，改了会和自绘视图对不上
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
            let copied = match table {
                Some(grid) if grid.cols() >= 3 => crate::ocr::copy_table(grid),
                _ => crate::ocr::copy_text(to_copy),
            };
            if !copied {
                (*state).status = "剪贴板被别的程序占着，点「复制」再试".to_string();
            }
        }

        ui::round_corners(hwnd);
        layout_edit(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        if (*state).view == View::Ocr {
            SetFocus(edit);
            // 停在开头：EM_SETSEL(0,-1) 会把视图滚到文末
            SendMessageW(edit, EM_SETSEL, 0, 0);
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
            // 头部只有「空白处」可以拖窗口；按钮和胶囊要能点
            let on_header = point.y < ui::s(HEAD_H, (*state).scale);
            let on_widget = contains(&l.close, point)
                || ((*state).has_translation()
                    && (contains(&l.view_lines, point)
                        || contains(&l.view_prose, point)
                        || contains(&l.view_ocr, point)
                        || contains(&l.target, point)));
            if on_header && !on_widget {
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
            if let Some(grab) = state.scroll_drag {
                let mut client: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut client);
                let l = layout(state, client);
                if let Some((track_top, _bottom, _thumb_h, travel, _x)) = scrollbar_geometry(state, &l)
                {
                    let y = ((lparam as i32) >> 16) & 0xFFFF;
                    let thumb_top = (y - grab).clamp(track_top, track_top + travel);
                    let ratio = (thumb_top - track_top) as f32 / travel as f32;
                    let next = (ratio * state.max_scroll as f32).round() as i32;
                    if next != state.scroll {
                        state.scroll = next.clamp(0, state.max_scroll);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
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
            let l = layout(state, client);

            let hover = hit_test(state, &l, point);

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
        WM_APP_LANG_PICK => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            let index = wparam.min(crate::settings_ui::TARGETS.len() - 1);
            state.lang_popup = std::ptr::null_mut();
            // 记进设置：下次截图翻译直接用这个语言（面板负责写盘）
            let panel = find_panel();
            if !panel.is_null() {
                PostMessageW(panel, WM_APP_OCR_TARGET, index, 0);
            }
            if index != state.target {
                state.target = index;
                request_retranslate(hwnd, index);
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_APP_REVEAL => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let state = &mut *state;
            if state.reveal_total == 0 || state.reveal >= state.reveal_total {
                state.reveal = state.reveal_total;
                return 0;
            }
            // 约 0.9 秒打完：每帧补 total/56 个字，最少 2 个
            let step = (state.reveal_total / 56).max(2);
            state.reveal = (state.reveal + step).min(state.reveal_total);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_APP_RESULT => {
            if lparam == 0 {
                return 0;
            }
            let update = Box::from_raw(lparam as *mut AsyncUpdate);
            apply_update(hwnd, *update);
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
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut client);
            let l = layout(state, client);
            if let Some((track_top, _bottom, thumb_h, travel, x)) = scrollbar_geometry(state, &l) {
                // 命中区域放宽到 10px：4px 的条子太细，不好抓
                if point.x >= x - ui::s(6.0, state.scale) && point.x <= x + ui::s(10.0, state.scale) {
                    let offset = if state.max_scroll > 0 {
                        (state.scroll as f32 / state.max_scroll as f32 * travel as f32) as i32
                    } else {
                        0
                    };
                    let thumb_top = track_top + offset;
                    let grab = (point.y - thumb_top).clamp(0, thumb_h);
                    let _ = thumb_h;
                    state.scroll_drag = Some(grab);
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
            }
            0
        }
        WM_LBUTTONUP => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            if (*state).scroll_drag.is_some() {
                (*state).scroll_drag = None;
                windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                InvalidateRect(hwnd, std::ptr::null(), 0);
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
            // 点了别的控件 / 空白：先把下拉收掉（胶囊自己除外，它负责开关）
            let popup = (*state).lang_popup;
            if !popup.is_null() && IsWindow(popup) != 0 && !contains(&l.target, point) {
                DestroyWindow(popup);
                (*state).lang_popup = std::ptr::null_mut();
            }
            if contains(&l.close, point) {
                DestroyWindow(hwnd);
                return 0;
            }
            if contains(&l.copy, point) {
                if (*state).error.is_some() {
                    // 翻失败：再来一次，不用重新截图
                    (*state).status = "正在重试…".to_string();
                    (*state).error = None;
                    request_retranslate(hwnd, (*state).target);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else {
                    copy_all(hwnd);
                }
                return 0;
            }
            if (*state).table.is_some()
                && (*state).view != View::Table
                && contains(&l.copy_source, point)
            {
                copy_as_table(hwnd);
                return 0;
            }
            if !(*state).has_translation() && (*state).table.is_some() {
                if contains(&l.view_prose, point) {
                    (*state).view = View::Table;
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
            if (*state).has_translation() {
                if contains(&l.copy_source, point) {
                    copy_source(hwnd);
                    return 0;
                }
                if contains(&l.view_lines, point) {
                    (*state).view = View::Lines;
                    LAST_VIEW.store(0, std::sync::atomic::Ordering::Relaxed);
                    (*state).scroll = 0;
                    // 整段翻出来的 rows 是「按行」的，不是按句：先按标点拆开再对照翻一次
                    if !(*state).aligned && crate::translate::split_units(&(*state).source).len() > 1 {
                        request_align(hwnd);
                    }
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
                if contains(&l.target, point) {
                    // 弹下拉挑语言（点一下换一个太容易点错）
                    let popup = (*state).lang_popup;
                    if !popup.is_null() && IsWindow(popup) != 0 {
                        DestroyWindow(popup);
                        (*state).lang_popup = std::ptr::null_mut();
                    } else {
                        (*state).lang_popup = std::ptr::null_mut();
                        let flags = &*state;
                        (*state).lang_popup = open_lang_popup(hwnd, flags, client);
                    }
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
                    || ((*state).table.is_some()
                        && (*state).view != View::Table
                        && contains(&l.copy_source, point))
                    || (!(*state).has_translation()
                        && (*state).table.is_some()
                        && (contains(&l.view_prose, point) || contains(&l.view_ocr, point)))
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
                    // 下拉开着就先收下拉，别一下把窗口关了
                    let popup = state.lang_popup;
                    if !popup.is_null() && IsWindow(popup) != 0 {
                        DestroyWindow(popup);
                        state.lang_popup = std::ptr::null_mut();
                    } else {
                        DestroyWindow(hwnd);
                    }
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
                // 1 / 2 / 3：逐句 / 整段 / 只取字（认出表格时是 1 表格 / 2 文字）
                0x31 | 0x32 | 0x33 => {
                    if !state.has_translation() && state.table.is_some() {
                        let next = match wparam as u32 {
                            0x31 => View::Table,
                            _ => View::Ocr,
                        };
                        if state.view != next {
                            state.view = next;
                            state.scroll = 0;
                            layout_edit(hwnd);
                            InvalidateRect(hwnd, std::ptr::null(), 0);
                        }
                    } else if state.has_translation() {
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
                            View::Ocr | View::Table => {}
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
        WM_DPICHANGED => {
            let state = state_of(hwnd);
            if state.is_null() {
                return 0;
            }
            let dpi = ((wparam >> 16) & 0xFFFF) as u32;
            let scale = (dpi.max(96) as f32) / 96.0;
            let state = &mut *state;
            state.scale = scale;
            state.fonts = Fonts::create(scale);
            state.measured_width = -1;
            let suggested = lparam as *const RECT;
            if !suggested.is_null() {
                let rect = *suggested;
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            layout_edit(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_SETTINGCHANGE => {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_DESTROY => {
            if LAST_HWND.with(|cell| cell.get()) == hwnd as isize {
                LAST_HWND.with(|cell| cell.set(0));
            }
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
    if view == "table" || view == "table-ocr" {
        let table = preview_table();
        open(std::ptr::null_mut(), &table.tsv(), Some(&table), None);
    } else {
        open_translate(std::ptr::null_mut(), source, translated, None);
    }

    // 找到刚创建的那个窗口，让它把第一帧画完
    let mut message: MSG = std::mem::zeroed();
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
    let hwnd: HWND = found;
    if hwnd.is_null() {
        return false;
    }
    // 按名字切到指定视图再截（"default" = 不动，就用真实默认视图）
    if view != "default" {
        let state = state_of(hwnd);
        if !state.is_null() {
            (*state).view = match view {
                "prose" => View::Prose,
                "ocr" | "table-ocr" => View::Ocr,
                "table" => View::Table,
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

/// 预览用：造一张 4 列 × 4 行的示例表（走真实的 detect，不是手搓 Table）
fn preview_table() -> netspeed_core::table::Table {
    use netspeed_core::table::{detect, LineBox};

    let cells: [[&str; 4]; 4] = [
        ["姓名", "部门", "工号", "本月工时"],
        ["张三", "平台研发", "A-1024", "168.5"],
        ["李四", "前端体验组（含设计）", "A-2048", "152"],
        ["王五", "数据平台", "A-3072", "176.5"],
    ];
    let columns = [16.0f32, 130.0, 330.0, 450.0];
    let mut lines = Vec::new();
    for (row, texts) in cells.iter().enumerate() {
        let top = 16.0 + row as f32 * 40.0;
        for (col, text) in texts.iter().enumerate() {
            let left = columns[col];
            lines.push(LineBox {
                text: (*text).to_string(),
                left,
                top,
                right: left + 96.0,
                bottom: top + 26.0,
            });
        }
    }
    detect(&lines).expect("预览用的样例本身就得是表格")
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

pub fn open(
    _parent: HWND,
    text: &str,
    table: Option<&netspeed_core::table::Table>,
    error: Option<&str>,
) {
    open_inner(Some(text), None, table, error);
}

/// 翻译结果：原文 + 译文分开给，窗口自己决定怎么排
pub fn open_translate(_parent: HWND, source: &str, translated: &str, error: Option<&str>) {
    open_inner(Some(source), Some(translated), None, error);
}

/// OCR 出来就先开窗（只有原文 + 「翻译中…」），别让人对着空白等几秒
pub fn open_pending(source: &str) {
    // 同一时刻只留一个结果窗：旧窗先关掉，免得两次 OCR 的译文串到同一个窗里
    let previous = last_hwnd_alive();
    if !previous.is_null() {
        unsafe {
            DestroyWindow(previous);
        }
    }
    open_inner(Some(source), None, None, None);
    // 标记成「等译文」状态
    unsafe {
        let hwnd = last_hwnd_alive();
        if hwnd.is_null() {
            return;
        }
        let state = state_of(hwnd);
        if state.is_null() {
            return;
        }
        let state = &mut *state;
        state.pending = 1;
        state.status = "正在翻译…".to_string();
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

/// 译文回来了：填进已经开着的窗口（带打字机效果）
pub fn update_translation(
    source: &str,
    translated: &str,
    pairs: Option<Vec<(String, String)>>,
    error: Option<&str>,
) {
    let hwnd = last_hwnd_alive();
    if hwnd.is_null() {
        open_inner(Some(source), Some(translated), None, error);
        return;
    }
    unsafe {
        let pairs = pairs.or_else(|| build_pairs(source, translated));
        let update = AsyncUpdate {
            aligned: false,
            source: source.to_string(),
            translated: translated.to_string(),
            error: error.map(|error| error.to_string()),
            pairs,
        };
        apply_update(hwnd, update);
    }
}

thread_local! {
    /// 最近一次创建的取字窗（异步结果回来时找它）
    static LAST_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

fn last_hwnd() -> HWND {
    LAST_HWND.with(|cell| cell.get() as HWND)
}

/// 记着的那个窗口还在不在（关掉以后指针就是野的了，用之前必须问一句）
fn last_hwnd_alive() -> HWND {
    let hwnd = last_hwnd();
    if hwnd.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        if IsWindow(hwnd) == 0 {
            LAST_HWND.with(|cell| cell.set(0));
            return std::ptr::null_mut();
        }
        // 句柄可能已经被别的窗口复用：类名对不上就不是我们那个
        let expected = wide(CLASS_NAME);
        let mut buffer = [0u16; 64];
        let length = GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        let name = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
        if name != CLASS_NAME {
            let _ = expected;
            LAST_HWND.with(|cell| cell.set(0));
            return std::ptr::null_mut();
        }
        hwnd
    }
}

/// 原文 / 译文按行配对；行数对不上就整段算一句
fn build_pairs(source: &str, translated: &str) -> Option<Vec<(String, String)>> {
    if translated.trim().is_empty() {
        return None;
    }
    let source_lines: Vec<&str> = source
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    let target_lines: Vec<&str> = translated
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    if !source_lines.is_empty() && source_lines.len() == target_lines.len() {
        return Some(
            source_lines
                .iter()
                .zip(target_lines.iter())
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        );
    }
    if target_lines.is_empty() {
        return None;
    }
    Some(vec![(
        source_lines.join(" "),
        target_lines.join(" "),
    )])
}
