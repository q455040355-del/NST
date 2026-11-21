//! 截一块屏幕区域录成动图 GIF。
//!
//! 体积优先的三件事：
//! 1. **全局调色板**：整段共用一个 255 色的 NeuQuant 调色板（第 256 号留给透明），
//!    帧里不再塞各自的局部色表；
//! 2. **帧间差分**：和上一帧没变化的像素写透明色，dispose = Keep，静止画面几乎不占字节；
//! 3. **重复帧合并**：连续完全相同的帧不写新帧，只把延迟累加到前一帧上。
//!
//! 采集和编码在同一个后台线程里，边拍边写进内存，不会攒几千张整帧位图。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};

use windows_sys::Win32::Foundation::{
    HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CombineRgn, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen,
    CreateRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    InvalidateRect, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    BITMAPINFO, BITMAPINFOHEADER, DT_CENTER, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HGDIOBJ,
    PAINTSTRUCT, RGN_DIFF, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;

const BORDER_CLASS: &str = "NetSpeedTaskbarRs.GifBorder";
const STOP_CLASS: &str = "NetSpeedTaskbarRs.GifStop";
const TOAST_CLASS: &str = "NetSpeedTaskbarRs.RecToast";
const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 边框画在选区**外面**这么多像素（不然会被录进 GIF）
const BORDER: i32 = 3;
/// 浮动操作条：左边「保存」，右边「✔（结束并复制到剪贴板）」
const BAR_W: i32 = 150;
const BAR_H: i32 = 42;

static RECORDING: AtomicBool = AtomicBool::new(false);
/// 操作条窗口（停止时先收起来）
static BAR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// 0 = 还在录，1 = 结束并复制到剪贴板，2 = 结束并另存为
static STOP_MODE: AtomicU8 = AtomicU8::new(0);
/// 鼠标悬停在操作条左半（0）还是右半（1）；9 = 没有悬停
static BAR_HOVER: AtomicI32 = AtomicI32::new(9);

pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// 结束录制并复制到剪贴板（热键 / ✔ 都走这里）
pub fn stop() {
    stop_with(1);
}

/// `1` = 复制到剪贴板，`2` = 弹另存为
pub fn stop_with(mode: u8) {
    if is_recording() && STOP_MODE.load(Ordering::SeqCst) == 0 {
        STOP_MODE.store(mode, Ordering::SeqCst);
        // 先收起操作条：编码要一两秒，不给反馈会像卡死
        let bar = BAR.load(Ordering::SeqCst) as HWND;
        if !bar.is_null() {
            unsafe {
                ShowWindow(bar, SW_HIDE);
            }
        }
    }
}

/// 开始录制一块屏幕区域（物理像素）。只负责开窗口，真正采集在后台线程。
pub unsafe fn start(panel: HWND, rect: (i32, i32, i32, i32), fps: u32) {
    let (x, y, w, h) = rect;
    if w < 16 || h < 16 || RECORDING.swap(true, Ordering::SeqCst) {
        return;
    }
    STOP_MODE.store(0, Ordering::SeqCst);
    crate::debug_log(&format!("rec start: {x},{y} {w}x{h} fps={fps}"));
    BAR_HOVER.store(9, Ordering::SeqCst);


    let instance = GetModuleHandleW(std::ptr::null());

    // 选区边框：在选区外侧画 3px，鼠标可穿透（不挡被录的窗口操作）
    let border_class = wide(BORDER_CLASS);
    let border_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(border_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: border_class.as_ptr(),
    };
    RegisterClassW(&border_wc);
    let border = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        border_class.as_ptr(),
        wide("GIF 录制区域").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        x - BORDER,
        y - BORDER,
        w + BORDER * 2,
        h + BORDER * 2,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if border.is_null() {
        RECORDING.store(false, Ordering::SeqCst);
        return;
    }
    // 只留一圈边框，中间留空（窗口本身是点击穿透的，留空只是为了不挡视觉）
    let outer = CreateRectRgn(0, 0, w + BORDER * 2, h + BORDER * 2);
    let inner = CreateRectRgn(BORDER, BORDER, BORDER + w, BORDER + h);
    CombineRgn(outer, outer, inner, RGN_DIFF);
    SetWindowRgn(border, outer, 0);
    DeleteObject(inner as *mut _);
    DeleteObject(outer as *mut _);

    // ✔ 结束按钮：放在选区右上角的外侧，不会被录进去
    let stop_class = wide(STOP_CLASS);
    let stop_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(stop_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_HAND),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: stop_class.as_ptr(),
    };
    RegisterClassW(&stop_wc);

    // 操作条：优先放在选区右上角**外侧**（不会被录进去）；
    // 贴着屏幕上沿/右沿时依次尝试右侧、下方，实在没地方才压进选区里
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let fits = |bx: i32, by: i32| bx >= vx && by >= vy && bx + BAR_W <= vx + vw && by + BAR_H <= vy + vh;
    let candidates = [
        (x + w - BAR_W, y - BAR_H - BORDER - 2),
        (x + w + BORDER + 4, y),
        (x + w - BAR_W, y + h + BORDER + 4),
        (x + w - BAR_W, y + 2),
    ];
    let (button_x, button_y) = candidates
        .into_iter()
        .find(|(bx, by)| fits(*bx, *by))
        .unwrap_or(candidates[3]);
    let button = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        stop_class.as_ptr(),
        wide("录制操作").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        button_x,
        button_y,
        BAR_W,
        BAR_H,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

    BAR.store(button as usize, Ordering::SeqCst);
    // 圆角用窗口区域切出来：否则圆角外面会露出窗口底色的黑点
    let rgn = windows_sys::Win32::Graphics::Gdi::CreateRoundRectRgn(
        0,
        0,
        BAR_W + 1,
        BAR_H + 1,
        s_round() * 2,
        s_round() * 2,
    );
    if !rgn.is_null() {
        SetWindowRgn(button, rgn, 1);
    }
    let panel = panel as usize;
    let border_raw = border as usize;
    let button_raw = button as usize;
    std::thread::spawn(move || run(panel, x, y, w, h, fps, border_raw, button_raw));
}

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut client);
            let brush = CreateSolidBrush(crate::theme::ui().accent);
            FillRect(dc, &client, brush);
            DeleteObject(brush as *mut _);
            EndPaint(hwnd, &mut ps);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn stop_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let hover = BAR_HOVER.load(Ordering::Relaxed);
            paint_bar(hwnd, dc, hover);
            EndPaint(hwnd, &mut ps);
            0
        }
        WM_MOUSEMOVE => {
            let x = (lparam & 0xFFFF) as i16 as i32;
            let hover = if x < BAR_W / 2 { 0 } else { 1 };
            if BAR_HOVER.swap(hover, Ordering::SeqCst) != hover {
                InvalidateRect(hwnd, std::ptr::null(), 0);
                // 鼠标移出窗口时要收回高亮
                let mut track: TRACKMOUSEEVENT = std::mem::zeroed();
                track.cbSize = std::mem::size_of::<TRACKMOUSEEVENT>() as u32;
                track.dwFlags = TME_LEAVE;
                track.hwndTrack = hwnd;
                TrackMouseEvent(&mut track);
            }
            0
        }
        WM_MOUSELEAVE => {
            BAR_HOVER.store(9, Ordering::SeqCst);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let x = (lparam & 0xFFFF) as i16 as i32;
            // 左半 = 保存，右半 = ✔（结束并复制到剪贴板）
            stop_with(if x < BAR_W / 2 { 2 } else { 1 });
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
            1
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 操作条：白色圆角卡片，左边下载图标（保存），右边对勾（结束并复制）
unsafe fn paint_bar(hwnd: HWND, dc: HDC, hover: i32) {
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    let width = BAR_W;
    let height = BAR_H;

    // 卡片底色 + 细边框（不用 UpdateLayeredWindow，圆角用 RoundRect 画）
    let bg = CreateSolidBrush(0x00FF_FFFF);
    let border = CreatePen(0, 1, 0x00D8_DCE0);
    let old_brush = SelectObject(dc, bg as HGDIOBJ);
    let old_pen = SelectObject(dc, border as HGDIOBJ);
    RoundRect(dc, 0, 0, width, height, s_round(), s_round());
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    DeleteObject(border as *mut _);
    DeleteObject(bg as *mut _);

    // 两个按钮各自一圈淡灰描边（中间不再画分隔线）
    let half = width / 2;
    let stroke = CreatePen(0, 1, 0x00D2_D7DC);
    let old_pen = SelectObject(dc, stroke as HGDIOBJ);
    let hollow = windows_sys::Win32::Graphics::Gdi::GetStockObject(
        windows_sys::Win32::Graphics::Gdi::NULL_BRUSH,
    );
    let old_brush = SelectObject(dc, hollow as HGDIOBJ);
    for index in 0..2 {
        let left = index as i32 * half + 3;
        let right = (index as i32 + 1) * half - 3;
        RoundRect(dc, left, 3, right, height - 3, s_round(), s_round());
    }
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(stroke as *mut _);

    // 悬停的那一半铺一层浅灰
    if hover == 0 || hover == 1 {
        let left = if hover == 0 { 2 } else { half + 1 };
        let right = if hover == 0 { half - 1 } else { width - 2 };
        let hot = CreateSolidBrush(0x00EE_F2F6);
        let pen = CreatePen(0, 1, 0x00EE_F2F6);
        let ob = SelectObject(dc, hot as HGDIOBJ);
        let op = SelectObject(dc, pen as HGDIOBJ);
        RoundRect(dc, left, 2, right, height - 2, 8, 8);
        SelectObject(dc, op);
        SelectObject(dc, ob);
        DeleteObject(pen as *mut _);
        DeleteObject(hot as *mut _);
    }

    // 文字比手绘图标清楚：「保存」/「结束」
    let font = CreateFontW(
        -16,
        0,
        0,
        0,
        500,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        wide("Microsoft YaHei UI").as_ptr(),
    );
    let old_font = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);

    for (index, label) in ["保存", "结束"].into_iter().enumerate() {
        let _ = index;
        SetTextColor(dc, 0x0030_3438);
        let mut rect = RECT {
            left: index as i32 * half,
            top: 0,
            right: (index as i32 + 1) * half,
            bottom: height,
        };
        DrawTextW(
            dc,
            wide(label).as_ptr(),
            -1,
            &mut rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
    SelectObject(dc, old_font);
    DeleteObject(font as *mut _);
}
/// 操作条的圆角半径（固定 10px，跟着系统缩放走）
fn s_round() -> i32 {
    12
}

/// 录完复制到剪贴板：CF_DIB（首帧静态图）+ 文件（动画本体，能贴进聊天窗口）
pub unsafe fn copy_to_clipboard(owner: HWND, bytes: Vec<u8>) {
    if bytes.is_empty() {
        return;
    }

    // 动画 WebP 先落到临时文件，剪贴板里放文件路径，微信/QQ/浏览器都认
    let mut stamp: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
    windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut stamp);
    let dir = std::env::temp_dir().join("NST");
    let _ = std::fs::create_dir_all(&dir);
    // 只留最新的一条：剪贴板里放着上一个文件就够了，缓存别越攒越多
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Rec_") && name.ends_with(".webp") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let path = dir.join(format!(
        "Rec_{:04}{:02}{:02}_{:02}{:02}{:02}_{}.webp",
        stamp.wYear,
        stamp.wMonth,
        stamp.wDay,
        stamp.wHour,
        stamp.wMinute,
        stamp.wSecond,
        stamp.wMilliseconds
    ));
    if std::fs::write(&path, &bytes).is_err() {
        return;
    }

    // 首帧解出来放 CF_DIB：不支持文件拖放的编辑器也能直接粘贴
    let dib = webp::AnimDecoder::new(&bytes)
        .decode()
        .ok()
        .and_then(|frames| frames.get_frame(0).and_then(|frame| rgba_to_dib(&frame)))
        ;

    // 剪贴板可能被别的程序占着，重试几次
    let mut opened = 0;
    for attempt in 0..6 {
        opened = OpenClipboard(owner);
        if opened != 0 {
            break;
        }
        crate::debug_log(&format!("clipboard open failed, attempt {attempt}"));
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    if opened == 0 {
        return;
    }
    EmptyClipboard();

    if let Some((data, len)) = dib {
        let handle = GlobalAlloc(GMEM_MOVEABLE, len);
        if !handle.is_null() {
            let lock = GlobalLock(handle) as *mut u8;
            if !lock.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), lock, len);
                GlobalUnlock(handle);
                SetClipboardData(CF_DIB, handle as *mut _);
            }
        }
    }

    // CF_HDROP：一个文件路径
    let wide_path: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let header = std::mem::size_of::<u32>() * 5; // DROPFILES
    let size = header + wide_path.len() * 2 + 2;
    let handle = GlobalAlloc(GMEM_MOVEABLE, size);
    if !handle.is_null() {
        let lock = GlobalLock(handle) as *mut u8;
        if !lock.is_null() {
            let drop = lock as *mut DropFiles;
            (*drop).pFiles = header as u32;
            (*drop).fWide = 1;
            let dest = lock.add(header) as *mut u16;
            std::ptr::copy_nonoverlapping(wide_path.as_ptr(), dest, wide_path.len());
            *dest.add(wide_path.len()) = 0;
            GlobalUnlock(handle);
            SetClipboardData(CF_HDROP, handle as *mut _);
        }
    }

    // 自定义格式：浏览器 / Electron 里能直接当图片粘
    let format = RegisterClipboardFormatW(wide("image/webp").as_ptr());
    if format != 0 {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if !handle.is_null() {
            let lock = GlobalLock(handle) as *mut u8;
            if !lock.is_null() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), lock, bytes.len());
                GlobalUnlock(handle);
                SetClipboardData(format, handle as *mut _);
            }
        }
    }

    CloseClipboard();
    show_toast("已复制到剪贴板");
}

