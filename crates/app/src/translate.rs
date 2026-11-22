//! 截图翻译：本地 OCR 出文字 → 只把**文字**发给 OpenAI / DeepSeek 的 chat/completions。
//!
//! 两家都是 OpenAI 兼容接口，所以只有 base_url 和默认模型不同；
//! 截图本身不出本机，key 只存在本地 settings.ini。

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::shot::Bitmap;

pub struct TranslateOutcome {
    pub source: String,
    pub translated: String,
    pub error: Option<String>,
}

/// 把「服务商 + 模型 + base」统一成实际请求要用的三样东西
fn endpoint(settings: &netspeed_core::settings::Settings) -> (String, String) {
    let provider = settings.translate_provider.trim().to_lowercase();
    let (default_base, default_model) = if provider == "deepseek" {
        ("https://api.deepseek.com/v1", "deepseek-chat")
    } else {
        ("https://api.openai.com/v1", "gpt-4o-mini")
    };
    let base = settings.translate_base.trim();
    let base = if base.is_empty() { default_base } else { base };
    let model = settings.translate_model.trim();
    let model = if model.is_empty() { default_model } else { model };
    (base.trim_end_matches('/').to_string(), model.to_string())
}

/// 目标语言：中文→英文，其它→中文（够用，先不做下拉）
fn target_language(text: &str) -> &'static str {
    let chinese = text
        .chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
        .count();
    let letters = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    if chinese * 2 >= letters.max(1) {
        "English"
    } else {
        "Chinese"
    }
}

fn prompt(text: &str) -> String {
    format!(
        "Translate the following text into {}. Keep the line breaks and the original order. \
Only output the translation, no explanations, no quotes.\n\n{}",
        target_language(text),
        text
    )
}

/// WinHttp POST（https），返回响应体
fn http_post(url: &str, headers: &str, body: &[u8]) -> Result<String, String> {
    use windows_sys::Win32::Networking::WinHttp::*;
    use std::ptr::null_mut;

    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "只支持 https 地址".to_string())?;
    let (host, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };

    let wide = |value: &str| -> Vec<u16> { value.encode_utf16().chain(std::iter::once(0)).collect() };
    let agent = wide("NetSpeedTaskbar-Translate");
    let host_w = wide(host);
    let path_w = wide(path);
    let post = wide("POST");
    let headers_w = wide(headers);

    unsafe {
        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            null_mut(),
            null_mut(),
            0,
        );
        if session.is_null() {
            return Err("无法初始化网络会话".to_string());
        }
        // 别让界面卡在一个不回的请求上
        WinHttpSetTimeouts(session, 5000, 5000, 8000, 20000);

        let connect = WinHttpConnect(session, host_w.as_ptr(), 443, 0);
        if connect.is_null() {
            WinHttpCloseHandle(session);
            return Err(format!("连不上 {host}"));
        }
        let request = WinHttpOpenRequest(
            connect,
            post.as_ptr(),
            path_w.as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            WINHTTP_FLAG_SECURE,
        );
        if request.is_null() {
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err("创建请求失败".to_string());
        }

        let mut ok = WinHttpSendRequest(
            request,
            headers_w.as_ptr(),
            u32::MAX,
            body.as_ptr() as *mut _,
            body.len() as u32,
            body.len() as u32,
            0,
        );
        if ok != 0 {
            ok = WinHttpReceiveResponse(request, null_mut());
        }

        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null_mut(),
            &mut status as *mut u32 as *mut _,
            &mut size,
            null_mut(),
        );

        let mut raw: Vec<u8> = Vec::new();
        if ok != 0 {
            loop {
                let mut chunk = vec![0u8; 16 * 1024];
                let mut read: u32 = 0;
                if WinHttpReadData(
                    request,
                    chunk.as_mut_ptr() as *mut _,
                    chunk.len() as u32,
                    &mut read,
                ) == 0
                    || read == 0
                {
                    break;
                }
                raw.extend_from_slice(&chunk[..read as usize]);
            }
        }

        WinHttpCloseHandle(request);
        WinHttpCloseHandle(connect);
        WinHttpCloseHandle(session);

        if ok == 0 {
            return Err("请求没有完成（网络或代理问题）".to_string());
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        if status != 200 {
            // 把服务端回来的错误摘出来，方便直接看到 key 错/额度用完
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.as_str())
                        .map(|message| message.to_string())
                })
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(format!("HTTP {status}：{detail}"));
        }
        Ok(text)
    }
}

