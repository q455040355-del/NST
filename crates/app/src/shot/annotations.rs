//! 标注绘制：箭头、矩形、椭圆、直线、画笔、高亮、序号、马赛克、聚光灯。
//!
//! 全部是「给定 DC + 图形参数就画」的纯绘制函数，不碰窗口状态。

use super::*;

pub(super) fn draw_arrow(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        MoveToEx(dc, a.0, a.1, std::ptr::null_mut());
        LineTo(dc, b.0, b.1);
        SelectObject(dc, old);

        let dx = (b.0 - a.0) as f32;
        let dy = (b.1 - a.1) as f32;
        let length = (dx * dx + dy * dy).sqrt().max(1.0);
        let (ux, uy) = (dx / length, dy / length);
        let head = (width as f32 * 4.5).max(10.0);
        let spread = 0.42;
        let p1 = (b.0, b.1);
        let p2 = (
            (b.0 as f32 - ux * head + uy * head * spread) as i32,
            (b.1 as f32 - uy * head - ux * head * spread) as i32,
        );
        let p3 = (
            (b.0 as f32 - ux * head - uy * head * spread) as i32,
            (b.1 as f32 - uy * head + ux * head * spread) as i32,
        );
        let brush = CreateSolidBrush(ANNOT);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let points = [POINT { x: p1.0, y: p1.1 }, POINT { x: p2.0, y: p2.1 }, POINT { x: p3.0, y: p3.1 }];
        Polygon(dc, points.as_ptr(), 3);
        SelectObject(dc, old_brush);
        DeleteObject(brush as *mut _);
        DeleteObject(pen as *mut _);
    }
}

pub(super) fn draw_rect(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(dc, brush);
        windows_sys::Win32::Graphics::Gdi::Rectangle(
            dc,
            a.0.min(b.0),
            a.1.min(b.1),
            a.0.max(b.0),
            a.1.max(b.1),
        );
        SelectObject(dc, old_brush);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }
}

pub(super) fn draw_ellipse(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(dc, brush);
        windows_sys::Win32::Graphics::Gdi::Ellipse(
            dc,
            a.0.min(b.0),
            a.1.min(b.1),
            a.0.max(b.0),
            a.1.max(b.1),
        );
        SelectObject(dc, old_brush);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }
}

pub(super) fn draw_pen(dc: HDC, points: &[(i32, i32)], width: i32, scale: f32) {    if points.len() < 2 {
        return;
    }
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        let pts: Vec<POINT> = points
            .iter()
            .map(|p| {
                let (x, y) = scaled(*p, scale);
                POINT { x, y }
            })
            .collect();
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }
}

pub(super) fn draw_line(dc: HDC, a: (i32, i32), b: (i32, i32), width: i32, scale: f32) {
    let (ax, ay) = scaled(a, scale);
    let (bx, by) = scaled(b, scale);
    unsafe {
        let pen = CreatePen(0, width, ANNOT);
        let old = SelectObject(dc, pen as HGDIOBJ);
        MoveToEx(dc, ax, ay, std::ptr::null_mut());
        LineTo(dc, bx, by);
        SelectObject(dc, old);
        DeleteObject(pen as *mut _);
    }
}

pub(super) fn draw_numbered(dc: HDC, at: (i32, i32), n: i32, scale: f32) {
    let (x, y) = scaled(at, scale);
    let r = (11.0 * scale).round().max(7.0) as i32;
    unsafe {
        let brush = CreateSolidBrush(NUMBER_RED);
        let pen = CreatePen(0, 1, NUMBER_RED);
        let old_b = SelectObject(dc, brush as HGDIOBJ);
        let old_p = SelectObject(dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::Ellipse(dc, x - r, y - r, x + r, y + r);
        SelectObject(dc, old_p);
        SelectObject(dc, old_b);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);

        let text = n.to_string();
        let font = cached_font(11.0 * scale, 700);
        let old_f = SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        let mut rect = RECT {
            left: x - r,
            top: y - r,
            right: x + r,
            bottom: y + r,
        };
        DrawTextW(
            dc,
            wide(&text).as_ptr(),
            -1,
            &mut rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_f);
    }
}

