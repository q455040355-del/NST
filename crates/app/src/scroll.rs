//! 滚动截图：选区固定，边滚边截，按「行指纹」找重叠，把新内容接成长图。
//!
//! 流程：截图遮罩里框好区域 → 这里接手 → 画一圈边框 + 一个「停止」胶囊
//! → 后台每 120ms 抓一帧，和已拼好的长图比行签名，重叠部分去掉、只接新行；
//! 同时往目标窗口发 WM_MOUSEWHEEL 自动往下滚（连滚 3 次都没有新内容就收工）。
//! 结束（Esc / 点停止 / 自动到底）后把长图丢回面板，走正常编辑窗。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    InvalidateRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, HGDIOBJ, PAINTSTRUCT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, RegisterClassW, WindowFromPoint,
    IDC_ARROW, WM_DESTROY, WM_LBUTTONUP, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_MOUSEWHEEL};

use crate::shot::{capture_rect, s, Bitmap};
use crate::theme::wide;

const DT_NOPREFIX: u32 = 0x0800;

const BORDER: i32 = 3;
const CHIP_W: i32 = 232;
const CHIP_H: i32 = 58;
const BORDER_CLASS: &str = "NetSpeedTaskbarRs.ScrollBorder";
const CHIP_CLASS: &str = "NetSpeedTaskbarRs.ScrollChip";
const HOTKEY_STOP: i32 = 0x5C01;

/// 每行抽 16 段算一个灰度签名：比整行逐像素稳，滚动时亚像素抖动也认得出
const SIG: usize = 16;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 已拼进去的行数（给 UI 显示进度）
static APPENDED: AtomicUsize = AtomicUsize::new(0);
/// 有没有在自动滚（UI 上显示状态）
static AUTO: AtomicBool = AtomicBool::new(true);
static STOP: AtomicBool = AtomicBool::new(false);
static CHIP: AtomicIsize = AtomicIsize::new(0);
static BORDER_HWND: AtomicIsize = AtomicIsize::new(0);
/// 胶囊是不是放在选区「里面」（放里面时抓帧要避开它那一条）
static INNER: AtomicBool = AtomicBool::new(false);

