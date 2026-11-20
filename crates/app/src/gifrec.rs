//! 截一块屏幕区域录成动图 GIF。
//!
//! 体积优先的三件事：
//! 1. **全局调色板**：整段共用一个 255 色的 NeuQuant 调色板（第 256 号留给透明），
//!    帧里不再塞各自的局部色表；
//! 2. **帧间差分**：和上一帧没变化的像素写透明色，dispose = Keep，静止画面几乎不占字节；
//! 3. **重复帧合并**：连续完全相同的帧不写新帧，只把延迟累加到前一帧上。
//!
//! 采集和编码在同一个后台线程里，边拍边写进内存，不会攒几千张整帧位图。

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CombineRgn, CreateCompatibleDC, CreateDIBSection, CreatePen, CreateRectRgn,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, ReleaseDC, SelectObject,
    SetWindowRgn, BITMAPINFO, BITMAPINFOHEADER, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, RGN_DIFF, SRCCOPY,
    LineTo, MoveToEx,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;

const BORDER_CLASS: &str = "NetSpeedTaskbarRs.GifBorder";
const STOP_CLASS: &str = "NetSpeedTaskbarRs.GifStop";

/// 边框画在选区**外面**这么多像素（不然会被录进 GIF）
const BORDER: i32 = 3;
const STOP_SIZE: i32 = 34;

static RECORDING: AtomicBool = AtomicBool::new(false);
static STOP_REQUEST: AtomicBool = AtomicBool::new(false);


pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// 让录制停下来（热键 / 点 ✔ 都走这里）
pub fn stop() {
    if is_recording() {
        STOP_REQUEST.store(true, Ordering::SeqCst);
    }
}

/// 开始录制一块屏幕区域（物理像素）。只负责开窗口，真正采集在后台线程。
pub unsafe fn start(panel: HWND, rect: (i32, i32, i32, i32), fps: u32) {
    let (x, y, w, h) = rect;
    if w < 16 || h < 16 || RECORDING.swap(true, Ordering::SeqCst) {
        return;
    }
    STOP_REQUEST.store(false, Ordering::SeqCst);


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

    // ✔ 结束按钮：优先放在选区右上角**外侧**（不会被录进去）；
    // 贴着屏幕上沿/右沿时依次尝试右侧、下方，实在没地方才压进选区里
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let fits = |bx: i32, by: i32| {
        bx >= vx && by >= vy && bx + STOP_SIZE <= vx + vw && by + STOP_SIZE <= vy + vh
    };
    let candidates = [
        (x + w - STOP_SIZE, y - STOP_SIZE - BORDER - 2),
        (x + w + BORDER + 4, y),
        (x + w - STOP_SIZE, y + h + BORDER + 4),
        (x + w - STOP_SIZE, y + 2),
    ];
    let (button_x, button_y) = candidates
        .into_iter()
        .find(|(bx, by)| fits(*bx, *by))
        .unwrap_or(candidates[3]);
    let button = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        stop_class.as_ptr(),
        wide("结束录制").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        button_x,
        button_y,
        STOP_SIZE,
        STOP_SIZE,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );

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
            paint_stop(dc);
            EndPaint(hwnd, &mut ps);
            0
        }
        WM_LBUTTONUP => {
            stop();
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

/// ✔ 按钮：深色圆角底 + 白色对勾
unsafe fn paint_stop(dc: HDC) {
    let size = STOP_SIZE;
    let brush = CreateSolidBrush(0x0026_1C18);
    let pen = CreatePen(0, 2, 0x00FF_FFFF);
    let old_brush = SelectObject(dc, brush as HGDIOBJ);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);
    windows_sys::Win32::Graphics::Gdi::Ellipse(dc, 0, 0, size, size);
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    DeleteObject(pen as *mut _);
    DeleteObject(brush as *mut _);

    let pen = CreatePen(0, 3, 0x00FF_FFFF);
    let old = SelectObject(dc, pen as HGDIOBJ);
    MoveToEx(dc, size / 4, size / 2, std::ptr::null_mut());
    LineTo(dc, size * 5 / 12, size * 3 / 4);
    LineTo(dc, size * 3 / 4, size / 3);
    SelectObject(dc, old);
    DeleteObject(pen as *mut _);
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
    let mut shots: Vec<Vec<u8>> = Vec::new();
    let mut last_raw: Vec<u8> = Vec::new();
    // 内存兜底：整段最多占 600MB
    // 内存兜底：整段最多占 900MB（120fps 下比 30fps 时限短得多）
    let max_frames = ((900 * 1024 * 1024) / (width.max(1) * height.max(1) * 4) as usize).max(30);
    // WebP 动画的时间戳是毫秒：120fps 取 8ms（≈125fps），60 取 16，30 取 33
    let delay_ms = (1000 / fps.max(1) as i32).max(1);

    for shot in &warmup {
        shots.push(to_rgba(shot));
        last_raw.clear();
        last_raw.extend_from_slice(shot);
    }

    let interval = std::time::Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(20);
    let mut next = std::time::Instant::now();

    while !STOP_REQUEST.load(Ordering::Relaxed)
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
        shots.push(to_rgba(&frame));
        last_raw.clear();
        last_raw.extend_from_slice(&frame);
    }

    let bytes = encode_webp(width, height, &shots, delay_ms);
    finish(border, button);

    if !bytes.is_empty() {
        unsafe {
            let pointer = Box::into_raw(Box::new(bytes));
            let target = panel as HWND;
            if !target.is_null() {
                PostMessageW(target, crate::WM_APP_GIF_DONE, pointer as WPARAM, 0);
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
fn encode_webp(width: i32, height: i32, frames: &[Vec<u8>], delay_ms: i32) -> Vec<u8> {
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
    for (index, frame) in frames.iter().enumerate() {
        let timestamp = index as i32 * delay_ms;
        encoder.add_frame(webp::AnimFrame::from_rgba(
            frame,
            width.max(1) as u32,
            height.max(1) as u32,
            timestamp,
        ));
    }
    match encoder.try_encode() {
        Ok(memory) => memory.to_vec(),
        Err(_) => Vec::new(),
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
    STOP_REQUEST.store(false, Ordering::SeqCst);
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
