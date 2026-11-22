//! 截一块屏幕区域录成 MP4（H.264）。
//!
//! 采集走 DXGI 桌面复制，没变化就跳过（静止画面不重复编码）；编码交给系统自带的
//! Media Foundation——有核显/独显时是硬件编码器，1080p60 也不用抓帧线程等它。
//! 文件边录边写，内存里只有当前这一帧，不攒整段帧。

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
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
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

/// 边框画在选区**外面**这么多像素（不然会被录进画面）
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
        wide("录制区域").as_ptr(),
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

/// 录完复制到剪贴板：CF_DIB（首帧静态图）+ 文件（mp4，能贴进聊天窗口）
pub unsafe fn copy_to_clipboard(owner: HWND, output: RecOutput) {
    // 首帧拼成 CF_DIB：不支持文件拖放的编辑器也能直接粘一张图
    let dib = dib_from_bgra(&output.first, output.width, output.height);

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

    // CF_HDROP：一个 mp4 文件路径，微信/QQ/浏览器都认
    let wide_path: Vec<u16> = output
        .path
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

    CloseClipboard();
    show_toast("已复制到剪贴板（MP4）");
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

/// 一张 BGRA 图的平均亮度（每 64 个像素采一个）
fn mean_luma(pixels: &[u8]) -> f32 {
    let mut sum = 0u64;
    let mut count = 0u64;
    let mut index = 0usize;
    while index + 2 < pixels.len() {
        let pixel = &pixels[index..index + 3];
        sum += (pixel[0] as u64 + pixel[1] as u64 + pixel[2] as u64) / 3;
        count += 1;
        index += 4 * 64;
    }
    if count == 0 {
        0.0
    } else {
        sum as f32 / count as f32
    }
}

impl ScreenGrabber {
    /// `width/height` 是屏幕上的选区大小，`out_w/out_h` 是实际写进视频的尺寸（缩放过）
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
    /// 开录前校验一次：DXGI 到底有没有画面。
    /// 有些机器（HDR 桌面 / 驱动）DXGI 会一直返回「没变化」+ 全黑缓冲，
    /// 表现出来就是录出来全黑视频，而日志里连一帧变化都没有。
    unsafe fn validate_dxgi(&mut self, x: i32, y: i32) -> bool {
        if self.dxgi.is_none() {
            return true;
        }
        // GDI 当参照物
        let Some(reference) = crate::shot::capture_rect(x, y, self.crop_w, self.crop_h) else {
            return true;
        };
        let gdi_mean = mean_luma(&reference.pixels);
        // DXGI 抓一帧看看
        let mut dxgi_mean = 0.0f32;
        if let Some(duplicator) = self.dxgi.as_mut() {
            let stride = (self.crop_w * 4) as usize;
            if let crate::dxgi::Grabbed::New = duplicator.grab(self.crop_bits, stride) {
                let len = (self.crop_w * self.crop_h * 4).max(1) as usize;
                let slice = std::slice::from_raw_parts(self.crop_bits, len);
                dxgi_mean = mean_luma(slice);
            }
        }
        crate::debug_log(&format!(
            "rec: DXGI 校验 GDI={gdi_mean:.1} DXGI={dxgi_mean:.1}"
        ));
        // GDI 有画面、DXGI 一片黑 -> 直接弃用 DXGI
        if gdi_mean > 20.0 && dxgi_mean < 12.0 {
            crate::debug_log("rec: DXGI 取不到画面，改用 GDI 采集");
            self.dxgi = None;
            return false;
        }
        true
    }

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
    // H.264 / NV12 要求宽高是偶数：这里就把输出尺寸压成偶数，
    // 抓帧缓冲也按这个尺寸抓——否则缓冲区行宽和编码器认定的行宽差 1 像素，
    // 画面会斜着错位（斜纹/歪掉）
    let out_w = (((width as f32 * out_scale).round() as i32).max(16)) & !15;
    let out_h = (((height as f32 * out_scale).round() as i32).max(16)) & !15;

    let Some(mut grabber) = ScreenGrabber::new(x, y, width, height, out_w, out_h) else {
        finish(border, button);
        return;
    };
    // 开录前先确认 DXGI 真能出画（HDR/驱动问题会让它一直给黑帧）
    grabber.validate_dxgi(x, y);

    // 只拍一帧当视频的底图（后面每帧都跟它比，没变化就不重复送）
    let mut first = Vec::new();
    if !grabber.grab_if_changed(x, y, &[], &mut first) {
        finish(border, button);
        return;
    }

    // 编码放到单独线程：MF 的 H.264 编码器多半走硬件，但接口调用还是别塞进抓帧线程。
    // 队列只放 4 帧，编不过来就丢帧，时间戳照样按真实时间走，所以不会拖长时长。
    let started_at = std::time::Instant::now();
    let stamp = |now: std::time::Instant| now.duration_since(started_at).as_millis() as i32;

    // fps = 0 表示「自动」：H.264 编码很快，直接按 30fps 走
    let fps = if fps == 0 { 30 } else { fps.clamp(1, 60) };
    let pixels = (out_w as i64) * (out_h as i64);
    let output_path = new_output_path();

    // 每帧耗时的统计窗口：编码线程写、抓帧线程读
    ENCODE_EMA_US.store(0, Ordering::Relaxed);
    ENCODE_WORST_US.store(0, Ordering::Relaxed);
    ENCODED_FRAMES.store(0, Ordering::Relaxed);
    DROPPED_FRAMES.store(0, Ordering::Relaxed);
    CAPTURES.store(0, Ordering::Relaxed);

    let (sender, receiver) = std::sync::mpsc::sync_channel::<(Vec<u8>, i32)>(4);
    // 编码线程用完的 Vec 还回来直接复用：几 MB 一块，省一次分配就省一次内存抖动
    let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let encode_path = output_path.clone();
    let encoder = std::thread::spawn(move || {
        let Some(mut writer) = Mp4Writer::new(&encode_path, out_w, out_h, fps, quality) else {
            crate::debug_log("mp4: 编码器创建失败");
            return 0u64;
        };
        let mut wrote = 0u32;
        for (frame, timestamp) in receiver {
            let t = std::time::Instant::now();
            if writer.write(&frame, timestamp) {
                wrote += 1;
                note_encode_time(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
                ENCODED_FRAMES.fetch_add(1, Ordering::Relaxed);
            }
            let _ = recycle_tx.send(frame);
        }
        crate::debug_log(&format!("mp4: 共写入 {wrote} 帧"));
        writer.finish()
    });

    // 底图以 0ms 进编码器，视频才有第一帧
    let _ = sender.try_send((first.clone(), 0));
    let mut prev = first;

    crate::debug_log(&format!(
        "rec: 起手 {fps}fps（{pixels} 像素，quality {quality}，mp4 编码）"
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
    let bytes = encoder.join().unwrap_or(0);
    let encoded = ENCODED_FRAMES.load(Ordering::Relaxed);
    let dropped = DROPPED_FRAMES.load(Ordering::Relaxed);
    let captures = CAPTURES.load(Ordering::Relaxed);
    let avg_ms = ENCODE_EMA_US.load(Ordering::Relaxed) as f64 / 1000.0;
    let worst_ms = ENCODE_WORST_US.load(Ordering::Relaxed) as f64 / 1000.0;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    crate::debug_log(&format!(
        "rec done: bytes={bytes} size={out_w}x{out_h} (src {width}x{height}) 抓 {captures} 次 / 写入 {encoded} 帧 / 丢 {dropped} 帧 \
         {avg_ms:.1}ms/帧(最差 {worst_ms:.1}ms) {secs:.1}s → 抓帧 {:.1}fps、编码 {:.1}fps",
        captures as f64 / secs,
        encoded as f64 / secs
    ));
    finish(border, button);

    if bytes == 0 {
        crate::debug_log(&format!("rec empty: mode={mode}"));
        let _ = std::fs::remove_file(&output_path);
        show_toast("录制失败：编码器没有输出");
        return;
    }
    unsafe {
        let output = RecOutput {
            path: output_path,
            bytes,
            width: out_w,
            height: out_h,
            first: prev,
        };
        let pointer = Box::into_raw(Box::new(output));
        let target = panel as HWND;
        if !target.is_null() {
            // lparam 告诉主窗口：1 = 复制到剪贴板，2 = 另存为
            PostMessageW(target, crate::WM_APP_GIF_DONE, pointer as WPARAM, mode as LPARAM);
        } else {
            drop(Box::from_raw(pointer));
        }
    }
}

// ---------------------------------------------------------------- MP4 编码（Media Foundation）
//
// 录出来的东西直接是 H.264 / MP4：编码交给系统自带的 Media Foundation（机器有核显/独显时
// 走硬件编码器），不再靠 libwebp 在后台线程里追帧率。文件边录边写，内存只占当前这一帧。

use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::{
    eAVEncH264VProfile_High, IMFAttributes, IMFMediaType, IMFSample,
    IMFSinkWriter,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFCreateSourceReaderFromURL, MFShutdown, MFStartup,
    MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_DEFAULT_STRIDE, MF_MT_SUBTYPE, MF_SINK_WRITER_DISABLE_THROTTLING,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION, MFSTARTUP_NOSOCKET,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// 一次录制的产物：文件已经落在磁盘上，顺带把首帧带上（剪贴板里放静态图用）
pub struct RecOutput {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub width: i32,
    pub height: i32,
    /// 首帧 BGRA（行序自顶向下）
    pub first: Vec<u8>,
}

impl RecOutput {
    pub fn size_mb(&self) -> f64 {
        self.bytes as f64 / (1024.0 * 1024.0)
    }
}

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

/// 本次录制的输出文件：%TEMP%\NST\Rec_<时间戳>.mp4，顺手清掉上一次留下的
fn new_output_path() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("NST");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Rec_") && (name.ends_with(".mp4") || name.ends_with(".webp")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let mut stamp: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut stamp);
    }
    dir.join(format!(
        "Rec_{:04}{:02}{:02}_{:02}{:02}{:02}_{}.mp4",
        stamp.wYear,
        stamp.wMonth,
        stamp.wDay,
        stamp.wHour,
        stamp.wMinute,
        stamp.wSecond,
        stamp.wMilliseconds
    ))
}

/// H.264 码率：屏幕内容按「每像素 0.08~0.16 bit」估，quality 越高给得越多。
/// 1080p30 / quality 65 ≈ 5 Mbps，文字边缘能压得住；再低就开始糊。
fn h264_bitrate(pixels: i64, fps: u32, quality: u32) -> u32 {
    let bpp = 0.06 + (quality.min(100) as f64 / 100.0) * 0.10;
    let raw = pixels as f64 * fps.max(1) as f64 * bpp;
    raw.clamp(600_000.0, 24_000_000.0) as u32
}

/// 流式 MP4 写入器：一个视频流，输入 NV12，编码交给 MF
struct Mp4Writer {
    writer: IMFSinkWriter,
    stream: u32,
    width: i32,
    height: i32,
    path: std::path::PathBuf,
    duration: i64,
    last_ts: i64,
    started: bool,
}

impl Mp4Writer {
    fn new(
        path: &std::path::Path,
        width: i32,
        height: i32,
        fps: u32,
        quality: u32,
    ) -> Option<Self> {
        // H.264 要求宽高是偶数（NV12 的色度是 2×2 抽样）
        let width = (width.max(2)) & !1;
        let height = (height.max(2)) & !1;
        let fps = fps.clamp(1, 60);
        let bitrate = h264_bitrate(width as i64 * height as i64, fps, quality);

        unsafe {
            // 编码线程自己起 COM（MTA）+ MF
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_err() {
                crate::debug_log("mp4: MFStartup 失败");
                return None;
            }
            let mut attrs: Option<IMFAttributes> = None;
            if MFCreateAttributes(&mut attrs, 2).is_err() {
                crate::debug_log("mp4: MFCreateAttributes 失败");
                let _ = MFShutdown();
                return None;
            }
            let attrs = attrs?;
            let _ = attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1);

            let wide_path: Vec<u16> = path
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let writer: IMFSinkWriter = match MFCreateSinkWriterFromURL(
                PCWSTR(wide_path.as_ptr()),
                None,
                &attrs,
            ) {
                Ok(writer) => writer,
                Err(error) => {
                    crate::debug_log(&format!("mp4: 建 SinkWriter 失败 {error}"));
                    let _ = MFShutdown();
                    return None;
                }
            };

            let frame_size = ((width as u64) << 32) | height as u64;
            let frame_rate = ((fps as u64) << 32) | 1;
            let par = (1u64 << 32) | 1;

            // 输出：H.264
            let out_type: IMFMediaType = MFCreateMediaType().ok()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).ok()?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).ok()?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ok()?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size).ok()?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, frame_rate).ok()?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, par).ok()?;
            let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32);

            let stream = match writer.AddStream(&out_type) {
                Ok(stream) => stream,
                Err(error) => {
                    crate::debug_log(&format!("mp4: AddStream 失败 {error}"));
                    let _ = MFShutdown();
                    return None;
                }
            };

            // 输入：RGB32（就是 BGRA 的内存布局）。NV12 的行宽对齐各家编码器要求不一，
            // 自己转很容易一行一行斜掉；交给 SinkWriter 插的视频处理器去转，它知道编码器要什么。
            let in_type: IMFMediaType = MFCreateMediaType().ok()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ok()?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size).ok()?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, frame_rate).ok()?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, par).ok()?;
            // 正值 = 行序自下而上（我们送进去的就是这个方向）；
            // 行宽按 64 字节对齐，视频处理器按这个 stride 取行
            let stride = (width as usize) * 4;
            let _ = in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride as u32);
            if let Err(error) = writer.SetInputMediaType(stream, &in_type, None) {
                crate::debug_log(&format!("mp4: SetInputMediaType 失败 {error}"));
                let _ = MFShutdown();
                return None;
            }
            let agreed = in_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(0);
            crate::debug_log(&format!(
                "mp4: 输入 RGB32 stride {agreed}（{width}x{height}）"
            ));
            if let Err(error) = writer.BeginWriting() {
                crate::debug_log(&format!("mp4: BeginWriting 失败 {error}"));
                let _ = MFShutdown();
                return None;
            }

            crate::debug_log(&format!(
                "mp4: {width}x{height} {fps}fps {:.1}Mbps（quality {quality}）",
                bitrate as f64 / 1_000_000.0
            ));
            Some(Self {
                writer,
                stream,
                width,
                height,
                path: path.to_path_buf(),
                duration: 10_000_000 / fps.max(1) as i64,
                last_ts: -1,
                started: false,
            })
        }
    }

    fn write(&mut self, bgra: &[u8], ts_ms: i32) -> bool {
        let (w, h) = (self.width as usize, self.height as usize);
        if w < 2 || h < 2 {
            return false;
        }
        // 行宽必须严格对上：差一个像素，整幅画面就会斜着错位
        if bgra.len() != w * h * 4 {
            crate::debug_log(&format!(
                "mp4: 帧尺寸不对 {} != {w}x{h}x4（录出来会歪，直接丢掉）",
                bgra.len()
            ));
            return false;
        }
        let stride = w * 4;
        let size = (stride * h) as u32;
        // 时间戳必须严格递增（同一毫秒抓到两帧时往后挤 1ms，肉眼看不出来）
        let mut ts = ts_ms as i64 * 10_000;
        if ts <= self.last_ts {
            ts = self.last_ts + 10_000;
        }
        self.last_ts = ts;
        unsafe {
            let Ok(buffer) = MFCreateMemoryBuffer(size) else {
                return false;
            };
            let mut ptr = std::ptr::null_mut();
            if buffer.Lock(&mut ptr, None, None).is_err() || ptr.is_null() {
                return false;
            }
            let dst = std::slice::from_raw_parts_mut(ptr as *mut u8, size as usize);
            // 自顶向下直接拷：编码器按自顶向下解释样本，倒过来就上下颠倒
            let row = w * 4;
            for y in 0..h {
                let src = &bgra[y * row..][..row];
                let dst_row = y * stride;
                dst[dst_row..dst_row + row].copy_from_slice(src);
            }
            let _ = buffer.Unlock();
            if buffer.SetCurrentLength(size).is_err() {
                return false;
            }
            let Ok(sample) = MFCreateSample() else {
                return false;
            };
            if sample.AddBuffer(&buffer).is_err() {
                return false;
            }
            let _ = sample.SetSampleTime(ts);
            let _ = sample.SetSampleDuration(self.duration);
            if let Err(error) = self.writer.WriteSample(self.stream, &sample) {
                crate::debug_log(&format!("mp4: WriteSample 失败 {error}"));
                return false;
            }
        }
        self.started = true;
        true
    }

    /// 收尾：Finalize 之后文件才收得住，返回文件大小
    fn finish(self) -> u64 {
        let Self {
            writer,
            path,
            started,
            ..
        } = self;
        unsafe {
            if started {
                if let Err(error) = writer.Finalize() {
                    crate::debug_log(&format!("mp4: Finalize 失败 {error}"));
                }
            }
            // 先把接口放掉，再关 MF，文件句柄才真的释放
            drop(writer);
            let _ = MFShutdown();
        }
        std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
    }
}

