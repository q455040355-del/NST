//! 滚动截图：选区固定，用户自己滚轮，程序只负责「抓帧 + 按重叠拼成长图」。
//!
//! 流程：编辑窗点「滚动截图」→ 关掉编辑窗、按那块区域起一个会话
//! → 选区外套一圈蓝框 + 选区里下沿浮一个胶囊（✓ 结束 / × 放弃）
//! → 用户自己往下滚，程序每 80ms 抓一帧、用行签名 + 逐像素比对找重叠，只接新行
//! → 点 ✓ 用拼好的长图回到编辑窗；点 × 把原来那张截图还回去。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use std::ffi::c_void;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, InvalidateRect, ReleaseDC, SelectObject,
    SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HALFTONE,
    HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
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
const CHIP_W: i32 = 300;
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

/// 长图能占多少内存（BGRA）。原来按「1200 万像素」卡死，窄选区能拼 2 万行、
/// 宽选区只有 6 千行；改成按字节预算后，宽度只影响能拼多长，不再有固定的行数上限。
const MAX_CANVAS_BYTES: i64 = 256 * 1024 * 1024;

/// 接缝混合用的重叠行数：接新内容前，把这 N 行和长图最后 N 行按权重混一下，
/// 误差不到 1px 的接缝就不会出现一条硬线（完全对齐时混合结果和原值相同，不会糊）
const SEAM_BLEND_ROWS: i32 = 3;

/// 滚动抓帧的节奏上下限：最快 30fps、最慢 8fps（滚得慢的时候没必要空转）
const MIN_SCROLL_INTERVAL_US: u64 = 33_000;
const MAX_SCROLL_INTERVAL_US: u64 = 125_000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// 点的是 ✓（要长图）还是 ×（回原图）
static CONFIRM: AtomicBool = AtomicBool::new(false);
/// 已拼到多少行（UI 上显示）
static APPENDED: AtomicUsize = AtomicUsize::new(0);
/// 胶囊上那句提示：0 = 等你滚，1 = 正在接，2 = 对不上要回滚，3 = 到上限
static HINT: AtomicU8 = AtomicU8::new(0);
static CHIP: AtomicIsize = AtomicIsize::new(0);
static BORDER_HWND: AtomicIsize = AtomicIsize::new(0);
/// 右侧缩略图窗口
static PREVIEW: AtomicIsize = AtomicIsize::new(0);
/// 缩略图刷新节流用：上次刷新的时间点（毫秒）与调用次数
static THUMB_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_THUMB_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
/// 缩略图像素（宽, 高, BGRA），由抓帧线程写、预览窗口读
static THUMB: std::sync::Mutex<Option<(i32, i32, std::sync::Arc<Vec<u8>>)>> =
    std::sync::Mutex::new(None);
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

/// 一次滚动的统计：出问题时靠这几个数判断是「没抓到」还是「没对上」
#[derive(Default, Clone, Copy)]
pub struct StitchStats {
    /// 抓到的帧数
    pub frames: u32,
    /// 接上新内容的帧数
    pub appended: u32,
    /// 跟上一帧一样（没滚动）
    pub unchanged: u32,
    /// 对不上（滚太快 / 内容变了）
    pub lost: u32,
    /// 两帧之间没重叠、直接跳过去的行数（这部分内容确实丢了）
    pub gap_rows: i64,
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
    /// 上一帧原图：用它和当前帧算「这一下滚了多少、往哪滚」，
    /// 比拿长图去猜方向靠谱（长图内容重复时两个方向都能对上，就会拼重）
    prev: Option<Bitmap>,
    /// 当前帧顶端落在长图的第几行（显示区顶端在长图里的位置）。
    /// 有了它才知道「这一帧看到的内容有多少是新的」：往下滚越过长图底部才多出新行，
    /// 往上滚越过长图顶部才多出新行；中间那段来回滚都是已经拼过的，不能再接一遍。
    offset: i32,
    /// 累计统计（收尾时打一条日志）
    pub stats: StitchStats,
}