/// 一行 16 段灰度签名
fn row_sig(bitmap: &Bitmap, row: i32) -> [u8; SIG] {
    let mut out = [0u8; SIG];
    let width = bitmap.width.max(1);
    for (index, slot) in out.iter_mut().enumerate() {
        let x0 = (index as i32 * width) / SIG as i32;
        let x1 = (((index + 1) as i32 * width) / SIG as i32).max(x0 + 1);
        let mut sum = 0u32;
        let mut count = 0u32;
        for x in x0..x1.min(width) {
            let offset = ((row * width + x) * 4) as usize;
            if offset + 2 >= bitmap.pixels.len() {
                continue;
            }
            let pixel = &bitmap.pixels[offset..offset + 3];
            sum += (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3;
            count += 1;
        }
        *slot = if count > 0 { (sum / count) as u8 } else { 0 };
    }
    out
}

fn sig_diff(a: &[u8; SIG], b: &[u8; SIG]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .sum()
}

/// 两行算「同一行」的阈值：16 段平均差 <= 6 灰阶
const ROW_MATCH: u32 = SIG as u32 * 6;

pub enum Pushed {
    /// 接进去了新内容
    Appended,
    /// 跟上一帧一模一样（没滚动）
    Unchanged,
    /// 找不到重叠：滚太快 / 内容变了
    Lost,
}

pub struct Stitcher {
    width: i32,
    frame_h: i32,
    image: Bitmap,
    sigs: Vec<[u8; SIG]>,
    /// 长图最多这么多行，超了就收工（省内存）
    max_rows: i32,
}

impl Stitcher {
    pub fn new(first: Bitmap) -> Self {
        let width = first.width;
        let frame_h = first.height;
        let max_rows = ((12_000_000 / width.max(1)).clamp(2000, 20_000)) as i32;
        let sigs = (0..first.height).map(|y| row_sig(&first, y)).collect();
        Self {
            width,
            frame_h,
            image: first,
            sigs,
            max_rows,
        }
    }

    pub fn image(&self) -> &Bitmap {
        &self.image
    }

    pub fn height(&self) -> i32 {
        self.image.height
    }

    pub fn full(&self) -> bool {
        self.image.height >= self.max_rows
    }

    pub fn push(&mut self, frame: &Bitmap) -> Pushed {
        if frame.width != self.width || frame.height != self.frame_h {
            return Pushed::Lost;
        }
        let fresh: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(frame, y)).collect();
        let total = self.sigs.len() as i32;

        // 完全没动
        if total >= self.frame_h {
            let tail = &self.sigs[(total - self.frame_h) as usize..];
            if tail
                .iter()
                .zip(fresh.iter())
                .all(|(a, b)| sig_diff(a, b) <= ROW_MATCH / 2)
            {
                return Pushed::Unchanged;
            }
        }

        // 找重叠：不再「从大到小第一个通过就收」，而是取整段差最小的那个
        // （纯色 / 表格这类重复内容，k 偏大会把旧的重复内容多接一截）
        let max_overlap = (self.frame_h - 4).min(total);
        let min_overlap = (self.frame_h / 4).max(24).min(max_overlap);
        let mut best: Option<(i32, u32)> = None;
        for k in (min_overlap..=max_overlap).rev() {
            let tail = &self.sigs[(total - k) as usize..];
            let mut sum = 0u32;
            let mut ok = true;
            for (a, b) in tail.iter().zip(fresh.iter()) {
                let diff = sig_diff(a, b);
                if diff > ROW_MATCH {
                    ok = false;
                    break;
                }
                sum += diff;
                // 已经不可能比当前最优更好就停
                if let Some((_, best_sum)) = best {
                    if sum >= best_sum {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                best = Some((k, sum));
            }
        }

        let Some((k, _)) = best else {
            return Pushed::Lost;
        };
        if k >= self.frame_h {
            return Pushed::Unchanged;
        }

        let new_rows = self.frame_h - k;
        let room = self.max_rows - self.image.height;
        let take = new_rows.min(room.max(0));
        if take <= 0 {
            return Pushed::Unchanged;
        }

        // 把 frame 的第 k..k+take 行接上去
        for row in 0..take {
            let src_row = k + row;
            let src = ((src_row * self.width) * 4) as usize;
            let dst = (self.image.height * self.width * 4) as usize;
            let len = (self.width * 4) as usize;
            if src + len <= frame.pixels.len() && dst + len <= self.image.pixels.len() {
                self.image.pixels[dst..dst + len].copy_from_slice(&frame.pixels[src..src + len]);
            }
            self.sigs.push(fresh[src_row as usize]);
            self.image.height += 1;
        }

        let _ = take;
        Pushed::Appended
    }
}

// ---------------------------------------------------------------- 会话

static mut PANEL: HWND = std::ptr::null_mut();
static mut REGION: (i32, i32, i32, i32) = (0, 0, 0, 0);

/// 截图遮罩选好区域后进来：拿第一帧当底图，开始滚动拼接。
pub unsafe fn start(panel: HWND, first: Bitmap, rect: (i32, i32, i32, i32)) {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    let (x, y, w, h) = rect;
    if w < 32 || h < 32 {
        ACTIVE.store(false, Ordering::SeqCst);
        return;
    }
    PANEL = panel;
    REGION = rect;
    STOP.store(false, Ordering::SeqCst);
    APPENDED.store(h as usize, Ordering::SeqCst);
    AUTO.store(true, Ordering::SeqCst);
    crate::debug_log(&format!("scroll start: {x},{y} {w}x{h}"));

    let instance = GetModuleHandleW(std::ptr::null());

    // 选区边框：外侧 3px，鼠标穿透（能继续操作被截的窗口）
    let border_class = wide(BORDER_CLASS);
    let wc = WNDCLASSW {
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
    RegisterClassW(&wc);
    let border = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        border_class.as_ptr(),
        wide("滚动截图区域").as_ptr(),
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
        ACTIVE.store(false, Ordering::SeqCst);
        return;
    }
    BORDER_HWND.store(border as isize, Ordering::SeqCst);
    // 只留一圈边框：整窗减去中间那块
    {
        use windows_sys::Win32::Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject as DeleteGdi, RGN_DIFF};
        let outer = CreateRectRgn(0, 0, w + BORDER * 2, h + BORDER * 2);
        let inner = CreateRectRgn(BORDER, BORDER, BORDER + w, BORDER + h);
        CombineRgn(outer, outer, inner, RGN_DIFF);
        windows_sys::Win32::Graphics::Gdi::SetWindowRgn(border, outer, 0);
        DeleteGdi(inner as *mut _);
        DeleteGdi(outer as *mut _);
    }

    // 提示胶囊：优先放在选区「里面」的下沿（微信那种），
    // 抓帧时把这条高度让出去，所以不会被拼进长图；选区太矮就退到外面
    let inner = h >= CHIP_H + s(120.0);
    INNER.store(inner, Ordering::SeqCst);
    let chip_x = if inner {
        (x + (w - CHIP_W) / 2).max(x)
    } else {
        x
    };
    let chip_y = if inner {
        y + h - CHIP_H - s(10.0)
    } else if y - CHIP_H - 12 >= 0 {
        y - CHIP_H - 12
    } else {
        y + h + 12
    };
    let chip_class = wide(CHIP_CLASS);
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(chip_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: chip_class.as_ptr(),
    };
    RegisterClassW(&wc);
    let chip = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        chip_class.as_ptr(),
        wide("滚动截图").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        chip_x,
        chip_y,
        CHIP_W,
        CHIP_H,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    CHIP.store(chip as isize, Ordering::SeqCst);
    RegisterHotKey(chip, HOTKEY_STOP, MOD_NOREPEAT, 0x1B); // Esc

    // 自动滚动的目标窗口：选区中心那个窗口
    let center = POINT {
        x: x + w / 2,
        y: y + h / 2,
    };
    let target_usize = WindowFromPoint(center) as usize;

    std::thread::spawn(move || {
        let mut stitcher = Stitcher::new(first);
        // 已经滚过的轮数、这一轮里连续「没新内容」的帧数
        let mut scrolls = 0u32;
        let mut idle = 0u32;
        let mut ticks = 0u32;
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
            let (x, y, w, h) = REGION;
            let cap_h = capture_height();
            if let Some(frame) = capture_rect(x, y, w, cap_h) {
                match stitcher.push(&frame) {
                    Pushed::Appended => {
                        idle = 0;
                        APPENDED.store(stitcher.height() as usize, Ordering::SeqCst);
                        let chip = CHIP.load(Ordering::SeqCst) as HWND;
                        if !chip.is_null() {
                            InvalidateRect(chip, std::ptr::null(), 0);
                        }
                    }
                    Pushed::Unchanged | Pushed::Lost => idle += 1,
                }
            }
            if stitcher.full() {
                break;
            }
            if !AUTO.load(Ordering::SeqCst) {
                continue;
            }
            // 每 ~6 帧（0.7 秒）滚一次；上一轮滚完一整轮都没有新内容 = 已经到底
            ticks += 1;
            if ticks < 6 {
                continue;
            }
            ticks = 0;
            if scrolls > 0 && idle >= 6 {
                break;
            }
            let target = target_usize as HWND;
            if target.is_null() {
                break;
            }
            // 负的 delta = 向下滚
            let wheel = 0xFF88u32 << 16;
            PostMessageW(
                target,
                WM_MOUSEWHEEL,
                wheel as WPARAM,
                (((y + h / 2) << 16 | (x + w / 2)) as isize) as LPARAM,
            );
            scrolls += 1;
            idle = 0;
        }
        // 收尾：胶囊压在选区里时，先把它收掉，补抓一帧完整区域，
        // 这样最后那条（被胶囊挡住的高度）也不会缺
        if INNER.load(Ordering::SeqCst) {
            let chip = CHIP.load(Ordering::SeqCst) as HWND;
            if !chip.is_null() {
                PostMessageW(chip, 0x0010, 0, 0); // WM_CLOSE
            }
            std::thread::sleep(std::time::Duration::from_millis(220));
            let (x, y, w, h) = REGION;
            if let Some(frame) = capture_rect(x, y, w, h) {
                stitcher.push(&frame);
            }
        }
        finish(stitcher);
    });
}

