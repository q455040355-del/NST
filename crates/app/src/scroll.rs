//! 滚动截图：选区固定，用户自己滚轮，程序只负责「抓帧 + 按重叠拼成长图」。
//!
//! 流程：编辑窗点「滚动截图」→ 关掉编辑窗、按那块区域起一个会话
//! → 选区外套一圈蓝框 + 选区里下沿浮一个胶囊（✓ 结束 / × 放弃）
//! → 用户自己往下滚，程序每 80ms 抓一帧、用行签名 + 逐像素比对找重叠，只接新行
//! → 点 ✓ 用拼好的长图回到编辑窗；点 × 把原来那张截图还回去。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use std::ffi::c_void;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, InvalidateRect, ReleaseDC, SelectObject,
    SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HALFTONE,
    HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, PostMessageW, RegisterClassW,
    IDC_ARROW, WM_DESTROY, WM_LBUTTONUP, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

use crate::shot::{capture_rect, s, Bitmap};
use crate::theme::wide;

const DT_NOPREFIX: u32 = 0x0800;

const BORDER: i32 = 3;
const CHIP_W: i32 = 268;
const CHIP_H: i32 = 62;
const BORDER_CLASS: &str = "NetSpeedTaskbarRs.ScrollBorder";
const PREVIEW_CLASS: &str = "NetSpeedTaskbarRs.ScrollPreview";
/// 右侧缩略图窗口宽 / 高
const THUMB_W: i32 = 150;
const PREVIEW_H: i32 = 420;
const CHIP_CLASS: &str = "NetSpeedTaskbarRs.ScrollChip";
const HOTKEY_STOP: i32 = 0x5C01;

/// 每行抽 16 段算一个灰度签名
const SIG: usize = 16;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// 点的是 ✓（要长图）还是 ×（回原图）
static CONFIRM: AtomicBool = AtomicBool::new(false);
/// 已拼到多少行（UI 上显示）
static APPENDED: AtomicUsize = AtomicUsize::new(0);
static CHIP: AtomicIsize = AtomicIsize::new(0);
static BORDER_HWND: AtomicIsize = AtomicIsize::new(0);
/// 右侧缩略图窗口
static PREVIEW: AtomicIsize = AtomicIsize::new(0);
/// 缩略图像素（宽, 高, BGRA），由抓帧线程写、预览窗口读
static THUMB: std::sync::Mutex<Option<(i32, i32, Vec<u8>)>> = std::sync::Mutex::new(None);
/// 胶囊是不是压在选区里
static INNER: AtomicBool = AtomicBool::new(false);
static mut PANEL: HWND = std::ptr::null_mut();
static mut REGION: (i32, i32, i32, i32) = (0, 0, 0, 0);

// ---------------------------------------------------------------- 拼接

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

