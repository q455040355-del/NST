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
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, RegisterClassW, SetForegroundWindow,
    WindowFromPoint,
    IDC_ARROW, WM_DESTROY, WM_LBUTTONUP, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, SetCursorPos, WM_MOUSEWHEEL};

use crate::shot::{capture_rect, s, Bitmap};
use crate::theme::wide;

const DT_NOPREFIX: u32 = 0x0800;

const BORDER: i32 = 3;
const CHIP_W: i32 = 232;
const CHIP_H: i32 = 58;
const BORDER_CLASS: &str = "NetSpeedTaskbarRs.ScrollBorder";
const CHIP_CLASS: &str = "NetSpeedTaskbarRs.ScrollChip";
const HOTKEY_STOP: i32 = 0x5C01;
/// 一格滚轮 = 120
const WHEEL_DELTA: i32 = 120;

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

pub enum Pushed {
    /// 接进去了新内容
    Appended,
    /// 跟上一帧一模一样（没滚动）
    Unchanged,
    /// 找不到重叠：滚太快 / 内容变了
    Lost,
}

/// 两段行签名（取前 k 行）的去均值相关系数，每 4 行取一行降开销
fn correlation(a: &[[u8; SIG]], b: &[[u8; SIG]], k: i32) -> f32 {
    let mut sa = 0f64;
    let mut sb = 0f64;
    let mut saa = 0f64;
    let mut sbb = 0f64;
    let mut sab = 0f64;
    let mut n = 0f64;
    let mut row = 0usize;
    for (x, y) in a.iter().zip(b.iter()).take(k as usize) {
        row += 1;
        if row % 4 != 0 {
            continue;
        }
        for (p, q) in x.iter().zip(y.iter()) {
            let (p, q) = (*p as f64, *q as f64);
            sa += p;
            sb += q;
            saa += p * p;
            sbb += q * q;
            sab += p * q;
            n += 1.0;
        }
    }
    if n < 1.0 {
        return 0.0;
    }
    let ma = sa / n;
    let mb = sb / n;
    let va = (saa / n - ma * ma).max(0.0);
    let vb = (sbb / n - mb * mb).max(0.0);
    if va < 1e-6 || vb < 1e-6 {
        return 0.0;
    }
    let cov = sab / n - ma * mb;
    (cov / (va.sqrt() * vb.sqrt())) as f32
}

pub struct Stitcher {
    width: i32,
    frame_h: i32,
    image: Bitmap,
    sigs: Vec<[u8; SIG]>,
    /// 长图最多这么多行，超了就收工（省内存）
    max_rows: i32,
    /// 上一次 push 找到的重叠（行数, 总差异），排查用
    pub last_best: Option<(i32, u32)>,
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
            last_best: None,
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

    /// 重叠区逐像素平均差（隔行隔列采样，够快也够准）
    fn pixel_diff(&self, frame: &Bitmap, k: i32) -> f32 {
        let width = self.width;
        if k <= 0 || k > self.image.height {
            return 255.0;
        }
        let top = self.image.height - k;
        let mut sum = 0u64;
        let mut count = 0u64;
        let mut y = 0;
        while y < k {
            let a_row = ((top + y) * width) as usize * 4;
            let b_row = (y * width) as usize * 4;
            let mut x = 0;
            while x < width {
                let a = a_row + x as usize * 4;
                let b = b_row + x as usize * 4;
                if a + 2 < self.image.pixels.len() && b + 2 < frame.pixels.len() {
                    for c in 0..3 {
                        let d = self.image.pixels[a + c] as i32 - frame.pixels[b + c] as i32;
                        sum += d.unsigned_abs() as u64;
                        count += 1;
                    }
                }
                x += 2;
            }
            y += 2;
        }
        if count == 0 {
            255.0
        } else {
            sum as f32 / count as f32
        }
    }