impl Stitcher {
    pub fn new(first: Bitmap) -> Self {
        let width = first.width;
        let frame_h = first.height;
        let max_rows = ((MAX_CANVAS_BYTES / (width.max(1) as i64 * 4)).clamp(2000, 300_000)) as i32;
        let sigs = (0..first.height).map(|y| row_sig(&first, y)).collect();
        // 首帧本身就是「上一帧」，这样第一次 push 就能算位移
        let prev = Some(first.clone());
        Self {
            width,
            frame_h,
            image: first,
            sigs,
            max_rows,
            last_best: None,
            prev,
            offset: 0,
            stats: StitchStats::default(),
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

    /// 算当前帧相对上一帧的位移：正数 = 内容往上走（往下滚），负数 = 往上滚。
    fn signed_shift(&self, prev: &Bitmap, frame: &Bitmap, fresh: &[[u8; SIG]]) -> Option<(i32, f32)> {
        let prev_sigs: Vec<[u8; SIG]> = (0..prev.height).map(|y| row_sig(prev, y)).collect();
        let compare_h = frame.height.min(prev.height);
        if compare_h < 32 {
            return None;
        }
        // 完全没动（帧间差极小）直接判定为没滚动
        let still = self.mismatch_prev(prev, 0, frame, 0, compare_h);
        if still < 0.02 {
            return Some((0, still));
        }
        let max_shift = compare_h - 8;
        let min_shift = 6;

        let mut candidates: Vec<(f32, i32)> = Vec::new();
        for raw in min_shift..=max_shift {
            let rows = (compare_h - raw) as usize;
            if rows < 16 {
                continue;
            }
            let shift = raw as usize;
            let down = correlation(&prev_sigs[shift..shift + rows], &fresh[..rows], rows as i32);
            if down > 0.5 {
                candidates.push((down, raw));
            }
            let up = correlation(&prev_sigs[..rows], &fresh[shift..shift + rows], rows as i32);
            if up > 0.5 {
                candidates.push((up, -raw));
            }
        }
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(10);

        let mut scored: Vec<(f32, i32)> = candidates
            .iter()
            .map(|(_, shift)| {
                let shift = *shift;
                let rows = compare_h - shift.abs();
                let ratio = if shift >= 0 {
                    // 往下滚：上一帧 shift 之后 对上 当前帧开头
                    self.mismatch_prev(prev, prev.height - rows, frame, 0, rows)
                } else {
                    self.mismatch_prev(prev, 0, frame, -shift, rows)
                };
                (ratio, shift)
            })
            .collect();
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let (best_ratio, best_shift) = scored[0];
        let second = scored
            .iter()
            .filter(|(_, shift)| (*shift - best_shift).abs() > 8)
            .map(|(ratio, _)| *ratio)
            .next()
            .unwrap_or(1.0);
        if best_ratio > 0.08 || second < best_ratio * 1.3 {
            return None;
        }
        Some((best_shift, best_ratio))
    }

    /// 两张图之间比「明显不一样」的像素占比
    fn mismatch_prev(
        &self,
        a: &Bitmap,
        a_row: i32,
        b: &Bitmap,
        b_row: i32,
        rows: i32,
    ) -> f32 {
        const HARD: i32 = 40;
        let width = self.width;
        let (a_height, b_height) = (a.height, b.height);
        if rows <= 0 || a_row < 0 || b_row < 0 {
            return 1.0;
        }
        if a_row + rows > a_height || b_row + rows > b_height {
            return 1.0;
        }
        let mut bad = 0u64;
        let mut count = 0u64;
        let mut y = 0;
        while y < rows {
            let row_a = ((a_row + y) * width) as usize * 4;
            let row_b = ((b_row + y) * width) as usize * 4;
            let mut x = 0;
            while x < width {
                let ia = row_a + x as usize * 4;
                let ib = row_b + x as usize * 4;
                if ia + 2 < a.pixels.len() && ib + 2 < b.pixels.len() {
                    let mut worst = 0i32;
                    for c in 0..3 {
                        let d = a.pixels[ia + c] as i32 - b.pixels[ib + c] as i32;
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

    pub fn push(&mut self, frame: Bitmap) -> Pushed {
        self.stats.frames += 1;
        if frame.width != self.width {
            self.stats.lost += 1;
            return Pushed::Lost;
        }
        if frame.height < 24 {
            self.stats.lost += 1;
            return Pushed::Lost;
        }
        let fresh: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(&frame, y)).collect();

        // 和上一帧比位移：方向、滚了多少都是算出来的，不用猜
        let shift = match self.prev.as_ref() {
            Some(prev) => self.signed_shift(prev, &frame, &fresh),
            None => None,
        };
        if let Some((value, ratio)) = shift {
            self.last_best = Some((value.abs(), (ratio * 1000.0) as u32));
        }
        // 完全没动：省掉下面那次「去长图里找位置」的开销
        if let Some((0, _)) = shift {
            self.prev = Some(frame);
            self.stats.unchanged += 1;
            return Pushed::Unchanged;
        }

        // 先按相邻帧算「这一帧顶端该落在长图第几行」，再去长图里核对；
        // 相邻帧对不上（滚太快/掉帧）或者两者差太多，就用长图里的绝对位置兜底。
        let guess = shift.map(|(value, _)| self.offset + value);
        let top = match (guess, self.anchor_top(&frame, &fresh)) {
            (Some(guess), Some((anchor, ratio))) => {
                if (anchor - guess).abs() <= 6 || ratio <= 0.02 {
                    anchor
                } else {
                    guess
                }
            }
            (Some(guess), None) => guess,
            (None, Some((anchor, ratio))) => {
                self.last_best = Some((anchor.abs(), (ratio * 1000.0) as u32));
                anchor
            }
            (None, None) => {
                self.last_best = None;
                self.prev = Some(frame);
                self.stats.lost += 1;
                return Pushed::Lost;
            }
        };
        let outcome = self.apply_top(&frame, top);
        match outcome {
            Pushed::Appended => self.stats.appended += 1,
            Pushed::Unchanged => self.stats.unchanged += 1,
            Pushed::Lost => self.stats.lost += 1,
        }
        // 这一帧成为新的「上一帧」，下次跟它比（所有权转移，不拷贝）
        let frame_height = frame.height;
        self.prev = Some(frame);
        if matches!(outcome, Pushed::Appended) {
            self.frame_h = frame_height;
        }
        outcome
    }

    /// 在已经拼好的长图里找「这一帧的顶端落在第几行」。
    ///
    /// 相邻两帧对不上时（滚太快、中间掉帧）光靠位移量是补不回来的：那一帧只能丢掉，
    /// 后面的内容也跟着错位。拿这一帧的头几行到长图尾部去对，就能算出绝对位置，
    /// 大半内容还能救回来，也不至于把已经拼好的行再拼一遍。
    fn anchor_top(&self, frame: &Bitmap, frame_sigs: &[[u8; SIG]]) -> Option<(i32, f32)> {
        const PROBE: i32 = 48;
        let reach = (frame.height * 2).max(PROBE);
        let low = (self.image.height - reach).max(0);
        let high = self.image.height - 8;
        if high <= low {
            return None;
        }
        let mut best: Option<(f32, i32)> = None;
        let mut runner_up = 1.0f32;
        let mut top = low;
        while top <= high {
            let rows = PROBE.min(self.image.height - top).min(frame.height);
            if rows < 12 {
                break;
            }
            let corr = correlation(
                &self.sigs[top as usize..(top + rows) as usize],
                &frame_sigs[..rows as usize],
                rows,
            );
            if corr > 0.75 {
                let ratio = self.mismatch_prev(&self.image, top, frame, 0, rows);
                if ratio < 0.06 {
                    match best {
                        Some((best_ratio, best_top)) if ratio < best_ratio => {
                            if (top - best_top).abs() > 16 {
                                runner_up = runner_up.min(best_ratio);
                            }
                            best = Some((ratio, top));
                        }
                        Some((best_ratio, best_top)) => {
                            if (top - best_top).abs() > 16 {
                                runner_up = runner_up.min(ratio);
                                let _ = best_ratio;
                            }
                        }
                        None => best = Some((ratio, top)),
                    }
                }
            }
            top += 2;
        }
        let (ratio, top) = best?;
        // 内容重复（同一段话出现两次）时会撞上两个一样好的位置，宁可放弃
        if ratio > 0.01 && runner_up < ratio * 1.5 {
            return None;
        }
        Some((top, ratio))
    }

    /// 把这一帧按「顶端落在长图第 top 行」拼进去
    fn apply_top(&mut self, frame: &Bitmap, top: i32) -> Pushed {
        if top < 0 {
            let need = (-top).min(frame.height);
            self.offset = 0;
            return self.prepend_rows_count(frame, need);
        }
        self.offset = top;
        let need = top + frame.height - self.image.height;
        if need <= 0 {
            return Pushed::Unchanged;
        }
        // 帧和长图之间有空档：中间那几行一帧都没抓到（滚太快）。记一笔，方便排查漏内容
        if top > self.image.height {
            let gap = (top - self.image.height) as i64;
            self.stats.gap_rows += gap;
            crate::debug_log(&format!("scroll: 漏了 {gap} 行（两帧之间没重叠，滚太快）"));
        }
        let need = need.min(frame.height);
        self.append_rows_from(frame, frame.height - need)
    }

    /// 接缝混合：长图最后 `SEAM_BLEND_ROWS` 行 和 这一帧里与之重叠的那几行，
    /// 按线性权重混一次（越靠接缝越偏向新帧）。两边本来就一样时结果不变。
    fn blend_seam(&mut self, frame: &Bitmap, from_row: i32) {
        let rows = SEAM_BLEND_ROWS.min(self.image.height).min(from_row);
        if rows <= 0 {
            return;
        }
        let stride = (self.width * 4) as usize;
        let weight_sum = (rows + 1) as u32;
        for index in 0..rows {
            let image_row = self.image.height - rows + index;
            // 这一帧里和 image_row 重叠的那一行
            let frame_row = from_row - rows + index;
            let src = (frame_row as usize) * stride;
            let dst = (image_row as usize) * stride;
            if src + stride > frame.pixels.len() || dst + stride > self.image.pixels.len() {
                continue;
            }
            // index 越大越靠近接缝 → 新帧权重越大
            let new_weight = (index + 1) as u32;
            let old_weight = weight_sum - new_weight;
            for offset in 0..stride {
                let old = self.image.pixels[dst + offset] as u32;
                let new = frame.pixels[src + offset] as u32;
                self.image.pixels[dst + offset] =
                    ((old * old_weight + new * new_weight) / weight_sum) as u8;
            }
        }
    }

    /// 把这一帧的 from_row 之后的行接在长图末尾
    fn append_rows_from(&mut self, frame: &Bitmap, from_row: i32) -> Pushed {
        if from_row < 0 || from_row >= frame.height {
            return Pushed::Unchanged;
        }
        let new_rows = frame.height - from_row;
        let room = self.max_rows - self.image.height;
        let take = new_rows.min(room.max(0));
        if take <= 0 {
            return Pushed::Unchanged;
        }
        // 接缝：进新行之前，先把「长图最后几行」和「这一帧对应的那几行」按权重混一遍
        self.blend_seam(frame, from_row);
        let target_len = ((self.image.height + take) * self.width * 4) as usize;
        if self.image.pixels.len() < target_len {
            self.image.pixels.resize(target_len, 0);
        }
        for row in 0..take {
            let src_row = from_row + row;
            let src = ((src_row * self.width) * 4) as usize;
            let dst = (self.image.height * self.width * 4) as usize;
            let len = (self.width * 4) as usize;
            if src + len <= frame.pixels.len() && dst + len <= self.image.pixels.len() {
                self.image.pixels[dst..dst + len].copy_from_slice(&frame.pixels[src..src + len]);
                self.sigs.push(row_sig(frame, src_row));
                self.image.height += 1;
            }
        }
        Pushed::Appended
    }

    /// 把这一帧开头 count 行插到长图最前面（整块像素后移）
    fn prepend_rows_count(&mut self, frame: &Bitmap, count: i32) -> Pushed {
        let room = self.max_rows - self.image.height;
        let add = count.min(room.max(0)).min(frame.height);
        if add <= 0 {
            return Pushed::Unchanged;
        }
        let row_bytes = (self.width * 4) as usize;
        let new_len = ((self.image.height + add) * self.width * 4) as usize;
        self.image.pixels.resize(new_len, 0);
        self.image
            .pixels
            .copy_within(0..new_len - add as usize * row_bytes, add as usize * row_bytes);
        for row in 0..add {
            let src = ((row * self.width) * 4) as usize;
            let dst = ((row * self.width) * 4) as usize;
            if src + row_bytes <= frame.pixels.len()
                && dst + row_bytes <= self.image.pixels.len()
            {
                self.image.pixels[dst..dst + row_bytes]
                    .copy_from_slice(&frame.pixels[src..src + row_bytes]);
                self.sigs.insert(row as usize, row_sig(frame, row));
                self.image.height += 1;
            }
        }
        Pushed::Appended
    }

    /// 收尾用：内容没滚动、但这一帧比之前高（胶囊收掉了），
    /// 把多出来的底部几行直接补上；找不到重叠就什么都不做。
    pub fn extend_with_tail(&mut self, frame: &Bitmap) -> bool {
        if frame.width != self.width || frame.height <= self.frame_h {
            return false;
        }
        let prev = match self.prev.clone() {
            Some(prev) => prev,
            None => return false,
        };
        let fresh: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(frame, y)).collect();
        let compare_h = prev.height.min(frame.height);
        if compare_h < 32 {
            return false;
        }
        // 内容没动：上一帧整块对上当前帧开头，剩下的是新露出来的底部
        let ratio = self.mismatch_prev(&prev, 0, frame, 0, compare_h);
        if ratio > 0.08 {
            return false;
        }
        let _ = fresh;
        // 内容没动，那么「超出长图底部的行」就是被胶囊挡住、这次才露出来的那条
        let need = self.offset + frame.height - self.image.height;
        if need <= 0 {
            return false;
        }
        let need = need.min(frame.height);
        matches!(
            self.append_rows_from(frame, frame.height - need),
            Pushed::Appended
        )
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
    // 节流：最多 8 次/秒。缩略图不用跟抓帧一样勤，也别把 UI 线程刷爆
    let now = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    if now.saturating_sub(LAST_THUMB_MS.load(Ordering::Relaxed)) < 120 {
        return;
    }
    LAST_THUMB_MS.store(now, Ordering::Relaxed);

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
        *guard = Some((thumb_w, thumb_h, std::sync::Arc::new(pixels)));
    }
    let calls = THUMB_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    if calls <= 5 || calls % 20 == 0 {
        crate::debug_log(&format!(
            "scroll: 缩略图 {thumb_w}x{thumb_h}（长图 {}x{}）第 {calls} 次",
            image.width, image.height
        ));
    }
    let preview = PREVIEW.load(Ordering::SeqCst) as HWND;
    if !preview.is_null() {
        InvalidateRect(preview, std::ptr::null(), 0);
    }
}

/// 下一拍等多久（微秒）：按实测「抓 + 拼」的开销留 25% 余量，夹在 33~125ms 之间，
/// 再和当前值做平滑，免得一帧抖动就把节奏带飞。
fn next_interval_us(current: u64, cost_us: u64) -> u64 {
    let want = cost_us
        .saturating_mul(5)
        .saturating_div(4)
        .clamp(MIN_SCROLL_INTERVAL_US, MAX_SCROLL_INTERVAL_US);
    (((current * 3 + want) + 2) / 4).clamp(MIN_SCROLL_INTERVAL_US, MAX_SCROLL_INTERVAL_US)
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
    HINT.store(0, Ordering::SeqCst);
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
    // 缩略图只在选区外面找位置。哪儿都放不下就挪到屏幕外（等于不显示）——
    // 宁可少个预览，也不能把自己的窗录进长图里
    let mut area: RECT = std::mem::zeroed();
    if windows_sys::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
        windows_sys::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
        0,
        &mut area as *mut RECT as *mut _,
        0,
    ) == 0
    {
        area = RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        };
    }
    let selection = RECT {
        left: x - 6,
        top: y - 6,
        right: x + w + 6,
        bottom: y + h + 6,
    };
    let candidates_preview = [
        (x + w + 10, y),
        (x - THUMB_W - 10, y),
        (x + w + 10, y + h - PREVIEW_H),
        (x - THUMB_W - 10, y + h - PREVIEW_H),
        (area.left + 8, area.top + 8),
        (area.right - THUMB_W - 8, area.bottom - PREVIEW_H - 8),
    ];
    let (preview_x, preview_y) = candidates_preview
        .into_iter()
        .map(|(px, py)| {
            (
                px.clamp(area.left, (area.right - THUMB_W).max(area.left)),
                py.clamp(area.top + 4, (area.bottom - PREVIEW_H - 4).max(area.top + 4)),
            )
        })
        .find(|(px, py)| {
            let rect = RECT {
                left: *px,
                top: *py,
                right: px + THUMB_W,
                bottom: py + PREVIEW_H,
            };
            rect.left >= selection.right
                || rect.right <= selection.left
                || rect.top >= selection.bottom
                || rect.bottom <= selection.top
        })
        .unwrap_or((-10000, -10000));
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
            // 先把首帧画到缩略图里，不然刚开始滚之前右边是个空白框
            refresh_thumb(&stitcher, THUMB_W);
            // 抓得勤不勤跟着实际开销走：起手按 30fps，跟不上就往下调（最低 8fps），
            // 快了再慢慢收回来。滚得快的时候多抓几帧，才不会整段漏掉。
            let started = std::time::Instant::now();
            let mut interval_us = MIN_SCROLL_INTERVAL_US;
            while !STOP.load(Ordering::SeqCst) {
                let tick = std::time::Instant::now();
                let mut cost_us = 0u64;
                if let Some(frame) = capture_rect(x, y, w, cap_h) {
                    let captured = std::time::Instant::now();
                    let outcome = stitcher.push(frame);
                    cost_us = captured.duration_since(tick).as_micros() as u64
                        + captured.elapsed().as_micros() as u64;
                    let hint = match outcome {
                        Pushed::Appended => 1u8,
                        Pushed::Lost => 2,
                        Pushed::Unchanged => HINT.load(Ordering::SeqCst),
                    };
                    if HINT.swap(hint, Ordering::SeqCst) != hint {
                        let chip = CHIP.load(Ordering::SeqCst) as HWND;
                        if !chip.is_null() {
                            InvalidateRect(chip, std::ptr::null(), 0);
                        }
                    }
                    if matches!(outcome, Pushed::Appended) {
                        APPENDED.store(stitcher.height() as usize, Ordering::SeqCst);
                        refresh_thumb(&stitcher, THUMB_W);
                        let chip = CHIP.load(Ordering::SeqCst) as HWND;
                        if !chip.is_null() {
                            InvalidateRect(chip, std::ptr::null(), 0);
                        }
                    }
                }
                if stitcher.full() {
                    crate::debug_log("scroll: 到长图上限，收工");
                    if HINT.swap(3, Ordering::SeqCst) != 3 {
                        let chip = CHIP.load(Ordering::SeqCst) as HWND;
                        if !chip.is_null() {
                            InvalidateRect(chip, std::ptr::null(), 0);
                        }
                    }
                    break;
                }
                interval_us = next_interval_us(interval_us, cost_us);
                let elapsed = tick.elapsed().as_micros() as u64;
                if interval_us > elapsed {
                    std::thread::sleep(std::time::Duration::from_micros(interval_us - elapsed));
                }
            }
            {
                let stats = stitcher.stats;
                let secs = started.elapsed().as_secs_f64().max(0.001);
                crate::debug_log(&format!(
                    "scroll 统计: 抓 {} 帧 / 接 {} / 没动 {} / 对不上 {} / 漏 {} 行，{:.1}s → 抓帧 {:.1}fps",
                    stats.frames,
                    stats.appended,
                    stats.unchanged,
                    stats.lost,
                    stats.gap_rows,
                    secs,
                    stats.frames as f64 / secs
                ));
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
                        stitcher.push(frame);
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

thread_local! {
    /// 预览窗口的后备缓冲（尺寸变了才重建），避免每帧先填底色再贴图闪烁
    static PREVIEW_BACK: std::cell::RefCell<Option<(i32, i32, HDC, HBITMAP)>> =
        std::cell::RefCell::new(None);
}

/// 把缩略图画进右侧那个小窗口：先画到后备缓冲，再整块贴上去
unsafe fn paint_preview(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let window_dc = BeginPaint(hwnd, &mut ps);
    let mut client: RECT = std::mem::zeroed();
    windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
    if client.right <= 0 || client.bottom <= 0 {
        EndPaint(hwnd, &mut ps);
        return;
    }

    // 双缓冲：先画到后备 DC，最后一次性贴到窗口（否则每帧先填底色再贴图会闪）
    let back = PREVIEW_BACK.with(|cell| {
        let mut guard = cell.borrow_mut();
        match guard.as_ref() {
            Some((w, h, dc, _dib)) if *w == client.right && *h == client.bottom => Some(*dc),
            _ => {
                if let Some((_, _, old_dc, old_dib)) = guard.take() {
                    DeleteObject(old_dib as *mut _);
                    DeleteDC(old_dc);
                }
                let (mem, dib, _bits) = make_dib(client.right, client.bottom);
                if mem.is_null() || dib.is_null() {
                    if !dib.is_null() {
                        DeleteObject(dib as *mut _);
                    }
                    if !mem.is_null() {
                        DeleteDC(mem);
                    }
                    None
                } else {
                    *guard = Some((client.right, client.bottom, mem, dib));
                    Some(mem)
                }
            }
        }
    });
    let Some(dc) = back else {
        EndPaint(hwnd, &mut ps);
        return;
    };

    let ui = crate::theme::ui();
    let brush = CreateSolidBrush(ui.chip);
    FillRect(dc, &client, brush);
    DeleteObject(brush as *mut _);

    // 缩略图：锁住期间直接拷进临时 DIB，别把整张缩略图 clone 一份
    let snapshot = THUMB
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|(w, h, p)| (*w, *h, p.clone())));
    if let Some((thumb_w, thumb_h, pixels)) = snapshot {
        if thumb_w > 0 && thumb_h > 0 {
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
                if draw_w == thumb_w && draw_h == thumb_h {
                    BitBlt(dc, left, top, draw_w, draw_h, mem, 0, 0, SRCCOPY);
                } else {
                    SetStretchBltMode(dc, HALFTONE);
                    StretchBlt(dc, left, top, draw_w, draw_h, mem, 0, 0, thumb_w, thumb_h, SRCCOPY);
                }
                DeleteObject(dib as *mut _);
                DeleteDC(mem);
            }
        }
    }

    // 一圈细边框
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

    BitBlt(window_dc, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
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
    let hint = match HINT.load(Ordering::SeqCst) {
        1 => "继续往下滚",
        2 => "重叠不足，往回滚一点",
        3 => "到上限了，点 ✓ 结束",
        _ => "把鼠标放进选区慢滚",
    };
    DrawTextW(
        dc,
        wide(hint).as_ptr(),
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
    let text = format!("已拼接 {rows} px", rows = rows);
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
            let outcome = match stitcher.push(frame) {
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
        assert!(matches!(stitcher.push(next), Pushed::Appended));
        assert_eq!(stitcher.height(), 260);
    }

    #[test]
    fn identical_frame_is_unchanged() {
        let first = frame(80, 200, 0);
        let mut stitcher = Stitcher::new(first);
        let same = frame(80, 200, 0);
        assert!(matches!(stitcher.push(same), Pushed::Unchanged));
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
        assert!(matches!(stitcher.push(noise), Pushed::Lost));
        assert_eq!(stitcher.height(), 200);
    }

    #[test]
    fn repeated_scrolls_keep_growing() {
        let mut stitcher = Stitcher::new(frame(60, 120, 0));
        for step in 1..=5 {
            let next = frame(60, 120, step * 40);
            assert!(matches!(stitcher.push(next), Pushed::Appended));
        }
        assert_eq!(stitcher.height(), 120 + 5 * 40);
    }



    #[test]
    fn scrolling_down_appends_exact_rows_without_duplicates() {
        // 文档 1500 行，窗口 400 行，一次滚 300 行：拼出来必须正好 1300 行且内容逐字节一致
        let doc_height = 1500;
        let window = 400;
        let step = 300;
        let doc = text_like(400, doc_height, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * 400 * 4) as usize;
            let len = (window * 400 * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(0));
        for round in 1..=3 {
            let next = crop(round * step);
            match stitcher.push(next) {
                Pushed::Appended => {}
                other => panic!("round {round} 没接上: {}", match other {
                    Pushed::Unchanged => "没滚动",
                    Pushed::Lost => "对不上",
                    _ => "?",
                }),
            }
        }
        assert_eq!(stitcher.height(), window + 3 * step);
        // 拼出来的整幅内容必须和文档的开头 window+3*step 行完全一样（有重复就对不上）
        let expect = &doc.pixels[..(stitcher.height() as usize * 400 * 4)];
        assert_eq!(stitcher.image().pixels, expect, "拼出来出现重复或缺行");
    }

    #[test]
    fn scrolling_up_prepends_rows() {
        for step in [19, 40, 113] {
            let first = text_like(600, 500, 400); // 文档第 400..900 行
            let mut stitcher = Stitcher::new(first);
            let up = text_like(600, 500, 400 - step); // 往上滚：上面多出 step 行
            match stitcher.push(up) {
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
    fn scroll_session_with_backtracking_matches_document() {
        // 一整段真实节奏：下去、回来一点、再下去、又回来……最后整幅必须和文档逐字节一致
        let width = 500;
        let window = 600;
        let doc = text_like(width, 2400, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * width * 4) as usize;
            let len = (window * width * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(0));
        let mut top = 0;
        let mut bottom = window;
        for step in [260, 90, -180, 300, -40, 120] {
            top += step;
            bottom = bottom.max(top + window);
            let _ = stitcher.push(crop(top));
            assert_eq!(stitcher.height(), bottom, "滚到 {top} 时长图高度不对");
            let expect = &doc.pixels[..(bottom as usize * width as usize * 4)];
            assert_eq!(stitcher.image().pixels, expect, "滚到 {top} 时内容重复/错行");
        }
        assert_eq!(stitcher.height(), 1150);
    }

    #[test]
    fn recovers_position_from_image_when_previous_frame_is_unusable() {
        // 中间掉了一帧：没有「上一帧」可比位移，只能去长图里对位置，把内容捞回来
        let width = 500;
        let window = 600;
        let doc = text_like(width, 2400, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * width * 4) as usize;
            let len = (window * width * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(0));
        assert!(matches!(stitcher.push(crop(300)), Pushed::Appended));
        assert_eq!(stitcher.height(), 900);

        // 上一帧没了：位移算不出来，但这帧其实和长图有 300 行重叠
        stitcher.prev = None;
        assert!(
            matches!(stitcher.push(crop(600)), Pushed::Appended),
            "应该靠长图把位置找回来"
        );
        assert_eq!(stitcher.height(), 1200);
        let expect = &doc.pixels[..(1200 * width * 4) as usize];
        assert_eq!(stitcher.image().pixels, expect, "掉帧后内容错位/漏行");
    }

    #[test]
    fn anchor_finds_row_and_rejects_foreign_frame() {
        let width = 500;
        let window = 600;
        let doc = text_like(width, 2400, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * width * 4) as usize;
            let len = (window * width * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(0));
        assert!(matches!(stitcher.push(crop(300)), Pushed::Appended)); // 长图 0..900

        for top in [0, 60, 240, 300] {
            let frame = crop(top);
            let sigs: Vec<[u8; SIG]> = (0..frame.height).map(|y| row_sig(&frame, y)).collect();
            let (found, ratio) = stitcher.anchor_top(&frame, &sigs).expect("应该找得到位置");
            assert_eq!(found, top, "回找的行号不对");
            assert!(ratio < 0.02, "差异率太高: {ratio}");
        }

        // 完全不搭界的一帧：必须拒绝，不能硬塞一个位置
        let mut foreign = crop(0);
        for (index, byte) in foreign.pixels.iter_mut().enumerate() {
            if index % 4 != 3 {
                *byte = ((index * 37 + 11) % 255) as u8;
            }
        }
        let sigs: Vec<[u8; SIG]> = (0..foreign.height).map(|y| row_sig(&foreign, y)).collect();
        assert!(stitcher.anchor_top(&foreign, &sigs).is_none(), "不该给无关帧找到位置");
    }

    #[test]
    fn scrolling_down_then_up_then_down_never_duplicates() {
        // 用户报的那个：往下滚 → 往上滚 → 再往下滚，图里出现两份同样的内容
        let width = 400;
        let window = 600;
        let doc = text_like(width, 2400, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * width * 4) as usize;
            let len = (window * width * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(0));
        for top in [300, 600, 900] {
            assert!(
                matches!(stitcher.push(crop(top)), Pushed::Appended),
                "往下滚到 {top} 应该接上"
            );
        }
        assert_eq!(stitcher.height(), 1500);

        // 往回滚：这段早拼过了，一行都不该再加
        assert!(
            matches!(stitcher.push(crop(600)), Pushed::Unchanged),
            "往回滚不该接新内容"
        );
        assert_eq!(stitcher.height(), 1500, "往回滚把内容拼重了");

        // 又滚回原处：还是不该变长
        assert!(
            matches!(stitcher.push(crop(900)), Pushed::Unchanged),
            "滚回原处不该接新内容"
        );
        assert_eq!(stitcher.height(), 1500, "滚回原处把内容拼重了");

        // 继续往下：只接新露出来的那 300 行
        assert!(matches!(stitcher.push(crop(1200)), Pushed::Appended));
        assert_eq!(stitcher.height(), 1800);

        // 整幅内容必须和文档 0..1800 行逐字节一致（重复、错行都会在这里露出来）
        let expect = &doc.pixels[..(stitcher.height() as usize * width as usize * 4)];
        assert_eq!(stitcher.image().pixels, expect, "来回滚之后内容重复/错行");
    }

    #[test]
    fn scrolling_up_past_top_only_adds_rows_above() {
        // 首帧从文档第 400 行起，往上滚 113 行：只该补 287..400 这 113 行
        let width = 600;
        let window = 500;
        let step = 113;
        let doc = text_like(width, 1200, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * width * 4) as usize;
            let len = (window * width * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(400));
        assert!(matches!(stitcher.push(crop(400 - step)), Pushed::Appended));
        assert_eq!(stitcher.height(), window + step);
        // 拼出来的应该是文档 287..1200 行：补在头上的 113 行 + 原来的 500 行
        let start = ((400 - step) as usize) * width as usize * 4;
        let end = start + (window + step) as usize * width as usize * 4;
        let expect = &doc.pixels[start..end];
        assert_eq!(stitcher.image().pixels, expect, "往上越顶之后头部内容不对");

        // 再往下一滚回 400：整段都在长图里了，不该再长
        assert!(matches!(stitcher.push(crop(400)), Pushed::Unchanged));
        assert_eq!(stitcher.height(), window + step, "滚回去又拼重了");
    }

    #[test]
    fn realistic_text_stitches_for_various_scroll_steps() {
        for step in [19, 37, 113, 240] {
            let first = text_like(600, 500, 0);
            let mut stitcher = Stitcher::new(first);
            let next = text_like(600, 500, step);
            match stitcher.push(next) {
                Pushed::Appended => {
                    assert_eq!(stitcher.height(), 500 + step, "step={step} 应该只接上 {step} 行");
                }
                _ => panic!("step={step} 没接上"),
            }
        }
    }

    #[test]
    fn seam_blend_mixes_only_the_overlap_band() {
        let width = 40;
        let image = Bitmap {
            width,
            height: 30,
            pixels: vec![100; (width * 30 * 4) as usize],
        };
        let mut stitcher = Stitcher::new(image);
        // 这一帧和长图末尾重叠 5 行，重叠的那几行是 200（模拟一点点错位）
        let frame = Bitmap {
            width,
            height: 30,
            pixels: vec![200; (width * 30 * 4) as usize],
        };
        stitcher.blend_seam(&frame, 5);

        let stride = (width * 4) as usize;
        let sample = |row: i32| stitcher.image().pixels[row as usize * stride] as i32;
        let (a, b, c) = (sample(27), sample(28), sample(29));
        assert!(a > 100 && b > a && c > b, "越靠近接缝越偏向新帧: {a} {b} {c}");
        assert!(c < 200, "接缝处不该整行被新帧替换");
        assert_eq!(sample(0), 100, "重叠带以外的行不能动");
        assert_eq!(sample(26), 100, "重叠带以外的行不能动");
    }

    #[test]
    fn canvas_budget_scales_with_width() {
        // 按字节预算：窄选区能拼得更长，宽选区短一些，但都至少 2000 行
        let narrow = Stitcher::new(text_like(200, 100, 0));
        let wide = Stitcher::new(text_like(2000, 100, 0));
        assert!(narrow.max_rows > wide.max_rows, "窄选区应该能拼更长");
        assert!(wide.max_rows >= 2000, "再宽也要留够 2000 行");
        assert!(
            wide.max_rows as i64 * 2000 * 4 <= MAX_CANVAS_BYTES + 2000 * 4,
            "宽选区的上限要落在字节预算内"
        );
    }

    #[test]
    fn cadence_adapts_to_cost_and_stays_in_range() {
        // 成本很低：一直贴着 30fps 的上限
        let mut interval = MIN_SCROLL_INTERVAL_US;
        for _ in 0..40 {
            interval = next_interval_us(interval, 500);
        }
        assert_eq!(interval, MIN_SCROLL_INTERVAL_US, "便宜的时候不该变慢");

        // 成本极高：退到最慢一档（平滑逼近，不会一步跳到位，也不会更慢）
        let mut interval = MIN_SCROLL_INTERVAL_US;
        for _ in 0..60 {
            interval = next_interval_us(interval, 10_000_000);
        }
        assert!(
            interval >= MAX_SCROLL_INTERVAL_US - 2 && interval <= MAX_SCROLL_INTERVAL_US,
            "贵的时候该退到 8fps 那一档，实际 {interval}"
        );

        // 中间档：成本和间隔大致成正比，且不会一下跳到底
        let slow = next_interval_us(MIN_SCROLL_INTERVAL_US, 60_000);
        assert!(slow > MIN_SCROLL_INTERVAL_US && slow < MAX_SCROLL_INTERVAL_US);
        assert_eq!(slow, (MIN_SCROLL_INTERVAL_US * 3 + 75_000) / 4);
    }

    #[test]
    fn stats_count_what_happened_to_each_frame() {
        let width = 400;
        let window = 600;
        let doc = text_like(width, 2400, 0);
        let crop = |top: i32| {
            let mut piece = doc.clone();
            let start = (top * width * 4) as usize;
            let len = (window * width * 4) as usize;
            piece.pixels = doc.pixels[start..start + len].to_vec();
            piece.height = window;
            piece
        };
        let mut stitcher = Stitcher::new(crop(0));
        assert!(matches!(stitcher.push(crop(300)), Pushed::Appended));
        assert!(matches!(stitcher.push(crop(600)), Pushed::Appended));
        assert!(matches!(stitcher.push(crop(600)), Pushed::Unchanged));
        assert_eq!(stitcher.stats.frames, 3);
        assert_eq!(stitcher.stats.appended, 2);
        assert_eq!(stitcher.stats.unchanged, 1);
        assert_eq!(stitcher.stats.lost, 0);
        assert_eq!(stitcher.stats.gap_rows, 0);
    }
}
