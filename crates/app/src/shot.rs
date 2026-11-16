//! 轻量截图 + 标注（自研，纯 Win32/GDI）。
//!
//! 交互参考 Snow Shot，但代码全部自己写，不引入其 GPL-3.0 实现：
//!   Ctrl+Shift+A / 右键菜单「截图…」
//!   → 全屏遮罩选区（带尺寸提示）
//!   → 编辑窗（箭头 / 矩形 / 画笔 / 文字 / 马赛克，撤销、复制、保存、贴图）
//!
//! 画面全部走 GDI 双缓冲；贴图窗口是置顶无边框窗口。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetStockObject, InvalidateRect, LineTo, MoveToEx, Polygon, Polyline, ReleaseDC, SelectObject,
    ScreenToClient, SetBkColor, SetBkMode, SetStretchBltMode, SetTextColor, SetViewportOrgEx, StretchBlt, TextOutW, BITMAPINFO,
    BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, COLORONCOLOR, DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    HALFTONE, HBITMAP, HDC, HFONT, HGDIOBJ, NULL_BRUSH, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
    UpdateWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::SystemInformation::GetLocalTime as GetLocalTime2;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;

const DT_NOPREFIX: u32 = 0x0800;
const CF_DIB: u32 = 8;
const CAPTUREBLT: u32 = 0x4000_0000;

const OVERLAY_CLASS: &str = "NetSpeedTaskbarRs.ShotOverlay";
const EDITOR_CLASS: &str = "NetSpeedTaskbarRs.ShotEditor";
const PIN_CLASS: &str = "NetSpeedTaskbarRs.ShotPin";

use crate::WM_APP_SHOT_FIRE;

/// 标记：截图/编辑进行中（面板的自动显示要让路）。
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 设置里的取字（OCR）开关：关掉后截图工具栏不出现 OCR，热键也不会触发
static OCR_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_ocr_enabled(enabled: bool) {
    OCR_ENABLED.store(enabled, Ordering::Relaxed);
}

fn ocr_enabled() -> bool {
    OCR_ENABLED.load(Ordering::Relaxed)
}

static mut PANEL: HWND = std::ptr::null_mut();
static mut RESTORE_PANEL: bool = false;

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// 选区确认后做什么：进编辑窗，还是直接取字
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Edit,
    Ocr,
}

static mut MODE: Mode = Mode::Edit;

/// 触发一次截图：先把任务栏面板藏起来（别把自己拍进去），稍等一下再抓屏。
pub fn request(panel: HWND) {
    start(panel, Mode::Edit);
}

/// 截图后直接取字（PaddleOCR）
pub fn request_ocr(panel: HWND) {
    if !ocr_enabled() {
        return;
    }
    start(panel, Mode::Ocr);
}

fn start(panel: HWND, mode: Mode) {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }

    unsafe {
        MODE = mode;
        PANEL = panel;
        RESTORE_PANEL = IsWindowVisible(panel) != 0;
        if RESTORE_PANEL {
            ShowWindow(panel, SW_HIDE);
        }
        // 用户从哪块屏开始截图，整套窗口（遮罩/编辑器/贴图）就按那块屏的缩放
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        use_scale_at(cursor);
    }

    // 让 DWM 先完成「面板消失」的合成，再抓屏，否则截图里会残留半个面板
    // 让 DWM 先完成「面板消失」的合成，再抓屏，否则截图里会残留半个面板
    let handle = panel as usize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(140));
        unsafe {
            PostMessageW(handle as HWND, WM_APP_SHOT_FIRE, 0, 0);
        }
    });
}
/// 选区确认：按模式进编辑窗或直接取字
unsafe fn deliver(panel: HWND, bitmap: Bitmap) {
    if MODE == Mode::Ocr {
        crate::ocr::recognize_async(panel, bitmap);
    } else {
        open_editor(panel, bitmap);
    }
}

unsafe fn finish_capture() {
    ACTIVE.store(false, Ordering::SeqCst);
    if RESTORE_PANEL && !PANEL.is_null() && IsWindow(PANEL) != 0 {
        ShowWindow(PANEL, SW_SHOWNOACTIVATE);
    }
    RESTORE_PANEL = false;
}

// ---------------------------------------------------------------- 位图

#[derive(Clone)]
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    /// BGRA，行序自顶向下
    pub pixels: Vec<u8>,
}

impl Bitmap {
    fn new(width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width * height * 4).max(1) as usize],
        }
    }

    fn crop(&self, x: i32, y: i32, w: i32, h: i32) -> Bitmap {
        let mut out = Bitmap::new(w, h);
        for row in 0..h {
            let src = ((y + row) * self.width + x) as usize * 4;
            let dst = row as usize * 4 * w as usize;
            let len = w as usize * 4;
            if src + len <= self.pixels.len() && dst + len <= out.pixels.len() {
                out.pixels[dst..dst + len].copy_from_slice(&self.pixels[src..src + len]);
            }
        }
        out
    }
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

fn capture_rect(x: i32, y: i32, width: i32, height: i32) -> Option<Bitmap> {
    if width <= 0 || height <= 0 {
        return None;
    }

    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let (dc, dib, bits) = make_dib(width, height);
        if dib.is_null() || dc.is_null() {
            if !dc.is_null() {
                DeleteDC(dc);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
            return None;
        }

        let ok = BitBlt(dc, 0, 0, width, height, screen, x, y, SRCCOPY | CAPTUREBLT) != 0;
        let mut bitmap = Bitmap::new(width, height);
        if ok && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits, bitmap.pixels.as_mut_ptr(), bitmap.pixels.len());
        }

        DeleteObject(dib as *mut _);
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);

        if ok {
            Some(bitmap)
        } else {
            None
        }
    }
}

/// 把位图画到任意 DC（1:1 或按 scale 缩放）。
unsafe fn blit_bitmap(dc: HDC, bitmap: &Bitmap, left: i32, top: i32, width: i32, height: i32) {
    let (mem_dc, dib, bits) = make_dib(bitmap.width, bitmap.height);
    if dib.is_null() {
        return;
    }
    if !bits.is_null() {
        std::ptr::copy_nonoverlapping(bitmap.pixels.as_ptr(), bits, bitmap.pixels.len());
    }

    SetStretchBltMode(dc, HALFTONE);
    StretchBlt(
        dc,
        left,
        top,
        width,
        height,
        mem_dc,
        0,
        0,
        bitmap.width,
        bitmap.height,
        SRCCOPY,
    );

    DeleteObject(dib as *mut _);
    DeleteDC(mem_dc);
}

/// 这套窗口（截图遮罩 / 编辑器 / 贴图）所在屏的缩放，打开时定下来
static UI_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn ui_scale() -> f32 {
    let bits = UI_SCALE.load(std::sync::atomic::Ordering::Relaxed);
    if bits == 0 {
        crate::screen_scale()
    } else {
        f32::from_bits(bits)
    }
}