    pub fn push(&mut self, frame: &Bitmap) -> Pushed {
        if frame.width != self.width || frame.height != self.frame_h {
            return Pushed::Lost;
        }
        let fresh: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(frame, y)).collect();
        let total = self.sigs.len() as i32;

        // 完全没动：和上一帧逐行比，整体几乎一样就算没滚
        if total >= self.frame_h {
            let tail = &self.sigs[(total - self.frame_h) as usize..];
            let mut sum = 0u64;
            for (a, b) in tail.iter().zip(fresh.iter()) {
                sum += sig_diff(a, b) as u64;
            }
            let rows = self.frame_h as u64 * SIG as u64;
            if sum * 100 < rows * 6 {
                return Pushed::Unchanged;
            }
        }

        // 用「去均值相关系数」找重叠：真实截图整行逐像素比对噪音太大
        // （抗锯齿、字体次像素渲染都会变），相关系数只看形状，稳得多。
        let max_overlap = (self.frame_h - 8).min(total);
        let min_overlap = (self.frame_h / 4).max(32).min(max_overlap);
        // 第一步：相关系数粗筛，留几个候选（重复内容会有好几个峰）
        let mut candidates: Vec<(f32, i32)> = Vec::new();
        for k in min_overlap..=max_overlap {
            let tail = &self.sigs[(total - k) as usize..];
            let corr = correlation(tail, &fresh, k);
            if corr > 0.5 {
                candidates.push((corr, k));
            }
        }
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(8);

        // 第二步：候选里用逐像素平均差定夺，挑差异最小的那个
        let mut best: Option<(f32, i32)> = None;
        for (_, k) in &candidates {
            let diff = self.pixel_diff(frame, *k);
            if best.map_or(true, |(best_diff, _)| diff < best_diff) {
                best = Some((diff, *k));
            }
        }

        self.last_best = best.map(|(diff, k)| (k, (diff * 100.0) as u32));
        let Some((diff, k)) = best else {
            return Pushed::Lost;
        };
        // 平均差超过 8 灰阶就认为对不上（真实截图同一位置一般 0~4）
        if diff > 8.0 || k >= self.frame_h {
            return if diff > 8.0 { Pushed::Lost } else { Pushed::Unchanged };
        }

        let new_rows = self.frame_h - k;
        let room = self.max_rows - self.image.height;
        let take = new_rows.min(room.max(0));
        if take <= 0 {
            return Pushed::Unchanged;
        }

        // 把 frame 的第 k..k+take 行接上去。
        // 先把像素缓冲扩到新高度 —— 否则后面的行写不进去（之前就是这个 bug：
        // 高度涨了、像素没长，拼出来的长图其实只有第一帧）
        let target_len = ((self.image.height + take) * self.width * 4) as usize;
        if self.image.pixels.len() < target_len {
            self.image.pixels.resize(target_len, 0);
        }
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

    // 0 = SendInput（真输入），1 = PostMessage（兜底）
    let mut method = 0u8;
    // 记住用户原来的鼠标位置，结束后还回去
    let mut origin_cursor = POINT { x: 0, y: 0 };
    GetCursorPos(&mut origin_cursor);