/// 两段行签名的去均值相关系数（每 4 行取一行降开销）
fn correlation(a: &[[u8; SIG]], b: &[[u8; SIG]], k: i32) -> f32 {
    let mut sa = 0f64;
    let mut sb = 0f64;
    let mut saa = 0f64;
    let mut sbb = 0f64;
    let mut sab = 0f64;
    let mut n = 0f64;
    for (row, (x, y)) in a.iter().zip(b.iter()).take(k as usize).enumerate() {
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

pub enum Pushed {
    /// 接进了新内容
    Appended,
    /// 跟上一帧一样（没滚动）
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
    /// 上一次 push 找到的重叠（行数, 差异 *100），排查用
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

    /// 重叠区里「明显不一样」的像素占比（隔行隔列采样）。
    /// image_row / frame_row 是两边比较的起始行，rows 是比较多少行。
    fn mismatch_at(&self, frame: &Bitmap, image_row: i32, frame_row: i32, rows: i32) -> f32 {
        const HARD: i32 = 40;
        let width = self.width;
        if rows <= 0 || image_row < 0 || frame_row < 0 {
            return 1.0;
        }
        if image_row + rows > self.image.height || frame_row + rows > frame.height {
            return 1.0;
        }
        let mut bad = 0u64;
        let mut count = 0u64;
        let mut y = 0;
        while y < rows {
            let a_row = ((image_row + y) * width) as usize * 4;
            let b_row = ((frame_row + y) * width) as usize * 4;
            let mut x = 0;
            while x < width {
                let a = a_row + x as usize * 4;
                let b = b_row + x as usize * 4;
                if a + 2 < self.image.pixels.len() && b + 2 < frame.pixels.len() {
                    let mut worst = 0i32;
                    for c in 0..3 {
                        let d = self.image.pixels[a + c] as i32 - frame.pixels[b + c] as i32;
                        worst = worst.max(d.abs());
                    }
                    if worst > HARD {
                        bad += 1;
                    }
                    count += 1;
                }
                x += 2;
            }
            y += 2;
        }
        if count == 0 {
            1.0
        } else {
            bad as f32 / count as f32
        }
    }

    /// 在候选重叠里挑最像的：先用行签名相关系数粗筛，再比真实像素占比
    fn best_overlap<F, G>(
        &self,
        min_overlap: i32,
        max_overlap: i32,
        corr_of: F,
        mismatch_of: G,
    ) -> Option<(f32, i32)>
    where
        F: Fn(i32) -> f32,
        G: Fn(i32) -> f32,
    {
        let mut candidates: Vec<(f32, i32)> = Vec::new();
        for k in min_overlap..=max_overlap {
            let corr = corr_of(k);
            if corr > 0.5 {
                candidates.push((corr, k));
            }
        }
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(8);

        let mut scored: Vec<(f32, i32)> = candidates
            .iter()
            .map(|(_, k)| (mismatch_of(*k), *k))
            .collect();
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let Some((best_ratio, best_k)) = scored.first().copied() else {
            return None;
        };
        let second = scored
            .iter()
            .filter(|(_, k)| (k - best_k).abs() > 12)
            .map(|(ratio, _)| *ratio)
            .next()
            .unwrap_or(1.0);
        // 明显不一样的像素超过 8%，或者和第二名拉不开 1.4 倍，都算没对准
        if best_ratio > 0.08 || second < best_ratio * 1.4 {
            return None;
        }
        Some((best_ratio, best_k))
    }

    pub fn push(&mut self, frame: &Bitmap) -> Pushed {
        if frame.width != self.width {
            return Pushed::Lost;
        }
        let compare_h = frame.height.min(self.frame_h);
        if compare_h < 24 {
            return Pushed::Lost;
        }
        let fresh: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(frame, y)).collect();
        let total = self.sigs.len() as i32;

        // 完全没动：和上一帧逐行比，几乎一样就算没滚
        if total >= compare_h {
            let tail = &self.sigs[(total - compare_h) as usize..];
            let mut sum = 0u64;
            for (a, b) in tail.iter().zip(fresh.iter()) {
                sum += sig_diff(a, b) as u64;
            }
            let cells = compare_h as u64 * SIG as u64;
            if sum * 100 < cells * 6 {
                return Pushed::Unchanged;
            }
        }

        let max_overlap = (compare_h - 8).min(total.min(frame.height));
        let min_overlap = (compare_h / 4).max(32);
        if max_overlap <= min_overlap {
            return Pushed::Lost;
        }

        // 往下滚：长图尾部 == 新帧头部
        let down = self.best_overlap(
            min_overlap,
            max_overlap,
            |k| {
                let total = self.sigs.len() as i32;
                correlation(&self.sigs[(total - k) as usize..], &fresh, k)
            },
            |k| {
                let total = self.sigs.len() as i32;
                self.mismatch_at(frame, total - k, 0, k)
            },
        );
        // 往上滚：长图头部 == 新帧尾部
        let up = self.best_overlap(
            min_overlap,
            max_overlap,
            |k| {
                let end = frame.height;
                correlation(&self.sigs[..k as usize], &fresh[(end - k) as usize..], k)
            },
            |k| self.mismatch_at(frame, 0, frame.height - k, k),
        );
        // 两个方向都试过了，取更像的那个（周期内容两个方向都能对上时别猜错）
        match (down, up) {
            (Some((down_ratio, k)), Some((up_ratio, _))) => {
                if down_ratio <= up_ratio {
                    self.append_rows(frame, &fresh, k)
                } else {
                    let (_, k) = up.unwrap_or((0.0, 0));
                    self.prepend_rows(frame, &fresh, k)
                }
            }
            (Some((_, k)), None) => self.append_rows(frame, &fresh, k),
            (None, Some((_, k))) => self.prepend_rows(frame, &fresh, k),
            (None, None) => Pushed::Lost,
        }
    }

    /// 收尾用：内容没滚动、但这一帧比之前高（胶囊收掉了），
    /// 把多出来的底部几行直接补上；找不到重叠就什么都不做。
    pub fn extend_with_tail(&mut self, frame: &Bitmap) -> bool {
        if frame.width != self.width || frame.height <= self.frame_h {
            return false;
        }
        let fresh: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(frame, y)).collect();
        let total = self.sigs.len() as i32;
        let compare_h = self.frame_h;
        let max_overlap = compare_h.min(total);
        let min_overlap = (compare_h / 4).max(32);
        if max_overlap <= min_overlap {
            return false;
        }
        let mut best: Option<(f32, i32)> = None;
        for k in (min_overlap..=max_overlap).rev() {
            let corr = correlation(&self.sigs[(total - k) as usize..], &fresh, k);
            if best.map_or(corr > 0.5, |(c, _)| corr > c + 0.05) {
                best = Some((corr, k));
            }
        }
        let Some((corr, k)) = best else { return false };
        if corr < 0.5 {
            return false;
        }
        let mismatch = self.mismatch_at(frame, total - k, 0, k);
        if mismatch > 0.08 {
            return false;
        }
        let added = self.append_rows(frame, &fresh, k);
        matches!(added, Pushed::Appended)
    }

    /// 下滚：把新帧 k 之后的行接到长图末尾
    fn append_rows(&mut self, frame: &Bitmap, fresh: &[[u8; SIG]], k: i32) -> Pushed {
        let new_rows = frame.height - k;
        let room = self.max_rows - self.image.height;
        let take = new_rows.min(room.max(0));
        if take <= 0 {
            return Pushed::Unchanged;
        }
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
        self.frame_h = frame.height;
        Pushed::Appended
    }

    /// 上滚：把新帧前面那几行插到长图最前面（整块像素往下挪）
    fn prepend_rows(&mut self, frame: &Bitmap, fresh: &[[u8; SIG]], k: i32) -> Pushed {
        let add = frame.height - k;
        if add <= 0 {
            return Pushed::Unchanged;
        }
        let room = self.max_rows - self.image.height;
        let add = add.min(room.max(0));
        if add <= 0 {
            return Pushed::Unchanged;
        }
        let row_bytes = (self.width * 4) as usize;
        let new_len = ((self.image.height + add) * self.width * 4) as usize;
        self.image.pixels.resize(new_len, 0);
        // 先把已有像素整体往后挪 add 行，再把新帧的开头拷进去
        self.image
            .pixels
            .copy_within(0..new_len - add as usize * row_bytes, add as usize * row_bytes);
        for row in 0..add {
            let src = ((row * self.width) * 4) as usize;
            let dst = ((row * self.width) * 4) as usize;
            let len = row_bytes;
            if src + len <= frame.pixels.len() && dst + len <= self.image.pixels.len() {
                self.image.pixels[dst..dst + len].copy_from_slice(&frame.pixels[src..src + len]);
            }
        }
        // 签名也要在前面补上
        let mut sigs = Vec::with_capacity(self.sigs.len() + add as usize);
        for row in 0..add {
            sigs.push(fresh[row as usize]);
        }
        sigs.extend_from_slice(&self.sigs);
        self.sigs = sigs;
        self.image.height += add;
        self.frame_h = frame.height;
        Pushed::Appended
    }
}