/// 按某个屏幕坐标所在显示器定缩放（多屏缩放不同时，窗口开在哪就按哪算）
fn use_scale_at(point: POINT) {
    UI_SCALE.store(
        crate::scale_at(point).to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn s(value: f32) -> i32 {
    (value * ui_scale()).round() as i32
}

fn make_font(points: f32, weight: i32) -> HFONT {
    let height = -((points * ui_scale() * 96.0 / 72.0).round() as i32);
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

thread_local! {
    /// 字体缓存。CreateFontW 要建一份字库，拖拽时每帧都建一次会明显发顿。
    /// 缓存里的句柄常驻，调用方**不要** DeleteObject。
    static FONTS: RefCell<HashMap<(i32, i32), HFONT>> = RefCell::new(HashMap::new());
}

fn cached_font(points: f32, weight: i32) -> HFONT {
    let key = ((points * 10.0).round() as i32, weight);
    FONTS.with(|cell| {
        if let Some(handle) = cell.borrow().get(&key) {
            return *handle;
        }
        let handle = make_font(points, weight);
        cell.borrow_mut().insert(key, handle);
        handle
    })
}

/// 把位图拷进一个常驻的内存 DC（只做一次，之后每帧直接 blit）。
unsafe fn source_dc(bitmap: &Bitmap) -> Option<(HDC, HBITMAP)> {
    let (dc, dib, bits) = make_dib(bitmap.width, bitmap.height);
    if dc.is_null() || dib.is_null() {
        return None;
    }
    if !bits.is_null() {
        std::ptr::copy_nonoverlapping(bitmap.pixels.as_ptr(), bits, bitmap.pixels.len());
    }
    Some((dc, dib))
}

/// 从常驻 DC 贴到目标：1:1 用 BitBlt（最快），缩放才走 HALFTONE StretchBlt。
unsafe fn blit_cached(
    dc: HDC,
    source: HDC,
    src_w: i32,
    src_h: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
) {
    if src_w == width && src_h == height {
        BitBlt(dc, left, top, width, height, source, 0, 0, SRCCOPY);
    } else {
        SetStretchBltMode(dc, HALFTONE);
        StretchBlt(dc, left, top, width, height, source, 0, 0, src_w, src_h, SRCCOPY);
    }
}

// ---------------------------------------------------------------- 选区遮罩

struct Overlay {
    shot: Bitmap,
    dimmed: Vec<u8>,
    origin: (i32, i32),
    start: Option<POINT>,
    /// 按下但还没拖动：用来区分「单击截窗口」和「拖框选区」
    press: Option<POINT>,
    current: POINT,
    /// 鼠标下自动聚焦的窗口（截图坐标）：没拖动时就是默认选区
    hover: Option<RECT>,
    /// 常驻的整屏画布：每帧只改动选区那几行，不再整屏重新分配 + 拷 14 MB
    canvas: Option<(HDC, HBITMAP, *mut u8)>,
    /// 上一帧的选区（用来把那些行恢复成压暗版本）
    last_rect: Option<RECT>,
}

/// 把整屏画布上某块矩形恢复成「压暗版」像素。
unsafe fn restore_rows(bits: *mut u8, dimmed: &[u8], width: i32, height: i32, rect: RECT) {
    let top = rect.top.max(0).min(height);
    let bottom = rect.bottom.max(0).min(height);
    let left = rect.left.max(0).min(width);
    let right = rect.right.max(0).min(width);
    if right <= left || bottom <= top {
        return;
    }

    let len = (right - left) as usize * 4;
    for row in top..bottom {
        let offset = (row * width) as usize * 4 + left as usize * 4;
        if offset + len <= dimmed.len() {
            std::ptr::copy_nonoverlapping(dimmed.as_ptr().add(offset), bits.add(offset), len);
        }
    }
}

unsafe fn dim_pixels(shot: &Bitmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(shot.pixels.len());
    for pixel in shot.pixels.chunks_exact(4) {
        // 灰度 + 压暗：截图外区域变暗，选区保持原样更醒目
        let gray = (pixel[0] as u32 * 30 + pixel[1] as u32 * 59 + pixel[2] as u32 * 11) / 100;
        let v = (gray * 42 / 100) as u8;
        out.extend_from_slice(&[v, v, v, 255]);
    }
    out
}

fn selection(overlay: &Overlay) -> Option<RECT> {
    // 还没开始拖：用鼠标下自动聚焦的窗口当默认选区（微信那种「点一下截这个窗口」）
    let Some(start) = overlay.start else {
        return overlay.hover;
    };
    let left = start.x.min(overlay.current.x).max(0);
    let top = start.y.min(overlay.current.y).max(0);
    let right = start.x.max(overlay.current.x).min(overlay.shot.width);
    let bottom = start.y.max(overlay.current.y).min(overlay.shot.height);
    if right - left < 2 || bottom - top < 2 {
        return None;
    }
    Some(RECT {
        left,
        top,
        right,
        bottom,
    })
}

// ---------------------------------------------------------------- 窗口自动聚焦

/// 截图自己的窗口和 shell 的桌面/任务栏不算「目标窗口」。
fn is_not_target(class: &str) -> bool {
    class == OVERLAY_CLASS
        || class == EDITOR_CLASS
        || class == PIN_CLASS
        || class == "NetSpeedTaskbarRs.Panel"
        || class == "NetSpeedTaskbarRs.Hint"
        || class == "NetSpeedTaskbarRs.Menu"
        || class == "Shell_TrayWnd"
        || class == "Shell_SecondaryTrayWnd"
        || class == "Progman"
        || class == "WorkerW"
}

struct WindowQuery {
    point: POINT,
    found: Option<RECT>,
}

/// 鼠标下那个窗口的屏幕矩形（微信那种「自动聚焦窗口」）。
///
/// EnumWindows 按 z-order 从上到下枚举，第一个包含鼠标点、可见且没被 DWM
/// 藏起来的顶层窗口，就是用户实际看到的那个；跳过桌面、任务栏和截图自己的窗口。
unsafe fn window_rect_under(point: POINT) -> Option<RECT> {
    let mut query = WindowQuery {
        point,
        found: None,
    };
    EnumWindows(
        Some(find_window_under),
        &mut query as *mut WindowQuery as isize,
    );
    query.found
}

unsafe extern "system" fn find_window_under(hwnd: HWND, lparam: LPARAM) -> i32 {
    let query = &mut *(lparam as *mut WindowQuery);

    if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
        return 1;
    }

    let mut class = [0u16; 128];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
    let name = String::from_utf16_lossy(&class[..length]);
    if is_not_target(&name) {
        return 1;
    }

    // 点击穿透的覆盖层（自己画的提示条之类）不是用户想截的对象
    if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT != 0 {
        return 1;
    }

    // UWP 切到后台后窗口还在 z-order 里，但 DWM 已经把它藏了
    let mut cloaked = 0_u32;
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED as u32,
        &mut cloaked as *mut u32 as *mut c_void,
        std::mem::size_of::<u32>() as u32,
    ) == 0
        && cloaked != 0
    {
        return 1;
    }

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if GetWindowRect(hwnd, &mut rect) == 0 {
        return 1;
    }

    if query.point.x < rect.left
        || query.point.x >= rect.right
        || query.point.y < rect.top
        || query.point.y >= rect.bottom
    {
        return 1;
    }

    if rect.right - rect.left < 16 || rect.bottom - rect.top < 16 {
        return 1;
    }

    query.found = Some(rect);
    0 // 找到就停：EnumWindows 是按 z-order 从上层往下的
}

/// 屏幕矩形 → 截图坐标（多屏时截图原点可能不是 0,0）
fn shot_rect(rect: RECT, origin: (i32, i32), width: i32, height: i32) -> Option<RECT> {
    let left = (rect.left - origin.0).clamp(0, width);
    let top = (rect.top - origin.1).clamp(0, height);
    let right = (rect.right - origin.0).clamp(0, width);
    let bottom = (rect.bottom - origin.1).clamp(0, height);
    if right - left < 2 || bottom - top < 2 {
        return None;
    }
    Some(RECT {
        left,
        top,
        right,
        bottom,
    })
}

fn rect_key(rect: Option<RECT>) -> Option<(i32, i32, i32, i32)> {
    rect.map(|r| (r.left, r.top, r.right, r.bottom))
}

unsafe fn overlay_paint(hwnd: HWND) {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
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
    let width = state.shot.width;
    let height = state.shot.height;

    let (dc, dib, bits) = match state.canvas {
        Some((dc, dib, bits)) => (dc, dib, bits),
        None => {
            let (dc, dib, bits) = make_dib(width, height);
            if dib.is_null() {
                return;
            }
            if !bits.is_null() {
                // 只铺一次压暗版
                std::ptr::copy_nonoverlapping(state.dimmed.as_ptr(), bits, state.dimmed.len());
            }
            state.canvas = Some((dc, dib, bits));
            (dc, dib, bits)
        }
    };
    let _ = dib;

    if !bits.is_null() {
        // 先把上一帧的选区恢复成压暗版，再把这一帧的选区还原成原始像素
        if let Some(rect) = state.last_rect {
            restore_rows(bits, &state.dimmed, width, height, rect);
        }

        let current = selection(state);
        if let Some(rect) = current {
            for row in rect.top..rect.bottom {
                let offset = (row * width) as usize * 4;
                let from = offset + rect.left as usize * 4;
                let len = (rect.right - rect.left) as usize * 4;
                std::ptr::copy_nonoverlapping(
                    state.shot.pixels.as_ptr().add(from),
                    bits.add(from),
                    len,
                );
            }
        }
        state.last_rect = current;
    }

    // 选区边框 + 尺寸
    if let Some(rect) = selection(state) {
        let pen = CreatePen(0, s(1.0), 0x00FF_FFFF);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        let brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(dc, brush);
        windows_sys::Win32::Graphics::Gdi::Rectangle(dc, rect.left, rect.top, rect.right, rect.bottom);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(pen as *mut _);

        let label = format!("{} × {}", rect.right - rect.left, rect.bottom - rect.top);
        let font = cached_font(9.0, 700);
        let old_font = SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);

        let mut label_rect = RECT {
            left: rect.left,
            top: (rect.top - s(24.0)).max(0),
            right: rect.right,
            bottom: (rect.top - s(24.0)).max(0) + s(20.0),
        };
        DrawTextW(
            dc,
            wide(&label).as_ptr(),
            -1,
            &mut label_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );

        SelectObject(dc, old_font);
    }

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, width, height, dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
            if !state.is_null() {
                let mut point = POINT { x: 0, y: 0 };
                GetCursorPos(&mut point);
                let state = &mut *state;
                let press = POINT {
                    x: (point.x - state.origin.0).clamp(0, state.shot.width),
                    y: (point.y - state.origin.1).clamp(0, state.shot.height),
                };
                // 先不急着开始拖：没移动就是「单击截取高亮窗口」
                state.press = Some(press);
                state.current = press;
                state.start = None;
                SetCapture(hwnd);
            }
            0
        }
        WM_MOUSEMOVE => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
            if !state.is_null() {
                let mut point = POINT { x: 0, y: 0 };
                GetCursorPos(&mut point);
                let state = &mut *state;
                let current = POINT {
                    x: (point.x - state.origin.0).clamp(0, state.shot.width),
                    y: (point.y - state.origin.1).clamp(0, state.shot.height),
                };

                if let Some(press) = state.press {
                    // 移动超过阈值才算框选，否则这次按下/抬起就是「点选窗口」
                    if state.start.is_none() {
                        let dx = current.x - press.x;
                        let dy = current.y - press.y;
                        let threshold = s(3.0).max(2);
                        if dx * dx + dy * dy >= threshold * threshold {
                            state.start = Some(press);
                        }
                    }
                    if state.start.is_some() {
                        state.current = current;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else {
                    // 没按下：跟着鼠标换高亮窗口
                    let hover = window_rect_under(point)
                        .and_then(|rect| shot_rect(rect, state.origin, state.shot.width, state.shot.height));
                    if rect_key(hover) != rect_key(state.hover) {
                        state.hover = hover;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
            if !state.is_null() {
                let state = &mut *state;
                // 拖过就用手画的框，没拖就截鼠标下高亮的窗口
                let rect = if state.start.is_some() {
                    selection(state)
                } else {
                    state.hover
                };
                state.press = None;
                state.start = None;

                if let Some(rect) = rect {
                    let cropped = state.shot.crop(
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                    );
                    let panel = PANEL;
                    DestroyWindow(hwnd);
                    deliver(panel, cropped);
                }
            }
            0
        }
        WM_RBUTTONUP | WM_MBUTTONUP => {
            DestroyWindow(hwnd);
            0
        }
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_CROSS));
            1
        }
        WM_KEYDOWN => {
            match wparam as u32 {
                // Esc / 右键 = 取消
                0x1B => {
                    DestroyWindow(hwnd);
                }
                // Enter = 用当前选区
                0x0D => {
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
                    if !state.is_null() {
                        let state = &mut *state;
                        if let Some(rect) = selection(state) {
                            let cropped = state.shot.crop(
                                rect.left,
                                rect.top,
                                rect.right - rect.left,
                                rect.bottom - rect.top,
                            );
                            let panel = PANEL;
                            DestroyWindow(hwnd);
                            deliver(panel, cropped);
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            overlay_paint(hwnd);
            0
        }
        WM_DESTROY => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
            if !state.is_null() {
                let state = Box::from_raw(state);
                if let Some((dc, dib, _)) = state.canvas {
                    DeleteObject(dib as *mut _);
                    DeleteDC(dc);
                }
                drop(state);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            finish_capture();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 抓屏并铺开选区遮罩。
pub unsafe fn begin(panel: HWND) {
    PANEL = panel;

    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

    let Some(shot) = capture_rect(vx, vy, vw, vh) else {
        finish_capture();
        return;
    };

    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(OVERLAY_CLASS);
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(overlay_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_CROSS),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);

    let dimmed = dim_pixels(&shot);

    // 打开遮罩前先看鼠标压在哪个窗口上：直接把它当成默认选区
    let mut cursor = POINT { x: 0, y: 0 };
    GetCursorPos(&mut cursor);
    let hover = window_rect_under(cursor).and_then(|rect| shot_rect(rect, (vx, vy), vw, vh));

    let state = Box::into_raw(Box::new(Overlay {
        shot,
        dimmed,
        origin: (vx, vy),
        start: None,
        press: None,
        current: POINT { x: 0, y: 0 },
        hover,
        canvas: None,
        last_rect: None,
    }));

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class.as_ptr(),
        wide("截图").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        vx,
        vy,
        vw,
        vh,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

    if hwnd.is_null() {
        drop(Box::from_raw(state));
        finish_capture();
        return;
    }

    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    crate::ui::round_corners(hwnd);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    UpdateWindow(hwnd);
}

// ---------------------------------------------------------------- 编辑窗

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Arrow,
    Rect,
    Pen,
    Text,
    Mosaic,
    /// 拖框选一块区域做 OCR
    Ocr,
}

#[derive(Clone)]
enum Shape {
    Arrow { a: (i32, i32), b: (i32, i32) },
    Rect { a: (i32, i32), b: (i32, i32) },
    Pen { points: Vec<(i32, i32)> },
    Text { at: (i32, i32), text: String },
    Mosaic { a: (i32, i32), b: (i32, i32) },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Tool(Tool),
    Undo,
    Copy,
    Save,
    Ocr,
    Pin,
    Minimize,
    Close,
}

struct TextEdit {
    hwnd: HWND,
    at: (i32, i32),
    /// 拖出来的框宽（图片坐标），输入时画框用
    width: i32,
}

struct Editor {
    base: Bitmap,
    shapes: Vec<Shape>,
    tool: Tool,
    scale: f32,
    image_left: i32,
    image_top: i32,
    drag_from: Option<(i32, i32)>,
    drag_to: Option<(i32, i32)>,
    pen_points: Vec<(i32, i32)>,
    text: Option<TextEdit>,
    buttons: Vec<(RECT, Action)>,
    /// 原图只拷进内存 DC 一次，之后每帧直接 blit
    source: Option<(HDC, HBITMAP)>,
    /// 客户区双缓冲（尺寸变了才重建）
    back: Option<(HDC, HBITMAP, i32, i32)>,
}

const ANNOT: u32 = 0x003A_3AE0; // 0x00BBGGRR，红

fn editor_state(hwnd: HWND) -> *mut Editor {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Editor }
}

fn text_of_action(action: Action) -> &'static str {
    match action {
        Action::Tool(Tool::Arrow) => "箭头",
        Action::Tool(Tool::Rect) => "矩形",
        Action::Tool(Tool::Pen) => "画笔",
        Action::Tool(Tool::Text) => "文字",
        Action::Tool(Tool::Mosaic) => "马赛克",
        Action::Tool(Tool::Ocr) => "OCR",
        Action::Undo => "撤销",
        Action::Copy => "复制",
        Action::Save => "保存",
        Action::Ocr => "OCR",
        Action::Pin => "贴图",
        Action::Minimize => "最小化",
        Action::Close => "关闭",
    }
}

fn draw_arrow(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32) {
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        MoveToEx(dc, a.0, a.1, std::ptr::null_mut());
        LineTo(dc, b.0, b.1);
        SelectObject(dc, old);

        let dx = (b.0 - a.0) as f32;
        let dy = (b.1 - a.1) as f32;
        let length = (dx * dx + dy * dy).sqrt().max(1.0);
        let (ux, uy) = (dx / length, dy / length);
        let head = (width as f32 * 4.5).max(10.0);
        let spread = 0.42;
        let p1 = (b.0, b.1);
        let p2 = (
            (b.0 as f32 - ux * head + uy * head * spread) as i32,
            (b.1 as f32 - uy * head - ux * head * spread) as i32,
        );
        let p3 = (
            (b.0 as f32 - ux * head - uy * head * spread) as i32,
            (b.1 as f32 - uy * head + ux * head * spread) as i32,
        );
        let brush = CreateSolidBrush(ANNOT);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let points = [POINT { x: p1.0, y: p1.1 }, POINT { x: p2.0, y: p2.1 }, POINT { x: p3.0, y: p3.1 }];
        Polygon(dc, points.as_ptr(), 3);
        SelectObject(dc, old_brush);
        DeleteObject(brush as *mut _);
        DeleteObject(pen as *mut _);
    }
}

fn draw_rect(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32) {
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(dc, brush);
        windows_sys::Win32::Graphics::Gdi::Rectangle(
            dc,
            a.0.min(b.0),
            a.1.min(b.1),
            a.0.max(b.0),
            a.1.max(b.1),
        );
        SelectObject(dc, old_brush);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }
}

