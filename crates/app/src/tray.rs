//! 托盘/窗口图标：程序自己算像素画一个 32×32 的 logo。
//!
//! 设计：圆角深蓝底 + 一上一下两个箭头（绿色 ↑ 上行、蓝色 ↓ 下行），
//! 也就是面板上和指示条上用的那两套颜色 —— 一眼能看出是「实时网速」。
//!
//! 没用 GDI 画形状：GDI 没有抗锯齿，缩到 16px 时箭头会毛边。
//! 这里按 8×8 超采样算覆盖率，直接写 32bpp BGRA，圆角外的像素透明度 0，
//! 再用单色 mask 把透明处抠掉。

use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    BITMAPINFO, BITMAPINFOHEADER,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICONINFO,
};

/// 每个像素 8×8 超采样（抗锯齿够用，启动时只算一次）
const SS: i32 = 8;

// ---------------------------------------------------------------- 设计参数（0..1）
/// 圆角方块离画布边缘的留白
const INSET: f64 = 0.045;
const RADIUS: f64 = 0.235;
/// 箭头的线宽
const STROKE: f64 = 0.105;

const UP: [(f64, f64); 3] = [(0.305, 0.415), (0.50, 0.225), (0.695, 0.415)];
const DOWN: [(f64, f64); 3] = [(0.305, 0.585), (0.50, 0.775), (0.695, 0.585)];

/// #2B394F → #121A28
const BG_TOP: (f64, f64, f64) = (0x2B as f64, 0x39 as f64, 0x4F as f64);
const BG_BOT: (f64, f64, f64) = (0x12 as f64, 0x1A as f64, 0x28 as f64);
/// #54DA98 上行绿
const UP_COLOR: (f64, f64, f64) = (0x54 as f64, 0xDA as f64, 0x98 as f64);
/// #5AB2FF 下行蓝
const DOWN_COLOR: (f64, f64, f64) = (0x5A as f64, 0xB2 as f64, 0xFF as f64);

fn seg_dist(px: f64, py: f64, a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let mut t = 0.0;
    if len2 > 0.0 {
        t = ((px - a.0) * dx + (py - a.1) * dy) / len2;
        t = t.clamp(0.0, 1.0);
    }
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt()
}

fn chevron_dist(px: f64, py: f64, points: [(f64, f64); 3]) -> f64 {
    seg_dist(px, py, points[0], points[1]).min(seg_dist(px, py, points[1], points[2]))
}

fn in_badge(px: f64, py: f64) -> bool {
    let (left, top, right, bottom) = (INSET, INSET, 1.0 - INSET, 1.0 - INSET);
    if px < left || px > right || py < top || py > bottom {
        return false;
    }
    // 只有落进四个角的圆弧内才需要用半径判断
    let mut dx = 0.0;
    let mut dy = 0.0;
    if px < left + RADIUS {
        dx = left + RADIUS - px;
    } else if px > right - RADIUS {
        dx = px - (right - RADIUS);
    }
    if py < top + RADIUS {
        dy = top + RADIUS - py;
    } else if py > bottom - RADIUS {
        dy = py - (bottom - RADIUS);
    }
    if dx == 0.0 || dy == 0.0 {
        return true;
    }
    (dx * dx + dy * dy).sqrt() <= RADIUS
}

/// 算出一整张 32bpp BGRA（自上而下）像素表
fn render(size: i32) -> Vec<u8> {
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let samples = (SS * SS) as f64;
    let half_stroke = STROKE / 2.0;

    for y in 0..size {
        for x in 0..size {
            let mut r = 0.0;
            let mut g = 0.0;
            let mut b = 0.0;
            let mut cover = 0.0;

            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f64 + (sx as f64 + 0.5) / SS as f64) / size as f64;
                    let py = (y as f64 + (sy as f64 + 0.5) / SS as f64) / size as f64;
                    if !in_badge(px, py) {
                        continue;
                    }

                    // 竖直渐变底色
                    let t = ((py - INSET) / (1.0 - 2.0 * INSET)).clamp(0.0, 1.0);
                    let mut cr = BG_TOP.0 + (BG_BOT.0 - BG_TOP.0) * t;
                    let mut cg = BG_TOP.1 + (BG_BOT.1 - BG_TOP.1) * t;
                    let mut cb = BG_TOP.2 + (BG_BOT.2 - BG_TOP.2) * t;

                    if chevron_dist(px, py, UP) <= half_stroke {
                        cr = UP_COLOR.0;
                        cg = UP_COLOR.1;
                        cb = UP_COLOR.2;
                    } else if chevron_dist(px, py, DOWN) <= half_stroke {
                        cr = DOWN_COLOR.0;
                        cg = DOWN_COLOR.1;
                        cb = DOWN_COLOR.2;
                    }

                    r += cr;
                    g += cg;
                    b += cb;
                    cover += 1.0;
                }
            }

            let index = ((y * size + x) * 4) as usize;
            if cover == 0.0 {
                continue;
            }
            pixels[index] = (b / cover).round() as u8;
            pixels[index + 1] = (g / cover).round() as u8;
            pixels[index + 2] = (r / cover).round() as u8;
            pixels[index + 3] = (cover / samples * 255.0).round() as u8;
        }
    }

    pixels
}

/// 单色 mask：透明处为 1（bit 顺序和 GDI 一致，每行按字节对齐）
fn render_mask(pixels: &[u8], size: i32) -> Vec<u8> {
    let stride = ((size + 7) / 8) as usize;
    let mut mask = vec![0u8; stride * size as usize];
    for y in 0..size {
        for x in 0..size {
            let alpha = pixels[((y * size + x) * 4 + 3) as usize];
            if alpha == 0 {
                let byte = (y as usize) * stride + (x as usize) / 8;
                mask[byte] |= 0x80 >> (x % 8);
            }
        }
    }
    mask
}

/// 图标尺寸：窗口图标用 32，托盘按系统的小图标尺寸出图
const DEFAULT_SIZE: i32 = 32;

pub fn create_icon() -> HICON {
    create_at(DEFAULT_SIZE)
}

/// 托盘图标：按 `SM_CXSMICON` 出图。
///
/// 以前只有 32×32 一张，150% 缩放时托盘要 24×24，靠系统缩放会糊；
/// 现在直接按目标尺寸算像素（设计坐标是 0..1 归一化的，换尺寸不用改参数）。
pub fn create_tray_icon() -> HICON {
    let size = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
        )
    };
    create_at(size.clamp(16, DEFAULT_SIZE))
}

fn create_at(size: i32) -> HICON {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return std::ptr::null_mut();
        }

        let dc = CreateCompatibleDC(screen);

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = size;
        info.bmiHeader.biHeight = -size; // top-down
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = 0;

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        if color.is_null() || bits.is_null() {
            DeleteDC(dc);
            ReleaseDC(std::ptr::null_mut(), screen);
            return std::ptr::null_mut();
        }

        let pixels = render(size);
        std::ptr::copy_nonoverlapping(
            pixels.as_ptr(),
            bits as *mut u8,
            pixels.len(),
        );

        let mask_bits = render_mask(&pixels, size);
        let mask = CreateBitmap(
            size,
            size,
            1,
            1,
            mask_bits.as_ptr() as *const std::ffi::c_void,
        );

        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&icon_info);

        if !mask.is_null() {
            DeleteObject(mask as *mut _);
        }
        DeleteObject(color as *mut _);
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);

        icon
    }
}

pub fn destroy_icon(icon: HICON) {
    if !icon.is_null() {
        unsafe {
            DestroyIcon(icon);
        }
    }
}