// ---------------------------------------------------------------- 会话

/// 建一块 32 位、行序自顶向下的 DIB（预览窗口画缩略图用）
unsafe fn make_dib(width: i32, height: i32) -> (HDC, HBITMAP, *mut u8) {
    let screen = GetDC(std::ptr::null_mut());
    let dc = CreateCompatibleDC(screen);
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width.max(1);
    info.bmiHeader.biHeight = -height.max(1);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
    if !dib.is_null() {
        SelectObject(dc, dib as HGDIOBJ);
    }
    ReleaseDC(std::ptr::null_mut(), screen);
    (dc, dib, bits as *mut u8)
}

/// 把当前长图采样成一张小图（最近邻，只要 O(缩略图) 的功夫）
unsafe fn refresh_thumb(stitcher: &Stitcher, width: i32) {
    let image = stitcher.image();
    if image.width <= 0 || image.height <= 0 {
        return;
    }
    let thumb_w = width.clamp(40, 240);
    let thumb_h = ((image.height as f32 * thumb_w as f32 / image.width as f32) as i32).max(1);
    let mut pixels = vec![0u8; (thumb_w * thumb_h * 4) as usize];
    for ty in 0..thumb_h {
        let sy = (ty * image.height / thumb_h).min(image.height - 1);
        for tx in 0..thumb_w {
            let sx = (tx * image.width / thumb_w).min(image.width - 1);
            let src = ((sy * image.width + sx) * 4) as usize;
            let dst = ((ty * thumb_w + tx) * 4) as usize;
            if src + 3 < image.pixels.len() {
                pixels[dst..dst + 4].copy_from_slice(&image.pixels[src..src + 4]);
            }
        }
    }
    if let Ok(mut guard) = THUMB.lock() {
        *guard = Some((thumb_w, thumb_h, pixels));
    }
    let preview = PREVIEW.load(Ordering::SeqCst) as HWND;
    if !preview.is_null() {
        InvalidateRect(preview, std::ptr::null(), 0);
    }
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

/// 连抓几帧并拼接（滚动动画中间那几帧也能用上）
unsafe fn burst_push(stitcher: &mut Stitcher, frames: usize) -> bool {
    let (x, y, w, _) = REGION;
    let cap_h = capture_height();
    let mut appended = false;
    for _ in 0..frames {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
        if let Some(frame) = capture_rect(x, y, w, cap_h) {
            if let Pushed::Appended = stitcher.push(&frame) {
                appended = true;
                APPENDED.store(stitcher.height() as usize, Ordering::SeqCst);
                refresh_thumb(stitcher, THUMB_W);
                let chip = CHIP.load(Ordering::SeqCst) as HWND;
                if !chip.is_null() {
                    InvalidateRect(chip, std::ptr::null(), 0);
                }
            }
        }
    }
    appended
}

/// 截图编辑窗点了「滚动截图」之后进来：拿当前这块图 + 屏幕位置开会话
pub unsafe fn start(panel: HWND, first: Bitmap, rect: (i32, i32, i32, i32)) {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        // 上一个会话没收拾干净（比如线程 panic 了）：只要胶囊窗口不在了就强制重开，
        // 否则用户会看到「点了滚动截图，编辑窗没了、也没有任何抓图界面」
        let chip = CHIP.load(Ordering::SeqCst) as HWND;
        let alive = !chip.is_null()
            && windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(chip) != 0;
        if alive {
            return;
        }
        crate::debug_log("scroll: 发现上一个会话没收干净，强制重开");
    }
    let (x, y, w, h) = rect;
    if w < 32 || h < 32 {
        ACTIVE.store(false, Ordering::SeqCst);
        return;
    }
    PANEL = panel;
    REGION = rect;
    STOP.store(false, Ordering::SeqCst);
    CONFIRM.store(false, Ordering::SeqCst);
    APPENDED.store(h as usize, Ordering::SeqCst);
    crate::debug_log(&format!("scroll start: {x},{y} {w}x{h}（手动滚动）"));

    let instance = GetModuleHandleW(std::ptr::null());

    // 选区边框：外侧 3px，鼠标穿透
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
    {
        use windows_sys::Win32::Graphics::Gdi::{
            CombineRgn, CreateRectRgn, DeleteObject as DeleteGdi, RGN_DIFF,
        };
        let outer = CreateRectRgn(0, 0, w + BORDER * 2, h + BORDER * 2);
        let inner = CreateRectRgn(BORDER, BORDER, BORDER + w, BORDER + h);
        CombineRgn(outer, outer, inner, RGN_DIFF);
        windows_sys::Win32::Graphics::Gdi::SetWindowRgn(border, outer, 0);
        DeleteGdi(inner as *mut _);
        DeleteGdi(outer as *mut _);
    }

    // 胶囊：优先压选区下沿（微信那种），选区太矮才退到外面
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
    RegisterHotKey(chip, HOTKEY_STOP, MOD_NOREPEAT, 0x1B); // Esc = 放弃

    // 放弃时回原图
    // 右侧缩略图：优先放在选区右边，右边没地方就放左边
    let mut preview_x = x + w + 10;
    let mut preview_y = y;
    let mut area: RECT = std::mem::zeroed();
    if windows_sys::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
        windows_sys::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
        0,
        &mut area as *mut RECT as *mut _,
        0,
    ) != 0
    {
        if preview_x + THUMB_W > area.right {
            preview_x = x - THUMB_W - 10;
        }
        if preview_x < area.left {
            preview_x = (x + w - THUMB_W - 8).max(area.left);
        }
        preview_y = preview_y
            .min(area.bottom - PREVIEW_H - 8)
            .max(area.top + 4);
    }
    let preview_class = wide(PREVIEW_CLASS);
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(preview_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: preview_class.as_ptr(),
    };
    RegisterClassW(&wc);
    let preview = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        preview_class.as_ptr(),
        wide("滚动截图预览").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        preview_x,
        preview_y,
        THUMB_W,
        PREVIEW_H,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    PREVIEW.store(preview as isize, Ordering::SeqCst);

    let original = first.clone();
    // 首帧要跟后面抓帧一样高（胶囊在里面时只有上半截），否则高度对不上永远接不了
    let cap_h = (h - if inner { CHIP_H + s(10.0) } else { 0 }).max(32);
    std::thread::spawn(move || {
        // 万一线程里 panic（索引越界之类），ACTIVE 必须清掉，
        // 否则下一次点「滚动截图」会被当成「已经在跑」直接忽略
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            unsafe {
            let seed = capture_rect(x, y, w, cap_h).unwrap_or_else(|| {
                // 抓不到就用传进来的图裁一段
                let mut copy = first.clone();
                copy.height = cap_h.min(first.height);
                copy.pixels.truncate((copy.width * copy.height * 4).max(0) as usize);
                copy
            });
            let mut stitcher = Stitcher::new(seed);
            while !STOP.load(Ordering::SeqCst) {
                burst_push(&mut stitcher, 3);
                if stitcher.full() {
                    crate::debug_log("scroll: 到长图上限，收工");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            let confirm = CONFIRM.load(Ordering::SeqCst);
            crate::debug_log(&format!(
                "scroll: 收尾 {}x{} 确认={confirm}",
                stitcher.image().width,
                stitcher.height()
            ));
            // 胶囊压在选区里时先收掉它，补抓一帧完整区域，把被挡住的那条补上
            if INNER.load(Ordering::SeqCst) {
                let chip = CHIP.load(Ordering::SeqCst) as HWND;
                if !chip.is_null() {
                    PostMessageW(chip, 0x0010, 0, 0); // WM_CLOSE
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                let (x, y, w, h) = REGION;
                if let Some(frame) = capture_rect(x, y, w, h) {
                    // 胶囊收掉之后这一帧比之前高：把多出来的底部补上，
                    // 不然最后那一条（被胶囊挡住的高度）永远拼不进去
                    if !stitcher.extend_with_tail(&frame) {
                        stitcher.push(&frame);
                    }
                }
            }
            finish(stitcher.image().clone(), confirm, original);
            }
        }));
        if outcome.is_err() {
            crate::debug_log("scroll: 线程 panic，已清理");
            unsafe {
                ACTIVE.store(false, Ordering::SeqCst);
                let chip = CHIP.swap(0, Ordering::SeqCst) as HWND;
                if !chip.is_null() {
                    PostMessageW(chip, 0x0010, 0, 0);
                }
                let preview = PREVIEW.swap(0, Ordering::SeqCst) as HWND;
                if !preview.is_null() {
                    PostMessageW(preview, 0x0010, 0, 0);
                }
            }
        }
    });
}