    std::thread::spawn(move || {
        let mut stitcher = Stitcher::new(first);
        // 每步滚多少：先从一格开始，跨太大就减半，滚不动就加倍
        let mut delta = -WHEEL_DELTA;
        let mut idle_rounds = 0u32;
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            let region = REGION;
            let cap_h = capture_height();
            let (cx, cy) = (region.0 + region.2 / 2, region.1 + region.3 / 2);
            if AUTO.load(Ordering::SeqCst) {
                match method {
                    0 => wheel_input(cx, cy, delta),
                    1 => vscroll_message(target_usize as HWND),
                    _ => wheel_message(target_usize as HWND, cx, cy, delta),
                }
            }
            let grew = burst_push(&mut stitcher, region, cap_h, 7);
            if stitcher.full() {
                crate::debug_log("scroll: 到长图上限，收工");
                break;
            }
            if grew {
                idle_rounds = 0;
                delta = (delta * 2).clamp(-WHEEL_DELTA * 3, -40);
            } else {
                idle_rounds += 1;
                // 走完一整步还是没接上：先缩步长；缩到最小还不行就收工
                if delta >= -30 {
                    crate::debug_log(&format!("scroll: 步长 {delta} 没接上，收工"));
                    break;
                }
                delta = (delta / 2).max(-30);
                if idle_rounds >= 8 {
                    crate::debug_log("scroll: 连续 8 步没有新内容，收工");
                    break;
                }
            }
            if idle_rounds >= 2 && method < 2 {
                // 这一步没接上：换一种发送方式再试（真输入 -> WM_VSCROLL -> 投消息）
                method += 1;
                idle_rounds = 0;
                crate::debug_log(&format!("scroll: 换发送方式 method={method}"));
            }
        }
        crate::debug_log(&format!(
            "scroll: 收尾 {}x{}",
            stitcher.image().width,
            stitcher.height()
        ));
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
        SetCursorPos(origin_cursor.x, origin_cursor.y);
        finish(stitcher);
    });
}

/// 真输入滚轮：先把光标挪进去，再发 MOUSEEVENTF_WHEEL（走系统输入栈，浏览器也吃）
unsafe fn wheel_input(x: i32, y: i32, delta: i32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    };
    SetCursorPos(x, y);
    let mut input: INPUT = std::mem::zeroed();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: delta as u32,
        dwFlags: MOUSEEVENTF_WHEEL,
        time: 0,
        dwExtraInfo: 0,
    };
    SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
}

/// 兜底二：直接投 WM_VSCROLL(SB_LINEDOWN)，经典 Win32 控件一行一行滚，步长正好够重叠
unsafe fn vscroll_message(target: HWND) {
    const WM_VSCROLL: u32 = 0x0115;
    const SB_LINEDOWN: usize = 1;
    PostMessageW(target, WM_VSCROLL, SB_LINEDOWN as WPARAM, 0);
}

/// 兜底：直接往目标窗口投 WM_MOUSEWHEEL（有些窗口只认消息）
unsafe fn wheel_message(target: HWND, x: i32, y: i32, delta: i32) {
    let wheel = ((delta as i16 as u16) as u32) << 16;
    PostMessageW(
        target,
        WM_MOUSEWHEEL,
        wheel as WPARAM,
        ((y << 16) | (x & 0xFFFF)) as isize as LPARAM,
    );
}

/// 滚一次之后连拍几帧：平滑滚动的程序（浏览器）在动画中间会有重叠，
/// 一直抓不到重叠就是「一步跨太大」，把步长减半再试。
unsafe fn burst_push(
    stitcher: &mut Stitcher,
    region: (i32, i32, i32, i32),
    cap_h: i32,
    frames: usize,
) -> bool {
    let (x, y, _, _) = region;
    let w = region.2;
    let mut appended = false;
    for _ in 0..frames {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(70));
        if let Some(frame) = capture_rect(x, y, w, cap_h) {
            if let Pushed::Appended = stitcher.push(&frame) {
                appended = true;
                APPENDED.store(stitcher.height() as usize, Ordering::SeqCst);
                let chip = CHIP.load(Ordering::SeqCst) as HWND;
                if !chip.is_null() {
                    InvalidateRect(chip, std::ptr::null(), 0);
                }
            }
        }
    }
    appended
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