pub(super) unsafe fn draw_marker(dc: HDC, scale: f32, points: &[(i32, i32)], stroke: u8) {
    if points.len() < 2 {
        return;
    }
    let pen_width = ((9.0 * STROKE_FACTORS[stroke.min(2) as usize]).round() * scale)
        .max(4.0) as i32;
    // 坐标只用「图片坐标系」：实时预览那边已经用 SetViewportOrgEx 把原点挪到图片左上角了，
    // 这里再自己加 image_left/top 就会错位一倍（导出时 image_left=0，所以只有实时画面是歪的）
    let pts: Vec<POINT> = points
        .iter()
        .map(|p| {
            let (x, y) = scaled(*p, scale);
            POINT { x, y }
        })
        .collect();

    let half = pen_width / 2 + 1;
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for p in &pts {
        left = left.min(p.x - half);
        top = top.min(p.y - half);
        right = right.max(p.x + half);
        bottom = bottom.max(p.y + half);
    }
    if right - left < 1 || bottom - top < 1 {
        return;
    }
    let width = right - left;
    let height = bottom - top;

    // 掩码：白线画在黑色上
    let (mask_dc, mask_bmp, mask_bits) = make_dib(width, height);
    if mask_dc.is_null() || mask_bits.is_null() {
        if !mask_bmp.is_null() {
            DeleteObject(mask_bmp as *mut _);
        }
        if !mask_dc.is_null() {
            DeleteDC(mask_dc);
        }
        return;
    }
    let black = CreateSolidBrush(0);
    let area = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    FillRect(mask_dc, &area, black);
    DeleteObject(black as *mut _);
    let white = CreatePen(0, pen_width, 0x00FF_FFFF);
    let old_pen = SelectObject(mask_dc, white as HGDIOBJ);
    let shifted: Vec<POINT> = pts
        .iter()
        .map(|p| POINT {
            x: p.x - left,
            y: p.y - top,
        })
        .collect();
    Polyline(mask_dc, shifted.as_ptr(), shifted.len() as i32);
    SelectObject(mask_dc, old_pen);
    DeleteObject(white as *mut _);

    // 图层：掩码处填预乘过的黄色，alpha 约 42%
    const ALPHA: u32 = 108;
    let (layer_dc, layer_bmp, layer_bits) = make_dib(width, height);
    if layer_dc.is_null() || layer_bits.is_null() {
        if !layer_bmp.is_null() {
            DeleteObject(layer_bmp as *mut _);
        }
        if !layer_dc.is_null() {
            DeleteDC(layer_dc);
        }
        DeleteObject(mask_bmp as *mut _);
        DeleteDC(mask_dc);
        return;
    }
    std::ptr::write_bytes(layer_bits, 0, (width * height * 4) as usize);
    // 预乘：0xFF/0xE6/0x66 的黄 × alpha/255
    let pre = |v: u32| ((v * ALPHA) / 255) as u8;
    // BGR 顺序：黄是 R=0xFF G=0xE6 B=0x66
    let (b, g, r) = (pre(0x66), pre(0xE6), pre(0xFF));
    for y in 0..height {
        for x in 0..width {
            let m = *mask_bits.add(((y * width + x) * 4) as usize);
            if m == 0 {
                continue;
            }
            let pixel = layer_bits.add(((y * width + x) * 4) as usize);
            *pixel = b;
            *pixel.add(1) = g;
            *pixel.add(2) = r;
            *pixel.add(3) = ALPHA.min(m as u32) as u8;
        }
    }

    let blend = windows_sys::Win32::Graphics::Gdi::BLENDFUNCTION {
        BlendOp: windows_sys::Win32::Graphics::Gdi::AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: windows_sys::Win32::Graphics::Gdi::AC_SRC_ALPHA as u8,
    };
    windows_sys::Win32::Graphics::Gdi::AlphaBlend(
        dc, left, top, width, height, layer_dc, 0, 0, width, height, blend,
    );

    DeleteObject(layer_bmp as *mut _);
    DeleteDC(layer_dc);
    DeleteObject(mask_bmp as *mut _);
    DeleteDC(mask_dc);
}