/// CF_HDROP 的 DROPFILES 头（字段名沿用 Win32）
#[repr(C)]
#[allow(non_snake_case)]
struct DropFiles {
    pFiles: u32,
    pt: [i32; 2],
    fNC: i32,
    fWide: i32,
}

/// RGBA 帧 → CF_DIB（32 位、行序自下而上）
fn rgba_to_dib(frame: &webp::AnimFrame) -> Option<(Vec<u8>, usize)> {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    let src = frame.get_image();
    if w == 0 || h == 0 || src.len() < w * h * 4 {
        return None;
    }
    let size = 40 + w * h * 4;
    let mut out = vec![0u8; size];
    out[0..4].copy_from_slice(&40u32.to_le_bytes());
    out[4..8].copy_from_slice(&(w as i32).to_le_bytes());
    out[8..12].copy_from_slice(&(h as i32).to_le_bytes());
    out[12..14].copy_from_slice(&1u16.to_le_bytes());
    out[14..16].copy_from_slice(&32u16.to_le_bytes());
    for row in 0..h {
        let src_row = h - 1 - row;
        let src_offset = src_row * w * 4;
        let dst_offset = 40 + row * w * 4;
        for x in 0..w {
            let s = src_offset + x * 4;
            let d = dst_offset + x * 4;
            out[d] = src[s + 2];
            out[d + 1] = src[s + 1];
            out[d + 2] = src[s];
            out[d + 3] = src[s + 3];
        }
    }
    Some((out, size))
}

