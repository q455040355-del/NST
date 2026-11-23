//! 自绘 UI 的公共件：配色、位图缓冲、画笔、字体缓存、圆角。
//!
//! 设置窗口 / 右键菜单 / 取字结果窗 都走这一套，保证三处观感一致。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateRoundRectRgn,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, FillRect, GetDC, LineTo, MoveToEx,
    ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, BITMAPINFO,
    BITMAPINFOHEADER, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CALCRECT, DT_SINGLELINE, HBITMAP, HDC,
    HFONT, HGDIOBJ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetCursor};

use crate::theme::{ui_face, wide};

pub const DT_NOPREFIX: u32 = 0x0800;
pub const DT_CENTER: u32 = 0x0001;
pub const DT_LEFT: u32 = 0x0000;
pub const DT_RIGHT: u32 = 0x0002;
pub const DT_VCENTER: u32 = 0x0004;

// 窗口配色统一走 theme::ui()（跟随系统明暗）；这里只留非颜色的常量
pub const IDC_HAND: *const u16 = 32649 as *const u16;

pub fn s(value: f32, scale: f32) -> i32 {
    (value * scale).round() as i32
}


/// 偏白（t=0 原色，t=1 纯白）
pub fn mix_white(color: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| -> u32 {
        let value = ((color >> shift) & 0xFF) as f32;
        (value + (255.0 - value) * t).round() as u32
    };
    ch(0) | (ch(8) << 8) | (ch(16) << 16)
}


pub fn make_dib(width: i32, height: i32) -> (HDC, HBITMAP, *mut u8) {
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

thread_local! {
    /// 字体缓存：CreateFontW 每次都要建字体，界面里每帧都建会明显发顿
    static FONTS: RefCell<HashMap<(i32, i32), HFONT>> = RefCell::new(HashMap::new());
}

pub fn font(points: f32, weight: i32, scale: f32) -> HFONT {
    let key = ((points * 10.0).round() as i32, weight);
    FONTS.with(|cell| {
        if let Some(handle) = cell.borrow().get(&key) {
            return *handle;
        }
        let height = -((points * scale * 96.0 / 72.0).round() as i32);
        let handle = unsafe {
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
                // 统一用系统自带的中文字体（数字也是它，别一会儿 Consolas 一会儿雅黑）
                ui_face().as_ptr(),
            )
        };
        cell.borrow_mut().insert(key, handle);
        handle
    })
}

/// 手型光标（下拉、按钮用）
pub fn set_hand_cursor() {
    unsafe {
        SetCursor(::windows_sys::Win32::UI::WindowsAndMessaging::LoadCursorW(
            std::ptr::null_mut(),
            IDC_HAND,
        ));
    }
}


/// 窗口圆角：Win11 交给 DWM；Win10 没有这个特性，就自己把窗口切成圆角区域
pub fn round_corners(hwnd: HWND) {
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_ROUND: i32 = 2;
    let value = DWMWCP_ROUND;
    let done;
    unsafe {
        done = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &value as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        ) == 0;
    }
    if done {
        return;
    }

    // Win10：按窗口大小切一个圆角区域（这些窗口都是固定尺寸，切一次就够）
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return;
        }
        let radius = (14.0 * crate::screen_scale()).round() as i32;
        let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius, radius);
        if !rgn.is_null() {
            SetWindowRgn(hwnd, rgn, 1);
        }
    }
}

pub struct Painter {
    pub dc: HDC,
    pub scale: f32,
}

impl Painter {
    pub fn new(dc: HDC, scale: f32) -> Self {
        Self { dc, scale }
    }

    pub fn s(&self, value: f32) -> i32 {
        s(value, self.scale)
    }

    pub unsafe fn fill(&self, left: i32, top: i32, right: i32, bottom: i32, color: u32) {
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

    pub unsafe fn round(
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

    pub unsafe fn line(&self, x1: i32, y1: i32, x2: i32, y2: i32, color: u32, width: i32) {
        let pen = CreatePen(0, width.max(1), color);
        let old = SelectObject(self.dc, pen as HGDIOBJ);
        MoveToEx(self.dc, x1, y1, std::ptr::null_mut());
        LineTo(self.dc, x2, y2);
        SelectObject(self.dc, old);
        DeleteObject(pen as *mut _);
    }

    pub unsafe fn circle(&self, cx: i32, cy: i32, radius: i32, fill: u32, border: u32) {
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, border);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        ::windows_sys::Win32::Graphics::Gdi::Ellipse(
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

    pub unsafe fn text(
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

    pub unsafe fn text_width(&self, text: &str, font: HFONT) -> i32 {
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


/// GDI 资源释放助手（Drop 时删）
pub struct Dib {
    pub dc: HDC,
    pub bitmap: HBITMAP,
}

impl Dib {
    pub fn new(width: i32, height: i32) -> Option<Self> {
        let (dc, bitmap, _bits) = make_dib(width, height);
        if dc.is_null() || bitmap.is_null() {
            if !dc.is_null() {
                unsafe { DeleteDC(dc) };
            }
            return None;
        }
        let _ = (width, height);
        Some(Self { dc, bitmap })
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.bitmap as *mut _);
            DeleteDC(self.dc);
        }
    }
}

/// 方角：编辑窗用。圆角会把图片四个角切掉，描边就只剩四条边中间那段。
pub fn square_corners(hwnd: HWND) {
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_DONOTROUND: i32 = 1;
    let value = DWMWCP_DONOTROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &value as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );
        // Win10 走的是 SetWindowRgn 那条路，这里清掉可能残留的圆角区域
        SetWindowRgn(hwnd, std::ptr::null_mut(), 1);
    }
}
