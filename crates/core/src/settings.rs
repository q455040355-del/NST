//! 设置持久化。
//!
//! 和 C# 版共用同一个 `%LOCALAPPDATA%\NetSpeedTaskbar\settings.ini`，
//! 这样迁移期间两份程序看到的是同一套配置（键名保持一致，写回时只改自己认得的键）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
    /// 录制时长上限（秒）：10 / 20 / 30
    pub gif_seconds: i32,
    /// GIF 录制热键
    pub gif_hotkey: String,
    /// 编辑窗是否置顶（记住上次的选择）
    pub shot_topmost: bool,
    /// 翻译开关（和 AI 统计一样：关着的时候菜单/工具栏不出现翻译，
    /// 设置页里的翻译区块也收起来）
    pub translate_enabled: bool,
    /// 翻译用的服务商：openai / deepseek（两家都是 OpenAI 兼容的 chat/completions）
    pub translate_provider: String,
    /// API Key（只存在本地 ini 里）
    pub translate_key: String,
    /// 模型名；空 = 用服务商默认值
    pub translate_model: String,
    /// Base URL；空 = 用服务商默认值
    pub translate_base: String,
    /// 译成什么语言："auto"（中→英、其它→中）/ "zh" / "en" / "ja" / "ko" / "sr"（塞尔维亚语拉丁）
    pub translate_target: String,
    /// 三家服务商各自的 Key / 模型 / Base：切换服务商时照着搬，切回来还是上次填的
    pub translate_profiles: [TranslateProfile; TRANSLATE_PROVIDERS.len()],
    /// 未识别的键原样保留，写回时补上，避免把 C# 版的设置弄丢。
    extra: BTreeMap<String, String>,
}

/// 一家服务商的一套翻译配置
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranslateProfile {
    pub key: String,
    pub model: String,
    pub base: String,
}

/// 服务商在 ini 里的名字（下标和设置页的三颗按钮一致）
pub const TRANSLATE_PROVIDERS: [&str; 2] = ["ChatGPT", "DeepSeek"];

