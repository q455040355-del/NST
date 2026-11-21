//! 轻量截图 + 标注（自研，纯 Win32/GDI）。
//!
//! 交互参考 Snow Shot，但代码全部自己写，不引入其 GPL-3.0 实现：
//!   Ctrl+Shift+A / 右键菜单「截图…」
//!   → 全屏遮罩选区（带尺寸提示）
//!   → 编辑窗（椭圆 / 箭头 / 矩形 / 画笔 / 文字 / 马赛克 / OCR，撤销、复制、保存、贴图；
//!     工具栏是微信那种一条图标底栏）
//!
//! 画面全部走 GDI 双缓冲；贴图窗口是置顶无边框窗口。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetStockObject, IntersectClipRect, InvalidateRect, LineTo, MoveToEx, Polygon, Polyline,
    ReleaseDC, RestoreDC, SaveDC, SelectObject,
    ScreenToClient, SetBkColor, SetBkMode, SetStretchBltMode, SetTextColor, SetViewportOrgEx, StretchBlt, TextOutW, BITMAPINFO,
    BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, COLORONCOLOR, DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    HALFTONE, HBITMAP, HDC, HFONT, HGDIOBJ, NULL_BRUSH, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
    UpdateWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
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
const WM_MOUSELEAVE: u32 = 0x02A3;
const CF_DIB: u32 = 8;
const CAPTUREBLT: u32 = 0x4000_0000;

const OVERLAY_CLASS: &str = "NetSpeedTaskbarRs.ShotOverlay";
const EDITOR_CLASS: &str = "NetSpeedTaskbarRs.ShotEditor";
const PIN_CLASS: &str = "NetSpeedTaskbarRs.ShotPin";

/// 编辑窗的「小提示该消失了」
const WM_APP_EDITOR_TOAST: u32 = WM_APP + 40;

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

/// 选区确认后做什么：进编辑窗，还是直接取字，还是开始录 GIF
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Edit,
    Ocr,
    Gif,
}

static mut MODE: Mode = Mode::Edit;
/// 录 GIF 时：选区在屏幕上的位置（物理像素）和帧率
static mut GIF_RECT: (i32, i32, i32, i32) = (0, 0, 0, 0);
static GIF_FPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(30);
/// 录制画质（libwebp quality）：跟着设置走，下一次录制生效
static GIF_QUALITY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(65);
/// 录制时长上限（秒）
static GIF_SECONDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(20);

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

