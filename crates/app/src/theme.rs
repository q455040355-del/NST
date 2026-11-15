//! 面板配色与字体：跟随任务栏明暗主题。

use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, HFONT, CLEARTYPE_QUALITY, DEFAULT_CHARSET, FW_BOLD, FW_NORMAL,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
};

#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub up: u32,
    pub down: u32,
    pub label: u32,
    pub value: u32,
    pub background: u32,
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

impl Colors {
    pub fn for_theme(light: bool, background: u32) -> Self {
        if light {
            Self {
                up: rgb(22, 122, 66),
                down: rgb(26, 96, 168),
                label: rgb(96, 102, 112),
                value: rgb(32, 36, 42),
                background,
            }
        } else {
            Self {
                up: rgb(120, 224, 150),
                down: rgb(120, 196, 255),
                label: rgb(158, 166, 178),
                value: rgb(238, 242, 246),
                background,
            }
        }
    }
}

/// 任务栏是否浅色主题（注册表 AppsUseLightTheme）。
pub fn taskbar_is_light() -> bool {
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("AppsUseLightTheme");
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut _,
            &mut size,
        )
    };

    // debug: ("registry status={status} value={value}"));
    status == 0 && value == 1
}

pub struct Fonts {
    pub label: HFONT,
    /// CPU/GPU 这类标签在小面板里要缩一档
    pub label_small: HFONT,
    pub value: HFONT,
    pub value_mid: HFONT,
    pub value_small: HFONT,
}

impl Fonts {
    pub fn create(dpi: u32) -> Self {
        Self {
            label: create_font(dpi, 9.5, FW_BOLD, "Microsoft YaHei UI"),
            label_small: create_font(dpi, 8.5, FW_BOLD, "Microsoft YaHei UI"),
            // 数字也走同一个字体（以前是 Consolas，和中文混排时字重/基线对不上）
            value: create_font(dpi, 11.0, FW_NORMAL, "Microsoft YaHei UI"),
            value_mid: create_font(dpi, 10.0, FW_NORMAL, "Microsoft YaHei UI"),
            value_small: create_font(dpi, 9.0, FW_NORMAL, "Microsoft YaHei UI"),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            for handle in [
                self.label,
                self.label_small,
                self.value,
                self.value_mid,
                self.value_small,
            ] {
                DeleteObject(handle as *mut _);
            }
        }
    }
}

fn create_font(dpi: u32, points: f32, weight: u32, face: &str) -> HFONT {
    let height = -((points * dpi as f32 / 72.0).round() as i32);
    let face = wide(face);
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            face.as_ptr(),
        )
    }
}

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