/// 自测：在给定屏幕区域上跑一遍完整流程（抓帧 + 真输入滚轮 + 拼接），把长图存成 PNG。
///
/// 用来在没人盯着屏幕的情况下验证「滚轮真的能滚、重叠真的能拼」：
///   NST_SCROLL_TEST="x,y,w,h,out.png" netspeed.exe
pub unsafe fn selftest(spec: &str) -> bool {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 5 {
        return false;
    }
    let x: i32 = parts[0].trim().parse().unwrap_or(0);
    let y: i32 = parts[1].trim().parse().unwrap_or(0);
    let w: i32 = parts[2].trim().parse().unwrap_or(0);
    let h: i32 = parts[3].trim().parse().unwrap_or(0);
    let path = parts[4].trim();
    if w < 32 || h < 32 || path.is_empty() {
        return false;
    }

    let center = POINT {
        x: x + w / 2,
        y: y + h / 2,
    };
    let target = WindowFromPoint(center);
    let focused = SetForegroundWindow(target);
    crate::debug_log(&format!(
        "scroll test: 目标窗口 {target:?} 前台={focused}"
    ));

    let Some(first) = capture_rect(x, y, w, h) else {
        return false;
    };
    let mut stitcher = Stitcher::new(first);
    let mut delta = -WHEEL_DELTA;
    for method in 0..3 {
        for round in 0..4 {
            match method {
                0 => wheel_input(center.x, center.y, delta),
                1 => vscroll_message(target),
                _ => wheel_message(target, center.x, center.y, delta),
            }
            let grew = burst_push(&mut stitcher, (x, y, w, h), h, 5);
            if grew {
                delta = (delta * 2).clamp(-WHEEL_DELTA * 3, -40);
                crate::debug_log(&format!(
                    "scroll test: method {method} round {round} 接上 -> {} 行",
                    stitcher.height()
                ));
            } else {

                delta = (delta / 2).max(-30);
                crate::debug_log(&format!(
                    "scroll test: method {method} round {round} 没接上 (best={:?})",
                    stitcher.last_best
                ));
            }
        }
    }

    let image = stitcher.image();
    let mut rgba = Vec::with_capacity((image.width * image.height * 4) as usize);
    for pixel in image.pixels.chunks_exact(4) {
        rgba.push(pixel[2]);
        rgba.push(pixel[1]);
        rgba.push(pixel[0]);
        rgba.push(255);
    }
    match image::RgbaImage::from_raw(image.width as u32, image.height as u32, rgba) {
        Some(buffer) => buffer.save(path).is_ok(),
        None => false,
    }
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
            let mut seed = ((top_row + y) as u32).wrapping_mul(2_654_435_761);
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


    /// 造一张「像截图」的图：白底 + 一行行深浅不一的横条（模拟文字行）+ 轻噪声
    fn text_like(width: i32, height: i32, top_row: i32) -> Bitmap {
        let mut bitmap = Bitmap {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        };
        let mut seed = 0x1234_5678u32;
        for y in 0..height {
            let doc_row = top_row + y;
            // 每 24 行一个文本行，行内有若干深色小块
            let in_line = (doc_row % 24) < 16;
            for x in 0..width {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 28) as i32 - 8).clamp(-6, 6);
                let mut value = 246i32;
                if in_line {
                    // 用 doc_row 决定墨块位置，保证滚动后同一行一致
                    let h = (doc_row * 31 + x / 7) % 11;
                    if h < 6 {
                        value = 60 + ((doc_row * 7 + x) % 40) as i32;
                    }
                }
                let v = (value + noise).clamp(0, 255) as u8;
                let offset = ((y * width + x) * 4) as usize;
                bitmap.pixels[offset] = v;
                bitmap.pixels[offset + 1] = v;
                bitmap.pixels[offset + 2] = v;
                bitmap.pixels[offset + 3] = 255;
            }
        }
        bitmap
    }

    #[test]
    fn realistic_text_stitches_for_various_scroll_steps() {
        for step in [19, 37, 113, 240] {
            let first = text_like(600, 500, 0);
            let mut stitcher = Stitcher::new(first);
            let next = text_like(600, 500, step);
            match stitcher.push(&next) {
                Pushed::Appended => {
                    assert_eq!(stitcher.height(), 500 + step, "step={step} 应该只接上 {step} 行");
                }
                other => panic!("step={step} 没接上（{:?}）", matches!(other, Pushed::Lost)),
            }
        }
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
