//! 设置持久化。
//!
//! 和 C# 版共用同一个 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，
//! 这样迁移期间两份程序看到的是同一套配置（键名保持一致，写回时只改自己认得的键）。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

pub const GAUGE_MEMORY: &str = "mem";
pub const GAUGE_AI: &str = "ai";
pub const GAUGE_NONE: &str = "none";

#[derive(Debug, Clone)]
pub struct Settings {
    pub panel_width: i32,
    pub panel_x: i32,
    pub panel_y: i32,
    pub network_interface: String,
    pub gauge_source: String,
    /// 面板刷新间隔（毫秒）：500 / 1000 / 2000
    pub refresh_ms: i32,
    /// 面板字号百分比：90 / 100 / 110 / 125
    pub font_scale: i32,
    /// 当前视图：speed / memory / temperature / ai
    pub view: String,
    /// AI 统计总开关：关着就完全不碰 AI（不起线程、不读文件/数据库）
    pub ai_enabled: bool,
    pub ai_opencode: bool,
    pub ai_claude: bool,
    pub ai_codex: bool,
    pub ai_dsh: bool,
    pub ai_workbuddy: bool,
    pub ai_workbuddy_ai: bool,
    /// 开机自启（写 HKCU\…\Run）
    pub auto_start: bool,
    /// 截图功能开关：关掉后菜单不显示、热键不注册
    pub shot_enabled: bool,
    /// 取字 / OCR 开关（默认关，开了才注册热键、截图工具栏才出现「OCR」）
    pub ocr_enabled: bool,
    /// 截图热键（如 `Ctrl+Shift+A`）
    pub shot_hotkey: String,
    /// 取字热键（如 `Ctrl+Shift+O`）
    pub ocr_hotkey: String,
    /// 截区域录 GIF 的开关（关了不注册热键、菜单不出）
    pub gif_enabled: bool,
    /// GIF 录制帧率（30 / 60 / 120）
    pub gif_fps: i32,
    /// 录制画质（50 / 65 / 80，对应 libwebp quality；只影响体积和细节，编码耗时几乎不变）
    pub gif_quality: i32,
    /// GIF 录制热键
    pub gif_hotkey: String,
    /// 编辑窗是否置顶（记住上次的选择）
    pub shot_topmost: bool,
    /// 未识别的键原样保留，写回时补上，避免把 C# 版的设置弄丢。
    extra: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            panel_width: 109,
            panel_x: -1,
            panel_y: -1,
            network_interface: "auto".to_string(),
            gauge_source: GAUGE_MEMORY.to_string(),
            refresh_ms: 500,
            font_scale: 100,
            view: "speed".to_string(),
            ai_enabled: false,
            ai_opencode: true,
            ai_claude: false,
            ai_codex: false,
            ai_dsh: true,
            ai_workbuddy: true,
            ai_workbuddy_ai: true,
            auto_start: false,
            shot_enabled: true,
            ocr_enabled: false,
            shot_hotkey: "Ctrl+Shift+A".to_string(),
            ocr_hotkey: "Ctrl+Shift+O".to_string(),
            gif_enabled: true,
            gif_fps: 30,
            gif_quality: 65,
            gif_hotkey: "Ctrl+Shift+G".to_string(),
            shot_topmost: false,
            extra: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir());
        base.join("NetSpeedTaskbar").join("settings.ini")
    }

    pub fn load() -> Self {
        let mut settings = Settings::default();
        let Ok(text) = fs::read_to_string(Self::path()) else {
            return settings;
        };

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            match key {
                "PanelWidth" => settings.panel_width = value.parse().unwrap_or(settings.panel_width),
                "PanelX" => settings.panel_x = value.parse().unwrap_or(-1),
                "PanelY" => settings.panel_y = value.parse().unwrap_or(-1),
                "NetworkInterface" => settings.network_interface = value.to_string(),
                "GaugeSource" => settings.gauge_source = normalize_gauge(value),
                "RefreshMs" => settings.refresh_ms = normalize_refresh(value.parse().unwrap_or(500)),
                "FontScale" => {
                    settings.font_scale = normalize_font_scale(value.parse().unwrap_or(100))
                }
                "View" => settings.view = value.to_string(),
                "AiEnabled" => settings.ai_enabled = parse_bool(value),
                "AiOpenCode" => settings.ai_opencode = parse_bool(value),
                "AiClaude" => settings.ai_claude = parse_bool(value),
                "AiCodex" => settings.ai_codex = parse_bool(value),
                "AiDsh" => settings.ai_dsh = parse_bool(value),
                "AiWorkBuddy" => settings.ai_workbuddy = parse_bool(value),
                "AiWorkBuddyAi" => settings.ai_workbuddy_ai = parse_bool(value),
                "AutoStart" => settings.auto_start = parse_bool(value),
                "ShotEnabled" => settings.shot_enabled = parse_bool(value),
                "OcrEnabled" => settings.ocr_enabled = parse_bool(value),
                "ShotHotkey" => {
                    if let Some((mods, vk)) = parse_hotkey(value) {
                        settings.shot_hotkey = format_hotkey(mods, vk);
                    }
                }
                "OcrHotkey" => {
                    if let Some((mods, vk)) = parse_hotkey(value) {
                        settings.ocr_hotkey = format_hotkey(mods, vk);
                    }
                }
                "GifEnabled" => settings.gif_enabled = parse_bool(value),
                "GifFps" => settings.gif_fps = normalize_fps(value.trim().parse().unwrap_or(30)),
                "GifQuality" => {
                    settings.gif_quality = normalize_quality(value.trim().parse().unwrap_or(65))
                }
                "ShotTopmost" => settings.shot_topmost = parse_bool(value),
                "GifHotkey" => {
                    if let Some((mods, vk)) = parse_hotkey(value) {
                        settings.gif_hotkey = format_hotkey(mods, vk);
                    }
                }
                _ => {
                    settings.extra.insert(key.to_string(), value.to_string());
                }
            }
        }

        settings.panel_width = settings.panel_width.clamp(72, 360);
        settings.refresh_ms = normalize_refresh(settings.refresh_ms);
        settings.font_scale = normalize_font_scale(settings.font_scale);

        // AI 关掉时不能停在 AI 视图 / AI 指示条上，否则面板会显示一个永远不变的空值
        if !settings.ai_enabled {
            if settings.view == "ai" {
                settings.view = "speed".to_string();
            }
            if settings.gauge_source == GAUGE_AI {
                settings.gauge_source = GAUGE_MEMORY.to_string();
            }
        }
        settings
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }

        let mut out = String::new();
        out.push_str("SettingsVersion=7\n");
        out.push_str(&format!("PanelWidth={}\n", self.panel_width));
        out.push_str(&format!("PanelX={}\n", self.panel_x));
        out.push_str(&format!("PanelY={}\n", self.panel_y));
        out.push_str(&format!("NetworkInterface={}\n", self.network_interface));
        out.push_str("DisplayMode=embedded\n");
        out.push_str(&format!("GaugeSource={}\n", self.gauge_source));
        out.push_str(&format!("RefreshMs={}\n", self.refresh_ms));
        out.push_str(&format!("FontScale={}\n", self.font_scale));
        out.push_str(&format!("View={}\n", self.view));
        out.push_str(&format!("AiEnabled={}\n", self.ai_enabled));
        out.push_str(&format!("AiOpenCode={}\n", self.ai_opencode));
        out.push_str(&format!("AiClaude={}\n", self.ai_claude));
        out.push_str(&format!("AiCodex={}\n", self.ai_codex));
        out.push_str(&format!("AiDsh={}\n", self.ai_dsh));
        out.push_str(&format!("AiWorkBuddy={}\n", self.ai_workbuddy));
        out.push_str(&format!("AiWorkBuddyAi={}\n", self.ai_workbuddy_ai));
        out.push_str(&format!("AutoStart={}\n", self.auto_start));
        out.push_str(&format!("ShotEnabled={}\n", self.shot_enabled));
        out.push_str(&format!("OcrEnabled={}\n", self.ocr_enabled));
        out.push_str(&format!("ShotHotkey={}\n", self.shot_hotkey));
        out.push_str(&format!("OcrHotkey={}\n", self.ocr_hotkey));
        out.push_str(&format!("GifEnabled={}\n", self.gif_enabled));
        out.push_str(&format!("GifFps={}\n", self.gif_fps));
        out.push_str(&format!("GifQuality={}\n", self.gif_quality));
        out.push_str(&format!("GifHotkey={}\n", self.gif_hotkey));
        out.push_str(&format!("ShotTopmost={}\n", self.shot_topmost));

        for (key, value) in &self.extra {
            // 上面这些键已经写过，不再重复
            if matches!(
                key.as_str(),
                "SettingsVersion"
                    | "PanelWidth"
                    | "PanelX"
                    | "PanelY"
                    | "NetworkInterface"
                    | "DisplayMode"
                    | "GaugeSource"
                    | "RefreshMs"
                    | "FontScale"
                    | "View"
                    | "AiEnabled"
                    | "AiOpenCode"
                    | "AiClaude"
                    | "AiCodex"
                    | "AiDsh"
                    | "AiWorkBuddy"
                    | "AiWorkBuddyAi"
                    | "AutoStart"
                    | "ShotEnabled"
                    | "OcrEnabled"
                    | "ShotHotkey"
                    | "OcrHotkey"
                    | "GifEnabled"
                    | "GifFps"
                    | "GifQuality"
                    | "GifHotkey"
                    | "ShotTopmost"
            ) {
                continue;
            }
            out.push_str(&format!("{key}={value}\n"));
        }

        // 先写临时文件再改名：写到一半崩掉/断电时不会留下半个 ini
        // （以前是直接 fs::write，崩一次配置就全丢，面板位置、字号、AI 开关全重置）
        let temp = path.with_extension("ini.tmp");
        if fs::write(&temp, &out).is_ok() && fs::rename(&temp, &path).is_ok() {
            return;
        }
        // 改名被别的进程占着（比如 C# 版正开着它）就退回直接覆盖
        let _ = fs::remove_file(&temp);
        let _ = fs::write(path, out);
    }

    pub fn gauge_visible(&self) -> bool {
        !self.gauge_source.eq_ignore_ascii_case(GAUGE_NONE)
    }

    pub fn cycle_view(&mut self) {
        self.view = match self.view.as_str() {
            "speed" => "memory",
            "memory" => "temperature",
            "temperature" => "ai",
            _ => "speed",
        }
        .to_string();
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

/// 录制帧率只认 30 / 60 / 120，别的值回落到 30
pub fn normalize_fps(fps: i32) -> i32 {
    if fps >= 120 {
        120
    } else if fps >= 60 {
        60
    } else {
        30
    }
}

/// 录制画质只认 50 / 65 / 80，别的值回落到 65。
/// 画质对编码耗时几乎没影响（1080p 单帧 25.1 / 25.5 / 25.7ms），影响的是体积（54 / 59 / 62KB）
pub fn normalize_quality(quality: i32) -> i32 {
    if quality >= 80 {
        80
    } else if quality >= 65 {
        65
    } else if quality >= 50 {
        50
    } else {
        65
    }
}

// ---------------------------------------------------------------- 全局热键

pub const HOTKEY_MOD_ALT: u32 = 0x0001;
pub const HOTKEY_MOD_CONTROL: u32 = 0x0002;
pub const HOTKEY_MOD_SHIFT: u32 = 0x0004;
pub const HOTKEY_MOD_WIN: u32 = 0x0008;

/// `Ctrl+Shift+A` → (修饰键位, 虚拟键码)。必须带至少一个修饰键，
/// 免得把普通打字键注册成全局热键。
pub fn parse_hotkey(text: &str) -> Option<(u32, u32)> {
    let mut mods = 0_u32;
    let mut key = None;

    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }

        let lower = part.to_ascii_lowercase();
        let bit = match lower.as_str() {
            "ctrl" | "control" => HOTKEY_MOD_CONTROL,
            "shift" => HOTKEY_MOD_SHIFT,
            "alt" => HOTKEY_MOD_ALT,
            "win" | "windows" => HOTKEY_MOD_WIN,
            _ => {
                if key.is_some() {
                    return None;
                }
                key = Some(key_code(&lower)?);
                continue;
            }
        };
        mods |= bit;
    }

    let key = key?;
    if mods == 0 {
        return None;
    }
    Some((mods, key))
}