fn draw_pen(dc: HDC, points: &[(i32, i32)], width: i32) {
    if points.len() < 2 {
        return;
    }
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let pts: Vec<POINT> = points.iter().map(|p| POINT { x: p.0, y: p.1 }).collect();
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }
}

fn draw_mosaic(dc: HDC, base: &Bitmap, a: (i32, i32), b: (i32, i32), scale: f32, block: i32) {
    let left = a.0.min(b.0);
    let top = a.1.min(b.1);
    let right = a.0.max(b.0);
    let bottom = a.1.max(b.1);
    let block = block.max(4);
    if right <= left || bottom <= top {
        return;
    }

    let cols = (right - left + block - 1) / block;
    let rows = (bottom - top + block - 1) / block;
    if cols <= 0 || rows <= 0 {
        return;
    }

    // 一个像素 = 一个马赛克块：先算成一张极小的位图，再一次性放大贴上去。
    // 旧写法是每块 CreateSolidBrush + FillRect，拖大区域时每帧要建上千个 GDI 对象，卡就卡在这。
    let (src_dc, src_dib, src_bits) = make_dib(cols, rows);
    if src_dc.is_null() || src_dib.is_null() || src_bits.is_null() {
        unsafe {
            if !src_dc.is_null() {
                DeleteDC(src_dc);
            }
            if !src_dib.is_null() {
                DeleteObject(src_dib as *mut _);
            }
        }
        return;
    }

    for row in 0..rows {
        for col in 0..cols {
            let x = left + col * block;
            let y = top + row * block;

            // 取块内四个采样点的平均值，才像「马赛克」而不是噪点
            let mut sum = (0u32, 0u32, 0u32, 0u32);
            for (sx, sy) in [(0.25f32, 0.25f32), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let px = ((x as f32 + block as f32 * sx) / scale) as i32;
                let py = ((y as f32 + block as f32 * sy) / scale) as i32;
                if px < 0 || py < 0 || px >= base.width || py >= base.height {
                    continue;
                }
                let index = (py * base.width + px) as usize * 4;
                if index + 3 < base.pixels.len() {
                    sum.0 += base.pixels[index] as u32;
                    sum.1 += base.pixels[index + 1] as u32;
                    sum.2 += base.pixels[index + 2] as u32;
                    sum.3 += 1;
                }
            }

            let count = sum.3.max(1);
            let offset = (row * cols + col) as usize * 4;
            unsafe {
                // DIB 是 BGRA，和 Bitmap::pixels 的顺序一致
                *src_bits.add(offset) = (sum.0 / count) as u8;
                *src_bits.add(offset + 1) = (sum.1 / count) as u8;
                *src_bits.add(offset + 2) = (sum.2 / count) as u8;
                *src_bits.add(offset + 3) = 255;
            }
        }
    }

    unsafe {
        SetStretchBltMode(dc, COLORONCOLOR);
        StretchBlt(
            dc,
            left,
            top,
            right - left,
            bottom - top,
            src_dc,
            0,
            0,
            cols,
            rows,
            SRCCOPY,
        );
        DeleteObject(src_dib as *mut _);
        DeleteDC(src_dc);
    }
}
/// 拖拽时的选框：虚线边框 + 右上角的尺寸标签，让「框到哪儿」一眼看得见
fn draw_drag_frame(dc: HDC, a: (i32, i32), b: (i32, i32), scale: f32) {
    let left = a.0.min(b.0);
    let top = a.1.min(b.1);
    let right = a.0.max(b.0);
    let bottom = a.1.max(b.1);
    if right - left < 1 || bottom - top < 1 {
        return;
    }

    unsafe {
        // 白色虚线：先在下面描一条深色，深浅背景都能看清
        for color in [0x0000_0000u32, 0x00FF_FFFFu32] {
            let pen = CreatePen(2, 1, color); // PS_DOT
            let old = SelectObject(dc, pen as HGDIOBJ);
            let inset = if color == 0 { 1 } else { 0 };
            MoveToEx(dc, left + inset, top + inset, std::ptr::null_mut());
            LineTo(dc, right - inset, top + inset);
            LineTo(dc, right - inset, bottom - inset);
            LineTo(dc, left + inset, bottom - inset);
            LineTo(dc, left + inset, top + inset);
            SelectObject(dc, old);
            DeleteObject(pen as *mut _);
        }

        // 尺寸标签
        let label = format!("{} × {}", right - left, bottom - top);
        let font = cached_font(9.0, 600);
        let old_font = SelectObject(dc, font as HGDIOBJ);
        let mut measure = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let units = wide(&label);
        // 先选字体再量，否则量的是上一个字体，标签会被切掉
        DrawTextW(
            dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut measure,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );

        let pad = (6.0 * scale).round().max(4.0) as i32;
        let label_h = (measure.bottom - measure.top).max(12) + pad / 2;
        let label_w = (measure.right - measure.left) + pad * 2 + (2.0 * scale) as i32;
        let chip_left = left;
        let chip_top = if top - label_h - 3 >= 0 {
            top - label_h - 3
        } else {
            top + 3
        };
        let brush = CreateSolidBrush(0x0022_1F1E); // 深灰
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let chip = RECT {
            left: chip_left,
            top: chip_top,
            right: chip_left + label_w,
            bottom: chip_top + label_h,
        };
        FillRect(dc, &chip, brush);
        SelectObject(dc, old_brush);
        DeleteObject(brush as *mut _);

        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        let mut text_rect = RECT {
            left: chip_left + pad,
            top: chip_top,
            right: chip_left + label_w - pad,
            bottom: chip_top + label_h,
        };
        DrawTextW(
            dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut text_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);
    }
}