/// 马赛克：别的 draw_* 都吃「图片坐标 + scale」，这里也一样——内部自己换算成显示像素。
/// （以前这个函数要调用方先 scaled() 再传，和其余工具不一致，很容易又踩一次「偏移算两遍」。）
pub(super) fn draw_mosaic(dc: HDC, base: &Bitmap, a: (i32, i32), b: (i32, i32), scale: f32, block: i32) {
    let (a, b) = (scaled(a, scale), scaled(b, scale));
    let left = a.0.min(b.0);
    let top = a.1.min(b.1);
    let right = a.0.max(b.0);
    let bottom = a.1.max(b.1);
    let block = block.max(4);
    if right <= left || bottom <= top {
        return;
    }

    let cols = (right - left + block - 1) / block;
    let rows = (bottom - top + block - 1) / block;
    if cols <= 0 || rows <= 0 {
        return;
    }

    // 一个像素 = 一个马赛克块：先算成一张极小的位图，再一次性放大贴上去。
    // 旧写法是每块 CreateSolidBrush + FillRect，拖大区域时每帧要建上千个 GDI 对象，卡就卡在这。
    let (src_dc, src_dib, src_bits) = make_dib(cols, rows);
    if src_dc.is_null() || src_dib.is_null() || src_bits.is_null() {
        unsafe {
            if !src_dc.is_null() {
                DeleteDC(src_dc);
            }
            if !src_dib.is_null() {
                DeleteObject(src_dib as *mut _);
            }
        }
        return;
    }

    for row in 0..rows {
        for col in 0..cols {
            let x = left + col * block;
            let y = top + row * block;

            // 取块内四个采样点的平均值，才像「马赛克」而不是噪点
            let mut sum = (0u32, 0u32, 0u32, 0u32);
            for (sx, sy) in [(0.25f32, 0.25f32), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let px = ((x as f32 + block as f32 * sx) / scale) as i32;
                let py = ((y as f32 + block as f32 * sy) / scale) as i32;
                if px < 0 || py < 0 || px >= base.width || py >= base.height {
                    continue;
                }
                let index = (py * base.width + px) as usize * 4;
                if index + 3 < base.pixels.len() {
                    sum.0 += base.pixels[index] as u32;
                    sum.1 += base.pixels[index + 1] as u32;
                    sum.2 += base.pixels[index + 2] as u32;
                    sum.3 += 1;
                }
            }

            let count = sum.3.max(1);
            let offset = (row * cols + col) as usize * 4;
            unsafe {
                // DIB 是 BGRA，和 Bitmap::pixels 的顺序一致
                *src_bits.add(offset) = (sum.0 / count) as u8;
                *src_bits.add(offset + 1) = (sum.1 / count) as u8;
                *src_bits.add(offset + 2) = (sum.2 / count) as u8;
                *src_bits.add(offset + 3) = 255;
            }
        }
    }

    unsafe {
        SetStretchBltMode(dc, COLORONCOLOR);
        StretchBlt(
            dc,
            left,
            top,
            right - left,
            bottom - top,
            src_dc,
            0,
            0,
            cols,
            rows,
            SRCCOPY,
        );
        DeleteObject(src_dib as *mut _);
        DeleteDC(src_dc);
    }
}