/// 复制成功的小提示：屏幕中上方一个深色胶囊，1.3 秒后自己消失
pub unsafe fn show_toast(text: &str) {
    let class = wide(TOAST_CLASS);
    let instance = GetModuleHandleW(std::ptr::null());
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(toast_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);

    let text_w = text.chars().count() as i32 * 14 + 40;
    let w = text_w.max(150);
    let h = 40;
    let mut cursor: POINT = std::mem::zeroed();
    GetCursorPos(&mut cursor);
    let x = cursor.x - w / 2;
    let y = cursor.y + 48;
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        class.as_ptr(),
        wide("提示").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        w,
        h,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return;
    }
    SetWindowTextW(hwnd, wide(text).as_ptr());
    InvalidateRect(hwnd, std::ptr::null(), 0);
    windows_sys::Win32::Graphics::Gdi::UpdateWindow(hwnd);
    let raw = hwnd as usize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1300));
        PostMessageW(raw as HWND, WM_CLOSE, 0, 0);
    });
}

unsafe extern "system" fn toast_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut client);
            let bg = CreateSolidBrush(0x0022_2629);
            let pen = CreatePen(0, 1, 0x0022_2629);
            let ob = SelectObject(dc, bg as HGDIOBJ);
            let op = SelectObject(dc, pen as HGDIOBJ);
            RoundRect(dc, 0, 0, client.right, client.bottom, client.bottom, client.bottom);
            SelectObject(dc, op);
            SelectObject(dc, ob);
            DeleteObject(pen as *mut _);
            DeleteObject(bg as *mut _);

            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00F5_F7F9);
            let font = CreateFontW(
                -14,
                0,
                0,
                0,
                500,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                wide("Microsoft YaHei UI").as_ptr(),
            );
            let old_font = SelectObject(dc, font as HGDIOBJ);
            let mut rect = client;
            DrawTextW(
                dc,
                wide("已复制到剪贴板").as_ptr(),
                -1,
                &mut rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
            SelectObject(dc, old_font);
            DeleteObject(font as *mut _);
            EndPaint(hwnd, &mut ps);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- 采集 + 编码

/// 一个屏幕区域的采集器（DIB 只建一次）
struct ScreenGrabber {
    screen: HDC,
    memory: HDC,
    dib: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl ScreenGrabber {
    fn new(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let memory = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = 0;
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
            if dib.is_null() || bits.is_null() {
                if !dib.is_null() {
                    DeleteObject(dib as *mut _);
                }
                DeleteDC(memory);
                ReleaseDC(std::ptr::null_mut(), screen);
                return None;
            }
            SelectObject(memory, dib as HGDIOBJ);
            let _ = (x, y);
            Some(Self {
                screen,
                memory,
                dib,
                bits: bits as *mut u8,
                width,
                height,
            })
        }
    }

    /// 抓一帧 BGRA（顺带把鼠标画进去）
    fn grab(&mut self, x: i32, y: i32, out: &mut Vec<u8>) -> bool {
        unsafe {
            let ok = BitBlt(self.memory, 0, 0, self.width, self.height, self.screen, x, y, SRCCOPY);
            if ok == 0 {
                return false;
            }

            // 鼠标指针也算「区域内的动态」
            let mut info: CURSORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut info) != 0 && info.flags & CURSOR_SHOWING != 0 {
                let mut icon: ICONINFO = std::mem::zeroed();
                let (mut hot_x, mut hot_y) = (0, 0);
                if GetIconInfo(info.hCursor, &mut icon) != 0 {
                    hot_x = icon.xHotspot as i32;
                    hot_y = icon.yHotspot as i32;
                    if !icon.hbmMask.is_null() {
                        DeleteObject(icon.hbmMask as *mut _);
                    }
                    if !icon.hbmColor.is_null() {
                        DeleteObject(icon.hbmColor as *mut _);
                    }
                }
                let dx = info.ptScreenPos.x - x - hot_x;
                let dy = info.ptScreenPos.y - y - hot_y;
                if dx > -32 && dy > -32 && dx < self.width && dy < self.height {
                    DrawIconEx(self.memory, dx, dy, info.hCursor, 0, 0, 0, std::ptr::null_mut(), DI_NORMAL);
                }
            }

            let len = (self.width * self.height * 4) as usize;
            out.clear();
            out.extend_from_slice(std::slice::from_raw_parts(self.bits, len));
            true
        }
    }
}

