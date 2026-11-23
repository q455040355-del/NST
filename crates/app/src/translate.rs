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
    let (default_base, default_model) = match provider.as_str() {
        "deepseek" => ("https://api.deepseek.com/v1", "deepseek-chat"),
        // OpenCode Zen：一个 key 走所有模型，chat/completions 兼容；默认用免费那个
        "opencode" => ("https://opencode.ai/zen/v1", "deepseek-v4-flash-free"),
        _ => ("https://api.openai.com/v1", "gpt-4o-mini"),
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

/// WinHttp 请求（https），返回响应体；body 为 None 就是 GET
fn http_request(method: &str, url: &str, headers: &str, body: Option<&[u8]>) -> Result<String, String> {
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
    let verb = wide(method);
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
            verb.as_ptr(),
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

        let (body_ptr, body_len) = match body {
            Some(bytes) => (bytes.as_ptr() as *mut _, bytes.len() as u32),
            None => (std::ptr::null_mut(), 0),
        };
        let mut ok = WinHttpSendRequest(
            request,
            headers_w.as_ptr(),
            u32::MAX,
            body_ptr,
            body_len,
            body_len,
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
            // OpenCode 的免费模型只在它自己的客户端里生效，走 API 会报 FreeUsageLimitError
            let hint = if detail.contains("Model access is disabled") {
                "这个模型没开权限：去 OpenCode console 打开模型访问"
            } else if detail.contains("Free usage limit") || detail.contains("FreeUsageLimit") {
                "免费模型只限 OpenCode 客户端"
            } else {
                match status {
                    401 => "Key 不对或已失效",
                    402 => "余额不足，去服务商充值",
                    403 => "这个 Key 没权限用该模型",
                    404 => "地址或模型名不对",
                    429 => "请求太频繁，等会儿再试",
                    500..=599 => "服务端出错",
                    _ => "",
                }
            };
            return if hint.is_empty() {
                Err(format!("HTTP {status}：{detail}"))
            } else {
                // 一行放得下：状态码 + 人话，别把服务端那一大段英文拼进来
                Err(format!("{status} · {hint}"))
            };
        }
        Ok(text)
    }
}

/// 服务商的默认模型（设置里没填模型时用）
pub fn default_model(provider: &str) -> &'static str {
    match provider.trim().to_lowercase().as_str() {
        "deepseek" => "deepseek-chat",
        "opencode" => "deepseek-v4-flash-free",
        _ => "gpt-4o-mini",
    }
}

/// 服务商的默认 base（设置里没填 Base URL 时用）
pub fn default_base(provider: &str) -> &'static str {
    match provider.trim().to_lowercase().as_str() {
        "deepseek" => "https://api.deepseek.com/v1",
        "opencode" => "https://opencode.ai/zen/v1",
        _ => "https://api.openai.com/v1",
    }
}

/// 测一下这套配置通不通：真发一条最短的 chat/completions，能拿到 choices 就算通。
/// 返回一句给人看的话（模型 + 往返耗时），失败返回服务端给的原因。
pub fn test_connection(
    provider: &str,
    base: &str,
    key: &str,
    model: &str,
) -> Result<String, String> {
    if key.trim().is_empty() {
        return Err("还没填 API Key".to_string());
    }
    let base = base.trim();
    let base = if base.is_empty() {
        default_base(provider)
    } else {
        base
    };
    let model = model.trim();
    let model = if model.is_empty() {
        default_model(provider)
    } else {
        model
    };
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let payload = serde_json::json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "ping" }]
    });
    let headers = format!(
        "Content-Type: application/json\r\nAuthorization: Bearer {}\r\n",
        key.trim()
    );
    let started = std::time::Instant::now();
    let raw = http_request("POST", &url, &headers, Some(payload.to_string().as_bytes()))?;
    let elapsed = started.elapsed().as_millis();

    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| "服务端返回的不是 JSON".to_string())?;
    if value.get("choices").is_none() {
        return Err("连通了，但返回里没有 choices（模型名可能不对）".to_string());
    }
    Ok(format!("往返 {elapsed} ms"))
}

/// 拉服务商的模型列表：GET {base}/models（OpenAI 兼容的 `{"data":[{"id":..}]}`）。
/// 返回 (模型 id, 显示名)，只留能走 chat/completions 的。
pub fn fetch_models(
    provider: &str,
    base: &str,
    key: &str,
) -> Result<Vec<(String, String)>, String> {
    let base = base.trim();
    let base = if base.is_empty() {
        default_base(provider)
    } else {
        base
    };
    let url = format!("{}/models", base.trim_end_matches('/'));
    let headers = if key.trim().is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {}\r\n", key.trim())
    };
    let raw = http_request("GET", &url, &headers, None)?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| "服务端返回的不是 JSON".to_string())?;
    let list = value
        .get("data")
        .and_then(|data| data.as_array())
        .ok_or_else(|| "返回里没有模型列表".to_string())?;

    // 免费模型在 OpenCode 里只限它自己的客户端用，走 API 一定 429，列出来只会误导
    let is_free = |item: &serde_json::Value| -> bool {
        if let Some(cost) = item.get("cost").and_then(|cost| cost.get("text")) {
            let price = |key: &str| -> Option<f64> {
                cost.get(key)
                    .and_then(|side| side.get("price"))
                    .and_then(|price| price.as_f64())
            };
            if let (Some(input), Some(output)) = (price("input"), price("output")) {
                return input <= 0.0 && output <= 0.0;
            }
        }
        item.get("id")
            .and_then(|id| id.as_str())
            .map(|id| id.ends_with("-free"))
            .unwrap_or(false)
    };

    let mut models: Vec<(String, String)> = Vec::new();
    for item in list {
        let Some(id) = item.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        // OpenCode Zen 会在每个模型上写它推荐的 API：只有 chat-completions 的能直接用
        if let Some(api) = item.get("recommended_api").and_then(|api| api.as_str()) {
            if !api.eq_ignore_ascii_case("chat-completions") {
                continue;
            }
        }
        if is_free(item) {
            continue;
        }
        // 图片 / 语音 / 向量这些不是聊天模型，选进去只会 500
        let lowered = id.to_lowercase();
        if [
            "image",
            "embedding",
            "tts",
            "whisper",
            "moderation",
            "audio",
            "realtime",
            "transcribe",
            "dall-e",
        ]
        .iter()
        .any(|bad| lowered.contains(bad))
        {
            continue;
        }
        let name = item
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or(id);
        models.push((id.to_string(), name.to_string()));
    }
    models.sort_by(|a, b| a.0.cmp(&b.0));
    models.dedup_by(|a, b| a.0 == b.0);
    if models.is_empty() {
        return Err("这家没返回能用的模型".to_string());
    }
    Ok(models)
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

    let raw = http_request("POST", &url, &headers, Some(body.as_bytes()))?;
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