/// 首帧 BGRA → CF_DIB（32 位、行序自下而上）
fn dib_from_bgra(first: &[u8], width: i32, height: i32) -> Option<(Vec<u8>, usize)> {
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    if first.len() < w * h * 4 {
        return None;
    }
    let header = std::mem::size_of::<BITMAPINFOHEADER>();
    let len = header + w * h * 4;
    let mut out = vec![0u8; len];
    unsafe {
        let info = out.as_mut_ptr() as *mut BITMAPINFOHEADER;
        (*info).biSize = header as u32;
        (*info).biWidth = w as i32;
        (*info).biHeight = h as i32;
        (*info).biPlanes = 1;
        (*info).biBitCount = 32;
        (*info).biCompression = 0; // BI_RGB
        (*info).biSizeImage = (w * h * 4) as u32;
    }
    for row in 0..h {
        let src = &first[(h - 1 - row) * w * 4..][..w * 4];
        let dst = header + row * w * 4;
        out[dst..dst + w * 4].copy_from_slice(src);
    }
    Some((out, len))
}

/// 自测用：把写好的 mp4 解一帧回来（RGB32、行序自下而上），验证文件真有画面
pub fn probe_first_frame(path: &std::path::Path) -> Option<(i32, i32, usize, Vec<u8>)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_err() {
            return None;
        }
        let wide_path: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // 要 RGB32 输出，得先允许 SourceReader 插视频处理器（默认只给 NV12）
        let mut reader_attrs: Option<IMFAttributes> = None;
        let mut reader = None;
        if MFCreateAttributes(&mut reader_attrs, 1).is_ok() {
            if let Some(attrs) = reader_attrs.as_ref() {
                let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);
            }
            reader = MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), reader_attrs.as_ref())
                .ok();
        }
        let Some(reader) = reader else {
            crate::debug_log("probe: 建 SourceReader 失败");
            let _ = MFShutdown();
            return None;
        };
        let out_type: IMFMediaType = MFCreateMediaType().ok()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        if let Err(error) = reader.SetCurrentMediaType(stream, None, &out_type) {
            crate::debug_log(&format!("probe: 设 RGB32 输出失败 {error}"));
            let _ = MFShutdown();
            return None;
        }
        let current = reader.GetCurrentMediaType(stream).ok()?;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let (w, h) = ((size >> 32) as i32, (size & 0xFFFF_FFFF) as i32);
        // 解码器给的每行字节数可能比 w*4 大（对齐），必须按它来拼
        let stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .map(|value| value as usize)
            .filter(|value| *value >= (w as usize) * 4)
            .unwrap_or((w as usize) * 4);

        let mut sample: Option<IMFSample> = None;
        let mut flags = 0u32;
        if let Err(error) = reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
        {
            crate::debug_log(&format!("probe: ReadSample 失败 {error}"));
            let _ = MFShutdown();
            return None;
        }
        let Some(sample) = sample else {
            crate::debug_log("probe: ReadSample 没给样本");
            let _ = MFShutdown();
            return None;
        };
        let buffer = match sample.ConvertToContiguousBuffer() {
            Ok(buffer) => buffer,
            Err(error) => {
                crate::debug_log(&format!("probe: 取连续缓冲失败 {error}"));
                let _ = MFShutdown();
                return None;
            }
        };
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec();
        let _ = buffer.Unlock();
        let _ = MFShutdown();
        Some((w, h, stride, bytes))
    }
}

