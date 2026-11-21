//! 从 GitHub 检查更新并自更新（方案 A：整包替换 exe，不做增量补丁）。

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WINHTTP_FLAG_SECURE, WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
};

use crate::theme::wide;

const API: &str = "https://api.github.com/repos/q455040355-del/NST/releases/latest";

/// 后台检查更新；有新版本就问一句，确认后下载替换并重启
pub fn check(owner: HWND) {
    let owner = owner as usize;
    std::thread::spawn(move || {
        let owner = owner as HWND;
        let Some(body) = http_get(API) else {
            crate::dialog::info(owner, "检查更新", "检查更新失败：连不上 GitHub（可能没网或被墙）。");
            return;
        };
        let json = String::from_utf8_lossy(&body).to_string();
        let Some(tag) = json_field(&json, "tag_name") else {
            return;
        };
        let current = env!("CARGO_PKG_VERSION");
        let latest = tag.trim_start_matches('v').to_string();
        if !is_newer(&latest, current) {
            crate::dialog::info(
                owner,
                "检查更新",
                &format!("已是最新版本 v{current}。"),
            );
            return;
        }

        let text = format!(
            "发现新版本 v{latest}（当前 v{current}）。\n\n现在下载并替换吗？程序会自动重启。"
        );
        if !crate::dialog::confirm(owner, "检查更新", &text) {
            return;
        }

        // Release 里除了 zip 还单独挂了一个 netspeed.exe，直接下它，不用解压
        let url = format!(
            "https://github.com/q455040355-del/NST/releases/download/v{latest}/netspeed.exe"
        );
        let Some(bytes) = http_get(&url) else {
            crate::dialog::info(
                owner,
                "检查更新",
                "下载失败，请稍后再试（或去 Releases 页面手动下载）。",
            );
            return;
        };
        if bytes.len() < 1_000_000 {
            // 多半是错误页/重定向，别把 exe 覆盖成垃圾
            crate::dialog::info(owner, "检查更新", "下载到的文件不对，已放弃更新。");
            return;
        }

        if let Err(error) = replace_and_restart(&bytes) {
            crate::dialog::info(owner, "检查更新", &format!("更新失败：{error}"));
        }
    });
}

/// GitHub API / 下载地址 → 文本
fn http_get(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    unsafe {
        let agent = wide("NetSpeedTaskbar");
        let host_w = wide(host);
        let path_w = wide(path);
        let get = wide("GET");
        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            null_mut(),
            null_mut(),
            0,
        );
        if session.is_null() {
            return None;
        }
        let connect = WinHttpConnect(session, host_w.as_ptr(), 443, 0);
        if connect.is_null() {
            WinHttpCloseHandle(session);
            return None;
        }
        let request = WinHttpOpenRequest(
            connect,
            get.as_ptr(),
            path_w.as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            WINHTTP_FLAG_SECURE,
        );
        if request.is_null() {
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return None;
        }
        let headers = wide("User-Agent: NetSpeedTaskbar\r\nAccept: application/octet-stream, application/vnd.github+json\r\n");
        let mut ok = WinHttpSendRequest(
            request,
            headers.as_ptr(),
            u32::MAX,
            null_mut(),
            0,
            0,
            0,
        );
        if ok != 0 {
            ok = WinHttpReceiveResponse(request, null_mut());
        }
        let mut body: Vec<u8> = Vec::new();
        if ok != 0 {
            loop {
                let mut chunk = vec![0u8; 64 * 1024];
                let mut read: u32 = 0;
                if WinHttpReadData(
                    request,
                    chunk.as_mut_ptr() as *mut c_void,
                    chunk.len() as u32,
                    &mut read,
                ) == 0
                    || read == 0
                {
                    break;
                }
                body.extend_from_slice(&chunk[..read as usize]);
            }
        }
        WinHttpCloseHandle(request);
        WinHttpCloseHandle(connect);
        WinHttpCloseHandle(session);
        if ok == 0 {
            None
        } else {
            Some(body)
        }
    }
}

/// 极简 JSON 取值：只认 `"key":"value"` 这种字符串字段，够用
fn json_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let colon = rest.find(':')? + 1;
    let rest = rest[colon..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |text: &str| -> (u32, u32, u32) {
        let mut parts = text.split('.').map(|part| part.parse::<u32>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    parse(latest) > parse(current)
}

/// 新 exe 落到临时目录，写个批处理等本进程退出后替换并重启
fn replace_and_restart(bytes: &[u8]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let dir = std::env::temp_dir().join("NST");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let fresh = dir.join("netspeed-new.exe");
    std::fs::write(&fresh, bytes).map_err(|error| error.to_string())?;

    let bat = dir.join("update.bat");
    // 只用 cmd 内建命令：tasklist / find 会被 PATH 里的 MSYS 工具劫持（find 是 GNU 的），
    // 之前那版会弹个 cmd 窗口报 "find: 'I:/': No such file or directory"。
    // 等进程退出改成「move 失败就重试」，目标被占用时 move 本来就会失败。
    let script = format!(
        "@echo off\r\n\
         setlocal\r\n\
         set \"NEW={}\"\r\n\
         set \"EXE={}\"\r\n\
         :retry\r\n\
         move /y \"%NEW%\" \"%EXE%\" > nul 2>&1\r\n\
         if exist \"%NEW%\" (\r\n\
           timeout /t 1 /nobreak > nul\r\n\
           goto retry\r\n\
         )\r\n\
         start \"\" \"%EXE%\"\r\n",
        fresh.display(),
        exe.display()
    );
    std::fs::write(&bat, script).map_err(|error| error.to_string())?;

    // CREATE_NO_WINDOW：不弹黑框
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd")
        .args(["/C", &bat.to_string_lossy()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| error.to_string())?;

    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    /// 真连一次 GitHub，确认 WinHTTP 和 JSON 取值都没问题
    #[test]
    fn fetch_latest_release() {
        let body = super::http_get(super::API).expect("请求 GitHub API 失败");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains("tag_name"), "返回内容不像 Release JSON");
        let tag = super::json_field(&text, "tag_name").expect("取不到 tag_name");
        assert!(tag.starts_with('v'), "tag 格式不对: {tag}");
    }

    #[test]
    fn version_compare() {
        assert!(super::is_newer("0.2.0", "0.1.32"));
        assert!(!super::is_newer("0.1.32", "0.1.32"));
        assert!(!super::is_newer("0.1.9", "0.1.32"));
    }
}