fn hint_for(tool: Tool) -> &'static str {
    match tool {
        Tool::Text => "拖出文字位置后直接输入 · Enter 提交 · Esc 取消",
        Tool::Ocr => "拖框选择要识别的区域 · Esc 取消",
        Tool::Mosaic => "拖出要打码的区域 · Esc 取消",
        _ => "拖动工具绘制 · Enter/Ctrl+C 复制 · Ctrl+S 保存 · Esc 取消",
    }
}

fn draw_shapes(dc: HDC, editor: &Editor, from: usize) {
    let width = (2.0 * editor.scale).round().max(2.0) as i32;
    for shape in editor.shapes.iter().skip(from) {
        match shape {
            Shape::Arrow { a, b } => draw_arrow(dc, *a, *b, width),
            Shape::Rect { a, b } => draw_rect(dc, *a, *b, width),
            Shape::Pen { points } => draw_pen(dc, points, width),
            Shape::Text { at, text } => {
                let font = cached_font(12.0, 700);
                unsafe {
                    let old = SelectObject(dc, font as HGDIOBJ);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, ANNOT);
                    TextOutW(dc, at.0, at.1, wide(text).as_ptr(), text.encode_utf16().count() as i32);
                    SelectObject(dc, old);
                }
            }
            Shape::Mosaic { a, b } => {
                draw_mosaic(dc, &editor.base, *a, *b, editor.scale, (8.0 * editor.scale) as i32)
            }
        }
    }
}