/// 直接按 NV12 解一帧（不让 SourceReader 插转换器），用来判断「歪」是码流本身还是转换环节
pub fn probe_first_frame_nv12(path: &std::path::Path) -> Option<(i32, i32, Vec<u8>)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_err() {
            return None;
        }
        let wide_path: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let reader = MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), None).ok()?;
        let out_type: IMFMediaType = MFCreateMediaType().ok()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok()?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetCurrentMediaType(stream, None, &out_type).ok()?;
        let current = reader.GetCurrentMediaType(stream).ok()?;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let (w, h) = ((size >> 32) as i32, (size & 0xFFFF_FFFF) as i32);
        let stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .map(|value| value as usize)
            .filter(|value| *value >= w as usize)
            .unwrap_or(w as usize);

        let mut sample: Option<IMFSample> = None;
        let mut flags = 0u32;
        reader
            .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
            .ok()?;
        let sample = sample?;
        let buffer = sample.ConvertToContiguousBuffer().ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
        let raw = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        let (uw, uh) = ((w / 2) as usize, (h / 2) as usize);
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        let uv_base = stride * h as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let yv = raw.get(y * stride + x).copied().unwrap_or(16) as f32;
                let ux = (x / 2).min(uw.saturating_sub(1));
                let uy = (y / 2).min(uh.saturating_sub(1));
                let uv_index = uv_base + uy * stride + ux * 2;
                let uu = raw.get(uv_index).copied().unwrap_or(128) as f32;
                let vv = raw.get(uv_index + 1).copied().unwrap_or(128) as f32;
                let luma = (yv - 16.0) * 255.0 / 219.0;
                let cb = (uu - 128.0) * 255.0 / 224.0;
                let cr = (vv - 128.0) * 255.0 / 224.0;
                let r = (luma + 1.5748 * cr).clamp(0.0, 255.0) as u8;
                let g = (luma - 0.1873 * cb - 0.4681 * cr).clamp(0.0, 255.0) as u8;
                let b = (luma + 1.8556 * cb).clamp(0.0, 255.0) as u8;
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);
                rgba.push(255);
            }
        }
        let _ = buffer.Unlock();
        let _ = MFShutdown();
        Some((w, h, rgba))
    }
}

