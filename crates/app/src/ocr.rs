//! PaddleOCR 取字：用 Apache-2.0 的 `simd-paddle-ocr-rs`（纯 Rust，无 ONNX Runtime）。
//!
//! 引擎在后台线程里现建现用（模型加载 ~17ms、识别 ~40ms），不阻塞 UI；
//! 模型文件放在 exe 旁边的 `models\`（或仓库根的 `models\`，开发时用）。

use std::path::PathBuf;

use netspeed_core::table::Table;
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::shot::Bitmap;
use crate::theme::wide;

const CF_UNICODETEXT: u32 = 13;

pub struct OcrOutcome {
    pub text: String,
    /// 几何上是表格时，按行列还原出来的网格（没有就是普通文字）
    pub table: Option<Table>,
    pub error: Option<String>,
}

/// 找模型目录：exe 同级 models → 仓库根 models（target/release 往上两级）→ 当前目录 models。
fn models_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("models"));
            if let Some(up) = dir.parent().and_then(|p| p.parent()) {
                candidates.push(up.join("models"));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        // 开发 / 跑测试时 cwd 可能是仓库根，也可能是 crates\app，往上找几级
        for ancestor in cwd.ancestors().take(4) {
            candidates.push(ancestor.join("models"));
        }
    }

    for candidate in candidates {
        if candidate.join("det.onnx").is_file() && candidate.join("rec.onnx").is_file() {
            return Some(candidate);
        }
    }
    None
}

/// EDIT 控件和大多数 Windows 程序都按 CRLF 换行，识别结果是 LF，这里统一一下。
fn normalize_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                out.push_str("\r\n");
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => out.push_str("\r\n"),
            _ => out.push(ch),
        }
    }
    out
}

/// 同步识别：翻译那条链路要的是纯文本，不要 Outcome。
pub fn recognize_pub(bitmap: &Bitmap) -> Result<String, String> {
    let outcome = recognize(bitmap);
    match outcome.error {
        Some(error) => Err(error),
        None => Ok(outcome.text),
    }
}

fn recognize(bitmap: &Bitmap) -> OcrOutcome {
    let Some(dir) = models_dir() else {
        return OcrOutcome {
            text: String::new(),
            table: None,
            error: Some("找不到 models 目录（需要 det.onnx / cls.onnx / rec.onnx）".to_string()),
        };
    };

    let mut rgb = Vec::with_capacity((bitmap.width * bitmap.height * 3) as usize);
    for pixel in bitmap.pixels.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }

    let result = (|| -> Result<(String, Option<Table>), String> {
        use simd_paddle_ocr_rs::image::{ImageView, PixelFormat};
        use simd_paddle_ocr_rs::{ModelPreset, OcrModelSet, RunOptions};

        let models = OcrModelSet::from_directory(&dir, ModelPreset::ChineseV6Tiny, true)
            .map_err(|err| format!("加载模型失败：{err}"))?;
        let plan = models.builder().build().map_err(|err| format!("构建推理图失败：{err}"))?;
        let mut request = plan.request().map_err(|err| format!("创建会话失败：{err}"))?;

        let view = ImageView::new(
            &rgb,
            bitmap.width as usize,
            bitmap.height as usize,
            0,
            PixelFormat::Rgb24,
        )
        .map_err(|err| format!("图像格式错误：{err}"))?;

        let output = request
            .run(view, RunOptions::default())
            .map_err(|err| format!("识别失败：{err}"))?;

        // 引擎给的是「一行文字 + 四角坐标」；表格靠这些框子还原
        let mut boxes = Vec::with_capacity(output.len());
        for line in output.iter() {
            let points = line.detection.points;
            let left = points.iter().map(|point| point[0]).fold(f32::INFINITY, f32::min);
            let top = points.iter().map(|point| point[1]).fold(f32::INFINITY, f32::min);
            let right = points.iter().map(|point| point[0]).fold(f32::NEG_INFINITY, f32::max);
            let bottom = points.iter().map(|point| point[1]).fold(f32::NEG_INFINITY, f32::max);
            boxes.push(netspeed_core::table::LineBox {
                text: line.text.to_string(),
                left,
                top,
                right,
                bottom,
            });
        }
        let table = netspeed_core::table::detect(&boxes);

        Ok((normalize_newlines(output.text), table))
    })();

    match result {
        Ok((text, table)) => OcrOutcome {
            text,
            table,
            error: None,
        },
        Err(error) => OcrOutcome {
            text: String::new(),
            table: None,
            error: Some(error),
        },
    }
}

