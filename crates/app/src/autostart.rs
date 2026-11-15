//! 开机自启：往 HKCU\…\Run 里写/删一个值。
//!
//! 用注册表而不是启动文件夹快捷方式：不用创建 .lnk，exe 换位置时也好改。

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_SZ,
};

use crate::theme::wide;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "NetSpeedTaskbar";

/// 当前 exe 的启动命令（带引号，路径里有空格也能用）
fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

/// 写入/删除自启项。失败就当没发生——自启不是致命功能。
pub fn set(enabled: bool) {
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        let sub = wide(RUN_KEY);
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if status != 0 || key.is_null() {
            return;
        }

        let name = wide(RUN_VALUE);
        if enabled {
            if let Some(command) = command() {
                let value = wide(&command);
                // REG_SZ 的长度是字节数，不含结尾的 0
                let bytes = ((value.len().saturating_sub(1)) * 2) as u32;
                let _ = RegSetValueExW(
                    key,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr() as *const u8,
                    bytes,
                );
            }
        } else {
            // 值本来就不存在时会返回错误，忽略即可
            let _ = RegDeleteValueW(key, name.as_ptr());
        }

        let _ = RegCloseKey(key);
    }
}

/// 启动时对一遍：exe 被移动/重命名后，自启项里的路径要跟着更新
pub fn sync(enabled: bool) {
    if enabled {
        set(true);
    }
}
