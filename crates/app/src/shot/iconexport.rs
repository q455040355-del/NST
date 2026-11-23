//! 工具栏图标导出：把每个按钮的自绘图标渲染成独立 PNG，方便拿去重新设计。
//!
//! 用法：`NST_ICONS_EXPORT=D:\某个目录 netspeed.exe`
//!
//! 产出的东西：
//! - `<序号>-<英文名>.png`：每个图标一张，白底深色线条，边长按导出倍率算；
//! - `contact-sheet.png`：所有图标排成一张表，下面写中文名，方便一眼对照；
//! - `names.txt`：序号 → 英文名 → 中文名，交给设计时对号入座。
//!
//! GDI 画图不写 alpha 通道，所以导出的是白底不透明图（要透明底请在设计工具里处理）。

use super::*;

/// 导出时把 `s()` 放大，线条才够粗（4 倍 ≈ 128px 级别的图标）
const EXPORT_SCALE: f32 = 4.0;

/// 工具栏里每个按钮的英文名（文件名用）和中文名（对照表用）
fn icon_specs() -> Vec<(Action, &'static str, &'static str)> {
    let mut specs = Vec::new();
    for (action, _separator) in TOOLBAR_ITEMS.iter().copied() {
        let (slug, title) = match action {
            Action::Tool(Tool::Rect) => ("rect", "矩形"),
            Action::Tool(Tool::Square) => ("square", "正方形"),
            Action::Tool(Tool::Ellipse) => ("ellipse", "椭圆"),
            Action::Tool(Tool::Line) => ("line", "直线"),
            Action::Tool(Tool::Arrow) => ("arrow", "箭头"),
            Action::Tool(Tool::Pen) => ("pen", "画笔"),
            Action::Tool(Tool::Marker) => ("marker", "高亮"),
            Action::Tool(Tool::Spotlight) => ("spotlight", "聚光灯"),
            Action::Tool(Tool::Numbered) => ("numbered", "序号"),
            Action::Tool(Tool::Mosaic) => ("mosaic", "马赛克"),
            Action::Tool(Tool::Eraser) => ("eraser", "橡皮"),
            Action::Tool(Tool::Text) => ("text", "文字"),
            Action::Tool(Tool::Ocr) => ("ocr", "OCR"),
            Action::Tool(Tool::Translate) => ("translate", "翻译"),
            Action::Scroll => ("scroll", "滚动截图"),
            Action::Rotate => ("rotate", "旋转"),
            Action::Beautify => ("beautify", "美化"),
            Action::Stroke => ("stroke", "浓淡"),
            Action::Undo => ("undo", "撤销"),
            Action::Redo => ("redo", "重做"),
            Action::Topmost => ("topmost", "置顶"),
            Action::Copy => ("copy", "复制"),
            Action::Save => ("save", "保存"),
            Action::Pin => ("pin", "贴图"),
            _ => continue,
        };
        specs.push((action, slug, title));
    }
    // 窗口栏右上角那三个也一起导出
    specs.push((Action::Minimize, "minimize", "最小化"));
    specs.push((Action::Close, "close", "关闭"));
    specs.push((Action::Done, "done", "完成"));
    specs
}

/// 把 DIB 存成 PNG（白底、不透明）
unsafe fn save_icon(bits: *mut u8, width: i32, height: i32, path: &std::path::Path) -> bool {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for index in 0..(width * height) as usize {
        let offset = index * 4;
        let b = *bits.add(offset);
        let g = *bits.add(offset + 1);
        let r = *bits.add(offset + 2);
        rgba.push(r);
        rgba.push(g);
        rgba.push(b);
        rgba.push(255);
    }
    match image::RgbaImage::from_raw(width as u32, height as u32, rgba) {
        Some(buffer) => buffer.save(path).is_ok(),
        None => false,
    }
}

