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
    /// 按句配对（有的话结果窗直接逐句显示，不用再拆一次）
    pub pairs: Option<Vec<(String, String)>>,
}

/// 把「服务商 + 模型 + base」统一成实际请求要用的三样东西
fn endpoint(settings: &netspeed_core::settings::Settings) -> (String, String) {
    let provider = settings.translate_provider.trim().to_lowercase();
    let (default_base, default_model) = match provider.as_str() {
        "deepseek" => ("https://api.deepseek.com/v1", "deepseek-chat"),
        _ => ("https://api.openai.com/v1", "gpt-4o-mini"),
    };
    let base = settings.translate_base.trim();
    let base = if base.is_empty() { default_base } else { base };
    let model = settings.translate_model.trim();
    let model = if model.is_empty() { default_model } else { model };
    (base.trim_end_matches('/').to_string(), model.to_string())
}

/// 目标语言：默认中文（截图来源基本是英文或塞尔维亚语）
fn target_for(settings: &netspeed_core::settings::Settings) -> &'static str {
    match settings.translate_target.trim().to_lowercase().as_str() {
        "en" | "english" => "English",
        "sr" | "serbian" => "Serbian written in the Latin alphabet (not Cyrillic)",
        // 默认（含老配置里的 auto / ja / ko）一律中文
        _ => "Simplified Chinese",
    }
}

fn prompt_for(text: &str, target: &str) -> String {
    format!(
        "Translate the following screenshot text into {target}. The source is usually English \
or Serbian. Keep the line breaks and the original order. Only output the translation, \
no explanations, no quotes.\n\n{text}"
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
                "这个模型没开权限：去服务商控制台打开模型访问"
            } else if detail.contains("Free usage limit") || detail.contains("FreeUsageLimit") {
                "免费模型不开放给 API"
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
        _ => "gpt-4o-mini",
    }
}