impl Drop for ScreenGrabber {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.dib as *mut _);
            DeleteDC(self.memory);
            ReleaseDC(std::ptr::null_mut(), self.screen);
        }
    }
}

unsafe fn run(panel: usize, x: i32, y: i32, width: i32, height: i32, fps: u32, border: usize, button: usize) {
    let (border, button) = (border as HWND, button as HWND);
    // 等遮罩彻底消失、边框画出来（边框在选区外面，不会被录到）
    std::thread::sleep(std::time::Duration::from_millis(160));

    let Some(mut grabber) = ScreenGrabber::new(x, y, width, height) else {
        finish(border, button);
        return;
    };

    // 先拍几帧用来算全局调色板（开始录制的头 200ms 一般就是画面主体）
    let mut warmup: Vec<Vec<u8>> = Vec::new();
    let mut frame = Vec::new();
    for _ in 0..5 {
        if grabber.grab(x, y, &mut frame) {
            warmup.push(frame.clone());
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    if warmup.is_empty() {
        finish(border, button);
        return;
    }

    // 帧先留着，停止后一次性交给 libwebp 的动画编码器（帧间优化它自己做）
    // (RGBA 帧, 真实经过的毫秒)：丢掉重复帧后时间戳仍按真实时间走，否则时长会被压缩
    let mut shots: Vec<(Vec<u8>, i32)> = Vec::new();
    let started_at = std::time::Instant::now();
    let stamp = |now: std::time::Instant| now.duration_since(started_at).as_millis() as i32;
    let mut last_raw: Vec<u8> = Vec::new();
    // 内存兜底：整段最多占 600MB
    // 内存兜底：整段最多占 900MB（120fps 下比 30fps 时限短得多）
    let max_frames = ((900 * 1024 * 1024) / (width.max(1) * height.max(1) * 4) as usize).max(30);
    // WebP 动画的时间戳是毫秒：120fps 取 8ms（≈125fps），60 取 16，30 取 33

    for shot in &warmup {
        shots.push((to_rgba(shot), stamp(std::time::Instant::now())));
        last_raw.clear();
        last_raw.extend_from_slice(shot);
    }

    let interval = std::time::Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(20);
    let mut next = std::time::Instant::now();

    while STOP_MODE.load(Ordering::Relaxed) == 0
        && started.elapsed() < limit
        && shots.len() < max_frames
    {
        next += interval;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }

        if !grabber.grab(x, y, &mut frame) {
            continue;
        }
        if frame == last_raw {
            // 画面没变：这一帧直接不要，时间轴照样往前走
            continue;
        }
        shots.push((to_rgba(&frame), stamp(std::time::Instant::now())));
        last_raw.clear();
        last_raw.extend_from_slice(&frame);
    }

    // 收尾：画面最后静止的那一段也要算进时长，补一帧（内容同上、时间戳取结束时）
    if let Some((frame, _)) = shots.last().cloned() {
        shots.push((frame, stamp(std::time::Instant::now())));
    }

    let mode = STOP_MODE.load(Ordering::SeqCst);
    let bytes = encode_webp(width, height, &shots, (1000 / fps.max(1) as i32).max(1));
    crate::debug_log(&format!("rec done: frames={} bytes={} size={width}x{height}", shots.len(), bytes.len()));
    finish(border, button);

    if bytes.is_empty() {
        crate::debug_log(&format!("rec empty: frames={} mode={mode}", shots.len()));
    }
    if !bytes.is_empty() {
        unsafe {
            let pointer = Box::into_raw(Box::new(bytes));
            let target = panel as HWND;
            if !target.is_null() {
                // lparam 告诉主窗口：1 = 复制到剪贴板，2 = 另存为
                PostMessageW(target, crate::WM_APP_GIF_DONE, pointer as WPARAM, mode as LPARAM);
            } else {
                drop(Box::from_raw(pointer));
            }
        }
    }
}

/// BGRA → RGBA（libwebp 的动画编码器只吃 RGB / RGBA）
fn to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for pixel in out.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    out
}