/// 导出一整套图标到 `dir`
pub unsafe fn export(dir: &str) -> bool {
    let root = std::path::PathBuf::from(dir);
    if let Err(error) = std::fs::create_dir_all(&root) {
        crate::debug_log(&format!("icons export: 建目录失败 {error}"));
        return false;
    }

    // 放大 s()：图标线条、序号字都跟着放大
    let previous = icon_export_scale();
    set_icon_export_scale(EXPORT_SCALE);

    let specs = icon_specs();
    let cell = s(40.0).max(64);
    let ink = 0x0020_2024u32; // 0x00BBGGRR：深灰，浅底深底都看得清
    let mut names = String::from("# 序号\t文件\t中文名\n");
    let mut exported = 0usize;

    let mut sheet_cells: Vec<(image::RgbaImage, &'static str)> = Vec::new();

    for (index, (action, slug, title)) in specs.iter().enumerate() {
        let file = format!("{:02}-{}.png", index + 1, slug);
        let path = root.join(&file);

        let (dc, dib, bits) = crate::ui::make_dib(cell, cell);
        if dc.is_null() || bits.is_null() {
            crate::debug_log("icons export: DIB 创建失败");
            break;
        }
        // 白底：不用色键抠透明 —— 文字类图标（OCR / 翻译的 "A"）是用 GDI 画的字形，
        // 边缘和色键混在一起，抠完字母会缺一块。要透明底请在设计工具里处理。
        let brush = CreateSolidBrush(0x00FF_FFFF);
        let rect = RECT {
            left: 0,
            top: 0,
            right: cell,
            bottom: cell,
        };
        FillRect(dc, &rect, brush);
        DeleteObject(brush as *mut _);

        draw_action_icon(dc, *action, rect, ink, 1, true);

        let ok = save_icon(bits, cell, cell, &path);
        if ok {
            exported += 1;
            names.push_str(&format!("{:02}\t{file}\t{title}\n", index + 1));
            if let Ok(buffer) = image::open(&path) {
                sheet_cells.push((buffer.to_rgba8(), *title));
            }
        }
        DeleteObject(dib as *mut _);
        DeleteDC(dc);
    }

    write_contact_sheet(&root, sheet_cells, cell);
    let _ = std::fs::write(root.join("names.txt"), names);

    set_icon_export_scale(previous);
    crate::debug_log(&format!("icons export: {exported} 张 -> {}", root.display()));
    exported > 0
}

/// 排一张总表：图标 + 下面的中文名
unsafe fn write_contact_sheet(
    root: &std::path::Path,
    cells: Vec<(image::RgbaImage, &'static str)>,
    cell: i32,
) {
    if cells.is_empty() {
        return;
    }
    let per_row = 8usize;
    let label_h = s(10.0).max(16);
    let rows = cells.len().div_ceil(per_row);
    let width = cell * per_row as i32;
    let height = (cell + label_h) * rows as i32;

    let (dc, dib, bits) = crate::ui::make_dib(width, height);
    if dc.is_null() || bits.is_null() {
        return;
    }
    let brush = CreateSolidBrush(0x00FF_FFFF); // 白底
    let full = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    FillRect(dc, &full, brush);
    DeleteObject(brush as *mut _);

    let font = cached_font(8.0, 500);
    let old_font = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, 0x0020_2024);

    for (index, (image, title)) in cells.iter().enumerate() {
        let column = index % per_row;
        let row = index / per_row;
        let left = column as i32 * cell;
        let top = row as i32 * (cell + label_h);

        // 图标本体：直接用 AlphaBlend 贴上去（背景已经抠成透明）
        let mut buffer = Vec::with_capacity((cell * cell * 4) as usize);
        for y in 0..cell as u32 {
            for x in 0..cell as u32 {
                let pixel = image.get_pixel(x.min(image.width() - 1), y.min(image.height() - 1));
                buffer.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
        let bitmap_info = windows_sys::Win32::Graphics::Gdi::BITMAPINFO {
            bmiHeader: windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER {
                biSize: std::mem::size_of::<windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER>()
                    as u32,
                biWidth: cell,
                biHeight: -cell, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: (cell * cell * 4) as u32,
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(
            dc,
            &bitmap_info,
            windows_sys::Win32::Graphics::Gdi::DIB_RGB_COLORS,
            &mut bits_ptr,
            std::ptr::null_mut(),
            0,
        );
        if !bitmap.is_null() && !bits_ptr.is_null() {
            std::ptr::copy_nonoverlapping(buffer.as_ptr(), bits_ptr as *mut u8, buffer.len());
            let mem = CreateCompatibleDC(dc);
            let old = SelectObject(mem, bitmap as HGDIOBJ);
            let blend = BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: 1, // AC_SRC_ALPHA
            };
            AlphaBlend(dc, left, top, cell, cell, mem, 0, 0, cell, cell, blend);
            SelectObject(mem, old);
            DeleteDC(mem);
        }
        if !bitmap.is_null() {
            DeleteObject(bitmap as *mut _);
        }

        // 中文名
        let mut label: RECT = RECT {
            left,
            top: top + cell,
            right: left + cell,
            bottom: top + cell + label_h,
        };
        let units = wide(title);
        DrawTextW(
            dc,
            units.as_ptr(),
            (units.len() - 1) as i32,
            &mut label,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    SelectObject(dc, old_font);

    // 存图（这里的背景是白，不是品红，所以不走抠色）
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for index in 0..(width * height) as usize {
        let offset = index * 4;
        rgba.push(*bits.add(offset + 2));
        rgba.push(*bits.add(offset + 1));
        rgba.push(*bits.add(offset));
        rgba.push(255);
    }
    if let Some(buffer) = image::RgbaImage::from_raw(width as u32, height as u32, rgba) {
        let _ = buffer.save(root.join("contact-sheet.png"));
    }
    DeleteObject(dib as *mut _);
    DeleteDC(dc);
}