/// 服务商的默认 base（设置里没填 Base URL 时用）
pub fn default_base(provider: &str) -> &'static str {
    match provider.trim().to_lowercase().as_str() {
        "deepseek" => "https://api.deepseek.com/v1",
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
            { "role": "user", "content": prompt_for(text, target_for(settings)) }
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
    if value
        .pointer("/choices/0/finish_reason")
        .and_then(|reason| reason.as_str())
        == Some("length")
    {
        return Err("译文被截断了（这块太长），分两次框选再翻".to_string());
    }
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

/// 把一段文字拆成「句」：**按标点切，不看换行**。
/// 换行只是 OCR 的排版痕迹（一行半句、两行一句都很常见），标点才是句子的边界。
pub fn split_units(text: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        // 换行、制表当成空格（不参与断句）
        let ch = if ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else {
            ch
        };
        if ch == ' ' && current.ends_with(' ') {
            continue;
        }
        current.push(ch);
        let boundary = matches!(
            ch,
            '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；' | '…'
        ) || (ch == ',' && current.chars().count() > 60);
        let length = current.chars().count();
        // 边界停顿太短不切（"12. 5" 这种），太长则无条件切（没有标点的长串）
        if (boundary && length > 2) || length >= 200 {
            units.push(current.trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        units.push(current.trim().to_string());
    }
    units.retain(|unit| !unit.is_empty());
    units
}

/// 按句翻译：把 N 句编号发过去，要求原样返回 N 行，返回 (原文, 译文) 列表。
/// 模型没按行返回就退回整段翻译，再按行硬分。
pub fn translate_pairs(
    settings: &netspeed_core::settings::Settings,
    source: &str,
) -> Result<Vec<(String, String)>, String> {
    let units = split_units(source);
    if units.is_empty() {
        return Err("没识别出文字".to_string());
    }
    if units.len() == 1 {
        let translated = translate_text(source, settings)?;
        return Ok(vec![(units[0].clone(), translated)]);
    }
    let target = target_for(settings);
    let mut numbered = String::new();
    for (index, unit) in units.iter().enumerate() {
        numbered.push_str(&format!("{}. {}\n", index + 1, unit));
    }
    let instruction = format!(
        "Translate each numbered line into {target}. Return exactly {} lines, \
one translation per input line, keeping the same order and the same leading number \
(\"1. ...\", \"2. ...\"). Do not merge or split lines. Output nothing else.",
        units.len()
    );

    let (base, model) = endpoint(settings);
    let url = format!("{base}/chat/completions");
    let payload = serde_json::json!({
        "model": model,
        "temperature": 0.2,
        "messages": [
            { "role": "system", "content": "You are a translation engine. Output only the translation." },
            { "role": "user", "content": format!("{instruction}\n\n{numbered}") }
        ]
    });
    let headers = format!(
        "Content-Type: application/json\r\nAuthorization: Bearer {}\r\n",
        settings.translate_key.trim()
    );
    let raw = http_request("POST", &url, &headers, Some(payload.to_string().as_bytes()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| "服务端返回的不是 JSON".to_string())?;
    let text = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    // 去掉行首编号，按顺序配对
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 只剥「1. / 12、/ 3）」这种纯 ASCII 数字前缀。
        // 不能直接 line[index+1..]：、） 是多字节，切在字符中间会 panic
        let digits = line.bytes().take_while(|byte| byte.is_ascii_digit()).count();
        let stripped = if (1..=3).contains(&digits) {
            let rest = &line[digits..];
            match rest.chars().next() {
                Some(sep) if matches!(sep, '.' | '、' | '）' | ')' | '．') => {
                    rest[sep.len_utf8()..].trim().to_string()
                }
                _ => line.to_string(),
            }
        } else {
            line.to_string()
        };
        if !stripped.is_empty() {
            lines.push(stripped);
        }
    }
    if lines.len() == units.len() {
        for (unit, line) in units.iter().zip(lines.iter()) {
            pairs.push((unit.clone(), line.clone()));
        }
        return Ok(pairs);
    }
    // 数量对不上：整段翻译一次，按行切，能切多少算多少
    let block = translate_text(source, settings)?;
    let block_lines: Vec<String> = block
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if block_lines.len() == units.len() {
        for (unit, line) in units.iter().zip(block_lines.iter()) {
            pairs.push((unit.clone(), line.clone()));
        }
        return Ok(pairs);
    }
    // 实在对不上：整段算一句，至少别丢内容
    Ok(vec![(units.join(" "), block)])
}

/// 后台线程：OCR → 翻译 → 投递回 owner（`WM_APP_TRANSLATE_READY`，wparam 是 Box 指针）
pub fn translate_region_async(owner: HWND, bitmap: Bitmap) {
    let settings = netspeed_core::settings::Settings::load();
    let handle = owner as usize;
    std::thread::spawn(move || {
        let outcome = match crate::ocr::recognize_pub(&bitmap) {
            Ok(source) => {
                // 先把原文投回去开窗（别让人对着空白等几秒），再慢慢翻译
                let early = Box::into_raw(Box::new(source.clone()));
                unsafe {
                    if PostMessageW(
                        handle as HWND,
                        crate::WM_APP_TRANSLATE_OCR,
                        early as usize,
                        0,
                    ) == 0
                    {
                        drop(Box::from_raw(early));
                    }
                }
                match translate_text(&source, &settings) {
                Ok(translated) => TranslateOutcome {
                    source,
                    translated,
                    error: None,
                    pairs: None,
                },
                Err(error) => TranslateOutcome {
                    source,
                    translated: String::new(),
                    error: Some(error),
                    pairs: None,
                },
                }
            }
            Err(error) => TranslateOutcome {
                source: String::new(),
                translated: String::new(),
                error: Some(error),
                pairs: None,
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
    fn split_units_breaks_on_punctuation_not_newlines() {
        // 换行只是排版，句子边界看标点
        let text = "Latency budget. The panel samples the taskbar twice a second.\nIf sampling fails, the last good color is kept for 30 seconds.";
        let units = split_units(text);
        assert_eq!(units.len(), 3, "按句号切三句：{units:?}");
        assert!(units[2].starts_with("If sampling"), "第二行要和上一句合并后重新按标点切");
        assert!(!units[0].contains('\n'), "结果里不该留换行");
    }

    #[test]
    fn split_units_handles_chinese_punctuation() {
        let text = "面板每秒采样两次。如果失败，会保留上一次的颜色，三十秒后再试。";
        let units = split_units(text);
        assert_eq!(units.len(), 2, "中文句号也要切：{units:?}");
    }

    #[test]
    fn split_units_does_not_split_short_numbers() {
        let units = split_units("3.5 倍");
        assert_eq!(units.len(), 1, "小数点不能被当成句号：{units:?}");
    }

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

    }