/// 收尾：关掉边框和胶囊，把图丢回面板（主线程开编辑窗）
unsafe fn finish(image: Bitmap, confirm: bool, original: Bitmap) {
    ACTIVE.store(false, Ordering::SeqCst);
    let chip = CHIP.load(Ordering::SeqCst) as HWND;
    if !chip.is_null() {
        UnregisterHotKey(chip, HOTKEY_STOP);
        PostMessageW(chip, 0x0010, 0, 0); // WM_CLOSE
    }
    let preview = PREVIEW.swap(0, Ordering::SeqCst) as HWND;
    if !preview.is_null() {
        PostMessageW(preview, 0x0010, 0, 0); // WM_CLOSE
    }
    let panel = PANEL;
    if panel.is_null() {
        return;
    }
    // ✓ -> 拼好的长图；× -> 原来那张截图
    let bitmap = if confirm { image } else { original };
    let payload = Box::new((bitmap, REGION));
    let pointer = Box::into_raw(payload);
    PostMessageW(panel, crate::WM_APP_SCROLL_DONE, pointer as WPARAM, 0);
}

// ---------------------------------------------------------------- 两个小窗口

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
            let brush = CreateSolidBrush(crate::theme::ui().accent);
            FillRect(dc, &rect, brush);
            DeleteObject(brush as *mut _);
            EndPaint(hwnd, &mut ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 胶囊里两个按钮的位置（客户区坐标）：✓ 和 ×
fn chip_buttons() -> (RECT, RECT) {
    let size = s(26.0);
    let top = (CHIP_H - size) / 2;
    let ok = RECT {
        left: CHIP_W - size - s(46.0),
        top,
        right: CHIP_W - s(46.0),
        bottom: top + size,
    };
    let cancel = RECT {
        left: CHIP_W - size - s(12.0),
        top,
        right: CHIP_W - s(12.0),
        bottom: top + size,
    };
    (ok, cancel)
}

unsafe extern "system" fn chip_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_chip(hwnd);
            0
        }
        WM_LBUTTONUP => {
            let x = (lparam as i32) & 0xFFFF;
            let y = ((lparam as i32) >> 16) & 0xFFFF;
            let (ok, cancel) = chip_buttons();
            let inside = |r: &RECT| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
            if inside(&ok) {
                CONFIRM.store(true, Ordering::SeqCst);
                STOP.store(true, Ordering::SeqCst);
            } else if inside(&cancel) {
                CONFIRM.store(false, Ordering::SeqCst);
                STOP.store(true, Ordering::SeqCst);
            }
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
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn preview_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_preview(hwnd);
            0
        }
        0x0010 => {
            // WM_CLOSE
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PREVIEW.store(0, Ordering::SeqCst);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 把缩略图画进右侧那个小窗口：先在底上画一块深色，再把缩略图按比例铺满
unsafe fn paint_preview(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut client: RECT = std::mem::zeroed();
    windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);

    let ui = crate::theme::ui();
    let brush = CreateSolidBrush(0x0020_2020);
    FillRect(dc, &client, brush);
    DeleteObject(brush as *mut _);

    let snapshot = THUMB.lock().ok().and_then(|guard| guard.clone());
    if let Some((thumb_w, thumb_h, pixels)) = snapshot {
        if thumb_w > 0 && thumb_h > 0 {
            // 按「塞得进窗口」的比例缩放，居中
            let max_w = (client.right - 8).max(1);
            let max_h = (client.bottom - 8).max(1);
            let scale = (max_w as f32 / thumb_w as f32).min(max_h as f32 / thumb_h as f32);
            let draw_w = (thumb_w as f32 * scale).max(1.0) as i32;
            let draw_h = (thumb_h as f32 * scale).max(1.0) as i32;
            let left = 4 + (max_w - draw_w) / 2;
            let top = 4 + (max_h - draw_h) / 2;

            let (mem, dib, bits) = make_dib(thumb_w, thumb_h);
            if !mem.is_null() && !dib.is_null() && !bits.is_null() {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr(),
                    bits,
                    (thumb_w * thumb_h * 4) as usize,
                );
                SetStretchBltMode(dc, HALFTONE);
                StretchBlt(
                    dc,
                    left,
                    top,
                    draw_w,
                    draw_h,
                    mem,
                    0,
                    0,
                    thumb_w,
                    thumb_h,
                    SRCCOPY,
                );
                DeleteObject(dib as *mut _);
                DeleteDC(mem);
            }
        }
    }

    // 一圈细边框，跟别的小窗保持一致
    let pen = windows_sys::Win32::Graphics::Gdi::CreatePen(0, 1, ui.border);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);
    let old_brush = SelectObject(
        dc,
        windows_sys::Win32::Graphics::Gdi::GetStockObject(
            windows_sys::Win32::Graphics::Gdi::NULL_BRUSH,
        ),
    );
    windows_sys::Win32::Graphics::Gdi::Rectangle(
        dc,
        client.left,
        client.top,
        client.right,
        client.bottom,
    );
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(pen as *mut _);

    EndPaint(hwnd, &mut ps);
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
        12,
        12,
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
        top: 4,
        right: rect.right - s(96.0),
        bottom: 32,
    };
    DrawTextW(
        dc,
        wide("滚动截图 · 用滚轮往下滚").as_ptr(),
        -1,
        &mut title,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SetTextColor(dc, ui.text3);
    let mut status = RECT {
        left: 12,
        top: 30,
        right: rect.right - s(96.0),
        bottom: 58,
    };
    let rows = APPENDED.load(Ordering::SeqCst);
    let text = format!("已拼接 {rows} px · 滚完点 ✓", rows = rows);
    DrawTextW(
        dc,
        wide(&text).as_ptr(),
        -1,
        &mut status,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(dc, old_font);
    DeleteObject(font as *mut _);

    // ✓：主题蓝底白勾
    let (ok, cancel) = chip_buttons();
    let ok_brush = CreateSolidBrush(ui.accent);
    let ok_pen = windows_sys::Win32::Graphics::Gdi::CreatePen(0, 1, ui.accent);
    let old_b = SelectObject(dc, ok_brush as HGDIOBJ);
    let old_p = SelectObject(dc, ok_pen as HGDIOBJ);
    windows_sys::Win32::Graphics::Gdi::RoundRect(
        dc,
        ok.left,
        ok.top,
        ok.right,
        ok.bottom,
        s(8.0),
        s(8.0),
    );
    SelectObject(dc, old_p);
    SelectObject(dc, old_b);
    DeleteObject(ok_pen as *mut _);
    DeleteObject(ok_brush as *mut _);
    let check_font = CreateFontW(
        -16,
        0,
        0,
        0,
        700,
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
    let old_cf = SelectObject(dc, check_font as HGDIOBJ);
    SetTextColor(dc, 0x00FF_FFFF);
    let mut ok_text = ok;
    DrawTextW(
        dc,
        wide("✓").as_ptr(),
        -1,
        &mut ok_text,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    // ×：灰底灰叉
    let cancel_brush = CreateSolidBrush(ui.chip);
    let cancel_pen = windows_sys::Win32::Graphics::Gdi::CreatePen(0, 1, ui.chip);
    let old_b = SelectObject(dc, cancel_brush as HGDIOBJ);
    let old_p = SelectObject(dc, cancel_pen as HGDIOBJ);
    windows_sys::Win32::Graphics::Gdi::RoundRect(
        dc,
        cancel.left,
        cancel.top,
        cancel.right,
        cancel.bottom,
        s(8.0),
        s(8.0),
    );
    SelectObject(dc, old_p);
    SelectObject(dc, old_b);
    DeleteObject(cancel_pen as *mut _);
    DeleteObject(cancel_brush as *mut _);
    SetTextColor(dc, ui.text2);
    let mut cancel_text = cancel;
    DrawTextW(
        dc,
        wide("×").as_ptr(),
        -1,
        &mut cancel_text,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(dc, old_cf);
    DeleteObject(check_font as *mut _);

    EndPaint(hwnd, &mut ps);
}

/// 自测：在给定屏幕区域上跑 N 秒「抓帧 + 拼接」，把结果存 PNG（滚动由外面驱动）。
/// 用来在没人盯着屏幕时验证拼接算法对真实内容是否成立：
///   NST_SCROLL_TEST="x,y,w,h,秒数,out.png" netspeed.exe
pub unsafe fn selftest(spec: &str) -> bool {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 6 {
        return false;
    }
    let x: i32 = parts[0].trim().parse().unwrap_or(0);
    let y: i32 = parts[1].trim().parse().unwrap_or(0);
    let w: i32 = parts[2].trim().parse().unwrap_or(0);
    let h: i32 = parts[3].trim().parse().unwrap_or(0);
    let seconds: u64 = parts[4].trim().parse().unwrap_or(6);
    let path = parts[5].trim();
    if w < 32 || h < 32 || path.is_empty() {
        return false;
    }
    let Some(seed) = capture_rect(x, y, w, h) else {
        return false;
    };
    let mut stitcher = Stitcher::new(seed);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(80));
        if let Some(frame) = capture_rect(x, y, w, h) {
            let outcome = match stitcher.push(&frame) {
                Pushed::Appended => "接上".to_string(),
                Pushed::Unchanged => "没滚动".to_string(),
                Pushed::Lost => format!("对不上 best={:?}", stitcher.last_best),
            };
            crate::debug_log(&format!(
                "scroll test: {outcome} -> {} 行",
                stitcher.height()
            ));
        }
    }
    let image = stitcher.image();
    crate::debug_log(&format!(
        "scroll test: 结束 {}x{}",
        image.width, image.height
    ));
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

    /// 造一张「像截图」的图：白底 + 一行行深浅不一的墨块（模拟文字行）+ 轻噪声。
    /// 每一行的墨块位置由「文档行号」播种，行与行之间没有周期性，
    /// 这样才能测出「会不会对错行」。
    fn text_like(width: i32, height: i32, top_row: i32) -> Bitmap {
        let mut bitmap = Bitmap {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        };
        for y in 0..height {
            let doc_row = top_row + y;
            let mut seed = (doc_row as u32).wrapping_mul(2_654_435_761) ^ 0x9E37_79B9;
            let in_line = (doc_row % 24) < 16;
            // 这一行的墨块：起点和长度都由行号决定
            let mut ink = vec![(0i32, 0i32); 6];
            for slot in ink.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x0 = (seed >> 8) % (width as u32) as u32;
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let len = 10 + (seed >> 20) % 80;
                *slot = (x0 as i32, len as i32);
            }
            for x in 0..width {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 28) as i32 - 8).clamp(-6, 6);
                let mut value = 246i32;
                if in_line {
                    for (x0, len) in &ink {
                        if x >= *x0 && x < *x0 + *len {
                            value = 70;
                            break;
                        }
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
    fn tail_extension_adds_bottom_strip_when_frame_grows() {
        // 先按「短帧」拼一段（模拟胶囊压着、只抓上半截）
        let doc = text_like(600, 800, 0);
        let mut short = doc.clone();
        short.height = 500;
        short.pixels.truncate((600 * 500 * 4) as usize);
        let mut stitcher = Stitcher::new(short);
        assert_eq!(stitcher.height(), 500);

        // 收尾：胶囊收掉，同一块内容整帧抓下来（内容没滚，但多了下面 300 行）
        let mut full = doc.clone();
        full.height = 800;
        full.pixels.truncate((600 * 800 * 4) as usize);
        assert!(stitcher.extend_with_tail(&full), "应该能把底部补上");
        assert_eq!(stitcher.height(), 800);
    }

    #[test]
    fn stitches_scrolled_frame() {
        let first = frame(80, 200, 0);
        let mut stitcher = Stitcher::new(first);
        let next = frame(80, 200, 60);
        assert!(matches!(stitcher.push(&next), Pushed::Appended));
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


    #[test]
    fn scrolling_up_prepends_rows() {
        for step in [19, 40, 113] {
            let first = text_like(600, 500, 400); // 文档第 400..900 行
            let mut stitcher = Stitcher::new(first);
            let up = text_like(600, 500, 400 - step); // 往上滚：上面多出 step 行
            match stitcher.push(&up) {
                Pushed::Appended => {
                    assert_eq!(stitcher.height(), 500 + step, "step={step}");
                    // 新接上的应该是文档最上面的那 step 行
                    let expect = text_like(600, step, 400 - step);
                    let got = &stitcher.image().pixels[..(600 * step * 4) as usize];
                    assert_eq!(got, &expect.pixels[..], "step={step} 头部内容不对");
                }
                _ => panic!("往上滚 step={step} 没接上"),
            }
        }
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
                _ => panic!("step={step} 没接上"),
            }
        }
    }
}