/// 调试用：把 NV12 直解的首帧存 PNG（上/下方向按自顶向下直接拼）
pub fn probe_nv12_to_png(spec: &str) -> bool {
    let path = std::path::PathBuf::from(spec.trim());
    let Some((pw, ph, rgba)) = probe_first_frame_nv12(&path) else {
        crate::debug_log("nv12 probe: 解不出首帧");
        return false;
    };
    let dir = std::env::temp_dir().join("NST");
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join(
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "probe.mp4".to_string())
            .replace(".mp4", ".nv12.png"),
    );
    match image::RgbaImage::from_raw(pw as u32, ph as u32, rgba) {
        Some(image) => {
            let ok = image.save(&out).is_ok();
            crate::debug_log(&format!("nv12 probe: {pw}x{ph} -> {} ({ok})", out.display()));
            ok
        }
        None => false,
    }
}

/// 调试用：把现成 mp4 的首帧解出来存成 PNG，肉眼查「录歪了没有 / 颜色正不正」
///   NST_REC_PROBE="D:\path\rec.mp4" netspeed.exe
pub fn probe_to_png(spec: &str) -> bool {
    let path = std::path::PathBuf::from(spec.trim());
    let Some((pw, ph, stride, pixels)) = probe_first_frame(&path) else {
        crate::debug_log("probe: 解不出首帧");
        return false;
    };
    let mut rgba = Vec::with_capacity((pw * ph * 4) as usize);
    for row in 0..ph {
        // MF 给的 RGB32 是自下而上的
        let src_row = (ph - 1 - row) as usize * stride;
        for col in 0..pw {
            let i = src_row + (col * 4) as usize;
            if i + 3 < pixels.len() {
                rgba.push(pixels[i + 2]);
                rgba.push(pixels[i + 1]);
                rgba.push(pixels[i]);
            } else {
                rgba.push(0);
                rgba.push(0);
                rgba.push(0);
            }
            rgba.push(255);
        }
    }
    let out = path.with_extension("probe.png");
    let expect = (pw as usize) * (ph as usize) * 4;
    crate::debug_log(&format!(
        "probe: {pw}x{ph} rgba={} expect={expect} src={}",
        rgba.len(),
        pixels.len()
    ));
    match image::RgbaImage::from_raw(pw as u32, ph as u32, rgba) {
        Some(image) => {
            // 原目录可能没写权限（比如桌面被策略挡住），退到临时目录再存一次
            let mut target = out.clone();
            if image.save(&target).is_err() {
                let dir = std::env::temp_dir().join("NST");
                let _ = std::fs::create_dir_all(&dir);
                target = dir.join(
                    path.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "probe.mp4".to_string())
                        .replace(".mp4", ".probe.png"),
                );
                if image.save(&target).is_err() {
                    crate::debug_log("probe: 保存失败（原目录和临时目录都不行）");
                    return false;
                }
            }
            crate::debug_log(&format!("probe: 存到 {}", target.display()));
            true
        }
        None => {
            crate::debug_log("probe: from_raw 失败");
            false
        }
    }
}