/// 在后台线程识别，完成后把结果投递回 `owner`（`WM_APP_OCR_READY`，wparam 是 Box<OcrOutcome> 指针）。
pub fn recognize_async(owner: HWND, bitmap: Bitmap) {
    let handle = owner as usize;
    std::thread::spawn(move || {
        let outcome = recognize(&bitmap);
        let payload = Box::into_raw(Box::new(outcome));
        unsafe {
            let _ok = PostMessageW(handle as HWND, crate::WM_APP_OCR_READY, payload as usize, 0);
        }
    });
}

/// 把文字写进剪贴板（CF_UNICODETEXT）。**返回是否真的写进去了**：
/// 剪贴板被别的程序占着时 OpenClipboard 会失败，调用方不能一律说「已复制」。
pub fn copy_text(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let bytes: Vec<u8> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    unsafe { set_clipboard(&[(CF_UNICODETEXT, bytes)]) }
}

/// 按表格复制：CF_UNICODETEXT 里是 TSV（Excel / WPS / Word 表格里贴进去自动分格），
/// 再加一份 "HTML Format"（粘到 Word / 网页里是真正的表格）。
/// 谁先谁后由目标程序自己挑，两个格式说的是同一张表。
pub fn copy_table(table: &Table) -> bool {
    let text = table.tsv();
    if text.is_empty() {
        return false;
    }
    let text_bytes: Vec<u8> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    let html = html_document(table);

    let html_format = unsafe { RegisterClipboardFormatW(wide("HTML Format").as_ptr()) };
    if html_format == 0 {
        return unsafe { set_clipboard(&[(CF_UNICODETEXT, text_bytes)]) };
    }
    unsafe { set_clipboard(&[(CF_UNICODETEXT, text_bytes), (html_format, html)]) }
}

/// CF_HTML 要求的数据块：开头一段固定宽度的偏移头，后面跟 UTF-8 的 HTML。
/// 偏移全是 10 位补零，所以可以先用占位串量长度再回填。
fn html_document(table: &Table) -> Vec<u8> {
    let prefix = "<html><head><meta charset=\"utf-8\"></head><body>\r\n<!--StartFragment-->\r\n";
    let suffix = "\r\n<!--EndFragment-->\r\n</body></html>";
    let fragment = table.html();

    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\n\
                      StartFragment:0000000000\r\nEndFragment:0000000000\r\n"
        .len();
    let start_html = header_len;
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + suffix.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
         StartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    debug_assert_eq!(header.len(), header_len, "偏移头长度必须和占位串一致");

    let mut out = Vec::with_capacity(end_html);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(prefix.as_bytes());
    out.extend_from_slice(fragment.as_bytes());
    out.extend_from_slice(suffix.as_bytes());
    out
}