/// 用 libwebp 的动画编码器出 WebP（帧间优化、重复帧、调色板它自己处理）
fn encode_webp(width: i32, height: i32, frames: &[(Vec<u8>, i32)], delay_ms: i32) -> Vec<u8> {
    if frames.is_empty() {
        return Vec::new();
    }
    let mut config = webp::WebPConfig::new().unwrap();
    config.quality = 60.0;
    config.method = 4;
    config.lossless = 0;
    config.thread_level = 1;

    let mut encoder = webp::AnimEncoder::new(width.max(1) as u32, height.max(1) as u32, &config);
    encoder.set_loop_count(0);
    // 时间戳必须是递增的真实时间：中间丢掉重复帧后不能拿序号乘帧间隔，否则时长会变短
    let mut previous_ts = -1;
    for (frame, ts) in frames {
        let mut timestamp = (*ts).max(previous_ts + 1).max(0);
        if false && previous_ts >= 0 {
            // 长时间不变（比如录屏时画面静止）：留一点真实等待，但不要超长
            timestamp = previous_ts + delay_ms * 8;
        }
        previous_ts = timestamp;
        encoder.add_frame(webp::AnimFrame::from_rgba(
            frame,
            width.max(1) as u32,
            height.max(1) as u32,
            timestamp,
        ));
    }
    match encoder.try_encode() {
        Ok(memory) => memory.to_vec(),
        Err(error) => {
            crate::debug_log(&format!("webp encode failed: {error:?} frames={}", frames.len()));
            Vec::new()
        }
    }
}

