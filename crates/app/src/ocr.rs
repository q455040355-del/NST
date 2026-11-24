//! PaddleOCR 取字：用 Apache-2.0 的 `simd-paddle-ocr-rs`（纯 Rust，无 ONNX Runtime）。
//!
//! 引擎在后台线程里现建现用（模型加载 ~17ms、识别 ~40ms），不阻塞 UI；
//! 模型文件放在 exe 旁边的 `models\`（或仓库根的 `models\`，开发时用）。

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::shot::Bitmap;

const CF_UNICODETEXT: u32 = 13;

pub struct OcrOutcome {
    pub text: String,
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
        candidates.push(cwd.join("models"));
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
            error: Some("找不到 models 目录（需要 det.onnx / cls.onnx / rec.onnx）".to_string()),
        };
    };

    let mut rgb = Vec::with_capacity((bitmap.width * bitmap.height * 3) as usize);
    for pixel in bitmap.pixels.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }

    let result = (|| -> Result<String, String> {
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

        Ok(normalize_newlines(output.text))
    })();

    match result {
        Ok(text) => OcrOutcome { text, error: None },
        Err(error) => OcrOutcome {
            text: String::new(),
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

    unsafe {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let size = wide.len() * 2;
        let handle = GlobalAlloc(GMEM_MOVEABLE, size);
        if handle.is_null() {
            return false;
        }

        let pointer = GlobalLock(handle) as *mut u8;
        if pointer.is_null() {
            GlobalFree(handle as *mut _);
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, pointer, size);
        GlobalUnlock(handle);

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            // 占用中：把句柄还回去，别泄漏
            GlobalFree(handle as *mut _);
            return false;
        }
        EmptyClipboard();
        let ok = !SetClipboardData(CF_UNICODETEXT, handle as *mut _).is_null();
        CloseClipboard();
        if !ok {
            GlobalFree(handle as *mut _);
        }
        ok
    }
}