unsafe fn editor_paint(hwnd: HWND) {
    let started = std::time::Instant::now();
    let editor = editor_state(hwnd);
    if editor.is_null() {
        return;
    }
    let editor = &mut *editor;

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

    // 原图只拷一次
    if editor.source.is_none() {
        editor.source = source_dc(&editor.base);
    }

    // 双缓冲：尺寸没变就复用，别每帧重新建 5 MB 的 DIB
    let (dc, dib) = match editor.back {
        Some((dc, dib, w, h)) if w == client.right && h == client.bottom => (dc, dib),
        _ => {
            if let Some((old_dc, old_dib, _, _)) = editor.back.take() {
                DeleteObject(old_dib as *mut _);
                DeleteDC(old_dc);
            }
            let (dc, dib, _bits) = make_dib(client.right, client.bottom);
            if dc.is_null() || dib.is_null() {
                if !dc.is_null() {
                    DeleteDC(dc);
                }
                if !dib.is_null() {
                    DeleteObject(dib as *mut _);
                }
                return;
            }
            editor.back = Some((dc, dib, client.right, client.bottom));
            (dc, dib)
        }
    };
    let _ = dib;

    let toolbar = s(44.0);
    let bg = CreateSolidBrush(crate::theme::ui().bg);
    let all = RECT {
        left: 0,
        top: 0,
        right: client.right,
        bottom: client.bottom,
    };
    FillRect(dc, &all, bg);
    DeleteObject(bg as *mut _);

    // 图片（从常驻 DC 直接贴，不再每帧分配 + 拷整张图）
    if let Some((source, _)) = editor.source {
        blit_cached(
            dc,
            source,
            editor.base.width,
            editor.base.height,
            editor.image_left,
            editor.image_top,
            (editor.base.width as f32 * editor.scale) as i32,
            (editor.base.height as f32 * editor.scale) as i32,
        );
    }

    // 图形都在「图片坐标系」里，这里把视口原点挪到图片左上角
    let mut old_origin = POINT { x: 0, y: 0 };
    SetViewportOrgEx(dc, editor.image_left, editor.image_top, &mut old_origin);

    // 已提交的图形
    draw_shapes(dc, editor, 0);

    // 正在拖拽的图形（临时）
    let width = (2.0 * editor.scale).round().max(2.0) as i32;
    if let (Some(from), Some(to)) = (editor.drag_from, editor.drag_to) {
        match editor.tool {
            Tool::Arrow => draw_arrow(dc, from, to, width),
            Tool::Rect => draw_rect(dc, from, to, width),
            Tool::Mosaic => {
                // 实时看到打码效果，外面再套一层选框
                draw_mosaic(dc, &editor.base, from, to, editor.scale, (8.0 * editor.scale) as i32);
                draw_drag_frame(dc, from, to, editor.scale);
            }
            Tool::Text | Tool::Ocr => draw_drag_frame(dc, from, to, editor.scale),
            _ => {}
        }
    }
    if editor.tool == Tool::Pen && editor.pen_points.len() > 1 {
        draw_pen(dc, &editor.pen_points, width);
    }

    // 正在输入文字：把位置框出来，免得白底白字看不见输入框在哪
    if let Some(text) = &editor.text {
        let height = (24.0 * editor.scale).round() as i32;
        let left = text.at.0;
        let top = text.at.1;
        let right = left + text.width.max(40);
        let bottom = top + height;
        let pen = CreatePen(2, 1, crate::theme::ui().accent); // PS_DOT
        let old = SelectObject(dc, pen as HGDIOBJ);
        MoveToEx(dc, left, top, std::ptr::null_mut());
        LineTo(dc, right, top);
        LineTo(dc, right, bottom);
        LineTo(dc, left, bottom);
        LineTo(dc, left, top);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }

    SetViewportOrgEx(dc, old_origin.x, old_origin.y, std::ptr::null_mut());

    // 工具栏
    editor.buttons.clear();
    let font = cached_font(9.0, 400);
    let old_font = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);

    let height = s(26.0);
    let mut x = s(12.0);
    let actions = vec![
        Action::Tool(Tool::Arrow),
        Action::Tool(Tool::Rect),
        Action::Tool(Tool::Pen),
        Action::Tool(Tool::Text),
        Action::Tool(Tool::Mosaic),
        Action::Tool(Tool::Ocr),
    ];

    for action in actions {
        let label = text_of_action(action);
        let width_px = wide_width(dc, label) + s(16.0);
        let rect = RECT {
            left: x,
            top: s(9.0),
            right: x + width_px,
            bottom: s(9.0) + height,
        };
        let active = matches!(action, Action::Tool(tool) if tool == editor.tool);
        draw_pill(dc, rect, active);
        SetTextColor(
            dc,
            if active {
                crate::theme::ui().text
            } else {
                crate::theme::ui().text3
            },
        );
        let mut text_rect = rect;
        text_rect.bottom += 1;
        DrawTextW(
            dc,
            wide(label).as_ptr(),
            -1,
            &mut text_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );

        editor.buttons.push((rect, action));
        x = rect.right + s(5.0);
    }

    x += s(10.0);
    let mut tail = vec![Action::Undo, Action::Copy, Action::Save, Action::Ocr, Action::Pin];
    for action in tail.drain(..) {
        let label = text_of_action(action);
        let width_px = wide_width(dc, label) + s(16.0);
        let rect = RECT {
            left: x,
            top: s(9.0),
            right: x + width_px,
            bottom: s(9.0) + height,
        };
        draw_pill(dc, rect, false);
        SetTextColor(dc, crate::theme::ui().text);
        let mut text_rect = rect;
        text_rect.bottom += 1;
        DrawTextW(
            dc,
            wide(label).as_ptr(),
            -1,
            &mut text_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        editor.buttons.push((rect, action));
        x = rect.right + s(5.0);
    }

    // 右上角：最小化 / 关闭，用普通窗口的符号，不占工具栏文字
    let control = s(28.0);
    let close = RECT {
        left: client.right - s(12.0) - control,
        top: s(9.0),
        right: client.right - s(12.0),
        bottom: s(9.0) + height,
    };
    let minimize = RECT {
        left: close.left - s(4.0) - control,
        top: close.top,
        right: close.left - s(4.0),
        bottom: close.bottom,
    };

    let icon_color = crate::theme::ui().text2;
    let pen = CreatePen(0, s(1.4).max(1), icon_color);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);

    // —
    let mx = (minimize.left + minimize.right) / 2;
    let my = (minimize.top + minimize.bottom) / 2;
    let arm = s(6.0);
    MoveToEx(dc, mx - arm, my, std::ptr::null_mut());
    LineTo(dc, mx + arm, my);

    // ×
    let cx = (close.left + close.right) / 2;
    let cy = (close.top + close.bottom) / 2;
    let arm = s(5.0);
    MoveToEx(dc, cx - arm, cy - arm, std::ptr::null_mut());
    LineTo(dc, cx + arm, cy + arm);
    MoveToEx(dc, cx - arm, cy + arm, std::ptr::null_mut());
    LineTo(dc, cx + arm, cy - arm);

    SelectObject(dc, old_pen);
    DeleteObject(pen as *mut _);
    editor.buttons.push((minimize, Action::Minimize));
    editor.buttons.push((close, Action::Close));

    SelectObject(dc, old_font);

    // 提示
    let hint = hint_for(editor.tool);
    let hint_font = cached_font(8.0, 400);
    let old_hint = SelectObject(dc, hint_font as HGDIOBJ);
    SetTextColor(dc, crate::theme::ui().text3);
    let mut hint_rect = RECT {
        left: s(12.0),
        top: client.bottom - s(22.0),
        right: client.right - s(12.0),
        bottom: client.bottom - s(4.0),
    };
    DrawTextW(
        dc,
        wide(hint).as_ptr(),
        -1,
        &mut hint_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(dc, old_hint);

    let _ = toolbar;
    let _ = all;

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
    EndPaint(hwnd, &mut ps);

    // 拖拽时每帧都走这里，超过一帧（16ms）就记一笔，方便以后查回归
    let cost_us = started.elapsed().as_micros();
    if cost_us > 16_000 {
        crate::debug_log(&format!("editor paint={cost_us}us"));
    }
}