fn finish(border: HWND, button: HWND) {
    unsafe {
        // 窗口得由创建它的（UI）线程销毁，这里只投消息
        if !border.is_null() {
            PostMessageW(border, WM_CLOSE, 0, 0);
        }
        if !button.is_null() {
            PostMessageW(button, WM_CLOSE, 0, 0);
        }
    }
    RECORDING.store(false, Ordering::SeqCst);
    BAR.store(0, Ordering::SeqCst);
    STOP_MODE.store(0, Ordering::SeqCst);
}

// ---------------------------------------------------------------- 保存

/// 录完弹「另存为」，存完给个结果提示
pub unsafe fn save_and_notify(owner: HWND, bytes: Vec<u8>) {
    let mut name = vec![0u16; 512];
    let mut filter: Vec<u16> = "WebP 动图\0*.webp\0所有文件\0*.*\0\0".encode_utf16().collect();
    let mut title = wide("保存 WebP");
    let mut ext = wide("webp");

    let mut local: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
    windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut local);
    let default_name = format!(
        "Rec_{:04}{:02}{:02}_{:02}{:02}{:02}.webp",
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
    dialog.lpstrTitle = title.as_mut_ptr();
    dialog.lpstrDefExt = ext.as_mut_ptr();
    dialog.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST;

    if GetSaveFileNameW(&mut dialog) == 0 {
        // 取消就算了，不提示错误
        let _ = CommDlgExtendedError();
        return;
    }

    let length = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    let path = String::from_utf16_lossy(&name[..length]);
    if std::fs::write(&path, &bytes).is_err() {
        MessageBoxW(
            owner,
            wide("WebP 保存失败：磁盘空间不足、没有写入权限，或路径无效。").as_ptr(),
            wide("保存 WebP").as_ptr(),
            MB_OK | MB_ICONWARNING,
        );
        return;
    }

    let size = bytes.len() as f64 / 1024.0 / 1024.0;
    let text = format!("已保存（{size:.2} MB）\n{path}");
    MessageBoxW(
        owner,
        wide(&text).as_ptr(),
        wide("WebP 已保存").as_ptr(),
        MB_OK | MB_ICONINFORMATION,
    );
}
