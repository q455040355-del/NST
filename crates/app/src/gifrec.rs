//! 截一块屏幕区域录成动图 GIF。
//!
//! 体积优先的三件事：
//! 1. **全局调色板**：整段共用一个 255 色的 NeuQuant 调色板（第 256 号留给透明），
//!    帧里不再塞各自的局部色表；
//! 2. **帧间差分**：和上一帧没变化的像素写透明色，dispose = Keep，静止画面几乎不占字节；
//! 3. **重复帧合并**：连续完全相同的帧不写新帧，只把延迟累加到前一帧上。
//!
//! 采集和编码在同一个后台线程里，边拍边写进内存，不会攒几千张整帧位图。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{
    HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CombineRgn, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen,
    CreateRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    InvalidateRect, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetStretchBltMode, SetTextColor,
    SetWindowRgn, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    COLORONCOLOR, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, RGN_DIFF, SRCCOPY, TRANSPARENT,
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
/// 录制输出的分辨率上限：最高 1080p（最长边 1920 且总面积不超过 1920×1080）
const MAX_OUT_SIDE: i32 = 1920;
const MAX_OUT_PIXELS: i64 = 1920 * 1080;

static RECORDING: AtomicBool = AtomicBool::new(false);
/// 操作条窗口（停止时先收起来）
static BAR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// 0 = 还在录，1 = 结束并复制到剪贴板，2 = 结束并另存为
static STOP_MODE: AtomicU8 = AtomicU8::new(0);
/// 鼠标悬停在操作条左半（0）还是右半（1）；9 = 没有悬停
static BAR_HOVER: AtomicI32 = AtomicI32::new(9);
/// 编码线程实测的单帧耗时（微秒，指数平均）：抓帧线程用它决定下一帧什么时候抓
static ENCODE_EMA_US: AtomicU32 = AtomicU32::new(0);
/// 本次录制最慢的一帧（微秒），只用来写日志
static ENCODE_WORST_US: AtomicU32 = AtomicU32::new(0);
/// 真正进了编码器的帧数 / 队列满了被丢掉的帧数 / 抓帧循环转了几圈
static ENCODED_FRAMES: AtomicU32 = AtomicU32::new(0);
static DROPPED_FRAMES: AtomicU32 = AtomicU32::new(0);
static CAPTURES: AtomicU32 = AtomicU32::new(0);

/// 抓帧间隔上限（1/2 秒），再慢就说明机器已经跟不上了，继续拉长没有意义
const MAX_INTERVAL_US: u64 = 500_000;

pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// 结束录制并复制到剪贴板（热键 / ✔ 都走这里）
pub fn stop() {
    stop_with(1);
}

/// 倒计时窗口
static HUD: AtomicUsize = AtomicUsize::new(0);
static HUD_SECONDS: AtomicI32 = AtomicI32::new(-1);

/// 更新倒计时（没变化就不重绘）
fn set_hud(seconds: u64) {
    let seconds = seconds as i32;
    if HUD_SECONDS.swap(seconds, Ordering::Relaxed) == seconds {
        return;
    }
    let hud = HUD.load(Ordering::SeqCst) as HWND;
    if !hud.is_null() {
        unsafe {
            InvalidateRect(hud, std::ptr::null(), 0);
            windows_sys::Win32::Graphics::Gdi::UpdateWindow(hud);
        }
    }
}