/// 抓帧高度：胶囊在里面时只抓到它上沿，那一条留给最后补抓
unsafe fn capture_height() -> i32 {
    let (_, _, _, h) = REGION;
    if INNER.load(Ordering::SeqCst) {
        (h - CHIP_H - s(10.0)).max(32)
    } else {
        h
    }
}

/// 结束：收掉窗口，把长图丢回面板（主线程开编辑窗）
unsafe fn finish(stitcher: Stitcher) {
    ACTIVE.store(false, Ordering::SeqCst);
    let bitmap = stitcher.image().clone();
    crate::debug_log(&format!("scroll done: {}x{}", bitmap.width, bitmap.height));
    // 窗口都是 UI 线程建的，只能让它们自己销毁：给胶囊发 WM_CLOSE
    let chip = CHIP.load(Ordering::SeqCst) as HWND;
    if !chip.is_null() {
        UnregisterHotKey(chip, HOTKEY_STOP);
        PostMessageW(chip, 0x0010, 0, 0); // WM_CLOSE
    }
    let panel = PANEL;
    if !panel.is_null() {
        let payload = Box::new((bitmap, REGION));
        let pointer = Box::into_raw(payload);
        PostMessageW(panel, crate::WM_APP_SCROLL_DONE, pointer as WPARAM, 0);
    }
}