/// 调试用：抓一帧原始画面存 PNG（不经过编码器），用来判断「歪」是采集还是编码造成的
///   NST_REC_RAW="x,y,w,h,out.png" netspeed.exe
pub unsafe fn raw_frame_to_png(spec: &str) -> bool {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 5 {
        return false;
    }
    let x: i32 = parts[0].trim().parse().unwrap_or(0);
    let y: i32 = parts[1].trim().parse().unwrap_or(0);
    let w: i32 = parts[2].trim().parse().unwrap_or(0);
    let h: i32 = parts[3].trim().parse().unwrap_or(0);
    let out = std::path::PathBuf::from(parts[4].trim());
    if w < 16 || h < 16 {
        return false;
    }
    let Some(mut grabber) = ScreenGrabber::new(x, y, w, h, w & !1, h & !1) else {
        return false;
    };
    grabber.validate_dxgi(x, y);
    let mut buffer = Vec::new();
    if !grabber.grab_if_changed(x, y, &[], &mut buffer) {
        crate::debug_log("raw: 抓帧失败");
        return false;
    }
    let (pw, ph) = (w & !1, h & !1);
    crate::debug_log(&format!("raw: 缓冲 {} 字节，期望 {}", buffer.len(), pw * ph * 4));
    let mut rgba = Vec::with_capacity((pw * ph * 4) as usize);
    for i in 0..(pw * ph) as usize {
        let base = i * 4;
        if base + 3 < buffer.len() {
            rgba.push(buffer[base + 2]);
            rgba.push(buffer[base + 1]);
            rgba.push(buffer[base]);
            rgba.push(255);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 255]);
        }
    }
    match image::RgbaImage::from_raw(pw as u32, ph as u32, rgba) {
        Some(image) => {
            let ok = image.save(&out).is_ok();
            crate::debug_log(&format!("raw: -> {} ({ok})", out.display()));
            ok
        }
        None => false,
    }
}