impl Settings {
    /// 服务商名（"chatgpt"/"deepseek"/"opencode" 或显示名）→ 下标
    pub fn translate_provider_index(name: &str) -> usize {
        let name = name.trim().to_lowercase();
        match name.as_str() {
            "deepseek" => 1,
            _ => 0,
        }
    }
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
            gif_seconds: 20,
            gif_hotkey: "Ctrl+Shift+G".to_string(),
            shot_topmost: false,
            translate_enabled: false,
            translate_provider: "openai".to_string(),
            translate_key: String::new(),
            translate_model: String::new(),
            translate_base: String::new(),
            translate_target: "auto".to_string(),
            translate_profiles: Default::default(),
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
        Self::load_from(&Self::path())
    }

    /// 从指定文件读（测试用；正常走 load()）
    pub fn load_from(path: &Path) -> Self {
        let mut settings = Settings::default();
        let Ok(text) = fs::read_to_string(path) else {
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
                "GifSeconds" => {
                    settings.gif_seconds = normalize_seconds(value.trim().parse().unwrap_or(20))
                }
                "ShotTopmost" => settings.shot_topmost = parse_bool(value),
                "TranslateEnabled" => settings.translate_enabled = parse_bool(value),
                "TranslateProvider" => settings.translate_provider = value.to_string(),
                "TranslateKey" => settings.translate_key = value.to_string(),
                "TranslateModel" => settings.translate_model = value.to_string(),
                "TranslateBase" => settings.translate_base = value.to_string(),
                "TranslateTarget" => settings.translate_target = value.to_string(),
                "GifHotkey" => {
                    if let Some((mods, vk)) = parse_hotkey(value) {
                        settings.gif_hotkey = format_hotkey(mods, vk);
                    }
                }
                _ => {
                    // 每家一份：TranslateKey.ChatGPT / TranslateModel.OpenCode ...
                    let mut stored = false;
                    if let Some((field, provider)) = key.split_once('.') {
                        if matches!(field, "TranslateKey" | "TranslateModel" | "TranslateBase") {
                            let slot = Settings::translate_provider_index(provider);
                            if slot < TRANSLATE_PROVIDERS.len() {
                                let profile = &mut settings.translate_profiles[slot];
                                match field {
                                    "TranslateKey" => profile.key = value.to_string(),
                                    "TranslateModel" => profile.model = value.to_string(),
                                    _ => profile.base = value.to_string(),
                                }
                                stored = true;
                            }
                        }
                    }
                    if !stored {
                        settings.extra.insert(key.to_string(), value.to_string());
                    }
                }
            }
        }

        // 老配置没有 TranslateEnabled 这一行：填过 Key 就算「开着」，升级不丢功能
        if !text.contains("TranslateEnabled=") {
            settings.translate_enabled = !settings.translate_key.trim().is_empty();
        }

        // 老配置只有一份（没有 TranslateKey.xxx）：把它当成当前服务商的那份
        if settings
            .translate_profiles
            .iter()
            .all(|profile| profile.key.is_empty() && profile.model.is_empty() && profile.base.is_empty())
        {
            let index = Settings::translate_provider_index(&settings.translate_provider);
            settings.translate_profiles[index] = TranslateProfile {
                key: settings.translate_key.clone(),
                model: settings.translate_model.clone(),
                base: settings.translate_base.clone(),
            };
        }
        // 当前服务商那份和扁平字段对齐（扁平字段是「正在用的」，translate.rs 只读它）
        {
            let index = Settings::translate_provider_index(&settings.translate_provider);
            if let Some(profile) = settings.translate_profiles.get(index) {
                if !profile.key.is_empty() || !profile.model.is_empty() || !profile.base.is_empty() {
                    settings.translate_key = profile.key.clone();
                    settings.translate_model = profile.model.clone();
                    settings.translate_base = profile.base.clone();
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
        // 落盘之前，把「正在用的这套」同步进对应服务商那一格，
        // 这样切走再切回来还能拿到上次填的 Key / 模型
        let mut synced = self.clone();
        let index = Settings::translate_provider_index(&self.translate_provider);
        if index < TRANSLATE_PROVIDERS.len() {
            synced.translate_profiles[index] = TranslateProfile {
                key: self.translate_key.clone(),
                model: self.translate_model.clone(),
                base: self.translate_base.clone(),
            };
        }
        synced.save_to(&Self::path());
    }

    /// 写到指定文件（测试用；正常走 save()）
    pub fn save_to(&self, path: &Path) {
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
        out.push_str(&format!("GifSeconds={}\n", self.gif_seconds));
        out.push_str(&format!("GifHotkey={}\n", self.gif_hotkey));
        out.push_str(&format!("ShotTopmost={}\n", self.shot_topmost));
        out.push_str(&format!("TranslateEnabled={}\n", self.translate_enabled));
        out.push_str(&format!("TranslateProvider={}\n", self.translate_provider));
        out.push_str(&format!("TranslateKey={}\n", self.translate_key));
        out.push_str(&format!("TranslateModel={}\n", self.translate_model));
        out.push_str(&format!("TranslateBase={}\n", self.translate_base));
        out.push_str(&format!("TranslateTarget={}\n", self.translate_target));
        for (index, name) in TRANSLATE_PROVIDERS.iter().enumerate() {
            if let Some(profile) = self.translate_profiles.get(index) {
                out.push_str(&format!("TranslateKey.{name}={}\n", profile.key));
                out.push_str(&format!("TranslateModel.{name}={}\n", profile.model));
                out.push_str(&format!("TranslateBase.{name}={}\n", profile.base));
            }
        }

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
                    | "GifSeconds"
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

/// 录制帧率只认 0（自动）/ 30 / 60 / 120，别的值回落到 30
pub fn normalize_fps(fps: i32) -> i32 {
    if fps <= 0 {
        0
    } else if fps >= 120 {
        120
    } else if fps >= 60 {
        60
    } else {
        30
    }
}

/// 录制时长只认 10 / 20 / 30 秒，别的值回落到 20
pub fn normalize_seconds(seconds: i32) -> i32 {
    if seconds >= 30 {
        30
    } else if seconds >= 20 {
        20
    } else if seconds >= 10 {
        10
    } else {
        20
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
        // 0 = 自动（交给录制时的吞吐上限 + 闭环）
        assert_eq!(normalize_fps(0), 0);
        assert_eq!(normalize_fps(-5), 0);
        assert_eq!(normalize_quality(80), 80);
        assert_eq!(normalize_quality(70), 65);
        assert_eq!(normalize_quality(52), 50);
        assert_eq!(normalize_quality(0), 65);
        assert_eq!(normalize_seconds(30), 30);
        assert_eq!(normalize_seconds(21), 20);
        assert_eq!(normalize_seconds(9), 20);
        assert_eq!(normalize_seconds(15), 10);
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

#[cfg(test)]
mod roundtrip_tests {
    use super::*;

    /// 每个字段都填一个和默认值不同的值，用来验证「保存 → 读回」不丢东西
    fn sample() -> Settings {
        let mut settings = Settings::default();
        settings.panel_width = 137;
        settings.panel_x = 42;
        settings.panel_y = -7;
        settings.network_interface = "以太网 2".to_string();
        settings.gauge_source = "ai".to_string();
        settings.refresh_ms = 2000;
        settings.font_scale = 125;
        settings.view = "cpu".to_string();
        settings.ai_enabled = true;
        settings.ai_opencode = true;
        settings.ai_claude = true;
        settings.ai_codex = true;
        settings.ai_dsh = true;
        settings.ai_workbuddy = true;
        settings.ai_workbuddy_ai = true;
        settings.auto_start = true;
        settings.shot_enabled = false;
        settings.ocr_enabled = true;
        settings.shot_hotkey = "Ctrl+Alt+Q".to_string();
        settings.ocr_hotkey = "Ctrl+Alt+W".to_string();
        settings.gif_enabled = true;
        settings.gif_fps = 60;
        settings.gif_quality = 80;
        settings.gif_seconds = 10;
        settings.gif_hotkey = "Ctrl+Alt+E".to_string();
        settings.shot_topmost = true;
        settings.translate_provider = "deepseek".to_string();
        settings.translate_key = "sk-test-123".to_string();
        settings.translate_model = "deepseek-chat".to_string();
        settings.translate_base = "https://api.example.com/v1".to_string();
        settings
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("nst-settings-test-{}-{}", std::process::id(), name));
        dir.push("settings.ini");
        dir
    }

    #[test]
    fn fields_survive_a_save_and_load_roundtrip() {
        let path = temp_path("roundtrip");
        let original = sample();
        original.save_to(&path);

        let loaded = Settings::load_from(&path);
        let _ = fs::remove_dir_all(path.parent().unwrap());

        // 逐字段比较，失败时能直接看出是哪个字段丢了
        assert_eq!(loaded.panel_width, original.panel_width, "panel_width");
        assert_eq!(loaded.panel_x, original.panel_x, "panel_x");
        assert_eq!(loaded.panel_y, original.panel_y, "panel_y");
        assert_eq!(loaded.network_interface, original.network_interface, "network_interface");
        assert_eq!(loaded.gauge_source, original.gauge_source, "gauge_source");
        assert_eq!(loaded.refresh_ms, original.refresh_ms, "refresh_ms");
        assert_eq!(loaded.font_scale, original.font_scale, "font_scale");
        assert_eq!(loaded.view, original.view, "view");
        assert_eq!(loaded.ai_enabled, original.ai_enabled, "ai_enabled");
        assert_eq!(loaded.ai_opencode, original.ai_opencode, "ai_opencode");
        assert_eq!(loaded.ai_claude, original.ai_claude, "ai_claude");
        assert_eq!(loaded.ai_codex, original.ai_codex, "ai_codex");
        assert_eq!(loaded.ai_dsh, original.ai_dsh, "ai_dsh");
        assert_eq!(loaded.ai_workbuddy, original.ai_workbuddy, "ai_workbuddy");
        assert_eq!(loaded.ai_workbuddy_ai, original.ai_workbuddy_ai, "ai_workbuddy_ai");
        assert_eq!(loaded.auto_start, original.auto_start, "auto_start");
        assert_eq!(loaded.shot_enabled, original.shot_enabled, "shot_enabled");
        assert_eq!(loaded.ocr_enabled, original.ocr_enabled, "ocr_enabled");
        assert_eq!(loaded.shot_hotkey, original.shot_hotkey, "shot_hotkey");
        assert_eq!(loaded.ocr_hotkey, original.ocr_hotkey, "ocr_hotkey");
        assert_eq!(loaded.gif_enabled, original.gif_enabled, "gif_enabled");
        assert_eq!(loaded.gif_fps, original.gif_fps, "gif_fps");
        assert_eq!(loaded.gif_quality, original.gif_quality, "gif_quality");
        assert_eq!(loaded.gif_seconds, original.gif_seconds, "gif_seconds");
        assert_eq!(loaded.gif_hotkey, original.gif_hotkey, "gif_hotkey");
        assert_eq!(loaded.shot_topmost, original.shot_topmost, "shot_topmost");
        assert_eq!(loaded.translate_provider, original.translate_provider, "translate_provider");
        assert_eq!(loaded.translate_key, original.translate_key, "translate_key");
        assert_eq!(loaded.translate_model, original.translate_model, "translate_model");
        assert_eq!(loaded.translate_base, original.translate_base, "translate_base");
    }

    #[test]
    fn unknown_keys_are_kept_for_the_next_version() {
        // 别的版本写进去、当前程序不认识的键要原样留着，不能被覆盖丢掉
        let path = temp_path("extra");
        let mut settings = Settings::default();
        settings.save_to(&path);

        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("SomeFutureKey=hello\n");
        fs::write(&path, text).unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.extra.get("SomeFutureKey").map(String::as_str), Some("hello"));

        // 再存一次也不能把它弄丢
        loaded.save_to(&path);
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_dir_all(path.parent().unwrap());
        assert!(text.contains("SomeFutureKey=hello"), "未知键要保留");
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        // 第一次运行（没有 ini）不能 panic，也不能读到半个配置
        let path = temp_path("missing");
        let loaded = Settings::load_from(&path);
        let defaults = Settings::default();
        assert_eq!(loaded.panel_width, defaults.panel_width);
        assert_eq!(loaded.refresh_ms, defaults.refresh_ms);
    }

    #[test]
    fn broken_values_fall_back_instead_of_dropping_everything() {
        // 某一行值坏了：该字段回默认，其余字段照常读出来
        let path = temp_path("broken");
        let mut settings = Settings::default();
        settings.panel_x = 77;
        settings.save_to(&path);

        let text = fs::read_to_string(&path).unwrap();
        let broken = text.replace(
            &format!("PanelWidth={}\n", settings.panel_width),
            "PanelWidth=not-a-number\n",
        );
        fs::write(&path, broken).unwrap();

        let loaded = Settings::load_from(&path);
        let _ = fs::remove_dir_all(path.parent().unwrap());
        assert_eq!(loaded.panel_width, Settings::default().panel_width, "坏值回默认");
        assert_eq!(loaded.panel_x, 77, "其它字段不受影响");
    }
}