/// 框一块区域录成 GIF：复用截图那套遮罩选区，选完直接开录。
pub fn request_gif(panel: HWND, fps: u32, quality: u32, seconds: u32) {
    if crate::gifrec::is_recording() || ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    GIF_FPS.store(fps, Ordering::Relaxed);
    GIF_QUALITY.store(quality.clamp(1, 100), Ordering::Relaxed);
    GIF_SECONDS.store(seconds.clamp(5, 60), Ordering::Relaxed);
    start(panel, Mode::Gif);
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
/// 选区确认：按模式进编辑窗 / 直接取字 / 开始录 GIF
unsafe fn deliver(panel: HWND, bitmap: Bitmap) {
    match MODE {
        Mode::Ocr => crate::ocr::recognize_async(panel, bitmap),
        Mode::Gif => crate::gifrec::start(
            panel,
            GIF_RECT,
            GIF_FPS.load(Ordering::Relaxed),
            GIF_QUALITY.load(Ordering::Relaxed),
            GIF_SECONDS.load(Ordering::Relaxed),
        ),
        Mode::Edit => open_editor(panel, bitmap, GIF_RECT),
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
    static FONTS: RefCell<HashMap<(i32, i32, i32), HFONT>> = RefCell::new(HashMap::new());
}

fn cached_font(points: f32, weight: i32) -> HFONT {
    // 缩放也进缓存键：窗口被拖到另一块缩放不同的屏时，字号要重新建，
    // 不然会沿用旧屏那份（偏小/偏大）
    let key = (
        (points * 10.0).round() as i32,
        weight,
        (ui_scale() * 100.0).round() as i32,
    );
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
    /// 上一帧「宽 × 高」标签胶囊的区域（同样要恢复，不然会叠字/留残影）
    last_label: Option<RECT>,
    /// 放大镜取色用的鼠标位置（截图坐标）
    cursor: POINT,
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

/// 把矩形往外扩 `pad` 像素（还原上一帧时把描边/文字的抗锯齿边一起抹掉）。
fn inflate(rect: RECT, pad: i32, width: i32, height: i32) -> RECT {
    RECT {
        left: (rect.left - pad).max(0),
        top: (rect.top - pad).max(0),
        right: (rect.right + pad).min(width),
        bottom: (rect.bottom + pad).min(height),
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

    // 窗口的实际可见边界（不含那圈看不见的拖拽边框）：
    // 最大化窗口的 GetWindowRect 会往外多出十几像素，直接用会截偏、还会被屏幕裁掉一截
    let mut frame: RECT = std::mem::zeroed();
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        &mut frame as *mut RECT as *mut c_void,
        std::mem::size_of::<RECT>() as u32,
    ) == 0
        && frame.right - frame.left >= 16
        && frame.bottom - frame.top >= 16
    {
        rect = frame;
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
        // 先把上一帧的选区 + 尺寸标签恢复成压暗版，再把这一帧的选区还原成原始像素。
        // 标签画在选区外面，如果不单独还原，每次重绘都会把字叠一遍——看起来又粗又糊，
        // 换窗口时还会留下上一个尺寸的残影。
        if let Some(rect) = state.last_rect {
            restore_rows(bits, &state.dimmed, width, height, inflate(rect, s(2.0), width, height));
        }
        if let Some(rect) = state.last_label {
            restore_rows(bits, &state.dimmed, width, height, inflate(rect, s(2.0), width, height));
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
        let text_w = wide_width(dc, &label);
        let pad_x = s(7.0);
        let pill = RECT {
            left: rect.left.max(0),
            top: (rect.top - s(24.0)).max(0),
            right: rect.left.max(0) + (text_w + pad_x * 2).max(s(40.0)),
            bottom: (rect.top - s(24.0)).max(0) + s(20.0),
        };

        // 深色胶囊打底：白字直接画在花花绿绿的壁纸/窗口上会发虚，也容易糊成一片
        let fill = 0x0018_1818;
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, fill);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            dc,
            pill.left,
            pill.top,
            pill.right,
            pill.bottom,
            s(5.0),
            s(5.0),
        );
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(brush as *mut _);
        DeleteObject(pen as *mut _);

        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);

        let mut label_rect = RECT {
            left: pill.left + pad_x,
            top: pill.top,
            right: pill.right - pad_x,
            bottom: pill.bottom,
        };
        DrawTextW(
            dc,
            wide(&label).as_ptr(),
            -1,
            &mut label_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );

        SelectObject(dc, old_font);
        state.last_label = Some(pill);
    }

    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    BitBlt(hdc, 0, 0, width, height, dc, 0, 0, SRCCOPY);
    // 放大镜直接画在窗口 DC 上，不去动常驻画布（每帧重画，不会有残影）
    draw_loupe(hdc, state);
    EndPaint(hwnd, &mut ps);
}

/// 微信那种放大镜：光标附近放大 8 倍、画格子、标中心像素，下面给坐标和色值
unsafe fn draw_loupe(dc: HDC, state: &Overlay) {
    let (cx, cy) = (state.cursor.x, state.cursor.y);
    if cx < 0 || cy < 0 || cx >= state.shot.width || cy >= state.shot.height {
        return;
    }

    let zoom = s(4.0).max(3);
    let cells = 19;
    let grid = cells * zoom;
    let pad = s(1.0);
    let info_h = s(72.0);
    let width = (grid + pad * 2).max(s(150.0));
    let height = grid + pad * 2 + info_h;

    // 默认放鼠标右下，贴边就翻到另一侧
    let gap = s(26.0);
    let mut left = cx + gap;
    let mut top = cy + gap;
    if left + width > state.shot.width {
        left = cx - gap - width;
    }
    if top + height > state.shot.height {
        top = cy - gap - height;
    }
    let left = left.max(0).min((state.shot.width - width).max(0));
    let top = top.max(0).min((state.shot.height - height).max(0));

    // 面板：浅色圆角底 + 细边框
    let panel = 0x00F7_F8FA;
    let border = 0x00D8_DCE0;
    let brush = CreateSolidBrush(panel);
    let pen = CreatePen(0, 1, border);
    let old_brush = SelectObject(dc, brush as HGDIOBJ);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);
    windows_sys::Win32::Graphics::Gdi::RoundRect(
        dc,
        left,
        top,
        left + width,
        top + height,
        s(8.0),
        s(8.0),
    );
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(brush as *mut _);
    DeleteObject(pen as *mut _);

    let half = cells / 2;
    let mut sample = vec![0u8; (cells * cells * 4) as usize];
    for row in 0..cells {
        for col in 0..cells {
            let sx = (cx - half + col).clamp(0, state.shot.width - 1);
            let sy = (cy - half + row).clamp(0, state.shot.height - 1);
            let src = ((sy * state.shot.width + sx) as usize) * 4;
            let dst = ((row * cells + col) as usize) * 4;
            sample[dst..dst + 4].copy_from_slice(&state.shot.pixels[src..src + 4]);
        }
    }

    let screen = GetDC(std::ptr::null_mut());
    let memory = CreateCompatibleDC(screen);
    let old_bitmap = SelectObject(
        memory,
        CreateBitmap(cells, cells, 1, 32, sample.as_ptr() as *const _) as HGDIOBJ,
    );
    let saved = SaveDC(dc);
    IntersectClipRect(dc, left + pad, top + pad, left + pad + grid, top + pad + grid);
    SetStretchBltMode(dc, COLORONCOLOR);
    StretchBlt(
        dc,
        left + pad,
        top + pad,
        grid,
        grid,
        memory,
        0,
        0,
        cells,
        cells,
        SRCCOPY,
    );
    RestoreDC(dc, saved);
    let bitmap = SelectObject(memory, old_bitmap);
    DeleteObject(bitmap as *mut _);
    DeleteDC(memory);
    ReleaseDC(std::ptr::null_mut(), screen);

    // 网格线
    let grid_pen = CreatePen(0, 1, 0x00C8_CDD2);
    let old = SelectObject(dc, grid_pen as HGDIOBJ);
    for index in 1..cells {
        let x = left + pad + index * zoom;
        MoveToEx(dc, x, top + pad, std::ptr::null_mut());
        LineTo(dc, x, top + pad + grid);
        let y = top + pad + index * zoom;
        MoveToEx(dc, left + pad, y, std::ptr::null_mut());
        LineTo(dc, left + pad + grid, y);
    }
    SelectObject(dc, old);
    DeleteObject(grid_pen as *mut _);

    // 中心像素：套一个白框，一眼看出取的是哪个点
    let center = CreatePen(0, s(1.6).max(1), 0x00FF_FFFF);
    let old = SelectObject(dc, center as HGDIOBJ);
    let box_left = left + pad + half * zoom;
    let box_top = top + pad + half * zoom;
    MoveToEx(dc, box_left, box_top, std::ptr::null_mut());
    LineTo(dc, box_left + zoom, box_top);
    LineTo(dc, box_left + zoom, box_top + zoom);
    LineTo(dc, box_left, box_top + zoom);
    LineTo(dc, box_left, box_top);
    SelectObject(dc, old);
    DeleteObject(center as *mut _);

    // 下面写坐标 / 色值
    let index = ((cy * state.shot.width + cx) as usize) * 4;
    let pixel = &state.shot.pixels[index..index + 3];
    let hex = format!("#{:02X}{:02X}{:02X}", pixel[2], pixel[1], pixel[0]);
    let screen_x = state.origin.0 + cx;
    let screen_y = state.origin.1 + cy;

    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, 0x0030_3438);
    let font = cached_font(9.0, 400);
    let old_font = SelectObject(dc, font as HGDIOBJ);
    let line = s(22.0);
    let mut text_top = top + pad + grid + s(6.0);

    for (label, value) in [
        ("坐标", format!("{screen_x}, {screen_y}")),
        ("色值", hex.clone()),
    ] {
        let mut rect = RECT {
            left: left + s(10.0),
            top: text_top,
            right: left + width - s(10.0),
            bottom: text_top + line,
        };
        DrawTextW(
            dc,
            wide(label).as_ptr(),
            -1,
            &mut rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        let mut value_rect = RECT {
            left: left + s(52.0),
            ..rect
        };
        DrawTextW(
            dc,
            wide(&value).as_ptr(),
            -1,
            &mut value_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        text_top += line;
    }

    SetTextColor(dc, 0x0080_868C);
    let small = cached_font(8.5, 400);
    SelectObject(dc, small as HGDIOBJ);
    let mut hint = RECT {
        left: left + s(10.0),
        top: text_top,
        right: left + width - s(10.0),
        bottom: text_top + line,
    };
    DrawTextW(
        dc,
        wide("按 Ctrl+C 复制色值").as_ptr(),
        -1,
        &mut hint,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(dc, old_font);
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
                        state.cursor = current;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else {
                    // 没按下：跟着鼠标换高亮窗口，同时让放大镜跟着走
                    let hover = window_rect_under(point)
                        .and_then(|rect| shot_rect(rect, state.origin, state.shot.width, state.shot.height));
                    let moved = state.cursor.x != current.x || state.cursor.y != current.y;
                    state.cursor = current;
                    if rect_key(hover) != rect_key(state.hover) || moved {
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
                    GIF_RECT = (
                        state.origin.0 + rect.left,
                        state.origin.1 + rect.top,
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
                // Ctrl+C = 复制放大镜中心那一点的色值
                0x43 => {
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Overlay;
                    if !state.is_null() && GetKeyState(0x11) < 0 {
                        let state = &*state;
                        let (cx, cy) = (state.cursor.x, state.cursor.y);
                        if cx >= 0
                            && cy >= 0
                            && cx < state.shot.width
                            && cy < state.shot.height
                        {
                            let index = ((cy * state.shot.width + cx) as usize) * 4;
                            let pixel = &state.shot.pixels[index..index + 3];
                            let hex = format!(
                                "#{:02X}{:02X}{:02X}",
                                pixel[2], pixel[1], pixel[0]
                            );
                            crate::ocr::copy_text(&hex);
                        }
                    }
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
                            GIF_RECT = (
                                state.origin.0 + rect.left,
                                state.origin.1 + rect.top,
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
        last_label: None,
        cursor: POINT { x: vw / 2, y: vh / 2 },
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
    /// 没选任何工具：点图上是「不做任何事」，画布保持干净
    None,
    Arrow,
    Rect,
    /// 空心椭圆（微信那排第一个）
    Ellipse,
    Pen,
    Text,
    Mosaic,
    /// 拖框选一块区域做 OCR
    Ocr,
}

#[derive(Clone)]
enum Shape {
    Arrow {
        a: (i32, i32),
        b: (i32, i32),
        stroke: u8,
    },
    Rect {
        a: (i32, i32),
        b: (i32, i32),
        stroke: u8,
    },
    Ellipse {
        a: (i32, i32),
        b: (i32, i32),
        stroke: u8,
    },
    Pen {
        points: Vec<(i32, i32)>,
        stroke: u8,
    },
    Text {
        at: (i32, i32),
        text: String,
    },
    Mosaic {
        a: (i32, i32),
        b: (i32, i32),
        stroke: u8,
    },
}

/// 描边浓淡：细 / 中 / 粗（倍数）
const STROKE_LABELS: [&str; 3] = ["细", "中", "粗"];
const STROKE_FACTORS: [f32; 3] = [1.0, 1.8, 3.0];

fn stroke_width(scale: f32, stroke: u8) -> i32 {
    let factor = STROKE_FACTORS[stroke.min(2) as usize];
    ((2.0 * factor) * scale).round().max(2.0) as i32
}

/// 马赛克格子也跟着浓淡走：细 = 格子更小（更细的码）
fn mosaic_block(scale: f32, stroke: u8) -> i32 {
    let factor = [6.0, 8.0, 12.0][stroke.min(2) as usize];
    (factor * scale).round().max(4.0) as i32
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    /// 置顶开关（默认关）
    Topmost,
    /// 描边浓淡切换（细 / 中 / 粗）
    Stroke,
    Tool(Tool),
    Undo,
    Copy,
    Save,
    Pin,
    /// 完成：复制到剪贴板 + 关窗（微信那条栏最右边的 ✓）
    Done,
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
    /// 描边浓淡：0 细 / 1 中 / 2 粗
    stroke: u8,
    buttons: Vec<(RECT, Action)>,
    /// 正按着哪个按钮（按下高亮、松开才执行）
    pressed: Option<Action>,
    /// 鼠标停在哪个按钮上（图标没有文字，靠它弹小标签）
    hover: Option<Action>,
    /// 底部小提示（复制成功之类），带出现时间
    toast: Option<(String, std::time::Instant)>,
    /// 是否置顶（默认关，截图完不挡别的窗口）
    topmost: bool,
    /// 原图只拷进内存 DC 一次，之后每帧直接 blit
    source: Option<(HDC, HBITMAP)>,
    /// 客户区双缓冲（尺寸变了才重建）
    back: Option<(HDC, HBITMAP, i32, i32)>,
}

const ANNOT: u32 = 0x003A_3AE0; // 0x00BBGGRR，红

fn editor_state(hwnd: HWND) -> *mut Editor {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Editor }
}

/// 底部小提示：显示 1.4 秒后自己消失。
///
/// 本进程里 `SetTimer` / `WM_TIMER` 不触发（见 README），所以用线程睡完再投递消息。
fn show_toast(hwnd: HWND, text: &str) {
    unsafe {
        let editor = editor_state(hwnd);
        if editor.is_null() {
            return;
        }
        (*editor).toast = Some((text.to_string(), std::time::Instant::now()));
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }

    let raw = hwnd as isize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1400));
        unsafe {
            PostMessageW(raw as HWND, WM_APP_EDITOR_TOAST, 0, 0);
        }
    });
}

fn text_of_action(action: Action) -> &'static str {
    match action {
        Action::Tool(Tool::None) => "",
        Action::Tool(Tool::Arrow) => "箭头",
        Action::Tool(Tool::Rect) => "矩形",
        Action::Tool(Tool::Ellipse) => "椭圆",
        Action::Tool(Tool::Pen) => "画笔",
        Action::Tool(Tool::Text) => "文字",
        Action::Tool(Tool::Mosaic) => "马赛克",
        Action::Tool(Tool::Ocr) => "OCR",
        Action::Undo => "撤销",
        Action::Copy => "复制",
        Action::Save => "保存",
        Action::Stroke => "浓淡",
        Action::Topmost => "置顶",
        Action::Pin => "贴图",
        Action::Done => "完成",
        Action::Minimize => "最小化",
        Action::Close => "取消",
    }
}

/// 图形都存「图片坐标」，画到窗口上要乘显示缩放。
///
/// 大图（整屏截图）会被缩到工作区的 85% 显示，之前只平移没缩放，
/// 箭头 / 矩形就会画在离左上角更近的地方 —— 用户看到的「标注错位」就是这个。
fn scaled(point: (i32, i32), scale: f32) -> (i32, i32) {
    (
        (point.0 as f32 * scale).round() as i32,
        (point.1 as f32 * scale).round() as i32,
    )
}

fn draw_arrow(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
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

fn draw_rect(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
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

fn draw_ellipse(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(dc, brush);
        windows_sys::Win32::Graphics::Gdi::Ellipse(
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

fn draw_pen(dc: HDC, points: &[(i32, i32)], width: i32, scale: f32) {    if points.len() < 2 {
        return;
    }
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let pts: Vec<POINT> = points
            .iter()
            .map(|p| {
                let (x, y) = scaled(*p, scale);
                POINT { x, y }
            })
            .collect();
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
    // 尺寸标签显示图片坐标（真实像素），框本身按显示缩放画
    let (left_px, top_px) = (a.0.min(b.0), a.1.min(b.1));
    let (right_px, bottom_px) = (a.0.max(b.0), a.1.max(b.1));
    let (left, top) = scaled((left_px, top_px), scale);
    let (right, bottom) = scaled((right_px, bottom_px), scale);
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
        let label = format!("{} × {}", right_px - left_px, bottom_px - top_px);
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
        Tool::None => "选一个工具再画 · Enter/Ctrl+C 复制 · Ctrl+S 保存 · Esc 关闭",
        Tool::Text => "拖出文字位置后直接输入 · Enter 提交 · Esc 取消",
        Tool::Ellipse => "拖出椭圆 · Esc 取消",
        Tool::Ocr => "拖框选择要识别的区域 · Esc 取消",
        Tool::Mosaic => "拖出要打码的区域 · Esc 取消",
        _ => "拖动工具绘制 · Enter/Ctrl+C 复制 · Ctrl+S 保存 · Esc 取消",
    }
}

fn draw_shapes(dc: HDC, editor: &Editor, from: usize) {
    let scale = editor.scale;
    for shape in editor.shapes.iter().skip(from) {
        match shape {
            Shape::Arrow { a, b, stroke } => {
                draw_arrow(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Rect { a, b, stroke } => {
                draw_rect(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Ellipse { a, b, stroke } => {
                draw_ellipse(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Pen { points, stroke } => {
                draw_pen(dc, points, stroke_width(scale, *stroke), scale)
            }
            Shape::Text { at, text } => {
                // 字号也跟着显示缩放走，导出时 scale=1 不受影响
                let font = cached_font(12.0 * scale, 700);
                let (x, y) = scaled(*at, scale);
                unsafe {
                    let old = SelectObject(dc, font as HGDIOBJ);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, ANNOT);
                    TextOutW(dc, x, y, wide(text).as_ptr(), text.encode_utf16().count() as i32);
                    SelectObject(dc, old);
                }
            }
            Shape::Mosaic { a, b, stroke } => {
                let (from, to) = (scaled(*a, scale), scaled(*b, scale));
                draw_mosaic(
                    dc,
                    &editor.base,
                    from,
                    to,
                    scale,
                    mosaic_block(scale, *stroke),
                )
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

    editor_draw(dc, editor, client);

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

/// 把编辑窗画进任意 DC（窗口用背缓冲，预览用离屏 DIB）
unsafe fn editor_draw(dc: HDC, editor: &mut Editor, client: RECT) {
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

    // 原图只拷一次
    if editor.source.is_none() {
        editor.source = source_dc(&editor.base);
    }

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

    // 图片下面那条（工具栏 + 提示）铺底色：图片贴到窗口边上时不会从按钮缝里透出来
    let band = CreateSolidBrush(crate::theme::ui().bg);
    let image_bottom =
        editor.image_top + (editor.base.height as f32 * editor.scale) as i32;
    let below = RECT {
        left: 0,
        top: image_bottom.max(0),
        right: client.right,
        bottom: client.bottom,
    };
    FillRect(dc, &below, band);
    DeleteObject(band as *mut _);

    // 图形都在「图片坐标系」里，这里把视口原点挪到图片左上角
    let mut old_origin = POINT { x: 0, y: 0 };
    SetViewportOrgEx(dc, editor.image_left, editor.image_top, &mut old_origin);

    // 已提交的图形
    draw_shapes(dc, editor, 0);

    // 正在拖拽的图形（临时）
    let width = stroke_width(editor.scale, editor.stroke);
    if let (Some(from), Some(to)) = (editor.drag_from, editor.drag_to) {
        match editor.tool {
            Tool::Arrow => draw_arrow(dc, from, to, width, editor.scale),
            Tool::Rect => draw_rect(dc, from, to, width, editor.scale),
            Tool::Ellipse => draw_ellipse(dc, from, to, width, editor.scale),
            Tool::Mosaic => {
                // 实时看到打码效果，外面再套一层选框
                let (from_px, to_px) = (scaled(from, editor.scale), scaled(to, editor.scale));
                draw_mosaic(dc, &editor.base, from_px, to_px, editor.scale, mosaic_block(editor.scale, editor.stroke));
                draw_drag_frame(dc, from, to, editor.scale);
            }
            Tool::Text | Tool::Ocr => draw_drag_frame(dc, from, to, editor.scale),
            _ => {}
        }
    }
    if editor.tool == Tool::Pen && editor.pen_points.len() > 1 {
        draw_pen(dc, &editor.pen_points, width, editor.scale);
    }

    // 正在输入文字：把位置框出来，免得白底白字看不见输入框在哪
    if let Some(text) = &editor.text {
        let height = (24.0 * editor.scale).round().max(1.0) as i32;
        let (left, top) = scaled(text.at, editor.scale);
        let right = left + ((text.width as f32 * editor.scale) as i32).max(40);
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

    // 工具栏：微信那种一条圆角底栏，纯图标（文字靠 hover 小标签提示）
    editor.buttons.clear();
    let ui = crate::theme::ui();
    let image_h = (editor.base.height as f32 * editor.scale) as i32;
    let bar_h = s(38.0);
    let bar_top = editor.image_top + image_h + s(3.0);
    let bar_bottom = bar_top + bar_h;
    let item = s(30.0);
    let gap = s(2.0);
    let sep_w = s(11.0);
    let pad = s(6.0);

    // bool = 这一格前面要不要画一条分组竖线
    let layout_items: Vec<(Action, bool)> = vec![
        (Action::Tool(Tool::Ellipse), false),
        (Action::Tool(Tool::Arrow), false),
        (Action::Tool(Tool::Rect), false),
        (Action::Tool(Tool::Pen), false),
        (Action::Tool(Tool::Mosaic), false),
        (Action::Tool(Tool::Text), false),
        (Action::Tool(Tool::Ocr), false),
        (Action::Stroke, true),
        (Action::Undo, false),
        (Action::Topmost, true),
        (Action::Copy, false),
        (Action::Save, false),
        (Action::Pin, false),
        (Action::Minimize, true),
        (Action::Close, false),
        (Action::Done, false),
    ];

    let groups = layout_items.iter().filter(|(_, sep)| *sep).count() as i32;
    let bar_w = layout_items.len() as i32 * item
        + (layout_items.len() as i32 - 1) * gap
        + groups * (sep_w + gap)
        + pad * 2;
    let bar_left = ((client.right - bar_w) / 2).max(s(8.0));

    // 底栏背板：圆角 + 1px 描边
    {
        let brush = CreateSolidBrush(ui.surface);
        let pen = CreatePen(0, 1, ui.border);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            dc,
            bar_left,
            bar_top,
            bar_left + bar_w,
            bar_bottom,
            s(13.0),
            s(13.0),
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }

    let icon_top = bar_top + (bar_h - item) / 2;
    let mut x = bar_left + pad;
    let mut hovered: Option<(Action, RECT)> = None;
    for (action, new_group) in layout_items {
        if new_group {
            let pen = CreatePen(0, 1, ui.border);
            let old = SelectObject(dc, pen as HGDIOBJ);
            let sx = x + sep_w / 2;
            MoveToEx(dc, sx, bar_top + s(11.0), std::ptr::null_mut());
            LineTo(dc, sx, bar_bottom - s(11.0));
            SelectObject(dc, old);
            DeleteObject(pen as *mut _);
            x += sep_w + gap;
        }

        let rect = RECT {
            left: x,
            top: icon_top,
            right: x + item,
            bottom: icon_top + item,
        };
        let active = match action {
            Action::Tool(tool) => editor.tool == tool,
            Action::Topmost => editor.topmost,
            _ => false,
        };
        let pressed = editor.pressed == Some(action);
        let hover = editor.hover == Some(action);

        if active || pressed || hover {
            let fill = if active {
                ui.accent
            } else if pressed {
                ui.chip_hover
            } else {
                ui.chip
            };
            let brush = CreateSolidBrush(fill);
            let pen = CreatePen(0, 1, fill);
            let old_brush = SelectObject(dc, brush as HGDIOBJ);
            let old_pen = SelectObject(dc, pen as HGDIOBJ);
            windows_sys::Win32::Graphics::Gdi::RoundRect(
                dc,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                s(8.0),
                s(8.0),
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(pen as *mut _);
            DeleteObject(brush as *mut _);
        }

        let color = if active {
            ui.bg
        } else if matches!(action, Action::Done) {
            ui.accent
        } else {
            ui.text2
        };
        draw_action_icon(dc, action, rect, color, editor.stroke);
        if hover {
            hovered = Some((action, rect));
        }
        editor.buttons.push((rect, action));
        x = rect.right + gap;
    }

    // 图标没有文字，靠 hover 时在底栏上方弹一个小标签（微信也是这么做的）
    if let Some((action, rect)) = hovered {
        draw_bar_tooltip(dc, action, rect, bar_top, editor.stroke);
    }

    // 小提示（复制成功 / 已保存）：底部中间一个深色胶囊，1.4 秒后自己消失
    if let Some((text, at)) = &editor.toast {
        if at.elapsed() < std::time::Duration::from_millis(1400) {
            let ui = crate::theme::ui();
            let font = cached_font(9.0, 600);
            let old_toast_font = SelectObject(dc, font as HGDIOBJ);
            SetBkMode(dc, TRANSPARENT as i32);
            let text_w = wide_width(dc, text) + s(28.0);
            let height_px = s(26.0);
            let left = (client.right - text_w) / 2;
            let top = client.bottom - s(60.0);
            let rect = RECT {
                left,
                top,
                right: left + text_w,
                bottom: top + height_px,
            };
            let brush = CreateSolidBrush(ui.text);
            let pen = CreatePen(0, 1, ui.text);
            let old_brush = SelectObject(dc, brush as HGDIOBJ);
            let old_toast_pen = SelectObject(dc, pen as HGDIOBJ);
            windows_sys::Win32::Graphics::Gdi::RoundRect(
                dc,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                height_px,
                height_px,
            );
            SelectObject(dc, old_toast_pen);
            SelectObject(dc, old_brush);
            DeleteObject(pen as *mut _);
            DeleteObject(brush as *mut _);

            SetTextColor(dc, ui.bg);
            let mut text_rect = rect;
            text_rect.bottom += 1;
            DrawTextW(
                dc,
                wide(text).as_ptr(),
                -1,
                &mut text_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            SelectObject(dc, old_toast_font);
        }
    }

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
}

/// 调试用：把编辑窗（图片 + 一条底栏）离屏画出来存成 PNG，不弹窗。
/// 设了 `NST_EDITOR_PREVIEW=<路径>` 就走这里，改工具栏 / 图标时不用真截图就能看效果。
pub unsafe fn preview_png(path: &str) -> bool {
    let width = 900;
    let height = 560;

    // 假的「截图内容」：上面一条深色标题栏，下面浅灰底 + 两个色块，方便看底栏压在哪里
    let mut base = Bitmap::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let index = ((y * width + x) * 4) as usize;
            let (mut r, mut g, mut b) = if y < 52 {
                (34u8, 36u8, 40u8)
            } else {
                (246u8, 246u8, 248u8)
            };
            if y >= 120 && y < 260 && x >= 60 && x < 400 {
                (r, g, b) = (232, 240, 252);
            }
            if y >= 300 && y < 440 && x >= 60 && x < 400 {
                (r, g, b) = (240, 250, 236);
            }
            if y >= 120 && y < 440 && x >= 460 && x < 840 {
                (r, g, b) = (250, 244, 236);
            }
            base.pixels[index] = b;
            base.pixels[index + 1] = g;
            base.pixels[index + 2] = r;
            base.pixels[index + 3] = 255;
        }
    }

    let toolbar = s(44.0);
    let hint = s(24.0);
    let client = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height + toolbar + hint,
    };

    let (dc, dib, bits) = make_dib(client.right, client.bottom);
    if dc.is_null() || dib.is_null() {
        return false;
    }

    let mut editor = Editor {
        base,
        shapes: Vec::new(),
        // 选一个正在用的工具、一个 hover 项：一张图里能同时看到「选中」和「悬停提示」
        tool: Tool::Rect,
        scale: 1.0,
        image_left: 0,
        image_top: 0,
        drag_from: None,
        drag_to: None,
        pen_points: Vec::new(),
        text: None,
        stroke: 1,
        buttons: Vec::new(),
        pressed: None,
        hover: Some(Action::Tool(Tool::Mosaic)),
        toast: None,
        topmost: false,
        source: None,
        back: None,
    };

    editor_draw(dc, &mut editor, client);
    windows_sys::Win32::Graphics::Gdi::GdiFlush();

    // BGRA（自顶向下）→ PNG 要的 RGBA
    let mut rgba = Vec::with_capacity((client.right * client.bottom * 4) as usize);
    let raw = std::slice::from_raw_parts(bits, (client.right * client.bottom * 4) as usize);
    for pixel in raw.chunks_exact(4) {
        rgba.push(pixel[2]);
        rgba.push(pixel[1]);
        rgba.push(pixel[0]);
        rgba.push(255);
    }
    let ok = match image::RgbaImage::from_raw(
        client.right as u32,
        client.bottom as u32,
        rgba,
    ) {
        Some(buffer) => buffer.save(path).is_ok(),
        None => false,
    };

    if let Some((source, _)) = editor.source.take() {
        DeleteDC(source);
    }
    DeleteObject(dib as *mut _);
    DeleteDC(dc);
    ok
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

/// 工具栏按钮的图标：全部用 GDI 基本图元画，不依赖图标资源
fn draw_action_icon(dc: HDC, action: Action, rect: RECT, color: u32, stroke: u8) {
    unsafe {
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        let cx = (rect.left + rect.right) / 2;
        let cy = (rect.top + rect.bottom) / 2;
        let r = (w.min(h) as f32 * 0.29).round().max(3.0) as i32;
        let thin = s(1.6).max(1);
        let pen = CreatePen(0, thin, color);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));

        match action {
            Action::Tool(Tool::Rect) => {
                windows_sys::Win32::Graphics::Gdi::Rectangle(dc, cx - r, cy - r, cx + r, cy + r);
            }
            Action::Tool(Tool::Ellipse) => {
                windows_sys::Win32::Graphics::Gdi::Ellipse(dc, cx - r, cy - r, cx + r, cy + r);
            }
            Action::Tool(Tool::Arrow) => {
                let (ax, ay) = (cx - r, cy + r);
                let (bx, by) = (cx + r - 1, cy - r + 1);
                MoveToEx(dc, ax, ay, std::ptr::null_mut());
                LineTo(dc, bx, by);
                let arm = (r as f32 * 0.8) as i32;
                // 箭头两条边：从尖端往左、往下各一条
                MoveToEx(dc, bx, by, std::ptr::null_mut());
                LineTo(dc, bx - arm, by + 1);
                MoveToEx(dc, bx, by, std::ptr::null_mut());
                LineTo(dc, bx - 1, by + arm);
            }
            Action::Tool(Tool::Pen) => {
                // 笔：一根粗斜线当笔杆，左下角一个实心笔尖
                let body = CreatePen(0, (s(2.6)).max(2), color);
                let old_body = SelectObject(dc, body as HGDIOBJ);
                MoveToEx(dc, cx - r + s(3.0), cy + r - s(3.0), std::ptr::null_mut());
                LineTo(dc, cx + r - s(1.0), cy - r + s(1.0));
                SelectObject(dc, old_body);
                DeleteObject(body as *mut _);

                let nib = [
                    POINT { x: cx - r, y: cy + r },
                    POINT { x: cx - r + s(2.0), y: cy + r - s(6.0) },
                    POINT { x: cx - r + s(6.0), y: cy + r - s(2.0) },
                ];
                let brush = CreateSolidBrush(color);
                let old = SelectObject(dc, brush as HGDIOBJ);
                let nib_pen = CreatePen(0, 1, color);
                let old_p = SelectObject(dc, nib_pen as HGDIOBJ);
                Polygon(dc, nib.as_ptr(), 3);
                SelectObject(dc, old_p);
                SelectObject(dc, old);
                DeleteObject(nib_pen as *mut _);
                DeleteObject(brush as *mut _);
            }
            Action::Tool(Tool::Mosaic) => {
                let cell = ((r * 2) as f32 / 3.0).round().max(2.0) as i32;
                for gy in 0..3 {
                    for gx in 0..3 {
                        let l = cx - r + gx * cell;
                        let t = cy - r + gy * cell;
                        if (gx + gy) % 2 == 0 {
                            let brush = CreateSolidBrush(color);
                            let old = SelectObject(dc, brush as HGDIOBJ);
                            let fill_pen = CreatePen(0, 1, color);
                            let old_p = SelectObject(dc, fill_pen as HGDIOBJ);
                            windows_sys::Win32::Graphics::Gdi::Rectangle(
                                dc,
                                l,
                                t,
                                l + cell,
                                t + cell,
                            );
                            SelectObject(dc, old_p);
                            SelectObject(dc, old);
                            DeleteObject(fill_pen as *mut _);
                            DeleteObject(brush as *mut _);
                        } else {
                            windows_sys::Win32::Graphics::Gdi::Rectangle(
                                dc,
                                l,
                                t,
                                l + cell,
                                t + cell,
                            );
                        }
                    }
                }
            }
            Action::Tool(Tool::Text) => draw_glyph(dc, "T", rect, color),
            Action::Tool(Tool::Ocr) => {
                // 「A」+ 一圈框（微信取字就是这个意思）
                let f = r + 2;
                let pen = CreatePen(0, 1, color);
                let old = SelectObject(dc, pen as HGDIOBJ);
                windows_sys::Win32::Graphics::Gdi::RoundRect(
                    dc,
                    cx - f,
                    cy - f,
                    cx + f,
                    cy + f,
                    s(4.0),
                    s(4.0),
                );
                SelectObject(dc, old);
                DeleteObject(pen as *mut _);
                draw_glyph(dc, "A", rect, color);
            }
            Action::Stroke => {
                // 三根线，粗细 1 / 2 / 3，当前那档标个圆点（微信的线宽也是这么示意）
                let len = r;
                let current = stroke.min(2) as usize;
                for (i, level) in [1i32, 2, 3].iter().enumerate() {
                    let p = CreatePen(0, (*level * s(1.0)).max(1), color);
                    let old = SelectObject(dc, p as HGDIOBJ);
                    let ly = cy - s(6.0) + i as i32 * s(6.0);
                    MoveToEx(dc, cx - len, ly, std::ptr::null_mut());
                    LineTo(dc, cx + len, ly);
                    SelectObject(dc, old);
                    DeleteObject(p as *mut _);
                    if i == current {
                        // 当前档右边点一个小圆，跟线同色
                        let brush = CreateSolidBrush(color);
                        let old_b = SelectObject(dc, brush as HGDIOBJ);
                        let dot = s(1.8).max(1);
                        windows_sys::Win32::Graphics::Gdi::Ellipse(
                            dc,
                            cx + len + s(2.0) - dot,
                            ly - dot,
                            cx + len + s(2.0) + dot,
                            ly + dot,
                        );
                        SelectObject(dc, old_b);
                        DeleteObject(brush as *mut _);
                    }
                }
            }
            Action::Undo => {
                // 上半圈 + 左端向下的箭头（↺ 那个意思）
                let pen = CreatePen(0, (thin + 1).max(2), color);
                let old = SelectObject(dc, pen as HGDIOBJ);
                windows_sys::Win32::Graphics::Gdi::Arc(
                    dc,
                    cx - r,
                    cy - r,
                    cx + r,
                    cy + r,
                    cx + r,
                    cy,
                    cx - r,
                    cy,
                );
                SelectObject(dc, old);
                DeleteObject(pen as *mut _);

                let head = [
                    POINT { x: cx - r, y: cy + s(5.0) },
                    POINT { x: cx - r - s(4.0), y: cy - s(1.0) },
                    POINT { x: cx - r + s(4.0), y: cy - s(1.0) },
                ];
                let brush = CreateSolidBrush(color);
                let old_b = SelectObject(dc, brush as HGDIOBJ);
                Polygon(dc, head.as_ptr(), 3);
                SelectObject(dc, old_b);
                DeleteObject(brush as *mut _);
            }
            Action::Copy => {
                // 两张叠在一起的纸
                let d = r;
                windows_sys::Win32::Graphics::Gdi::Rectangle(
                    dc,
                    cx - d,
                    cy - d + s(3.0) as i32,
                    cx + d - s(3.0) as i32,
                    cy + d,
                );
                windows_sys::Win32::Graphics::Gdi::Rectangle(
                    dc,
                    cx - d + s(3.0) as i32,
                    cy - d,
                    cx + d,
                    cy + d - s(3.0) as i32,
                );
            }
            Action::Save => {
                // 向下的箭头 + 底下的托盘
                MoveToEx(dc, cx, cy - r, std::ptr::null_mut());
                LineTo(dc, cx, cy + r / 2);
                MoveToEx(dc, cx - r / 2, cy, std::ptr::null_mut());
                LineTo(dc, cx, cy + r / 2);
                MoveToEx(dc, cx + r / 2, cy, std::ptr::null_mut());
                LineTo(dc, cx, cy + r / 2);
                MoveToEx(dc, cx - r, cy + r, std::ptr::null_mut());
                LineTo(dc, cx + r, cy + r);
            }
            Action::Pin => {
                // 图钉：实心圆头 + 一根针 + 底座
                let brush = CreateSolidBrush(color);
                let old_b = SelectObject(dc, brush as HGDIOBJ);
                let head_pen = CreatePen(0, 1, color);
                let old_p = SelectObject(dc, head_pen as HGDIOBJ);
                windows_sys::Win32::Graphics::Gdi::Ellipse(
                    dc,
                    cx - r / 2,
                    cy - r,
                    cx + r / 2,
                    cy,
                );
                SelectObject(dc, old_p);
                SelectObject(dc, old_b);
                DeleteObject(head_pen as *mut _);
                DeleteObject(brush as *mut _);
                MoveToEx(dc, cx, cy, std::ptr::null_mut());
                LineTo(dc, cx, cy + r - 2);
                MoveToEx(dc, cx - r / 2, cy + r - 2, std::ptr::null_mut());
                LineTo(dc, cx + r / 2, cy + r - 2);
            }
            Action::Topmost => {
                // 窗口 + 向上的箭头（这个窗口要留在最上面）
                windows_sys::Win32::Graphics::Gdi::Rectangle(
                    dc,
                    cx - r,
                    cy - r + s(2.0) as i32,
                    cx + r,
                    cy + r,
                );
                MoveToEx(dc, cx, cy + r - s(2.0) as i32, std::ptr::null_mut());
                LineTo(dc, cx, cy - r + s(3.0) as i32);
                MoveToEx(dc, cx - s(3.0) as i32, cy - r + s(6.0) as i32, std::ptr::null_mut());
                LineTo(dc, cx, cy - r + s(3.0) as i32);
                MoveToEx(dc, cx + s(3.0) as i32, cy - r + s(6.0) as i32, std::ptr::null_mut());
                LineTo(dc, cx, cy - r + s(3.0) as i32);
            }
            Action::Minimize => {
                let arm = r;
                MoveToEx(dc, cx - arm, cy, std::ptr::null_mut());
                LineTo(dc, cx + arm, cy);
            }
            Action::Close => {
                let arm = r - s(1.0) as i32;
                MoveToEx(dc, cx - arm, cy - arm, std::ptr::null_mut());
                LineTo(dc, cx + arm, cy + arm);
                MoveToEx(dc, cx - arm, cy + arm, std::ptr::null_mut());
                LineTo(dc, cx + arm, cy - arm);
            }
            Action::Done => {
                // 对勾：两段折线，稍粗一点（微信那个绿色的 ✓）
                let pen = CreatePen(0, thin + 1, color);
                let old = SelectObject(dc, pen as HGDIOBJ);
                let arm = r;
                MoveToEx(dc, cx - arm, cy, std::ptr::null_mut());
                LineTo(dc, cx - s(2.0) as i32, cy + arm - s(2.0) as i32);
                LineTo(dc, cx + arm, cy - arm + s(1.0) as i32);
                SelectObject(dc, old);
                DeleteObject(pen as *mut _);
            }
            _ => {}
        }

        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(pen as *mut _);
    }
}

/// 画一个居中的字符（T / A 这种，直接借系统字体，不用自己拼笔画）
fn draw_glyph(dc: HDC, text: &str, rect: RECT, color: u32) {
    unsafe {
        let font = cached_font(11.0, 700);
        let old = SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        let mut target = rect;
        target.top += 1;
        DrawTextW(
            dc,
            wide(text).as_ptr(),
            -1,
            &mut target,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old);
    }
}

/// hover 时在底栏上方弹一个小标签（图标没文字，靠它认工具）
fn draw_bar_tooltip(dc: HDC, action: Action, anchor: RECT, bar_top: i32, stroke: u8) {
    unsafe {
        let label = match action {
            Action::Stroke => STROKE_LABELS[stroke.min(2) as usize],
            _ => text_of_action(action),
        };
        if label.is_empty() {
            return;
        }
        let font = cached_font(8.5, 500);
        let old_font = SelectObject(dc, font as HGDIOBJ);
        let width = wide_width(dc, label) + s(16.0);
        let height = s(20.0);
        let left = ((anchor.left + anchor.right) / 2 - width / 2).max(s(4.0));
        let top = bar_top - height - s(5.0);
        let rect = RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };

        // 和底栏同色 + 1px 描边：浅色主题下是白底灰边，深色主题下是深底浅字，
        // 弹在图片上方（图片大多偏亮）也看得清
        let ui = crate::theme::ui();
        let (fill, ink) = (ui.surface, ui.text);
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, ui.border);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            s(8.0),
            s(8.0),
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);

        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, ink);
        let mut target = rect;
        DrawTextW(
            dc,
            wide(label).as_ptr(),
            -1,
            &mut target,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);
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
            stroke: 1,
            topmost: self.topmost,
            buttons: Vec::new(),
            pressed: None,
            hover: None,
            toast: None,
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

/// 保存结果：取消和真失败要区分开 —— 用户点「取消」不该弹权限/磁盘的警告
enum SaveOutcome {
    Saved,
    Cancelled,
    Failed,
}

fn save_bitmap(bitmap: &Bitmap, owner: HWND) -> SaveOutcome {
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
            // 对话框返回 0 有两种：用户点了取消（扩展错误码为 0），或者真出错
            return if CommDlgExtendedError() == 0 {
                SaveOutcome::Cancelled
            } else {
                SaveOutcome::Failed
            };
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
            None => return SaveOutcome::Failed,
        };

        if buffer.save(&path).is_ok() {
            SaveOutcome::Saved
        } else {
            SaveOutcome::Failed
        }
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
            // 再点一次同一个工具就取消选中
            editor.tool = if editor.tool == tool { Tool::None } else { tool };
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        Action::Undo => {
            let editor = &mut *editor;
            editor.shapes.pop();
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        Action::Stroke => {
            let editor = &mut *editor;
            editor.stroke = (editor.stroke + 1) % 3;
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        Action::Copy => {
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            clipboard_bitmap(&bitmap);
            SetWindowTextW(hwnd, wide("截图编辑 · 已复制到剪贴板").as_ptr());
            show_toast(hwnd, "已复制到剪贴板");
        }
        Action::Save => {
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            match save_bitmap(&bitmap, hwnd) {
                SaveOutcome::Saved => {
                    SetWindowTextW(hwnd, wide("截图编辑 · 已保存").as_ptr());
                    show_toast(hwnd, "已保存");
                }
                // 用户自己点了取消，什么都不用说
                SaveOutcome::Cancelled => {}
                SaveOutcome::Failed => {
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
        }
        Action::Pin => {
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            open_pin(bitmap);
        }
        Action::Topmost => {
            let editor = &mut *editor;
            editor.topmost = !editor.topmost;
            crate::set_editor_topmost(PANEL, editor.topmost);
            let insert = if editor.topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
            // 置顶要盖过任务栏：先补上 WS_EX_TOPMOST，再插到 topmost 组的最前面
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let style = if editor.topmost {
                style | WS_EX_TOPMOST
            } else {
                style & !WS_EX_TOPMOST
            };
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
            SetWindowPos(hwnd, insert, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            if editor.topmost {
                // 任务栏也是 topmost：把自己再抬到 topmost 组最前面，并激活一下，才盖得住
                SetForegroundWindow(hwnd);
                SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            }
            SetWindowTextW(
                hwnd,
                wide(if editor.topmost {
                    "截图编辑 · 已置顶"
                } else {
                    "截图编辑 · 不置顶"
                })
                .as_ptr(),
            );
            show_toast(hwnd, if editor.topmost { "已置顶" } else { "已取消置顶" });
        }
        Action::Minimize => {
            ShowWindow(hwnd, SW_MINIMIZE);
        }
        Action::Done => {
            // 微信的 ✓：复制到剪贴板然后直接收工
            commit_text(hwnd);
            let editor = &*editor;
            let bitmap = composite(editor);
            clipboard_bitmap(&bitmap);
            DestroyWindow(hwnd);
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

/// 窗口被拖到另一块缩放不同的屏：按新屏的缩放重排（工具条高度、图片显示比例、居中偏移）。
///
/// 不处理 `WM_DPICHANGED` 的话，`s()` 和图片 `scale` 都还是旧屏那份，
/// 箭头 / 矩形就会重新出现偏移（就是修掉的那个「错位」的另一种触发方式）。
unsafe fn editor_relayout(hwnd: HWND, suggested: *const RECT) {
    let editor = editor_state(hwnd);
    if editor.is_null() {
        return;
    }
    let editor = &mut *editor;

    // 按窗口中心所在的显示器重定缩放
    let mut rect: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut rect) == 0 {
        return;
    }
    let anchor = POINT {
        x: (rect.left + rect.right) / 2,
        y: (rect.top + rect.bottom) / 2,
    };
    use_scale_at(anchor);

    let work = crate::work_area_at(anchor);
    let max_w = ((work.right - work.left) as f32 * 0.85).max(1.0);
    let chrome = s(44.0) + s(24.0);
    let max_h = (((work.bottom - work.top) as f32 * 0.8) as i32 - chrome).max(1) as f32;
    let scale = (max_w / editor.base.width as f32)
        .min(max_h / editor.base.height as f32)
        .min(1.0);

    let image_w = (editor.base.width as f32 * scale) as i32;
    let image_h = (editor.base.height as f32 * scale) as i32;
    let toolbar = s(44.0);
    let hint = s(24.0);
    let client_w = image_w.max(s(600.0));
    let client_h = image_h + toolbar + hint;

    editor.scale = scale;
    editor.image_left = (client_w - image_w) / 2;
    editor.image_top = 0;

    let (x, y) = if suggested.is_null() {
        (rect.left, rect.top)
    } else {
        ((*suggested).left, (*suggested).top)
    };
    // 背缓冲尺寸变了会自己重建；这里只管窗口尺寸和坐标
    SetWindowPos(
        hwnd,
        std::ptr::null_mut(),
        x,
        y,
        client_w,
        client_h,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

unsafe extern "system" fn editor_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DPICHANGED => {
            editor_relayout(hwnd, lparam as *const RECT);
            0
        }
        // 置顶着的时候，被别的窗口/任务栏顶下去就再顶回来
        WM_ACTIVATE | WM_WINDOWPOSCHANGED => {
            let editor = editor_state(hwnd);
            if !editor.is_null() && (*editor).topmost {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_EDITOR_TOAST => {
            let editor = editor_state(hwnd);
            if !editor.is_null() {
                let expired = (*editor)
                    .toast
                    .as_ref()
                    .map(|(_, at)| at.elapsed() >= std::time::Duration::from_millis(1400))
                    .unwrap_or(true);
                if expired {
                    (*editor).toast = None;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            0
        }
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
                // 先记下「按着」，松开鼠标才执行 —— 按下有反馈，滑出去松开就不算点击
                let editor = &mut *editor;
                editor.pressed = Some(action);
                SetCapture(hwnd);
                InvalidateRect(hwnd, std::ptr::null(), 0);
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
                Tool::None => {}
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
            // 工具栏图标没有文字，hover 时要在上方弹小标签：这里维护 hover 状态
            let cursor_point = POINT {
                x: (lparam as i32) & 0xFFFF,
                y: ((lparam as i32) >> 16) & 0xFFFF,
            };
            let hover = hit_button(editor, cursor_point);
            if editor.hover != hover {
                editor.hover = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            // 让鼠标离开客户区时能收到 WM_MOUSELEAVE，把高亮和标签清掉
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);

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
        // 鼠标移出客户区：把 hover 高亮和小标签收掉
        WM_MOUSELEAVE => {
            let editor = editor_state(hwnd);
            if !editor.is_null() && (*editor).hover.is_some() {
                (*editor).hover = None;
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

            // 按钮：松开时还停在同一个按钮上才算点击
            if let Some(action) = editor.pressed.take() {
                let mut point = POINT {
                    x: (lparam as i32) & 0xFFFF,
                    y: ((lparam as i32) >> 16) & 0xFFFF,
                };
                let _ = &mut point;
                let same = hit_button(editor, point) == Some(action);
                InvalidateRect(hwnd, std::ptr::null(), 0);
                if same {
                    editor_command(hwnd, action);
                }
                return 0;
            }

            if editor.tool == Tool::Pen {
                if editor.pen_points.len() > 1 {
                    let points = std::mem::take(&mut editor.pen_points);
                    editor.shapes.push(Shape::Pen {
                        points,
                        stroke: editor.stroke,
                    });
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
                        editor.shapes.push(Shape::Arrow {
                            a: from,
                            b: to,
                            stroke: editor.stroke,
                        })
                    }
                    Tool::Rect if (from.0 - to.0).abs() >= 3 || (from.1 - to.1).abs() >= 3 => {
                        editor.shapes.push(Shape::Rect {
                            a: from,
                            b: to,
                            stroke: editor.stroke,
                        })
                    }
                    Tool::Ellipse if (from.0 - to.0).abs() >= 3 || (from.1 - to.1).abs() >= 3 => {
                        editor.shapes.push(Shape::Ellipse {
                            a: from,
                            b: to,
                            stroke: editor.stroke,
                        })
                    }
                    Tool::Mosaic if wide_enough => {
                        editor.shapes.push(Shape::Mosaic {
                            a: from,
                            b: to,
                            stroke: editor.stroke,
                        })
                    }
                    Tool::Text if wide_enough => {
                        // 框的左下角起笔，输入框宽度跟框一样宽
                        let width = right - left;
                        start_text(hwnd, (left, top), width);
                    }
                    Tool::Ocr if wide_enough => {
                        // 只对框里这块做 OCR
                        let region = editor.base.crop(left, top, right - left, bottom - top);
                        editor.tool = Tool::None;
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
                        (*editor).tool = Tool::None;
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
                    show_toast(hwnd, "已复制到剪贴板");
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
                    show_toast(hwnd, "已复制到剪贴板");
                }
                0x53 if GetKeyState(0x11) < 0 => {
                    commit_text(hwnd);
                    let editor = &*editor_state(hwnd);
                    let bitmap = composite(editor);
                    match save_bitmap(&bitmap, hwnd) {
                        SaveOutcome::Saved => {
                            SetWindowTextW(hwnd, wide("截图编辑 · 已保存").as_ptr());
                            show_toast(hwnd, "已保存");
                        }
                        SaveOutcome::Cancelled => {}
                        SaveOutcome::Failed => {
                            MessageBoxW(
                                hwnd,
                                wide("保存失败：磁盘空间不足、没有写入权限，或路径无效。")
                                    .as_ptr(),
                                wide("保存截图").as_ptr(),
                                MB_OK | MB_ICONWARNING,
                            );
                        }
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
pub unsafe fn open_editor(panel: HWND, bitmap: Bitmap, rect: (i32, i32, i32, i32)) {
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
    // 底部一条工具栏 + 底部提示也要占高度，算缩放时先扣掉，否则窗口比屏幕高、底部会被裁掉
    let chrome = s(44.0) + s(24.0);
    let work_w = work.right - work.left;
    let work_h = work.bottom - work.top;
    let max_h = (work_h as f32 * 0.8) as i32 - chrome;

    let mut scale = (max_w as f32 / bitmap.width as f32)
        .min(max_h as f32 / bitmap.height as f32)
        .min(1.0);

    let toolbar = s(44.0);
    let hint = s(24.0);

    // 原位摆放装不下（比如整窗最大化，底下没地方放工具栏）就缩一点，
    // 保证「整张图 + 底部工具栏 + 提示」都在屏幕里 —— 宁可稍微缩小，也不能把图裁掉
    let fits_in_place = |scale: f32| {
        let height = (bitmap.height as f32 * scale) as i32 + toolbar + hint;
        rect.1 >= work.top && rect.1 + height <= work.bottom
    };
    if !fits_in_place(scale) {
        let fit_h = ((work_h - chrome) as f32 / bitmap.height as f32).min(1.0);
        let fit_w = ((work_w - s(24.0)) as f32 / bitmap.width as f32).min(1.0);
        scale = scale.min(fit_h).min(fit_w);
    }

    let image_w = (bitmap.width as f32 * scale) as i32;
    let image_h = (bitmap.height as f32 * scale) as i32;

    let client_w = image_w.max(s(600.0));
    // 图片在上面，底部一条工具栏 + 提示，始终在图片外面（微信那种单条底栏）
    let client_h = image_h + toolbar + hint;

    let image_left = (client_w - image_w) / 2;
    let mut image_top = 0;
    let mut window_x = rect.0 - image_left;
    let mut window_y = rect.1;
    if !fits_in_place(scale) {
        // 原位放不下：整窗居中，图片完整显示（不裁切）
        window_y = work.top + ((work_h - client_h) / 2).max(0);
        image_top = 0;
    } else if window_y < work.top {
        window_y = work.top;
    }
    if window_y + client_h > work.bottom {
        window_y = (work.bottom - client_h).max(work.top);
    }
    // 整窗必须装得下：否则底部（图片下半截）会被屏幕切掉
    if client_h <= work.bottom - work.top && window_y + client_h > work.bottom {
        window_y = (work.bottom - client_h).max(work.top);
    }
    if window_x + client_w > work.right {
        window_x = (work.right - client_w).max(work.left);
    }
    if window_x < work.left {
        window_x = work.left;
    }

    let state = Box::into_raw(Box::new(Editor {
        base: bitmap,
        shapes: Vec::new(),
        tool: Tool::None,
        scale,
        image_left,
        image_top,
        drag_from: None,
        drag_to: None,
        pen_points: Vec::new(),
        stroke: 1,
        topmost: crate::editor_topmost(panel),
        text: None,
        buttons: Vec::new(),
        pressed: None,
        hover: None,
        toast: None,
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
        client_w,
        client_h,
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

    // 上次选了置顶就沿用
    let editor = editor_state(hwnd);
    if !editor.is_null() && (*editor).topmost {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 | WS_EX_TOPMOST;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}