/// 自测：抓一块屏幕录 N 秒，走跟正式录制一样的 MP4 编码路径，然后解一帧回来验证有画面。
///   NST_REC_TEST="x,y,w,h,秒数,out.mp4" netspeed.exe
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
    let path = std::path::PathBuf::from(parts[5].trim());
    if w < 16 || h < 16 || path.as_os_str().is_empty() {
        return false;
    }
    // 跟正式录制一样：抓帧尺寸和编码尺寸必须一致，而且 H.264 要偶数宽高
    let out_w = (w & !15).max(16);
    let out_h = (h & !15).max(16);
    // 走和正式录制同一条采集路径（ScreenGrabber，含缩放），这样奇偶尺寸/缩放的问题才测得到
    let Some(mut grabber) = ScreenGrabber::new(x, y, w, h, out_w, out_h) else {
        crate::debug_log("rec test: 采集器创建失败");
        return false;
    };
    grabber.validate_dxgi(x, y);

    let Some(mut writer) = Mp4Writer::new(&path, out_w, out_h, 30, 65) else {
        crate::debug_log("rec test: 编码器创建失败");
        return false;
    };
    let started = std::time::Instant::now();
    let mut frames = 0;
    let mut buffer = Vec::new();
    let mut prev: Vec<u8> = Vec::new();
    while started.elapsed().as_secs() < seconds {
        if grabber.grab_if_changed(x, y, &prev, &mut buffer) {
            let ts = started.elapsed().as_millis() as i32;
            if writer.write(&buffer, ts) {
                frames += 1;
            }
            prev.clear();
            prev.extend_from_slice(&buffer);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let bytes = writer.finish();
    crate::debug_log(&format!(
        "rec test: {frames} 帧, {bytes} 字节 -> {}",
        path.display()
    ));
    if bytes == 0 || frames == 0 {
        return false;
    }

    // 解一帧回来，确认不是黑屏、也不是上下颠倒
    let Some((pw, ph, stride, pixels)) = probe_first_frame(&path) else {
        crate::debug_log("rec test: 解不出首帧");
        return false;
    };
    let mut sum = 0u64;
    for pixel in pixels.chunks_exact(4).take((pw * ph) as usize) {
        sum += (pixel[0] as u64 + pixel[1] as u64 + pixel[2] as u64) / 3;
    }
    let count = (pw * ph).max(1) as u64;
    let mean = sum / count;
    crate::debug_log(&format!(
        "rec test: 首帧 {pw}x{ph} 平均亮度 {mean}（0/255 说明全黑或解错了）"
    ));

    // 顺手把解码出来的首帧存成 PNG：验颜色/上下方向时肉眼一看就知道
    let png_path = path.with_extension("probe.png");
    let mut rgba = Vec::with_capacity((pw * ph * 4) as usize);
    for row in 0..ph {
        let src_row = (ph - 1 - row) as usize * stride; // RGB32 是自下而上的
        for col in 0..pw {
            let i = src_row + (col * 4) as usize;
            if i + 3 < pixels.len() {
                rgba.push(pixels[i + 2]);
                rgba.push(pixels[i + 1]);
                rgba.push(pixels[i]);
                rgba.push(255);
            }
        }
    }
    if let Some(image) = image::RgbaImage::from_raw(pw as u32, ph as u32, rgba) {
        let _ = image.save(&png_path);
        crate::debug_log(&format!("rec test: 首帧存到 {}", png_path.display()));
    }
    mean > 8 && mean < 250
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
pub unsafe fn save_and_notify(owner: HWND, output: RecOutput) {
    let mut name = vec![0u16; 512];
    let mut filter: Vec<u16> = "MP4 视频\0*.mp4\0所有文件\0*.*\0\0".encode_utf16().collect();
    let mut title = wide("保存录制");
    let mut ext = wide("mp4");

    let mut local: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
    windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut local);
    let default_name = format!(
        "Rec_{:04}{:02}{:02}_{:02}{:02}{:02}.mp4",
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
    if std::fs::copy(&output.path, &path).is_err() {
        crate::dialog::info(
            owner,
            "保存录制",
            "MP4 保存失败：磁盘空间不足、没有写入权限，或路径无效。",
        );
        return;
    }

    let text = format!("已保存（{:.2} MB）\n{path}", output.size_mb());
    crate::dialog::info(owner, "MP4 已保存", &text);
}