fn key_code(lower: &str) -> Option<u32> {
    if lower.len() == 1 {
        let ch = lower.chars().next()?.to_ascii_uppercase();
        if ch.is_ascii_alphanumeric() {
            return Some(ch as u32);
        }
    }

    if let Some(rest) = lower.strip_prefix('f') {
        if let Ok(number) = rest.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0x70 + number - 1);
            }
        }
    }

    match lower {
        "space" => Some(0x20),
        "tab" => Some(0x09),
        "enter" | "return" => Some(0x0D),
        _ => None,
    }
}

/// (修饰键位, 虚拟键码) → `Ctrl+Shift+A`（显示/落盘用固定顺序）
pub fn format_hotkey(mods: u32, vk: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if mods & HOTKEY_MOD_WIN != 0 {
        parts.push("Win".to_string());
    }
    if mods & HOTKEY_MOD_CONTROL != 0 {
        parts.push("Ctrl".to_string());
    }
    if mods & HOTKEY_MOD_SHIFT != 0 {
        parts.push("Shift".to_string());
    }
    if mods & HOTKEY_MOD_ALT != 0 {
        parts.push("Alt".to_string());
    }
    if let Some(key) = key_name(vk) {
        parts.push(key);
    }
    parts.join("+")
}

fn key_name(vk: u32) -> Option<String> {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(|ch| ch.to_string()),
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        0x20 => Some("Space".to_string()),
        0x09 => Some("Tab".to_string()),
        0x0D => Some("Enter".to_string()),
        _ => None,
    }
}

