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
        out.push_str("SettingsVersion=6\n");
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
            ) {
                continue;
            }
            out.push_str(&format!("{key}={value}\n"));
        }

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
}