fn translate_text(text: &str, settings: &netspeed_core::settings::Settings) -> Result<String, String> {
    if settings.translate_key.trim().is_empty() {
        return Err("还没填 API Key（右键菜单 → 翻译设置…）".to_string());
    }
    if text.trim().is_empty() {
        return Err("没识别出文字".to_string());
    }
    let (base, model) = endpoint(settings);
    let url = format!("{base}/chat/completions");
    let payload = serde_json::json!({
        "model": model,
        "temperature": 0.2,
        "messages": [
            { "role": "system", "content": "You are a translation engine. Output only the translation." },
            { "role": "user", "content": prompt(text) }
        ]
    });
    let body = payload.to_string();
    let headers = format!(
        "Content-Type: application/json\r\nAuthorization: Bearer {}\r\n",
        settings.translate_key.trim()
    );

    let raw = http_post(&url, &headers, body.as_bytes())?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| "服务端返回的不是 JSON".to_string())?;
    let translated = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(|content| content.trim().to_string())
        .ok_or_else(|| "返回里没有译文".to_string())?;
    if translated.is_empty() {
        return Err("返回的译文是空的".to_string());
    }
    Ok(translated)
}

/// 后台线程：OCR → 翻译 → 投递回 owner（`WM_APP_TRANSLATE_READY`，wparam 是 Box 指针）
pub fn translate_region_async(owner: HWND, bitmap: Bitmap) {
    let settings = netspeed_core::settings::Settings::load();
    let handle = owner as usize;
    std::thread::spawn(move || {
        let outcome = match crate::ocr::recognize_pub(&bitmap) {
            Ok(source) => match translate_text(&source, &settings) {
                Ok(translated) => TranslateOutcome {
                    source,
                    translated,
                    error: None,
                },
                Err(error) => TranslateOutcome {
                    source,
                    translated: String::new(),
                    error: Some(error),
                },
            },
            Err(error) => TranslateOutcome {
                source: String::new(),
                translated: String::new(),
                error: Some(error),
            },
        };
        let payload = Box::into_raw(Box::new(outcome));
        unsafe {
            let _ok = PostMessageW(
                handle as HWND,
                crate::WM_APP_TRANSLATE_READY,
                payload as usize,
                0,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_is_reported_before_any_request() {
        let mut settings = netspeed_core::settings::Settings::default();
        settings.translate_key.clear();
        let error = translate_text("hello", &settings).unwrap_err();
        assert!(error.contains("API Key"), "没填 key 时要直接提示，而不是发请求：{error}");
    }

    #[test]
    fn endpoint_follows_provider_and_overrides() {
        let mut settings = netspeed_core::settings::Settings::default();
        settings.translate_provider = "openai".to_string();
        let (base, model) = endpoint(&settings);
        assert_eq!(base, "https://api.openai.com/v1");
        assert_eq!(model, "gpt-4o-mini");

        settings.translate_provider = "deepseek".to_string();
        let (base, model) = endpoint(&settings);
        assert_eq!(base, "https://api.deepseek.com/v1");
        assert_eq!(model, "deepseek-chat");

        settings.translate_base = "https://relay.example.com/v1/".to_string();
        settings.translate_model = "gpt-4.1-mini".to_string();
        let (base, model) = endpoint(&settings);
        assert_eq!(base, "https://relay.example.com/v1", "尾部斜杠要去掉");
        assert_eq!(model, "gpt-4.1-mini");
    }

    #[test]
    fn target_language_flips_by_script() {
        assert_eq!(target_language("这是一段中文"), "English");
        assert_eq!(target_language("this is english"), "Chinese");
        assert_eq!(target_language("混合 mixed 内容"), "English");
    }
}