/// 指示条只允许两种指标：内存占用百分比 / Token 速度（外加关闭）。
/// 老配置里的 view / speed / cpu 一律落到内存占用。
fn normalize_gauge(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "ai" | "token" | "tokens" => GAUGE_AI,
        "none" | "off" => GAUGE_NONE,
        _ => GAUGE_MEMORY,
    }
    .to_string()
}

/// 刷新间隔只允许 0.5 / 1 / 2 秒，其它值就近吸附（手改 ini 也不会跑飞）。
pub fn normalize_refresh(value: i32) -> i32 {
    match value {
        v if v <= 750 => 500,
        v if v <= 1500 => 1000,
        _ => 2000,
    }
}

/// 字号只允许 90 / 100 / 110 / 125 %，其它值就近吸附。
pub fn normalize_font_scale(value: i32) -> i32 {
    match value {
        v if v <= 95 => 90,
        v if v <= 105 => 100,
        v if v <= 117 => 110,
        _ => 125,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_normalization() {
        assert_eq!(normalize_gauge("MEM"), "mem");
        assert_eq!(normalize_gauge("off"), "none");
        assert_eq!(normalize_gauge("token"), "ai");
        // 老配置里的「跟随视图 / 网速 / CPU 温度」都收敛到内存占用
        assert_eq!(normalize_gauge("view"), "mem");
        assert_eq!(normalize_gauge("speed"), "mem");
        assert_eq!(normalize_gauge("cpu"), "mem");
        assert_eq!(normalize_gauge("whatever"), "mem");
    }

    #[test]
    fn refresh_normalization() {
        assert_eq!(normalize_refresh(300), 500);
        assert_eq!(normalize_refresh(1400), 1000);
        assert_eq!(normalize_refresh(9000), 2000);
    }

    #[test]
    fn font_scale_normalization() {
        assert_eq!(normalize_font_scale(80), 90);
        assert_eq!(normalize_font_scale(100), 100);
        assert_eq!(normalize_font_scale(112), 110);
        assert_eq!(normalize_font_scale(200), 125);
    }

    #[test]
    fn view_cycles() {
        let mut settings = Settings::default();
        settings.view = "speed".into();
        settings.cycle_view();
        assert_eq!(settings.view, "memory");
        settings.cycle_view();
        assert_eq!(settings.view, "temperature");
        settings.cycle_view();
        assert_eq!(settings.view, "ai");
        settings.cycle_view();
        assert_eq!(settings.view, "speed");
    }

    #[test]
    fn recording_fps_and_quality_normalization() {
        assert_eq!(normalize_fps(120), 120);
        assert_eq!(normalize_fps(90), 60);
        assert_eq!(normalize_fps(24), 30);
        assert_eq!(normalize_quality(80), 80);
        assert_eq!(normalize_quality(70), 65);
        assert_eq!(normalize_quality(52), 50);
        assert_eq!(normalize_quality(0), 65);
    }

    #[test]
    fn hotkey_roundtrip() {
        let (mods, vk) = parse_hotkey("Ctrl+Shift+A").unwrap();
        assert_eq!(mods, HOTKEY_MOD_CONTROL | HOTKEY_MOD_SHIFT);
        assert_eq!(vk, 0x41);
        assert_eq!(format_hotkey(mods, vk), "Ctrl+Shift+A");

        // 大小写、别名、功能键
        let (mods, vk) = parse_hotkey("alt + f5").unwrap();
        assert_eq!(mods, HOTKEY_MOD_ALT);
        assert_eq!(format_hotkey(mods, vk), "Alt+F5");

        let (mods, vk) = parse_hotkey("win+shift+s").unwrap();
        assert_eq!(format_hotkey(mods, vk), "Win+Shift+S");

        // 必须有修饰键，也不能有多余的键
        assert!(parse_hotkey("A").is_none());
        assert!(parse_hotkey("Ctrl+").is_none());
        assert!(parse_hotkey("Ctrl+A+B").is_none());
        assert!(parse_hotkey("Ctrl+F25").is_none());
        assert!(parse_hotkey("Ctrl+中文").is_none());
    }
}