pub(super) unsafe fn draw_spotlight(dc: HDC, a: (i32, i32), b: (i32, i32), scale: f32, editor: &Editor) {
    let (ax, ay) = scaled(a, scale);
    let (bx, by) = scaled(b, scale);
    let (left, right) = (ax.min(bx), ax.max(bx));
    let (top, bottom) = (ay.min(by), ay.max(by));
    let width = (editor.base.width as f32 * scale) as i32;
    let height = (editor.base.height as f32 * scale) as i32;
    if width <= 0 || height <= 0 {
        return;
    }

    // 1x1 的黑：AlphaBlend 用 SourceConstantAlpha，会自动拉伸
    let (black_dc, black_bmp, bits) = make_dib(1, 1);
    if black_dc.is_null() || bits.is_null() {
        if !black_bmp.is_null() {
            DeleteObject(black_bmp as *mut _);
        }
        if !black_dc.is_null() {
            DeleteDC(black_dc);
        }
        return;
    }
    *bits = 0;
    *bits.add(1) = 0;
    *bits.add(2) = 0;
    *bits.add(3) = 255;

    let blend = windows_sys::Win32::Graphics::Gdi::BLENDFUNCTION {
        BlendOp: windows_sys::Win32::Graphics::Gdi::AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 115,
        AlphaFormat: 0,
    };
    let dim = |x: i32, y: i32, w: i32, h: i32| {
        if w <= 0 || h <= 0 {
            return;
        }
        windows_sys::Win32::Graphics::Gdi::AlphaBlend(
            dc, x, y, w, h, black_dc, 0, 0, 1, 1, blend,
        );
    };
    dim(0, 0, width, top.clamp(0, height));
    dim(0, bottom.clamp(0, height), width, (height - bottom).max(0));
    dim(0, top.clamp(0, height), left.clamp(0, width), (bottom - top).clamp(0, height));
    dim(right.clamp(0, width), top.clamp(0, height), (width - right).max(0), (bottom - top).clamp(0, height));

    DeleteObject(black_bmp as *mut _);
    DeleteDC(black_dc);
}

pub(super) fn draw_drag_frame(dc: HDC, a: (i32, i32), b: (i32, i32), scale: f32) {
    // 尺寸标签显示图片坐标（真实像素），框本身按显示缩放画
    let (left_px, top_px) = (a.0.min(b.0), a.1.min(b.1));
    let (right_px, bottom_px) = (a.0.max(b.0), a.1.max(b.1));
    let (left, top) = scaled((left_px, top_px), scale);
    let (right, bottom) = scaled((right_px, bottom_px), scale);
    if right - left < 1 || bottom - top < 1 {
        return;
    }

    unsafe {
        // 白色虚线：先在下面描一条深色，深浅背景都能看清
        for color in [0x0000_0000u32, 0x00FF_FFFFu32] {
            let pen = CreatePen(2, 1, color); // PS_DOT
            let old = SelectObject(dc, pen as HGDIOBJ);
            let inset = if color == 0 { 1 } else { 0 };
            MoveToEx(dc, left + inset, top + inset, std::ptr::null_mut());
            LineTo(dc, right - inset, top + inset);
            LineTo(dc, right - inset, bottom - inset);
            LineTo(dc, left + inset, bottom - inset);
            LineTo(dc, left + inset, top + inset);
            SelectObject(dc, old);
            DeleteObject(pen as *mut _);
        }

        // 尺寸标签
        let label = format!("{} × {}", right_px - left_px, bottom_px - top_px);
        let font = cached_font(9.0, 600);
        let old_font = SelectObject(dc, font as HGDIOBJ);
        let mut measure = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let units = wide(&label);
        // 先选字体再量，否则量的是上一个字体，标签会被切掉
        DrawTextW(
            dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut measure,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );

        let pad = (6.0 * scale).round().max(4.0) as i32;
        let label_h = (measure.bottom - measure.top).max(12) + pad / 2;
        let label_w = (measure.right - measure.left) + pad * 2 + (2.0 * scale) as i32;
        let chip_left = left;
        let chip_top = if top - label_h - 3 >= 0 {
            top - label_h - 3
        } else {
            top + 3
        };
        let brush = CreateSolidBrush(0x0022_1F1E); // 深灰
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let chip = RECT {
            left: chip_left,
            top: chip_top,
            right: chip_left + label_w,
            bottom: chip_top + label_h,
        };
        FillRect(dc, &chip, brush);
        SelectObject(dc, old_brush);
        DeleteObject(brush as *mut _);

        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        let mut text_rect = RECT {
            left: chip_left + pad,
            top: chip_top,
            right: chip_left + label_w - pad,
            bottom: chip_top + label_h,
        };
        DrawTextW(
            dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut text_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);
    }
}