/// 一次写多个格式：任何一个没写进去都算失败（调用方据此改状态栏文案）
unsafe fn set_clipboard(entries: &[(u32, Vec<u8>)]) -> bool {
    if OpenClipboard(std::ptr::null_mut()) == 0 {
        return false;
    }
    EmptyClipboard();

    let mut all_ok = true;
    for (format, bytes) in entries {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if handle.is_null() {
            all_ok = false;
            continue;
        }
        let pointer = GlobalLock(handle) as *mut u8;
        if pointer.is_null() {
            GlobalFree(handle as *mut _);
            all_ok = false;
            continue;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        GlobalUnlock(handle);
        if SetClipboardData(*format, handle as *mut _).is_null() {
            GlobalFree(handle as *mut _);
            all_ok = false;
        }
    }

    CloseClipboard();
    all_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use netspeed_core::table::{detect, LineBox};

    fn sample_table() -> Table {
        let lines = vec![
            LineBox { text: "姓名".into(), left: 10.0, top: 10.0, right: 60.0, bottom: 26.0 },
            LineBox { text: "年龄".into(), left: 100.0, top: 10.0, right: 150.0, bottom: 26.0 },
            LineBox { text: "城市".into(), left: 200.0, top: 10.0, right: 250.0, bottom: 26.0 },
            LineBox { text: "张三".into(), left: 10.0, top: 40.0, right: 60.0, bottom: 56.0 },
            LineBox { text: "28".into(), left: 100.0, top: 40.0, right: 150.0, bottom: 56.0 },
            LineBox { text: "杭州".into(), left: 200.0, top: 40.0, right: 250.0, bottom: 56.0 },
        ];
        detect(&lines).expect("样例本身就是表格")
    }

    #[test]
    fn cf_html_offsets_point_at_the_fragment() {
        let table = sample_table();
        let document = html_document(&table);
        let text = String::from_utf8(document.clone()).expect("CF_HTML 是 UTF-8");

        let offset = |name: &str| -> usize {
            let key = format!("{name}:");
            let start = text.find(&key).expect("偏移头里有这个字段") + key.len();
            text[start..start + 10].parse().expect("偏移是 10 位数字")
        };
        let start_html = offset("StartHTML");
        let end_html = offset("EndHTML");
        let start_fragment = offset("StartFragment");
        let end_fragment = offset("EndFragment");

        assert_eq!(&text[start_html..start_html + 6], "<html>");
        assert!(text.ends_with("</body></html>"), "文档尾部对不上");
        assert_eq!(end_html, document.len(), "EndHTML 应该指向文档末尾");
        assert_eq!(&document[start_fragment..end_fragment], table.html().as_bytes());
    }

    #[test]
    #[ignore = "会占用真实剪贴板，手动跑：cargo test -p netspeed-app clipboard_has_table_formats -- --ignored"]
    fn clipboard_has_table_formats() {
        use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;

        let table = sample_table();
        assert!(copy_table(&table), "写剪贴板失败（可能被别的程序占着）");
        unsafe {
            assert_ne!(IsClipboardFormatAvailable(CF_UNICODETEXT), 0, "缺文本格式");
            let html = RegisterClipboardFormatW(wide("HTML Format").as_ptr());
            assert_ne!(IsClipboardFormatAvailable(html), 0, "缺 HTML Format");
        }
    }

    /// 真模型 + GDI：画一张三列三行的表，看整条链路能不能还原出来
    #[test]
    #[ignore = "要加载 OCR 模型，手动跑：cargo test -p netspeed-app ocr_rebuilds_a_rendered_table -- --ignored --nocapture"]
    fn ocr_rebuilds_a_rendered_table() {
        let bitmap = render_table();
        let outcome = recognize(&bitmap);
        assert!(outcome.error.is_none(), "识别失败：{:?}", outcome.error);
        let table = outcome.table.expect("这张图应该被当成表格");
        assert_eq!(table.cols(), 3, "列数不对：{}", table.tsv());
        assert_eq!(table.rows(), 3, "行数不对：{}", table.tsv());
        eprintln!("{}", table.tsv());
    }

    fn render_table() -> crate::shot::Bitmap {
        use windows_sys::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
            DeleteObject, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
            TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, TRANSPARENT,
        };

        const W: i32 = 460;
        const H: i32 = 170;
        const CELLS: [[&str; 3]; 3] = [
            ["Name", "Age", "City"],
            ["Ann", "28", "Rome"],
            ["Bob", "31", "Oslo"],
        ];

        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let dc = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H, // 自顶向下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            assert!(!dib.is_null() && !bits.is_null(), "建 DIB 失败");
            let old_bitmap = SelectObject(dc, dib);

            let white = CreateSolidBrush(0x00FF_FFFF);
            let mut full = windows_sys::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: W,
                bottom: H,
            };
            FillRect(dc, &mut full, white);
            DeleteObject(white);

            let font = CreateFontW(
                -30,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                1,
                0,
                0,
                0,
                0,
                wide("Microsoft YaHei").as_ptr(),
            );
            let old_font = SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x0000_0000);

            for (row, texts) in CELLS.iter().enumerate() {
                for (col, text) in texts.iter().enumerate() {
                    let units: Vec<u16> = text.encode_utf16().collect();
                    TextOutW(
                        dc,
                        30 + col as i32 * 150,
                        20 + row as i32 * 46,
                        units.as_ptr(),
                        units.len() as i32,
                    );
                }
            }

            let length = (W * H * 4) as usize;
            let pixels = std::slice::from_raw_parts(bits as *const u8, length).to_vec();

            SelectObject(dc, old_font);
            DeleteObject(font);
            SelectObject(dc, old_bitmap);
            DeleteObject(dib);
            DeleteDC(dc);
            ReleaseDC(std::ptr::null_mut(), screen);

            crate::shot::Bitmap {
                width: W,
                height: H,
                pixels,
            }
        }
    }
}
