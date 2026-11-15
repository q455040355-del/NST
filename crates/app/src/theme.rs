//! 面板配色与字体：跟随任务栏明暗主题。

use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, HFONT, CLEARTYPE_QUALITY, DEFAULT_CHARSET, FW_BOLD, FW_NORMAL,
};
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
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
///
/// 绘制路径上每帧都会问一次（动画时 60fps），注册表读一次要几十微秒，
/// 这里缓存 2 秒：主题切换最多慢 2 秒生效，但省掉每帧一次 RegGetValueW。
pub fn taskbar_is_light() -> bool {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const TTL: Duration = Duration::from_secs(2);

    static CACHE: OnceLock<Mutex<(bool, Instant)>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new((true, Instant::now() - TTL)));

    if let Ok(mut guard) = cell.lock() {
        if guard.1.elapsed() < TTL {
            return guard.0;
        }

        let value = read_theme();
        *guard = (value, Instant::now());
        return value;
    }

    read_theme()
}

fn read_theme() -> bool {
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
    /// `percent`：字号百分比（90 / 100 / 110 / 125），五个字号等比缩放
    pub fn create(dpi: u32, percent: i32) -> Self {
        let k = percent.clamp(50, 200) as f32 / 100.0;
        Self {
            label: create_font(dpi, 9.5 * k, FW_BOLD, "Microsoft YaHei UI"),
            label_small: create_font(dpi, 8.5 * k, FW_BOLD, "Microsoft YaHei UI"),
            // 数字也走同一个字体（以前是 Consolas，和中文混排时字重/基线对不上）
            value: create_font(dpi, 11.0 * k, FW_NORMAL, "Microsoft YaHei UI"),
            value_mid: create_font(dpi, 10.0 * k, FW_NORMAL, "Microsoft YaHei UI"),
            value_small: create_font(dpi, 9.0 * k, FW_NORMAL, "Microsoft YaHei UI"),
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

// ---------------------------------------------------------------- 窗口配色

/// 设置窗 / 历史窗 / 取字窗共用的一套颜色（跟随系统明暗）。
///
/// 面板本身早就跟着任务栏深浅走了，但这两个窗口以前写死浅色卡片，
/// 晚上切到深色模式会亮得刺眼。
#[derive(Debug, Clone, Copy)]
pub struct Ui {
    /// 窗口底色
    pub bg: u32,
    /// 卡片 / 输入框底色
    pub surface: u32,
    /// 1px 描边
    pub border: u32,
    /// 主文字
    pub text: u32,
    /// 次要文字（标签、说明）
    pub text2: u32,
    /// 更弱的文字（占位、刻度）
    pub text3: u32,
    /// 控件底（下拉、开关轨道）
    pub chip: u32,
    pub chip_hover: u32,
    /// 表格斑马纹
    pub row_tint: u32,
    pub accent: u32,
    pub accent_pressed: u32,
    /// 深色底上画箭头/勾用的强调色
    pub dark: u32,
}

impl Ui {
    fn light() -> Self {
        Self {
            bg: rgb(0xF5, 0xF6, 0xF8),
            surface: rgb(0xFF, 0xFF, 0xFF),
            border: rgb(0xE8, 0xEA, 0xEF),
            text: rgb(0x14, 0x16, 0x1A),
            text2: rgb(0x6B, 0x72, 0x80),
            text3: rgb(0x9A, 0xA3, 0xAF),
            chip: rgb(0xEF, 0xF1, 0xF3),
            chip_hover: rgb(0xE7, 0xEA, 0xEE),
            row_tint: rgb(0xFA, 0xFB, 0xFC),
            accent: rgb(0x2F, 0x6F, 0xE4),
            accent_pressed: rgb(0x1F, 0x5A, 0xC2),
            dark: rgb(0x14, 0x16, 0x1A),
        }
    }

    fn dark() -> Self {
        Self {
            bg: rgb(0x1B, 0x1C, 0x1E),
            surface: rgb(0x23, 0x24, 0x27),
            border: rgb(0x34, 0x36, 0x3A),
            text: rgb(0xF2, 0xF3, 0xF5),
            text2: rgb(0xB9, 0xBD, 0xC4),
            text3: rgb(0x85, 0x8A, 0x93),
            chip: rgb(0x2C, 0x2E, 0x32),
            chip_hover: rgb(0x36, 0x38, 0x3D),
            row_tint: rgb(0x26, 0x28, 0x2B),
            accent: rgb(0x4C, 0x8D, 0xFF),
            accent_pressed: rgb(0x3B, 0x76, 0xE0),
            dark: rgb(0x14, 0x16, 0x1A),
        }
    }
}

/// 当前该用哪套配色（AppsUseLightTheme，2 秒缓存）
pub fn ui() -> Ui {
    if forced_dark() {
        Ui::dark()
    } else if taskbar_is_light() {
        Ui::light()
    } else {
        Ui::dark()
    }
}

/// 调试用：设了 NST_FORCE_DARK 就强制深色，方便截图验证（正常使用不受影响）
fn forced_dark() -> bool {
    std::env::var_os("NST_FORCE_DARK").is_some()
}

/// 系统是否深色（ui() 的另一面，图表配色要用）
pub fn is_dark() -> bool {
    forced_dark() || !taskbar_is_light()
}