unsafe extern "system" fn border_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut rect: RECT = std::mem::zeroed();
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let ui = crate::theme::ui();
            let brush = CreateSolidBrush(ui.accent);
            FillRect(dc, &rect, brush);
            DeleteObject(brush as *mut _);
            EndPaint(hwnd, &mut ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn chip_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_chip(hwnd);
            0
        }
        WM_LBUTTONUP => {
            STOP.store(true, Ordering::SeqCst);
            0
        }
        0x0010 => {
            // WM_CLOSE
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            let border = BORDER_HWND.swap(0, Ordering::SeqCst) as HWND;
            if !border.is_null() {
                DestroyWindow(border);
            }
            CHIP.store(0, Ordering::SeqCst);
            0
        }
        0x0312 => {  // WM_HOTKEY
            STOP.store(true, Ordering::SeqCst);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint_chip(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut rect: RECT = std::mem::zeroed();
    windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);

    let ui = crate::theme::ui();
    let brush = CreateSolidBrush(ui.surface);
    FillRect(dc, &rect, brush);
    DeleteObject(brush as *mut _);

    let pen = windows_sys::Win32::Graphics::Gdi::CreatePen(0, 1, ui.border);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);
    let old_brush = SelectObject(
        dc,
        windows_sys::Win32::Graphics::Gdi::GetStockObject(
            windows_sys::Win32::Graphics::Gdi::NULL_BRUSH,
        ),
    );
    windows_sys::Win32::Graphics::Gdi::RoundRect(
        dc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        10,
        10,
    );
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(pen as *mut _);

    let font = CreateFontW(
        -14,
        0,
        0,
        0,
        600,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        0,
        0,
        CLEARTYPE_QUALITY as u32,
        0,
        wide("Microsoft YaHei UI").as_ptr(),
    );
    let old_font = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, 1);
    SetTextColor(dc, ui.text);
    let mut title = RECT {
        left: 12,
        top: 6,
        right: rect.right - 12,
        bottom: 30,
    };
    DrawTextW(
        dc,
        wide("滚动截图 · 正在自动滚动").as_ptr(),
        -1,
        &mut title,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SetTextColor(dc, ui.text3);
    let mut status = RECT {
        left: 12,
        top: 30,
        right: rect.right - 12,
        bottom: 52,
    };
    let rows = APPENDED.load(Ordering::SeqCst);
    let text = format!("已拼接 {rows} px · 点这里或按 Esc 结束", rows = rows);
    DrawTextW(
        dc,
        wide(&text).as_ptr(),
        -1,
        &mut status,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(dc, old_font);
    DeleteObject(font as *mut _);
}

/// 滚动截图是不是在进行中（菜单里改文案用）
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: i32, height: i32, top_row: i32) -> Bitmap {
        let mut bitmap = Bitmap {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        };
        // 每行用「绝对行号」派生的伪随机花纹：滚动后同一行还是同一串像素，
        // 但不同行之间没有线性关系（用 (行号*7)%251 这种会混叠，测不出真问题）
        for y in 0..height {
            let mut seed = (top_row + y) as u32 * 2_654_435_761;
            for x in 0..width {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let value = (seed >> 24) as u8;
                let offset = ((y * width + x) * 4) as usize;
                bitmap.pixels[offset] = value;
                bitmap.pixels[offset + 1] = value;
                bitmap.pixels[offset + 2] = value;
                bitmap.pixels[offset + 3] = 255;
            }
        }
        bitmap
    }

    #[test]
    fn stitches_scrolled_frame() {
        let first = frame(80, 200, 0);
        let mut stitcher = Stitcher::new(first);
        // 往下滚了 60 行：新帧第 0 行 = 绝对第 60 行
        let next = frame(80, 200, 60);
        match stitcher.push(&next) {
            Pushed::Appended => assert_eq!(stitcher.height(), 260),
            _ => panic!("应该接上 60 行"),
        }
        assert_eq!(stitcher.height(), 260);
    }

    #[test]
    fn identical_frame_is_unchanged() {
        let first = frame(80, 200, 0);
        let mut stitcher = Stitcher::new(first);
        let same = frame(80, 200, 0);
        assert!(matches!(stitcher.push(&same), Pushed::Unchanged));
        assert_eq!(stitcher.height(), 200);
    }

    #[test]
    fn unrelated_frame_is_lost() {
        let first = frame(80, 200, 0);
        let mut stitcher = Stitcher::new(first);
        // 完全不重叠的随机内容
        let mut noise = frame(80, 200, 0);
        for (index, byte) in noise.pixels.iter_mut().enumerate() {
            if index % 4 != 3 {
                *byte = ((index * 37) % 255) as u8;
            }
        }
        assert!(matches!(stitcher.push(&noise), Pushed::Lost));
        assert_eq!(stitcher.height(), 200);
    }

    #[test]
    fn repeated_scrolls_keep_growing() {
        let mut stitcher = Stitcher::new(frame(60, 120, 0));
        for step in 1..=5 {
            let next = frame(60, 120, step * 40);
            assert!(matches!(stitcher.push(&next), Pushed::Appended));
        }
        assert_eq!(stitcher.height(), 120 + 5 * 40);
    }
}