unsafe extern "system" fn hud_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut client);
            let bg = CreateSolidBrush(0x00C8_2323);
            let pen = CreateSolidBrush(0x00C8_2323);
            let ob = SelectObject(dc, bg as HGDIOBJ);
            let op = SelectObject(dc, pen as HGDIOBJ);
            let h = client.bottom;
            RoundRect(dc, 0, 0, client.right, client.bottom, h, h);
            SelectObject(dc, op);
            SelectObject(dc, ob);
            DeleteObject(pen as *mut _);
            DeleteObject(bg as *mut _);

            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FF_FFFF);
            let font = CreateFontW(-13, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, wide("Microsoft YaHei UI").as_ptr());
            let old = SelectObject(dc, font as HGDIOBJ);
            let seconds = HUD_SECONDS.load(Ordering::Relaxed);
            let text = format!("● {}s", seconds.max(0));
            let mut rect = client;
            DrawTextW(dc, wide(&text).as_ptr(), -1, &mut rect, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
            SelectObject(dc, old);
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
pub unsafe fn start(
    panel: HWND,
    rect: (i32, i32, i32, i32),
    fps: u32,
    quality: u32,
    seconds: u32,
) {
    let (x, y, w, h) = rect;
    if w < 16 || h < 16 || RECORDING.swap(true, Ordering::SeqCst) {
        return;
    }
    STOP_MODE.store(0, Ordering::SeqCst);
    crate::debug_log(&format!(
        "rec start: {x},{y} {w}x{h} fps={fps} quality={quality}"
    ));
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

    // 倒计时：放在选区外侧（上方中间），不挡画面也不会被录进去
    let hud_class = wide("NetSpeedTaskbarRs.RecHud");
    let hud_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(hud_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: hud_class.as_ptr(),
    };
    RegisterClassW(&hud_wc);
    let hud_w = 84;
    let hud_h = 26;
    let mut hud_x = x + (w - hud_w) / 2;
    let mut hud_y = y - hud_h - BORDER - 4;
    if hud_y < vy {
        hud_y = y + h + BORDER + 4;
    }
    if hud_x < vx {
        hud_x = vx;
    }
    if hud_x + hud_w > vx + vw {
        hud_x = vx + vw - hud_w;
    }
    let hud = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        hud_class.as_ptr(),
        wide("录制倒计时").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        hud_x,
        hud_y,
        hud_w,
        hud_h,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    HUD.store(hud as usize, Ordering::SeqCst);
    let seconds = seconds.clamp(5, 60);
    HUD_SECONDS.store(seconds as i32, Ordering::SeqCst);
    let panel = panel as usize;
    let border_raw = border as usize;
    let button_raw = button as usize;
    std::thread::spawn(move || run(panel, x, y, w, h, fps, quality, seconds as u64, border_raw, button_raw));
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
    let dib = first_frame_dib(&bytes);

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

/// 解出动画的第一帧，直接拼成 CF_DIB（32 位、行序自下而上）。
/// 解码器给的是 BGRA + 行序自顶向下，正好是 DIB 的字节序，只需要按行倒过来拷。
fn first_frame_dib(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    unsafe {
        let mut options: libwebp_sys::WebPAnimDecoderOptions = std::mem::zeroed();
        if libwebp_sys::WebPAnimDecoderOptionsInitInternal(
            &mut options,
            libwebp_sys::WEBP_DEMUX_ABI_VERSION as i32,
        ) == 0
        {
            return None;
        }
        options.color_mode = libwebp_sys::WEBP_CSP_MODE::MODE_BGRA;
        options.use_threads = 0;
        let data = libwebp_sys::WebPData {
            bytes: bytes.as_ptr(),
            size: bytes.len(),
        };
        let decoder = libwebp_sys::WebPAnimDecoderNewInternal(
            &data,
            &options,
            libwebp_sys::WEBP_DEMUX_ABI_VERSION as i32,
        );
        if decoder.is_null() {
            crate::debug_log("webp: 解码器创建失败（首帧 DIB 跳过）");
            return None;
        }

        let mut info: libwebp_sys::WebPAnimInfo = std::mem::zeroed();
        let mut frame: *mut u8 = std::ptr::null_mut();
        let mut timestamp = 0i32;
        let ok = libwebp_sys::WebPAnimDecoderGetInfo(decoder, &mut info) != 0
            && libwebp_sys::WebPAnimDecoderGetNext(decoder, &mut frame, &mut timestamp) != 0
            && !frame.is_null();
        let mut dib = None;
        if ok {
            let w = info.canvas_width as usize;
            let h = info.canvas_height as usize;
            if w > 0 && h > 0 {
                let stride = w * 4;
                let size = 40 + stride * h;
                let src = std::slice::from_raw_parts(frame, stride * h);
                let mut out = vec![0u8; size];
                out[0..4].copy_from_slice(&40u32.to_le_bytes());
                out[4..8].copy_from_slice(&(w as i32).to_le_bytes());
                out[8..12].copy_from_slice(&(h as i32).to_le_bytes());
                out[12..14].copy_from_slice(&1u16.to_le_bytes());
                out[14..16].copy_from_slice(&(32u16).to_le_bytes());
                for row in 0..h {
                    let src_row = (h - 1 - row) * stride;
                    let dst = 40 + row * stride;
                    out[dst..dst + stride].copy_from_slice(&src[src_row..src_row + stride]);
                }
                dib = Some((out, size));
            }
        }
        libwebp_sys::WebPAnimDecoderDelete(decoder);
        dib
    }
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
    /// 输出尺寸（可能比选区小）
    width: i32,
    height: i32,
    /// 屏幕上的选区尺寸
    src_w: i32,
    src_h: i32,
    /// DXGI 复制会话：建得起来就走显卡那条路（1080p 抓一帧约 4~7ms，GDI 要 12ms）
    dxgi: Option<crate::dxgi::DesktopDuplicator>,
    /// DXGI 路的中间 DIB：选区**原生**尺寸，GPU 回读的像素先落这里，再用 GDI 缩到输出尺寸
    crop_dc: HDC,
    crop_dib: HBITMAP,
    crop_bits: *mut u8,
    crop_w: i32,
    crop_h: i32,
    /// 鼠标热点按光标句柄缓存：GetIconInfo 每次都会新建两张位图，没必要每帧都问一遍
    cursor: HCURSOR,
    cursor_hot: (i32, i32),
    /// 上一帧的光标状态（句柄 + 位置）：桌面没变时用它判断能不能整帧跳过
    last_cursor: (usize, i32, i32),
    /// 连续多少帧 DXGI 给的是黑屏
    black_frames: u32,
}

/// DXGI 偶尔会返回一整片黑（HDR 桌面 / 驱动转换失败），抽样判断一下
unsafe fn bits_look_black(bits: *const u8, width: i32, height: i32) -> bool {
    if bits.is_null() {
        return false;
    }
    let pixels = (width * height).max(1) as usize;
    let step = (pixels / 4096).max(1);
    let mut dark = 0usize;
    let mut seen = 0usize;
    let mut index = 0usize;
    while index < pixels {
        let offset = index * 4;
        let mut peak = 0u8;
        for c in 0..3 {
            peak = peak.max(*bits.add(offset + c));
        }
        if peak < 12 {
            dark += 1;
        }
        seen += 1;
        index += step;
    }
    seen > 0 && dark * 100 >= seen * 98
}

impl ScreenGrabber {
    /// `width/height` 是屏幕上的选区大小，`out_w/out_h` 是实际写进 GIF 的尺寸（缩放过）
    fn new(x: i32, y: i32, width: i32, height: i32, out_w: i32, out_h: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let memory = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = out_w;
            info.bmiHeader.biHeight = -out_h;
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

            // 先试试 DXGI：区域完整落在一个显示器里才能建复制会话，失败就纯 GDI
            let mut dxgi = crate::dxgi::DesktopDuplicator::new(x, y, width, height);
            let mut crop_dc: HDC = std::ptr::null_mut();
            let mut crop_dib: HBITMAP = std::ptr::null_mut();
            let mut crop_bits: *mut u8 = std::ptr::null_mut();
            let (mut crop_w, mut crop_h) = (0, 0);
            if let Some(duplicator) = &dxgi {
                let (cw, ch) = (duplicator.width(), duplicator.height());
                let mut crop_info: BITMAPINFO = std::mem::zeroed();
                crop_info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                crop_info.bmiHeader.biWidth = cw;
                // 负高度 = 行序自顶向下，正好和 DXGI 回读的行序一致
                crop_info.bmiHeader.biHeight = -ch;
                crop_info.bmiHeader.biPlanes = 1;
                crop_info.bmiHeader.biBitCount = 32;
                crop_info.bmiHeader.biCompression = 0;
                let mut crop_cbits: *mut std::ffi::c_void = std::ptr::null_mut();
                let crop_dc_new = CreateCompatibleDC(screen);
                let crop_dib_new = CreateDIBSection(
                    screen,
                    &crop_info,
                    0,
                    &mut crop_cbits,
                    std::ptr::null_mut(),
                    0,
                );
                if !crop_dc_new.is_null()
                    && !crop_dib_new.is_null()
                    && !crop_cbits.is_null()
                {
                    SelectObject(crop_dc_new, crop_dib_new as HGDIOBJ);
                    crop_dc = crop_dc_new;
                    crop_dib = crop_dib_new;
                    crop_bits = crop_cbits as *mut u8;
                    crop_w = cw;
                    crop_h = ch;
                } else {
                    if !crop_dib_new.is_null() {
                        DeleteObject(crop_dib_new as *mut _);
                    }
                    if !crop_dc_new.is_null() {
                        DeleteDC(crop_dc_new);
                    }
                    dxgi = None;
                }
            }
            if dxgi.is_some() {
                let screens = dxgi.as_ref().map(|d| d.output_count()).unwrap_or(0);
                crate::debug_log(&format!(
                    "rec: DXGI 采集 {width}x{height}（{screens} 块屏）→ 输出 {out_w}x{out_h}"
                ));
            } else {
                crate::debug_log(&format!(
                    "rec: GDI 采集 {width}x{height} → 输出 {out_w}x{out_h}"
                ));
            }

            Some(Self {
                screen,
                memory,
                dib,
                bits: bits as *mut u8,
                width: out_w,
                height: out_h,
                src_w: width,
                src_h: height,
                dxgi,
                crop_dc,
                crop_dib,
                crop_bits,
                crop_w,
                crop_h,
                cursor: std::ptr::null_mut(),
                last_cursor: (0, 0, 0),
        black_frames: 0,
                cursor_hot: (0, 0),
            })
        }
    }

    /// 抓一帧到 `out`；如果和 `prev` 一模一样就返回 false，连拷贝都省了。
    /// 返回 false 也可能是抓屏失败——两种情况在录制循环里处理方式相同（这一帧不要）。
    fn grab_if_changed(&mut self, x: i32, y: i32, prev: &[u8], out: &mut Vec<u8>) -> bool {
        unsafe {
            // 光标状态先取：静态画面时要用它决定能不能整帧跳过
            let mut info: CURSORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            let cursor_shown = GetCursorInfo(&mut info) != 0 && info.flags & CURSOR_SHOWING != 0;
            let cursor_key = if cursor_shown {
                (
                    info.hCursor as usize,
                    info.ptScreenPos.x,
                    info.ptScreenPos.y,
                )
            } else {
                (0, 0, 0)
            };

            // 源：DXGI（已经回读到 crop DIB）或屏幕 DC
            let mut lost = false;
            let mut fresh = true;
            let (origin, src_dc, sx, sy, sw, sh) = if let Some(duplicator) = self.dxgi.as_mut() {
                let stride = (self.crop_w * 4) as usize;
                match duplicator.grab(self.crop_bits, stride) {
                    // 会话断了（切分辨率 / 全屏 / 远程桌面）：这帧不要，之后回落 GDI
                    crate::dxgi::Grabbed::Lost => {
                        lost = true;
                        ((0, 0), self.crop_dc, 0, 0, self.crop_w, self.crop_h)
                    }
                    crate::dxgi::Grabbed::New => {
                        // 抽检：HDR 桌面/驱动不兼容时 DXGI 会给一整片黑，
                        // 这时候录出来就是全黑视频，直接回落 GDI
                        if bits_look_black(self.crop_bits, self.crop_w, self.crop_h) {
                            self.black_frames += 1;
                            if self.black_frames >= 2 {
                                crate::debug_log(
                                    "rec: DXGI 连续返回黑屏，回落 GDI 采集",
                                );
                                self.dxgi = None;
                                return false;
                            }
                        } else {
                            self.black_frames = 0;
                        }
                        (
                            duplicator.origin(),
                            self.crop_dc,
                            0,
                            0,
                            self.crop_w,
                            self.crop_h,
                        )
                    }
                    // 桌面这一帧没变：crop DIB 里还是上一帧，光标也没动的话整帧跳过
                    _ => {
                        fresh = false;
                        (
                            duplicator.origin(),
                            self.crop_dc,
                            0,
                            0,
                            self.crop_w,
                            self.crop_h,
                        )
                    }
                }
            } else {
                ((x, y), self.screen, x, y, self.src_w, self.src_h)
            };
            if lost {
                crate::debug_log("rec: DXGI 会话失效，回落 GDI 采集");
                self.dxgi = None;
                return false;
            }
            // 桌面没变 + 光标没动：连 BitBlt/StretchBlt 和 8MB memcmp 都不用做
            if !fresh && !prev.is_empty() && cursor_key == self.last_cursor {
                return false;
            }
            self.last_cursor = cursor_key;

            // 缩放录制：直接让 GDI 边抓边缩，比抓完再缩快一倍
            let ok = if self.width == sw && self.height == sh {
                BitBlt(self.memory, 0, 0, self.width, self.height, src_dc, sx, sy, SRCCOPY)
            } else {
                SetStretchBltMode(self.memory, COLORONCOLOR);
                StretchBlt(
                    self.memory,
                    0,
                    0,
                    self.width,
                    self.height,
                    src_dc,
                    sx,
                    sy,
                    sw,
                    sh,
                    SRCCOPY,
                )
            };
            if ok == 0 {
                return false;
            }

            // 鼠标指针也算「区域内的动态」
            if cursor_shown {
                // 只有光标样子变了才去问热点（图标 / I 形 / 手形之间切换）
                if info.hCursor != self.cursor {
                    let mut icon: ICONINFO = std::mem::zeroed();
                    if GetIconInfo(info.hCursor, &mut icon) != 0 {
                        self.cursor = info.hCursor;
                        self.cursor_hot = (icon.xHotspot as i32, icon.yHotspot as i32);
                        if !icon.hbmMask.is_null() {
                            DeleteObject(icon.hbmMask as *mut _);
                        }
                        if !icon.hbmColor.is_null() {
                            DeleteObject(icon.hbmColor as *mut _);
                        }
                    }
                }
                let (hot_x, hot_y) = self.cursor_hot;
                // 光标位置也要按缩放换算
                let ratio = self.width as f32 / sw.max(1) as f32;
                let dx = (((info.ptScreenPos.x - origin.0) as f32 - hot_x as f32) * ratio) as i32;
                let dy = (((info.ptScreenPos.y - origin.1) as f32 - hot_y as f32) * ratio) as i32;
                if dx > -32 && dy > -32 && dx < self.width && dy < self.height {
                    DrawIconEx(
                        self.memory,
                        dx,
                        dy,
                        info.hCursor,
                        0,
                        0,
                        0,
                        std::ptr::null_mut(),
                        DI_NORMAL,
                    );
                }
            }
            // GDI 是批处理的：不刷一次，读出来的 DIB 里可能还没有鼠标
            windows_sys::Win32::Graphics::Gdi::GdiFlush();

            let len = (self.width * self.height * 4) as usize;
            // 直接拿 DIB 里的像素和上一帧比：没变化就一次拷贝都不做
            // （静止画面时这是最省的一条路：一帧只是 BitBlt + 8MB memcmp）
            let bits = std::slice::from_raw_parts(self.bits, len);
            if bits == prev {
                return false;
            }
            out.clear();
            out.extend_from_slice(bits);
            true
        }
    }
}

impl Drop for ScreenGrabber {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.dib as *mut _);
            DeleteDC(self.memory);
            if !self.crop_dib.is_null() {
                DeleteObject(self.crop_dib as *mut _);
            }
            if !self.crop_dc.is_null() {
                DeleteDC(self.crop_dc);
            }
            ReleaseDC(std::ptr::null_mut(), self.screen);
        }
    }
}