pub(super) fn draw_shapes(dc: HDC, editor: &Editor, from: usize) {
    let scale = editor.scale;
    for shape in editor.shapes.iter().skip(from) {
        match shape {
            Shape::Arrow { a, b, stroke } => {
                draw_arrow(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Rect { a, b, stroke } => {
                draw_rect(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Square { a, b, stroke } => {
                draw_rect(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Ellipse { a, b, stroke } => {
                draw_ellipse(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Line { a, b, stroke } => {
                draw_line(dc, *a, *b, stroke_width(scale, *stroke), scale)
            }
            Shape::Marker { points, stroke } => unsafe {
                draw_marker(dc, scale, points, *stroke)
            },
            Shape::Spotlight { a, b } => unsafe { draw_spotlight(dc, *a, *b, scale, editor) },
            Shape::Numbered { at, n } => draw_numbered(dc, *at, *n, scale),
            Shape::Pen { points, stroke } => {
                draw_pen(dc, points, stroke_width(scale, *stroke), scale)
            }
            Shape::Text { at, text } => {
                // 字号也跟着显示缩放走，导出时 scale=1 不受影响
                let font = cached_font(12.0 * scale, 700);
                let (x, y) = scaled(*at, scale);
                unsafe {
                    let old = SelectObject(dc, font as HGDIOBJ);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, ANNOT);
                    TextOutW(dc, x, y, wide(text).as_ptr(), text.encode_utf16().count() as i32);
                    SelectObject(dc, old);
                }
            }
            Shape::Mosaic { a, b, stroke } => draw_mosaic(
                dc,
                &editor.base,
                *a,
                *b,
                scale,
                mosaic_block(scale, *stroke),
            ),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 高亮笔只在「图片坐标系」里画：实时预览靠 SetViewportOrgEx 把原点挪到图片左上角，
    /// 这里如果又自己加 image_left/top，整条笔迹就会错位一倍——导出时 image_left=0，
    /// 所以只有屏幕上的实时画面是歪的（用户报的就是这个）。
    #[test]
    fn marker_lands_at_image_coordinates() {
        unsafe {
            let (dc, bitmap, bits) = make_dib(400, 300);
            assert!(!dc.is_null() && !bits.is_null(), "测试需要 GDI 会话");
            std::ptr::write_bytes(bits, 0, 400 * 300 * 4);

            // 模拟「图片居中」：原点挪到 (40, 30)
            let mut old = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
            windows_sys::Win32::Graphics::Gdi::SetViewportOrgEx(dc, 40, 30, &mut old);

            draw_marker(dc, 1.0, &[(100, 100), (200, 100)], 1);

            let (mut left, mut top) = (i32::MAX, i32::MAX);
            let (mut right, mut bottom) = (i32::MIN, i32::MIN);
            for y in 0..300 {
                for x in 0..400 {
                    let index = ((y * 400 + x) * 4) as usize;
                    let sum = *bits.add(index) as i32
                        + *bits.add(index + 1) as i32
                        + *bits.add(index + 2) as i32;
                    if sum > 30 {
                        left = left.min(x);
                        top = top.min(y);
                        right = right.max(x);
                        bottom = bottom.max(y);
                    }
                }
            }
            assert!(left < right && top < bottom, "高亮笔什么都没画上去");

            // 期望：视口原点 (40,30) + 图片坐标 (100..200, 100)，笔宽 16 → 上下各 9px
            let near = |value: i32, expect: i32| (value - expect).abs() <= 8;
            assert!(near(left, 131), "左边应在 131 附近（视口 40 + 图片 100 − 半笔宽），实际 {left}");
            assert!(near(right, 249), "右边应在 249 附近，实际 {right}");
            assert!(near(top, 121), "上边应在 121 附近（视口 30 + 图片 100 − 半笔宽），实际 {top}");
            assert!(near(bottom, 139), "下边应在 139 附近，实际 {bottom}");

            DeleteObject(bitmap as _);
            DeleteDC(dc);
        }
    }

    /// 导出走的 composite：image_left=0、视口原点 (0,0)，同样的图片坐标应该原样落在原地。
    #[test]
    fn marker_export_maps_one_to_one() {
        unsafe {
            let (dc, bitmap, bits) = make_dib(400, 300);
            assert!(!dc.is_null() && !bits.is_null(), "测试需要 GDI 会话");
            std::ptr::write_bytes(bits, 0, 400 * 300 * 4);

            draw_marker(dc, 1.0, &[(100, 100), (200, 100)], 1);

            let (mut left, mut top) = (i32::MAX, i32::MAX);
            let (mut right, mut bottom) = (i32::MIN, i32::MIN);
            for y in 0..300 {
                for x in 0..400 {
                    let index = ((y * 400 + x) * 4) as usize;
                    let sum = *bits.add(index) as i32
                        + *bits.add(index + 1) as i32
                        + *bits.add(index + 2) as i32;
                    if sum > 30 {
                        left = left.min(x);
                        top = top.min(y);
                        right = right.max(x);
                        bottom = bottom.max(y);
                    }
                }
            }
            let near = |value: i32, expect: i32| (value - expect).abs() <= 8;
            assert!(near(left, 91), "导出左边应在 91 附近，实际 {left}");
            assert!(near(right, 209), "导出右边应在 209 附近，实际 {right}");
            assert!(near(top, 91), "导出上边应在 91 附近，实际 {top}");
            assert!(near(bottom, 109), "导出下边应在 109 附近，实际 {bottom}");

            DeleteObject(bitmap as _);
            DeleteDC(dc);
        }
    }

    // ---------------------------------------------------------------- 坐标约定
    // 所有标注都在「图片坐标系」里画，落点由调用方的视口原点（实时）或零原点（导出）决定。
    // 这一组测试把「视口 + 缩放」都设上，任何「又加了一次偏移」或「忘了乘缩放」都会当场翻车。

    const VIEW_X: i32 = 40;
    const VIEW_Y: i32 = 30;
    const CANVAS: i32 = 520;

    unsafe fn fresh() -> (HDC, HBITMAP, *mut u8) {
        let (dc, bitmap, bits) = make_dib(CANVAS, CANVAS);
        assert!(!dc.is_null() && !bits.is_null(), "测试需要 GDI 会话");
        std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
        let mut old = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        SetViewportOrgEx(dc, VIEW_X, VIEW_Y, &mut old);
        (dc, bitmap, bits)
    }

    unsafe fn painted(bits: *mut u8) -> Option<(i32, i32, i32, i32)> {
        let (mut left, mut top) = (i32::MAX, i32::MAX);
        let (mut right, mut bottom) = (i32::MIN, i32::MIN);
        for y in 0..CANVAS {
            for x in 0..CANVAS {
                let index = ((y * CANVAS + x) * 4) as usize;
                let sum = *bits.add(index) as i32
                    + *bits.add(index + 1) as i32
                    + *bits.add(index + 2) as i32;
                if sum > 30 {
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x);
                    bottom = bottom.max(y);
                }
            }
        }
        (left < right && top < bottom).then_some((left, top, right, bottom))
    }

    fn near(actual: (i32, i32, i32, i32), expect: (i32, i32, i32, i32), slack: i32) -> bool {
        (actual.0 - expect.0).abs() <= slack
            && (actual.1 - expect.1).abs() <= slack
            && (actual.2 - expect.2).abs() <= slack
            && (actual.3 - expect.3).abs() <= slack
    }

    /// 每个工具都只吃图片坐标：视口 (40,30) + 缩放 1.5 之后，落点必须是 40 + 1.5×图片坐标
    #[test]
    fn every_tool_respects_viewport_and_scale() {
        const SCALE: f32 = 1.5;
        // 图片坐标 (100,100)-(200,150) → 设备坐标 (190,180)-(340,255)
        let box_from = (100, 100);
        let box_to = (200, 150);
        let expected_box = (190, 180, 340, 255);
        let width = stroke_width(SCALE, 1);

        unsafe {
            let (dc, bitmap, bits) = fresh();

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_rect(dc, box_from, box_to, width, SCALE);
            let got = painted(bits).expect("矩形没画出来");
            assert!(near(got, expected_box, 6), "矩形错位: {got:?} 期望 {expected_box:?}");

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_ellipse(dc, box_from, box_to, width, SCALE);
            let got = painted(bits).expect("椭圆没画出来");
            assert!(near(got, expected_box, 6), "椭圆错位: {got:?} 期望 {expected_box:?}");

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_line(dc, (100, 100), (200, 100), width, SCALE);
            let got = painted(bits).expect("直线没画出来");
            assert!(
                near(got, (190, 180, 340, 180), 6),
                "直线错位: {got:?} 期望 (190,180,340,180)"
            );

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_arrow(dc, (100, 100), (200, 100), width, SCALE);
            let got = painted(bits).expect("箭头没画出来");
            // 箭头带个脑袋，纵向会宽一点；关键是起点/终点得落对
            assert!(
                (got.0 - 190).abs() <= 10 && (got.2 - 340).abs() <= 10,
                "箭头横向错位: {got:?}"
            );
            assert!(
                (got.1 - 180).abs() <= 24 && (got.3 - 180).abs() <= 24,
                "箭头纵向错位: {got:?}"
            );

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_numbered(dc, (100, 100), 1, SCALE);
            let got = painted(bits).expect("序号没画出来");
            let radius = (11.0 * SCALE).round() as i32;
            let expect = (
                190 - radius,
                180 - radius,
                190 + radius,
                180 + radius,
            );
            assert!(near(got, expect, 6), "序号错位: {got:?} 期望 {expect:?}");

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_pen(dc, &[(100, 100), (150, 130), (200, 100)], width.max(6), SCALE);
            let got = painted(bits).expect("画笔没画出来");
            assert!(
                near(got, (190, 180, 340, 225), 10),
                "画笔错位: {got:?} 期望 (190,180,340,225) 附近"
            );

            std::ptr::write_bytes(bits, 0, (CANVAS * CANVAS * 4) as usize);
            draw_marker(dc, SCALE, &[(100, 100), (200, 100)], 1);
            let got = painted(bits).expect("高亮笔没画出来");
            let half = ((9.0 * 1.8f32).round() * SCALE).max(4.0) as i32 / 2 + 1;
            let expect = (
                190 - half,
                180 - half,
                340 + half,
                180 + half,
            );
            assert!(near(got, expect, 6), "高亮笔错位: {got:?} 期望 {expect:?}");

            DeleteObject(bitmap as _);
            DeleteDC(dc);
        }
    }
    /// 聚光灯压暗的区域必须正好是「框外」：框里保持原样，框外变暗。
    /// 底子先铺成灰色——黑底上压黑是看不出来的（这也提醒以后别用纯黑画布测这类效果）。
    #[test]
    fn spotlight_dims_outside_the_box_only() {
        unsafe {
            let (dc, bitmap, bits) = fresh();
            for index in 0..(CANVAS * CANVAS) as usize {
                *bits.add(index * 4) = 128;
                *bits.add(index * 4 + 1) = 128;
                *bits.add(index * 4 + 2) = 128;
            }

            let mut editor = Editor::default();
            editor.base = Bitmap::new(300, 200);
            editor.scale = 1.5;

            draw_spotlight(dc, (100, 100), (200, 150), 1.5, &editor);

            let gray = |x: i32, y: i32| -> i32 {
                let index = ((y * CANVAS + x) * 4) as usize;
                *bits.add(index) as i32
            };
            // 框内：设备 (250,220) ≈ 图片 (140,127)
            assert!(gray(250, 220) > 100, "框里不该被压暗（实际 {}）", gray(250, 220));
            // 框外：设备 (60,60) ≈ 图片 (13,20)
            assert!(gray(60, 60) < 100, "框外左上应该被压暗（实际 {}）", gray(60, 60));
            // 框外右侧：设备 (500,220) ≈ 图片 (306,127)
            assert!(gray(430, 220) < 100, "框外右侧应该被压暗（实际 {}）", gray(430, 220));

            DeleteObject(bitmap as _);
            DeleteDC(dc);
        }
    }

    /// 马赛克只改动框里的像素：框外原样、框里被抹平。
    /// 底图用黑白棋盘格——马赛克之后相邻像素应该几乎一样，框外还是硬边。
    #[test]
    fn mosaic_sits_on_the_box_and_leaves_outside_alone() {
        unsafe {
            let (dc, bitmap, bits) = fresh();

            let mut base = Bitmap::new(300, 200);
            for y in 0..200i32 {
                for x in 0..300i32 {
                    let index = ((y * 300 + x) * 4) as usize;
                    let value = if (x + y) % 2 == 0 { 0u8 } else { 255u8 };
                    base.pixels[index] = value;
                    base.pixels[index + 1] = value;
                    base.pixels[index + 2] = value;
                    base.pixels[index + 3] = 255;
                }
            }
            // 画布上铺同一张底图（视口原点 + 1:1 取样）
            for y in 0..CANVAS {
                for x in 0..CANVAS {
                    let (ix, iy) = (x - VIEW_X, y - VIEW_Y);
                    if ix >= 0 && iy >= 0 && ix < 300 && iy < 200 {
                        let index = ((y * CANVAS + x) * 4) as usize;
                        let src = ((iy * 300 + ix) * 4) as usize;
                        *bits.add(index) = base.pixels[src];
                        *bits.add(index + 1) = base.pixels[src + 1];
                        *bits.add(index + 2) = base.pixels[src + 2];
                    }
                }
            }

            // 图片坐标 (100,100)-(200,150) → 设备 (190,180)-(340,255)
            draw_mosaic(dc, &base, (100, 100), (200, 150), 1.5, 9);

            let red = |x: i32, y: i32| -> i32 {
                let index = ((y * CANVAS + x) * 4) as usize;
                *bits.add(index + 2) as i32
            };

            // 框外（设备 (60,60)）仍是棋盘格的硬边：左右相邻像素一个黑一个白
            let (a, b) = (red(60, 60), red(61, 60));
            assert!((a - b).abs() > 150, "框外不该被马赛克碰到（{a} vs {b}）");

            // 框里已经被抹平：横着 8 个像素的值都挤在中间，没有黑白硬边
            for x in 200..208 {
                assert!(
                    (40..=215).contains(&red(x, 200)),
                    "框里应该是平均色，实际 ({x},200) = {}",
                    red(x, 200)
                );
            }

            DeleteObject(bitmap as _);
            DeleteDC(dc);
        }
    }
}