fn wide_width(dc: HDC, text: &str) -> i32 {
    unsafe {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        DrawTextW(
            dc,
            wide(text).as_ptr(),
            -1,
            &mut rect,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        rect.right - rect.left
    }
}

const DT_CALCRECT: u32 = 0x0400;

fn draw_pill(dc: HDC, rect: RECT, active: bool) {
    unsafe {
        let c = crate::theme::ui();
        let brush = CreateSolidBrush(if active { c.chip } else { c.surface });
        let pen = CreatePen(0, 1, if active { c.accent } else { c.border });
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            s(12.0),
            s(12.0),
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }
}

unsafe fn composite(editor: &Editor) -> Bitmap {
    let (dc, dib, bits) = make_dib(editor.base.width, editor.base.height);
    let mut out = Bitmap::new(editor.base.width, editor.base.height);
    if dib.is_null() {
        return out;
    }

    let mut copy = editor.clone_for_render();
    copy.scale = 1.0;
    copy.image_left = 0;
    copy.image_top = 0;

    blit_bitmap(dc, &editor.base, 0, 0, editor.base.width, editor.base.height);
    draw_shapes(dc, &copy, 0);

    if !bits.is_null() {
        std::ptr::copy_nonoverlapping(bits, out.pixels.as_mut_ptr(), out.pixels.len());
    }

    DeleteObject(dib as *mut _);
    DeleteDC(dc);
    out
}

impl Editor {
    /// 只为了给 composite 传一份 scale=1 的参数
    fn clone_for_render(&self) -> Editor {
        Editor {
            base: self.base.clone(),
            shapes: self.shapes.clone(),
            tool: self.tool,
            scale: 1.0,
            image_left: 0,
            image_top: 0,
            drag_from: None,
            drag_to: None,
            pen_points: Vec::new(),
            text: None,
            buttons: Vec::new(),
            source: None,
            back: None,
        }
    }
}

fn clipboard_bitmap(bitmap: &Bitmap) {
    unsafe {
        let header = 40usize;
        let stride = bitmap.width as usize * 4;
        let size = header + stride * bitmap.height as usize;
        let handle = GlobalAlloc(GMEM_MOVEABLE, size);
        if handle.is_null() {
            return;
        }

        let pointer = GlobalLock(handle) as *mut u8;
        if pointer.is_null() {
            let _ = GlobalFree(handle);
            return;
        }

        // BITMAPINFOHEADER，高度为正 = 行序自底向上
        std::ptr::write_bytes(pointer, 0, header);
        let mut info: BITMAPINFOHEADER = std::mem::zeroed();
        info.biSize = header as u32;
        info.biWidth = bitmap.width;
        info.biHeight = bitmap.height;
        info.biPlanes = 1;
        info.biBitCount = 32;
        info.biCompression = 0;
        info.biSizeImage = (stride * bitmap.height as usize) as u32;
        std::ptr::copy_nonoverlapping(
            &info as *const _ as *const u8,
            pointer,
            header,
        );

        for row in 0..bitmap.height {
            let src = (bitmap.height - 1 - row) as usize * stride;
            let dst = header + row as usize * stride;
            std::ptr::copy_nonoverlapping(
                bitmap.pixels.as_ptr().add(src),
                pointer.add(dst),
                stride,
            );
        }

        GlobalUnlock(handle);

        if OpenClipboard(std::ptr::null_mut()) != 0 {
            EmptyClipboard();
            // 成功时所有权交给剪贴板；失败要自己释放，否则这块内存就漏了
            if SetClipboardData(CF_DIB, handle as *mut _).is_null() {
                let _ = GlobalFree(handle);
            }
            CloseClipboard();
        } else {
            let _ = GlobalFree(handle);
        }
    }
}

fn save_bitmap(bitmap: &Bitmap, owner: HWND) -> bool {
    unsafe {
        let mut name = vec![0u16; 512];
        let mut filter: Vec<u16> = "PNG 图片\0*.png\0所有文件\0*.*\0\0".encode_utf16().collect();
        let mut title = wide("保存截图");
        let mut ext = wide("png");

        let mut local: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        GetLocalTime2(&mut local);
        let default_name = format!(
            "Shot_{:04}{:02}{:02}_{:02}{:02}{:02}.png",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
        );
        let default: Vec<u16> = default_name.encode_utf16().collect();
        name[..default.len()].copy_from_slice(&default);

        let mut dialog: OPENFILENAMEW = std::mem::zeroed();
        dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = owner;
        dialog.lpstrFilter = filter.as_mut_ptr();
        dialog.lpstrFile = name.as_mut_ptr();
        dialog.nMaxFile = name.len() as u32;
        dialog.lpstrFileTitle = std::ptr::null_mut();
        dialog.nMaxFileTitle = 0;
        dialog.lpstrInitialDir = std::ptr::null();
        dialog.lpstrTitle = title.as_mut_ptr();
        dialog.lpstrDefExt = ext.as_mut_ptr();
        dialog.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST;

        if GetSaveFileNameW(&mut dialog) == 0 {
            let _code = CommDlgExtendedError();
            return false;
        }

        let length = name.iter().position(|c| *c == 0).unwrap_or(name.len());
        let path = String::from_utf16_lossy(&name[..length]);

        let mut rgba = Vec::with_capacity(bitmap.pixels.len());
        for pixel in bitmap.pixels.chunks_exact(4) {
            rgba.push(pixel[2]);
            rgba.push(pixel[1]);
            rgba.push(pixel[0]);
            rgba.push(255);
        }

        let buffer = match image::RgbaImage::from_raw(
            bitmap.width as u32,
            bitmap.height as u32,
            rgba,
        ) {
            Some(buffer) => buffer,
            None => return false,
        };

        let result = buffer.save(&path);
        result.is_ok()
    }
}

fn open_pin(bitmap: Bitmap) {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(PIN_CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(pin_proc),
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

        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);

        // 贴图开在鼠标那块屏：按那块屏的缩放算尺寸，不然换屏后大小会差一截
        use_scale_at(cursor);
        let scale = ui_scale();
        let width = ((bitmap.width as f32) / scale) as i32;
        let height = ((bitmap.height as f32) / scale) as i32;

        let state = Box::into_raw(Box::new(bitmap));
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide("贴图").as_ptr(),
            WS_POPUP | WS_VISIBLE,
            cursor.x,
            cursor.y,
            width.max(60),
            height.max(40),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );

        if hwnd.is_null() {
            drop(Box::from_raw(state));
            return;
        }

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        ShowWindow(hwnd, SW_SHOW);
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            ReleaseCapture();
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as WPARAM, 0);
            0
        }
        WM_LBUTTONDBLCLK | WM_RBUTTONUP => {
            DestroyWindow(hwnd);
            0
        }
        WM_KEYDOWN => {
            if wparam as u32 == 0x1B {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bitmap;
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !state.is_null() {
                let bitmap = &*state;
                let mut client = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetClientRect(hwnd, &mut client);
                blit_bitmap(hdc, bitmap, 0, 0, client.right, client.bottom);
            }
            EndPaint(hwnd, &mut ps);
            0
        }
        WM_DESTROY => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bitmap;
            if !state.is_null() {
                drop(Box::from_raw(state));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// 文字工具用真正的 EDIT 子窗口：这样才能用中文输入法
static mut TEXT_OLD_PROC: isize = 0;

unsafe extern "system" fn text_edit_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let parent = GetParent(hwnd);
    match msg {
        WM_KEYDOWN => {
            let key = wparam as u32;
            if key == 0x0D {
                commit_text(parent);
                return 0;
            }
            if key == 0x1B {
                cancel_text(parent);
                return 0;
            }
        }
        WM_KILLFOCUS => {
            commit_text(parent);
            return 0;
        }
        _ => {}
    }

    let old: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
        std::mem::transmute(TEXT_OLD_PROC);
    CallWindowProcW(Some(old), hwnd, msg, wparam, lparam)
}

unsafe fn cancel_text(parent: HWND) {
    let editor = editor_state(parent);
    if editor.is_null() {
        return;
    }
    if let Some(text) = (*editor).text.take() {
        DestroyWindow(text.hwnd);
    }
    let editor = &mut *editor;
    editor.text = None;
    SetFocus(parent);
    InvalidateRect(parent, std::ptr::null(), 0);
}

unsafe fn commit_text(parent: HWND) {
    let editor = editor_state(parent);
    if editor.is_null() {
        return;
    }
    let text_widget = (*editor).text.take();
    let Some(text_widget) = text_widget else {
        return;
    };

    let mut buffer = vec![0u16; 512];
    let length = GetWindowTextW(text_widget.hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    let value = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);

    DestroyWindow(text_widget.hwnd);

    let editor = &mut *editor;
    if !value.trim().is_empty() {
        editor.shapes.push(Shape::Text {
            at: text_widget.at,
            text: value,
        });
    }
    SetFocus(parent);
    InvalidateRect(parent, std::ptr::null(), 0);
}

unsafe fn start_text(parent: HWND, at: (i32, i32), box_width: i32) {
    let editor = editor_state(parent);
    if editor.is_null() {
        return;
    }
    let editor_ref = &*editor;
    if editor_ref.text.is_some() {
        commit_text(parent);
    }

    let instance = GetModuleHandleW(std::ptr::null());
    let height = s(26.0);
    let left = editor_ref.image_left + (at.0 as f32 * editor_ref.scale) as i32;
    let top = editor_ref.image_top + (at.1 as f32 * editor_ref.scale) as i32;
    // 输入框至少 160，跟着拖出来的框宽（超长会横向滚动）
    let width = ((box_width as f32 * editor_ref.scale) as i32).max(s(160.0));

    let edit = CreateWindowExW(
        0,
        wide("EDIT").as_ptr(),
        std::ptr::null(),
        WS_CHILD | WS_VISIBLE | 0x0080, // ES_AUTOHSCROLL
        left,
        top,
        width,
        height,
        parent,
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

    if edit.is_null() {
        return;
    }

    let font = cached_font(12.0, 700);
    SendMessageW(edit, WM_SETFONT, font as WPARAM, 1);
    TEXT_OLD_PROC = SetWindowLongPtrW(edit, GWLP_WNDPROC, text_edit_proc as *const () as isize);

    let editor = &mut *editor;
    editor.text = Some(TextEdit {
        hwnd: edit,
        at,
        width: box_width,
    });
    SetFocus(edit);
    InvalidateRect(parent, std::ptr::null(), 0);
}


unsafe fn editor_command(hwnd: HWND, action: Action) {
    let editor = editor_state(hwnd);
    if editor.is_null() {
        return;
    }

    match action {
        Action::Tool(tool) => {
            commit_text(hwnd);
            let editor = &mut *editor;
            editor.tool = tool;
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        Action::Undo => {
            let editor = &mut *editor;
            editor.shapes.pop();
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        Action::Copy => {
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            clipboard_bitmap(&bitmap);
            SetWindowTextW(hwnd, wide("截图编辑 · 已复制到剪贴板").as_ptr());
        }
        Action::Save => {
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            if save_bitmap(&bitmap, hwnd) {
                SetWindowTextW(hwnd, wide("截图编辑 · 已保存").as_ptr());
            } else {
                // 以前失败是完全静默的：磁盘满/没权限时用户以为存上了
                MessageBoxW(
                    hwnd,
                    wide("保存失败：磁盘空间不足、没有写入权限，或路径无效。")
                        .as_ptr(),
                    wide("保存截图").as_ptr(),
                    MB_OK | MB_ICONWARNING,
                );
            }
        }
        Action::Ocr => {
            // 进入框选模式：拖一块出来，只识别那块（整图就拖满）
            commit_text(hwnd);
            let editor = &mut *editor;
            editor.tool = Tool::Ocr;
            SetWindowTextW(hwnd, wide("截图编辑 · 拖框选择要识别的区域").as_ptr());
        }
        Action::Pin => {
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            open_pin(bitmap);
        }
        Action::Minimize => {
            ShowWindow(hwnd, SW_MINIMIZE);
        }
        Action::Close => {
            DestroyWindow(hwnd);
        }
    }
}

unsafe fn hit_button(editor: &Editor, point: POINT) -> Option<Action> {
    for (rect, action) in &editor.buttons {
        if point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom {
            return Some(*action);
        }
    }
    None
}

unsafe extern "system" fn editor_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // 无边框窗口：工具栏空白处按住能拖窗口，按钮还是要能点
            let mut point = POINT {
                x: (lparam as i32) & 0xFFFF,
                y: ((lparam as i32) >> 16) & 0xFFFF,
            };
            ScreenToClient(hwnd, &mut point);
            let editor = editor_state(hwnd);
            if !editor.is_null() && point.y < s(44.0) {
                for (rect, _) in &(*editor).buttons {
                    if point.x >= rect.left
                        && point.x <= rect.right
                        && point.y >= rect.top
                        && point.y <= rect.bottom
                    {
                        return HTCLIENT as LRESULT;
                    }
                }
                return HTCAPTION as LRESULT;
            }
            HTCLIENT as LRESULT
        }
        WM_CTLCOLOREDIT => {
            // 文字输入框：浅蓝底 + 深色字，图片上是白的也能看出输入框在哪
            let dc = wparam as HDC;
            SetBkColor(dc, 0x00FF_F2_E8);
            SetTextColor(dc, 0x001A_1614);
            let brush = CreateSolidBrush(0x00FF_F2_E8);
            brush as LRESULT
        }
        WM_LBUTTONDOWN => {
            let mut point = POINT {
                x: (lparam as i32) & 0xFFFF,
                y: ((lparam as i32) >> 16) & 0xFFFF,
            };
            let editor = editor_state(hwnd);
            if editor.is_null() {
                return 0;
            }

            if let Some(action) = hit_button(&*editor, point) {
                editor_command(hwnd, action);
                return 0;
            }

            commit_text(hwnd);

            let editor = &mut *editor;
            let ix = ((point.x - editor.image_left) as f32 / editor.scale) as i32;
            let iy = ((point.y - editor.image_top) as f32 / editor.scale) as i32;
            if ix < 0 || iy < 0 || ix > editor.base.width || iy > editor.base.height {
                return 0;
            }

            let _ = &mut point;
            match editor.tool {
                Tool::Text | Tool::Ocr => {
                    // 文字 / 取字都先拖一个框，框住要处理的区域
                    editor.drag_from = Some((ix, iy));
                    editor.drag_to = Some((ix, iy));
                    SetCapture(hwnd);
                }
                Tool::Pen => {
                    editor.pen_points = vec![(ix, iy)];
                    SetCapture(hwnd);
                }
                _ => {
                    editor.drag_from = Some((ix, iy));
                    editor.drag_to = Some((ix, iy));
                    SetCapture(hwnd);
                }
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_MOUSEMOVE => {
            let editor = editor_state(hwnd);
            if editor.is_null() {
                return 0;
            }
            let editor = &mut *editor;
            let ix = (((lparam as i32) & 0xFFFF) - editor.image_left) as f32 / editor.scale;
            let iy = ((((lparam as i32) >> 16) & 0xFFFF) - editor.image_top) as f32 / editor.scale;
            let point = (ix as i32, iy as i32);

            if editor.tool == Tool::Pen {
                if let Some(last) = editor.pen_points.last() {
                    if (last.0 - point.0).abs() + (last.1 - point.1).abs() >= 2 {
                        editor.pen_points.push(point);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
            } else if editor.drag_from.is_some() {
                editor.drag_to = Some(point);
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let editor = editor_state(hwnd);
            if editor.is_null() {
                return 0;
            }
            let editor = &mut *editor;

            if editor.tool == Tool::Pen {
                if editor.pen_points.len() > 1 {
                    let points = std::mem::take(&mut editor.pen_points);
                    editor.shapes.push(Shape::Pen { points });
                } else {
                    editor.pen_points.clear();
                }
            } else if let (Some(from), Some(to)) = (editor.drag_from, editor.drag_to) {
                let left = from.0.min(to.0);
                let top = from.1.min(to.1);
                let right = from.0.max(to.0);
                let bottom = from.1.max(to.1);
                let wide_enough = right - left >= 8 && bottom - top >= 8;

                match editor.tool {
                    Tool::Arrow if (from.0 - to.0).abs() >= 3 || (from.1 - to.1).abs() >= 3 => {
                        editor.shapes.push(Shape::Arrow { a: from, b: to })
                    }
                    Tool::Rect if (from.0 - to.0).abs() >= 3 || (from.1 - to.1).abs() >= 3 => {
                        editor.shapes.push(Shape::Rect { a: from, b: to })
                    }
                    Tool::Mosaic if wide_enough => {
                        editor.shapes.push(Shape::Mosaic { a: from, b: to })
                    }
                    Tool::Text if wide_enough => {
                        // 框的左下角起笔，输入框宽度跟框一样宽
                        let width = right - left;
                        start_text(hwnd, (left, top), width);
                    }
                    Tool::Ocr if wide_enough => {
                        // 只对框里这块做 OCR
                        let region = editor.base.crop(left, top, right - left, bottom - top);
                        editor.tool = Tool::Arrow;
                        let panel = PANEL;
                        crate::ocr::recognize_async(panel, region);
                        SetWindowTextW(hwnd, wide("截图编辑 · 正在识别…").as_ptr());
                    }
                    _ => {}
                }
            }

            editor.drag_from = None;
            editor.drag_to = None;
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_KEYDOWN => {
            match wparam as u32 {
                0x1B => {
                    // Esc：取消文字输入 → 退出取字框选 → 否则关闭窗口
                    let editor = editor_state(hwnd);
                    if !editor.is_null() && (*editor).text.is_some() {
                        cancel_text(hwnd);
                    } else if !editor.is_null() && (*editor).tool == Tool::Ocr {
                        (*editor).tool = Tool::Arrow;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    } else {
                        DestroyWindow(hwnd);
                    }
                }
                0x0D => {
                    commit_text(hwnd);
                    let editor = &*editor_state(hwnd);
                    let bitmap = composite(editor);
                    clipboard_bitmap(&bitmap);
                    SetWindowTextW(hwnd, wide("截图编辑 · 已复制到剪贴板").as_ptr());
                }
                0x5A if GetKeyState(0x11) < 0 => {
                    // Ctrl+Z
                    let editor = &mut *editor_state(hwnd);
                    editor.shapes.pop();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0x43 if GetKeyState(0x11) < 0 => {
                    commit_text(hwnd);
                    let editor = &*editor_state(hwnd);
                    let bitmap = composite(editor);
                    clipboard_bitmap(&bitmap);
                    SetWindowTextW(hwnd, wide("截图编辑 · 已复制到剪贴板").as_ptr());
                }
                0x53 if GetKeyState(0x11) < 0 => {
                    commit_text(hwnd);
                    let editor = &*editor_state(hwnd);
                    let bitmap = composite(editor);
                    if save_bitmap(&bitmap, hwnd) {
                        SetWindowTextW(hwnd, wide("截图编辑 · 已保存").as_ptr());
                    }
                }
                _ => {}
            }
            0
        }
        WM_ERASEBKGND => 1,
        // 系统切明暗主题：工具栏/胶囊要跟着换色
        WM_SETTINGCHANGE => {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_PAINT => {
            editor_paint(hwnd);
            0
        }
        WM_DESTROY => {
            let editor = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Editor;
            if !editor.is_null() {
                let editor = Box::from_raw(editor);
                if let Some((dc, dib)) = editor.source {
                    DeleteObject(dib as *mut _);
                    DeleteDC(dc);
                }
                if let Some((dc, dib, _, _)) = editor.back {
                    DeleteObject(dib as *mut _);
                    DeleteDC(dc);
                }
                drop(editor);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            finish_capture();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 打开编辑窗（截图选好区域后调用）。
pub unsafe fn open_editor(panel: HWND, bitmap: Bitmap) {
    PANEL = panel;

    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(EDITOR_CLASS);
    let wc = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(editor_proc),
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

    // 显示尺寸：不超过「截图所在那块屏」工作区的 85%
    let mut cursor = POINT { x: 0, y: 0 };
    GetCursorPos(&mut cursor);
    use_scale_at(cursor);
    let work = crate::work_area_at(cursor);
    let max_w = ((work.right - work.left) as f32 * 0.85) as i32;
    let max_h = ((work.bottom - work.top) as f32 * 0.8) as i32;

    let scale = (max_w as f32 / bitmap.width as f32)
        .min(max_h as f32 / bitmap.height as f32)
        .min(1.0);

    let image_w = (bitmap.width as f32 * scale) as i32;
    let image_h = (bitmap.height as f32 * scale) as i32;

    let toolbar = s(44.0);
    let hint = s(24.0);
    let client_w = image_w.max(s(600.0));
    let client_h = toolbar + image_h + hint;

    // 无边框：窗口就是客户区，位置居中
    let window_w = client_w;
    let window_h = client_h;
    let window_x = work.left + ((work.right - work.left) - window_w) / 2;
    let window_y = work.top + ((work.bottom - work.top) - window_h) / 3;

    let state = Box::into_raw(Box::new(Editor {
        base: bitmap,
        shapes: Vec::new(),
        tool: Tool::Arrow,
        scale,
        image_left: (client_w - image_w) / 2,
        image_top: toolbar,
        drag_from: None,
        drag_to: None,
        pen_points: Vec::new(),
        text: None,
        buttons: Vec::new(),
        source: None,
        back: None,
    }));

    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        wide("截图编辑").as_ptr(),
        WS_POPUP | WS_VISIBLE | WS_SYSMENU | WS_MINIMIZEBOX,
        window_x,
        window_y,
        window_w,
        window_h,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

    if hwnd.is_null() {
        drop(Box::from_raw(state));
        finish_capture();
        return;
    }

    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    crate::ui::round_corners(hwnd);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    UpdateWindow(hwnd);
}