unsafe fn run(
    panel: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    fps: u32,
    quality: u32,
    limit_seconds: u64,
    border: usize,
    button: usize,
) {
    let (border, button) = (border as HWND, button as HWND);
    // 等遮罩彻底消失、边框画出来（边框在选区外面，不会被录到）
    std::thread::sleep(std::time::Duration::from_millis(160));

    // 输出上限：最高 1080p。除了卡最长边，还要卡总面积——2000×2000 这种方形选区
    // 光卡边长缩完还有 1920×1920（3.7MP），是 1080p 的 1.8 倍，编码和体积都翻倍。
    let side_scale = MAX_OUT_SIDE as f32 / width.max(height).max(1) as f32;
    let area_scale =
        (MAX_OUT_PIXELS as f32 / (width.max(1) as f32 * height.max(1) as f32)).sqrt();
    let out_scale = side_scale.min(area_scale).min(1.0);
    let out_w = ((width as f32 * out_scale).round() as i32).max(2);
    let out_h = ((height as f32 * out_scale).round() as i32).max(2);

    let Some(mut grabber) = ScreenGrabber::new(x, y, width, height, out_w, out_h) else {
        finish(border, button);
        return;
    };

    // 只拍一帧当动画的底图。以前拍 5 帧是为了算 GIF 的全局调色板——WebP 没有调色板，
    // 多拍的那 4 帧纯粹是白等 160ms、白编 4 次，还可能把深度 4 的队列提前塞满。
    let mut first = Vec::new();
    if !grabber.grab_if_changed(x, y, &[], &mut first) {
        finish(border, button);
        return;
    }

    // 编码放到单独线程：libwebp 编一帧要十几到几十毫秒，留在抓帧线程里会把节奏拖垮（看着卡）。
    // 队列只放 4 帧，编不过来就丢帧，时间戳照样按真实时间走，所以不会拖长时长。
    let started_at = std::time::Instant::now();
    let stamp = |now: std::time::Instant| now.duration_since(started_at).as_millis() as i32;

    // fps = 0 表示「自动」：交给下面的吞吐上限 + 闭环自己去收敛
    let fps = if fps == 0 { FPS_MAX } else { fps };
    let pixels = (out_w as i64) * (out_h as i64);
    let (method, mpix_per_sec) = pick_method(pixels, fps);
    let fps = fps.min(fps_cap(pixels, mpix_per_sec));

    // 每帧耗时的统计窗口：编码线程写、抓帧线程读
    ENCODE_EMA_US.store(0, Ordering::Relaxed);
    ENCODE_WORST_US.store(0, Ordering::Relaxed);
    ENCODED_FRAMES.store(0, Ordering::Relaxed);
    DROPPED_FRAMES.store(0, Ordering::Relaxed);
    CAPTURES.store(0, Ordering::Relaxed);

    let (sender, receiver) = std::sync::mpsc::sync_channel::<(Vec<u8>, i32)>(4);
    // 编码线程用完的 Vec 还回来直接复用：8MB 一块，省一次分配就省一次内存抖动
    let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let encoder = std::thread::spawn(move || {
        let Some(mut streamer) = WebpStreamer::new(out_w, out_h, quality as f32, method) else {
            crate::debug_log("webp: 编码器创建失败");
            return Vec::new();
        };
        for (frame, timestamp) in receiver {
            let t = std::time::Instant::now();
            streamer.add(&frame, timestamp);
            note_encode_time(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
            ENCODED_FRAMES.fetch_add(1, Ordering::Relaxed);
            let _ = recycle_tx.send(frame);
        }
        streamer.finish()
    });

    // 底图以 0ms 进编码器，动画才有第一帧
    let _ = sender.try_send((first.clone(), 0));
    let mut prev = first;

    crate::debug_log(&format!(
        "rec: 起手 {fps}fps（{pixels} 像素，method {method}，quality {quality}，预测 {:.1}ms/帧）",
        encode_ms_per_frame(pixels, mpix_per_sec)
    ));

    let user_interval = std::time::Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let mut interval = user_interval;
    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(limit_seconds);
    let mut next = std::time::Instant::now();
    let mut out: Vec<u8> = recycle_rx.try_recv().unwrap_or_default();

    while STOP_MODE.load(Ordering::Relaxed) == 0 && started.elapsed() < limit {
        set_hud(limit_seconds.saturating_sub(started.elapsed().as_secs()));
        CAPTURES.fetch_add(1, Ordering::Relaxed);

        // 闭环：按编码线程实测的每帧耗时决定抓帧节奏。
        // 只往慢里调（不会超过用户选的帧率），留 15% 余量，且四分之一拍逼近，避免帧率抖。
        if let Some(ema) = encode_ema_us() {
            let want = (ema.saturating_mul(115) / 100)
                .max(user_interval.as_micros() as u64)
                .min(MAX_INTERVAL_US);
            let now = interval.as_micros() as u64;
            interval = std::time::Duration::from_micros((now * 3 + want) / 4);
        }

        next += interval;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }

        if !grabber.grab_if_changed(x, y, &prev, &mut out) {
            // 画面没变（或抓屏失败）：这一帧不要，时间轴照样往前走
            continue;
        }
        // prev 得留着下一帧做比较（这块拷贝省不掉），送进队列的用另一块
        prev.clear();
        prev.extend_from_slice(&out);
        let buffer = std::mem::take(&mut out);
        if sender
            .try_send((buffer, stamp(std::time::Instant::now())))
            .is_err()
        {
            // 编码器还是跟不上（刚起手或画面突然剧烈变化）：立刻把间隔拉长 20%
            DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
            // 这一帧没进队列：把比较基准清掉，下一帧无论如何都再送一次，
            // 否则画面刚好停在这一帧就会一直"没变化"，这段变化永远补不上
            prev.clear();
            interval = std::time::Duration::from_micros(
                (interval.as_micros() as u64 * 12 / 10).min(MAX_INTERVAL_US),
            );
        }
        // 拿回编码线程归还的缓冲；还没还回来就先空着，下一帧现分配
        out = recycle_rx.try_recv().unwrap_or_default();
    }

    // 收尾：把最后那段静止时间也算进时长（再喂一帧同样的内容，时间戳取结束时）
    let _ = sender.try_send((prev.clone(), stamp(std::time::Instant::now())));
    drop(sender);

    let mode = STOP_MODE.load(Ordering::SeqCst);
    let bytes = encoder.join().unwrap_or_default();
    let encoded = ENCODED_FRAMES.load(Ordering::Relaxed);
    let dropped = DROPPED_FRAMES.load(Ordering::Relaxed);
    let captures = CAPTURES.load(Ordering::Relaxed);
    let avg_ms = ENCODE_EMA_US.load(Ordering::Relaxed) as f64 / 1000.0;
    let worst_ms = ENCODE_WORST_US.load(Ordering::Relaxed) as f64 / 1000.0;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    crate::debug_log(&format!(
        "rec done: bytes={} size={out_w}x{out_h} (src {width}x{height}) 抓 {captures} 次 / 入队 {encoded} 帧 / 丢 {dropped} 帧 \
         {avg_ms:.1}ms/帧(最差 {worst_ms:.1}ms) {secs:.1}s → 抓帧 {:.1}fps、编码 {:.1}fps",
        bytes.len(),
        captures as f64 / secs,
        encoded as f64 / secs
    ));
    finish(border, button);

    if bytes.is_empty() {
        crate::debug_log(&format!("rec empty: mode={mode}"));
        show_toast("录制失败：编码器没有输出");
        return;
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
// ---------------------------------------------------------------- 流式 WebP 编码
//
// 不再把整段帧攒在内存里：每抓一帧就 WebPAnimEncoderAdd 进去，
// 内存只占当前这一帧，所以 20 秒 / 60fps / 全屏都扛得住。

/// 编码线程实测的单帧耗时（微秒，指数平均）；0 = 还没测到第一帧
fn encode_ema_us() -> Option<u64> {
    match ENCODE_EMA_US.load(Ordering::Relaxed) {
        0 => None,
        micros => Some(micros as u64),
    }
}

/// 记一帧的编码耗时：EMA 给抓帧线程做闭环，最差值只写日志
fn note_encode_time(micros: u32) {
    ENCODE_WORST_US.fetch_max(micros, Ordering::Relaxed);
    let prev = ENCODE_EMA_US.load(Ordering::Relaxed);
    let next = if prev == 0 {
        micros
    } else {
        ((prev as u64 * 4 + micros as u64) / 5) as u32
    };
    ENCODE_EMA_US.store(next, Ordering::Relaxed);
}

/// 编码吞吐（百万像素/秒）。i5-12400 实测（libwebp 编译时带 WEBP_USE_THREAD、
/// thread_level = 1，即"宏块分段分析"多开一个线程）：
///   method 2（原来的设置）1080p 31.4ms、720p 15.2ms ≈ 66 MPixel/s
///   method 1            1080p 25.3ms、720p 12.5ms ≈ 82 MPixel/s
/// 这里各留约 15% 余量给更弱的机器（i5-12400 已经是 2022 年的中端桌面 U）。
const ENCODE_MPIX_M2: f64 = 56.0;
const ENCODE_MPIX_M1: f64 = 70.0;
/// 每帧另有约 1.5ms 与像素数无关的固定开销（ARGB 导入、帧头、熵编码收尾）
const ENCODE_FIXED_MS: f64 = 1.5;
/// 用户能选的最高帧率
const FPS_MAX: u32 = 120;

/// 这块区域编码一帧大约要多久（毫秒）
fn encode_ms_per_frame(pixels: i64, mpix_per_sec: f64) -> f64 {
    ENCODE_FIXED_MS + pixels as f64 / 1e6 * 1000.0 / mpix_per_sec
}

/// 该 method 能稳定吃下的帧率上限（超过就只能丢帧）
fn fps_cap(pixels: i64, mpix_per_sec: f64) -> u32 {
    (1000.0 / encode_ms_per_frame(pixels, mpix_per_sec))
        .floor()
        .clamp(1.0, FPS_MAX as f64) as u32
}

/// 选 method：method 2 体积更小（同样画面小 10~20%），用户选的帧率它吃得下就用它；
/// 吃不消才降到 method 1（单帧快约 20~30%，体积 +10~20%）。
/// 于是"小区域保持画质、大区域保帧率"是算出来的，不是拍脑袋的像素阈值：
///   600×400 / 120fps、960×540 / 60fps → method 2
///   1280×720 / 60fps、1600×900 / 60fps、1920×1080 / 30fps → method 1
fn pick_method(pixels: i64, fps: u32) -> (i32, f64) {
    let fits = (fps as f64) <= 1000.0 / encode_ms_per_frame(pixels, ENCODE_MPIX_M2);
    if fits {
        (2, ENCODE_MPIX_M2)
    } else {
        (1, ENCODE_MPIX_M1)
    }
}


struct WebpStreamer {
    encoder: *mut libwebp_sys::WebPAnimEncoder,
    config: libwebp_sys::WebPConfig,
    width: i32,
    height: i32,
}

impl WebpStreamer {
    fn new(width: i32, height: i32, quality: f32, method: i32) -> Option<Self> {
        unsafe {
            let mut options: libwebp_sys::WebPAnimEncoderOptions = std::mem::zeroed();
            if libwebp_sys::WebPAnimEncoderOptionsInitInternal(
                &mut options,
                libwebp_sys::WEBP_MUX_ABI_VERSION as i32,
            ) == 0
            {
                return None;
            }
            options.anim_params.loop_count = 0;
            // minimize_size = 1 是 libwebp 的"多试几种 dispose/blend 组合"，实测纯亏：
            // 同一段内容编码慢 18~42%（1080p 面板 25.2→22.4ms、整屏滚动 67.6→39.7ms），
            // 最差帧 46→24ms，而体积几乎一样（0~1.6%）。所以关掉。
            options.minimize_size = 0;

            let encoder = libwebp_sys::WebPAnimEncoderNewInternal(
                width.max(1),
                height.max(1),
                &options,
                // 这个参数要的是 mux 的 ABI 版本，不是 encoder 的（传错会直接返回空）
                libwebp_sys::WEBP_MUX_ABI_VERSION as i32,
            );
            if encoder.is_null() {
                crate::debug_log("webp: NewInternal 返回空");
                return None;
            }

            let mut config: libwebp_sys::WebPConfig = std::mem::zeroed();
            if libwebp_sys::WebPConfigInitInternal(
                &mut config,
                libwebp_sys::WebPPreset::WEBP_PRESET_DEFAULT,
                quality,
                libwebp_sys::WEBP_ENCODER_ABI_VERSION as i32,
            ) == 0
            {
                crate::debug_log("webp: ConfigInit 失败");
                libwebp_sys::WebPAnimEncoderDelete(encoder);
                return None;
            }
            // method 4 对 2MP 区域每帧要上百毫秒，抓帧节奏会被编码拖住（看起来卡）；
            // method 2 是原来的折中；method 1 更快（1080p 单帧 36ms→25ms）但体积 +10~20%，
            // 具体用哪个由 pick_method() 按区域大小和用户选的帧率决定
            config.method = method;
            config.lossless = 0;
            config.thread_level = 1;
            if libwebp_sys::WebPValidateConfig(&config) == 0 {
                crate::debug_log("webp: 配置校验失败");
            }

            Some(Self {
                encoder,
                config,
                width: width.max(1),
                height: height.max(1),
            })
        }
    }

    /// 喂一帧（BGRA，行序自顶向下），timestamp 是毫秒
    fn add(&mut self, bgra: &[u8], timestamp_ms: i32) -> bool {
        unsafe {
            let mut picture: libwebp_sys::WebPPicture = std::mem::zeroed();
            if libwebp_sys::WebPPictureInitInternal(
                &mut picture,
                libwebp_sys::WEBP_ENCODER_ABI_VERSION as i32,
            ) == 0
            {
                return false;
            }
            picture.use_argb = 1;
            picture.width = self.width;
            picture.height = self.height;
            // 用 BGRX：GDI 抓来的 DIB 里 alpha 字节是 0（GDI 不写 alpha），
            // 用 ImportBGRA 会把整帧当成全透明导出去 —— 录出来的 webp 一打开啥都没有。
            // BGRX 的第 4 个字节当填充丢弃，alpha 按不透明处理。
            if libwebp_sys::WebPPictureImportBGRX(
                &mut picture,
                bgra.as_ptr(),
                self.width * 4,
            ) == 0
            {
                libwebp_sys::WebPPictureFree(&mut picture);
                return false;
            }
            let ok = libwebp_sys::WebPAnimEncoderAdd(
                self.encoder,
                &mut picture,
                timestamp_ms,
                &self.config,
            ) != 0;
            if !ok {
                let message = libwebp_sys::WebPAnimEncoderGetError(self.encoder);
                let text = if message.is_null() {
                    "?".to_string()
                } else {
                    std::ffi::CStr::from_ptr(message).to_string_lossy().to_string()
                };
                crate::debug_log(&format!("webp: Add 失败 ts={timestamp_ms}: {text}"));
            }
            libwebp_sys::WebPPictureFree(&mut picture);
            ok
        }
    }

    /// 收尾：拿到完整 WebP 字节
    fn finish(self) -> Vec<u8> {
        unsafe {
            let mut data = libwebp_sys::WebPData {
                bytes: std::ptr::null(),
                size: 0,
            };
            let mut out = Vec::new();
            if libwebp_sys::WebPAnimEncoderAssemble(self.encoder, &mut data) != 0
                && !data.bytes.is_null()
                && data.size > 0
            {
                out.extend_from_slice(std::slice::from_raw_parts(data.bytes, data.size));
            } else {
                crate::debug_log(&format!(
                    "webp: Assemble 失败 size={} ptr_null={}",
                    data.size,
                    data.bytes.is_null()
                ));
            }
            if !data.bytes.is_null() {
                libwebp_sys::WebPFree(data.bytes as *mut std::ffi::c_void);
            }
            libwebp_sys::WebPAnimEncoderDelete(self.encoder);
            out
        }
    }
}


/// 自测：抓一块屏幕录 N 秒，直接走跟正式录制一样的 WebP 编码路径，把结果写文件。
/// 用来在不开 UI 的情况下验证「录出来的 webp 是不是有画面、alpha 是不是不透明」：
///   NST_REC_TEST="x,y,w,h,秒数,out.webp" netspeed.exe
pub unsafe fn selftest(spec: &str) -> bool {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 6 {
        return false;
    }
    let x: i32 = parts[0].trim().parse().unwrap_or(0);
    let y: i32 = parts[1].trim().parse().unwrap_or(0);
    let w: i32 = parts[2].trim().parse().unwrap_or(0);
    let h: i32 = parts[3].trim().parse().unwrap_or(0);
    let seconds: u64 = parts[4].trim().parse().unwrap_or(3);
    let path = parts[5].trim();
    if w < 16 || h < 16 || path.is_empty() {
        return false;
    }
    let Some(mut streamer) = WebpStreamer::new(w, h, 65.0, 2) else {
        return false;
    };
    let started = std::time::Instant::now();
    let mut frames = 0;
    while started.elapsed().as_secs() < seconds {
        if let Some(frame) = crate::shot::capture_rect(x, y, w, h) {
            let ts = started.elapsed().as_millis() as i32;
            if streamer.add(&frame.pixels, ts) {
                frames += 1;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let bytes = streamer.finish();
    crate::debug_log(&format!(
        "rec test: {} 帧, {} 字节",
        frames,
        bytes.len()
    ));
    std::fs::write(path, &bytes).is_ok()
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
        let hud = HUD.load(Ordering::SeqCst) as HWND;
        if !hud.is_null() {
            PostMessageW(hud, WM_CLOSE, 0, 0);
        }
    }
    HUD.store(0, Ordering::SeqCst);
    HUD_SECONDS.store(-1, Ordering::SeqCst);
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
        crate::dialog::info(
            owner,
            "保存 WebP",
            "WebP 保存失败：磁盘空间不足、没有写入权限，或路径无效。",
        );
        return;
    }

    let size = bytes.len() as f64 / 1024.0 / 1024.0;
    let text = format!("已保存（{size:.2} MB）\n{path}");
    crate::dialog::info(owner, "WebP 已保存", &text);
}
