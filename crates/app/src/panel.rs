//! 任务栏子窗口面板：GDI 绘制、视图切换、指示条。

use netspeed_core::ai::{
    format_speed as format_ai_speed, format_tokens, AiEngine, AiSnapshot, Sources,
};
use netspeed_core::memory::read_memory;
use netspeed_core::temperature::{append_watts, format_temperature as format_celsius, TemperatureMonitor};
use netspeed_core::net::{NetSample, NetSampler};
use netspeed_core::settings::{Settings, GAUGE_AI, GAUGE_MEMORY};
use netspeed_core::{format_bytes_gb, format_speed, MemorySnapshot};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC,
    DeleteObject, FillRect, GetDC, GetPixel, GetTextExtentPoint32W, InvalidateRect,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, DT_LEFT,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowRect, SetWindowPos, UpdateLayeredWindow, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER,
};

// 分层窗口常量（windows-sys 没在这个 feature 集里导出）
const ULW_ALPHA: u32 = 0x0000_0002;
const AC_SRC_OVER: u8 = 0x00;
const AC_SRC_ALPHA: u8 = 0x01;

#[repr(C)]
struct BlenFunction {
    blend_op: u8,
    blend_flags: u8,
    source_constant_alpha: u8,
    alpha_format: u8,
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::theme::{taskbar_is_light, wide, Colors, Fonts};

pub const VIEW_SPEED: &str = "speed";

pub const VIEW_MEMORY: &str = "memory";
pub const VIEW_TEMPERATURE: &str = "temperature";
pub const VIEW_AI: &str = "ai";

pub struct Panel {
    pub hwnd: HWND,
    pub settings: Settings,
    sampler: NetSampler,
    sample: NetSample,
    memory: MemorySnapshot,
    up_text: String,
    down_text: String,
    used_text: String,
    total_text: String,
    temp_text: String,
    temp_gpu_text: String,
    temperatures: Option<TemperatureMonitor>,
    ai_speed_text: String,
    ai_today_text: String,
    ai: AiWorker,
    ai_snapshot: AiSnapshot,
    dpi: u32,
    colors: Colors,
    fonts: Fonts,
    back_dc: HDC,
    back_bitmap: HBITMAP,
    back_bits: *mut u8,
    width: i32,
    height: i32,
    gauge: f32,
    /// 指示条的目标值（动画缓动到它）
    gauge_target: f32,
    /// 上一帧动画的时间，用来算缓动步长
    last_anim: std::time::Instant,
    /// 上一次真正采样/重绘的时间（用来按设置的间隔限流）
    last_refresh: std::time::Instant,
    /// 任务栏底色缓存：每帧 8 次 GetPixel 读屏没必要，位置没变就复用
    bg_color: u32,
    bg_rect: (i32, i32, i32, i32),
    bg_at: std::time::Instant,
    /// 上一次真正画出来的内容指纹：一样就不重绘（数字常几十秒不动）
    last_frame: String,
}

impl Panel {
    pub fn new(hwnd: HWND, settings: Settings) -> Self {
        let dpi = crate::dpi_for_window(hwnd);
        // 采样节拍跟着设置走（0.5s / 1s / 2s）
        crate::set_tick_interval(settings.refresh_ms);
        let light = taskbar_is_light();
        let colors = Colors::for_theme(light, 0x00F3F3F3);

        let font_scale = settings.font_scale;
        let ai_sources = sources_of(&settings);

        let mut panel = Self {
            hwnd,
            sampler: NetSampler::new(&settings.network_interface),
            settings,
            sample: NetSample::default(),
            memory: MemorySnapshot::default(),
            up_text: format_speed(0.0),
            down_text: format_speed(0.0),
            used_text: "--".to_string(),
            total_text: "--".to_string(),
            temp_text: "--".to_string(),
            temp_gpu_text: "--".to_string(),
            temperatures: None,
            ai_speed_text: "0.0 t/s".to_string(),
            ai_today_text: "0".to_string(),
            ai: AiWorker::new(ai_sources),
            ai_snapshot: AiSnapshot::default(),
            dpi,
            colors,
            fonts: Fonts::create(dpi, font_scale),
            back_dc: std::ptr::null_mut(),
            back_bitmap: std::ptr::null_mut(),
            back_bits: std::ptr::null_mut(),
            width: 0,
            height: 0,
            gauge: 0.0,
            gauge_target: 0.0,
            last_anim: std::time::Instant::now(),
            last_refresh: std::time::Instant::now(),
            bg_color: 0,
            bg_rect: (0, 0, 0, 0),
            bg_at: std::time::Instant::now() - std::time::Duration::from_secs(10),
            last_frame: String::new(),
        };

        panel.ensure_buffer();
        // 设置里开了 AI 才起采集线程（关掉时连线程都不存在）
        if panel.settings.ai_enabled {
            panel.ai.start();
        }
        panel.refresh();
        panel
    }

    pub fn height(&self) -> i32 {
        scale(self.dpi, 40)
    }

    pub fn desired_width(&self) -> i32 {
        scale(self.dpi, self.settings.panel_width)
    }

    /// 计时器里调用：更新采样值并重绘。
    pub fn tick(&mut self) {
        // 计时器本身每 500ms 响一次，这里是「多久真正采一次样」的闸门。
        // 留 60ms 的余量，免得 500ms 的节拍抖动把这一帧跳过。
        let interval = self.settings.refresh_ms.max(50) as u128;
        let elapsed = self.last_refresh.elapsed().as_millis();
        if elapsed + 60 < interval {
            return;
        }

        let started = std::time::Instant::now();
        self.last_refresh = started;

        self.refresh();
        let refresh_us = started.elapsed().as_micros();

        // 内容没变就不重绘：网速/内存这些数字常常几十秒才动一次，
        // 以前每 500ms 无条件整窗重画一遍（分层窗口还要重新提交位图）
        let signature = self.frame_signature();
        if signature != self.last_frame {
            self.last_frame = signature;
            unsafe {
                InvalidateRect(self.hwnd, std::ptr::null(), 0);
            }
        }

        if refresh_us > 5_000 {
            crate::debug_log(&format!("tick refresh={refresh_us}us"));
        }
    }

    /// 面板上所有可见内容的指纹（数字文本 + 视图 + 指示条目标 + 明暗主题）
    fn frame_signature(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{:.1}|{}",
            self.settings.view,
            self.up_text,
            self.down_text,
            self.used_text,
            self.total_text,
            self.temp_text,
            self.temp_gpu_text,
            self.ai_speed_text,
            self.ai_today_text,
            self.gauge_target,
            crate::theme::taskbar_is_light() as u8,
        )
    }

    /// 改刷新间隔（毫秒）：立刻生效，下一次 tick 就会采一次样。
    pub fn set_refresh_ms(&mut self, ms: i32) {
        self.settings.refresh_ms = netspeed_core::settings::normalize_refresh(ms);
        self.settings.save();
        crate::set_tick_interval(self.settings.refresh_ms);
        self.last_refresh = std::time::Instant::now() - std::time::Duration::from_secs(10);
        self.tick();
    }

    pub fn cycle_view(&mut self) {
        // AI 关着就跳过 AI 视图
        loop {
            self.settings.cycle_view();
            if self.settings.view != VIEW_AI || self.settings.ai_enabled {
                break;
            }
        }
        self.settings.save();
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    pub fn set_view(&mut self, view: &str) {
        // 没开 AI 统计就不允许切到 AI 视图
        if view == VIEW_AI && !self.settings.ai_enabled {
            return;
        }
        self.settings.view = view.to_string();
        self.settings.save();
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 切换要统计的网卡（"auto" = 全部合计）
    pub fn set_network_interface(&mut self, interface: &str) {
        self.settings.network_interface = interface.to_string();
        self.settings.save();
        self.sampler.set_interface(interface);
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    pub fn set_gauge(&mut self, source: &str) {
        self.settings.gauge_source = source.to_string();
        self.settings.save();
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    pub fn gauge_source(&self) -> &str {
        &self.settings.gauge_source
    }

    /// AI 统计总开关：开了才起采集线程（常驻记录），关了立刻停。
    pub fn set_ai_enabled(&mut self, enabled: bool) {
        self.settings.ai_enabled = enabled;
        if enabled {
            self.ai.start();
        } else {
            self.ai.stop();
            // 关掉 AI 后不能停在 AI 视图 / AI 指示条上
            if self.settings.view == VIEW_AI {
                self.settings.view = VIEW_SPEED.to_string();
            }
            if self.settings.gauge_source == GAUGE_AI {
                self.settings.gauge_source = GAUGE_MEMORY.to_string();
            }
            self.ai_snapshot = AiSnapshot::default();
            self.ai_speed_text = format_ai_speed(0.0);
            self.ai_today_text = format_tokens(0);
        }
        self.settings.save();
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 采集哪些来源（OpenCode / dsh / WorkBuddy / WorkBuddy AI）
    pub fn set_ai_sources(&mut self, flags: [bool; 6]) {
        self.settings.ai_opencode = flags[0];
        self.settings.ai_dsh = flags[1];
        self.settings.ai_workbuddy = flags[2];
        self.settings.ai_workbuddy_ai = flags[3];
        self.settings.ai_claude = flags[4];
        self.settings.ai_codex = flags[5];
        self.ai.set_sources(sources_of(&self.settings));
        self.settings.save();
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 改字号（百分比）：重建字体并重绘。
    pub fn set_font_scale(&mut self, percent: i32) {        self.settings.font_scale = netspeed_core::settings::normalize_font_scale(percent);
        self.settings.save();
        self.fonts = Fonts::create(self.dpi, self.settings.font_scale);
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 取消拖动定位，重新贴回任务栏
    pub fn reset_anchor(&mut self) {
        self.settings.panel_x = -1;
        self.settings.panel_y = -1;
        self.settings.save();
    }

    /// 改面板宽度：更新设置、保存、立刻改窗口尺寸并重绘。
    /// 拖动滑杆时用：只改内存里的宽度 + 重排窗口，不落盘。
    /// 一次拖动会经过几十个 4px 档位，每档都写一遍 ini 没必要。
    pub fn preview_panel_width(&mut self, width: i32) {
        let width = width.clamp(72, 360);
        if self.settings.panel_width == width {
            return;
        }
        self.settings.panel_width = width;
        unsafe {
            SetWindowPos(
                self.hwnd,
                std::ptr::null_mut(),
                0,
                0,
                self.desired_width(),
                self.height(),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 拖动结束：把当前宽度落一次盘
    pub fn commit_panel_width(&mut self) {
        self.settings.save();
    }

    /// 托盘悬浮提示：和面板上看到的内容一致（太长会被系统截断）。
    pub fn tip_text(&self) -> String {
        match self.settings.view.as_str() {
            VIEW_MEMORY => format!("内存 {} / {}", self.used_text, self.total_text),
            VIEW_TEMPERATURE => format!("CPU {}  GPU {}", self.temp_text, self.temp_gpu_text),
            VIEW_AI => format!("AI {}  今日 {}", self.ai_speed_text, self.ai_today_text),
            _ => format!("网速 上 {}  下 {}", self.up_text, self.down_text),
        }
    }

    pub fn view(&self) -> &str {
        &self.settings.view
    }

    fn refresh(&mut self) {
        self.sample = self.sampler.sample();
        self.up_text = format_speed(self.sample.up_bytes_per_sec);
        self.down_text = format_speed(self.sample.down_bytes_per_sec);

        // 指示条盯内存时，即使当前视图不是内存视图也要采一次
        let need_memory =
            self.settings.view == VIEW_MEMORY || self.settings.gauge_source == GAUGE_MEMORY;
        if need_memory {
            self.memory = read_memory();
            self.used_text = format_bytes_gb(self.memory.used_bytes());
            self.total_text = format_bytes_gb(self.memory.total_bytes);
        }

        if self.settings.view == VIEW_TEMPERATURE {
            if self.temperatures.is_none() {
                self.temperatures = Some(TemperatureMonitor::start());
            }

            if let Some(monitor) = self.temperatures.as_ref() {
                let snapshot = monitor.read();
                self.temp_text = append_watts(format_celsius(snapshot.cpu_celsius), f64::NAN);
                self.temp_gpu_text =
                    append_watts(format_celsius(snapshot.gpu_celsius), snapshot.gpu_watts);
            }
        }

        // AI：设置里开着就一直采（常驻记录），关着就什么都不做
        if self.settings.ai_enabled {
            self.ai_snapshot = self.ai.snapshot();
            self.ai_speed_text = format_ai_speed(self.ai_snapshot.speed);
            self.ai_today_text = format_tokens(self.ai_snapshot.today_tokens);
        } else {
            self.ai_snapshot = AiSnapshot::default();
            self.ai_speed_text = format_ai_speed(0.0);
            self.ai_today_text = format_tokens(0);
        }

        self.gauge_target = self.compute_gauge();
        if (self.gauge_target - self.gauge).abs() > 0.001 {
            crate::request_anim(true);
        }
    }

    /// 推进一帧指示条动画（缓动到目标值）；返回 true 表示还要下一帧。
    pub fn animate(&mut self) -> bool {
        let now = std::time::Instant::now();
        let dt = now
            .duration_since(self.last_anim)
            .as_secs_f32()
            .clamp(0.001, 0.05);
        self.last_anim = now;

        // 面板被藏起来（全屏程序 / 截图）就别白跑了，直接跳到目标值
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.hwnd) } == 0 {
            self.gauge = self.gauge_target;
            return false;
        }

        let diff = self.gauge_target - self.gauge;
        if diff.abs() <= 0.002 {
            if self.gauge != self.gauge_target {
                self.gauge = self.gauge_target;
                unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
            }
            return false;
        }

        // 指数逼近：时间常数 120ms，小波动看起来是滑动而不是跳变
        let factor = 1.0 - (-dt / 0.12).exp();
        self.gauge += diff * factor;
        unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        true
    }

    /// 指示条当前代表的指标（给悬浮说明用）
    pub fn gauge_hint(&self) -> String {
        match self.settings.gauge_source.as_str() {
            GAUGE_AI => {
                // 只统计勾选的来源：数字和历史窗口对不上时，这里说清楚为什么
                let enabled = [
                    self.settings.ai_opencode,
                    self.settings.ai_dsh,
                    self.settings.ai_workbuddy,
                    self.settings.ai_workbuddy_ai,
                    self.settings.ai_claude,
                    self.settings.ai_codex,
                ]
                .iter()
                .filter(|flag| **flag)
                .count();

                if enabled < 6 {
                    format!(
                        "Token {} · 今日 {}（{enabled} / 6 来源）",
                        self.ai_speed_text, self.ai_today_text
                    )
                } else {
                    format!("Token {} · 今日 {}", self.ai_speed_text, self.ai_today_text)
                }
            }
            GAUGE_MEMORY => format!(
                "内存 {:.0}%（{} / {}）",
                self.memory.used_ratio() * 100.0,
                self.used_text,
                self.total_text
            ),
            _ => String::new(),
        }
    }

    /// 指示条的悬浮热区（屏幕坐标）；指示条关掉时返回 None
    pub fn gauge_hot_rect(&self) -> Option<RECT> {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(self.hwnd, &mut rect);
        }
        self.gauge_band(&rect)
    }

    /// 底部指示条占的那一条（屏幕坐标）。
    ///
    /// 只算「条本体 + 上下各 2px」：以前一律取底边往上 12px，会把下载行
    /// 最下面那几个像素抢走——鼠标刚移到「下」字下面，弹出来的却是内存说明。
    fn gauge_band(&self, frame: &RECT) -> Option<RECT> {
        if !self.settings.gauge_visible() {
            return None;
        }
        let bar = scale(self.dpi, 3);
        let gap = scale(self.dpi, 3);
        let top = frame.bottom - bar - gap - scale(self.dpi, 2);
        if top <= frame.top {
            return None;
        }
        Some(RECT {
            left: frame.left,
            top,
            right: frame.right,
            bottom: frame.bottom,
        })
    }

    /// 网速视图里第 `index` 行（0 = 上传，1 = 下载）的屏幕热区。
    ///
    /// 行高和 `row()` 里算的一模一样：面板高度减掉底部指示条，再对半分。
    pub fn speed_row_hot_rect(&self, index: usize) -> Option<RECT> {
        if self.settings.view != VIEW_SPEED || index > 1 {
            return None;
        }

        let mut frame = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(self.hwnd, &mut frame);
        }

        let gauge = if self.settings.gauge_visible() { scale(self.dpi, 3) } else { 0 };
        let row_height = ((frame.bottom - frame.top) - gauge) / 2;
        if row_height <= 0 {
            return None;
        }

        let top = frame.top + index as i32 * row_height;
        let mut bottom = top + row_height;
        // 下载行的下半截和底部指示条重叠：让给指示条，别一格热区两套说法
        if let Some(band) = self.gauge_band(&frame) {
            if top < band.top {
                bottom = bottom.min(band.top);
            }
        }
        Some(RECT {
            left: frame.left,
            top,
            right: frame.right,
            bottom,
        })
    }

    /// 悬停上传 / 下载时的小卡片：当前速度 + 前几名进程。
    ///
    /// 进程明细要提权（ETW 内核网络事件），所以这里把「为什么没有数据」
    /// 也照实写在卡片里，而不是弹个空气泡或者假装 0。
    pub fn process_hint(&self, up: bool) -> crate::hint::Hint {
        use crate::hint::Hint;
        use crate::netproc::{self, Status};
        use netspeed_core::netproc::MIN_RATE;

        let (speed, direction) = if up {
            (&self.up_text, "上传")
        } else {
            (&self.down_text, "下载")
        };
        let title = format!("{direction} {speed}");
        let tag = if up { "netproc-up" } else { "netproc-down" };

        if !self.settings.netproc_enabled && !netproc::mock() {
            // 关着的时候至少把「为什么没有进程名」说清楚：一句话引导去菜单里打开
            return Hint::new()
                .tag(tag)
                .title(title)
                .text("右键 → 进程明细（需一次管理员授权）");
        }

        let card = Hint::new().tag(tag).title(title);
        let (status, note) = netproc::status();
        match status {
            Status::Off => return Hint::new(),
            Status::Denied => return card.text(note).text("右键 → 进程明细 可重试"),
            Status::Failed => return card.text(note).text("右键 → 重新连接进程明细"),
            Status::Starting => {
                return card.text(if note.is_empty() {
                    "正在等管理员授权的助手…".to_string()
                } else {
                    note
                });
            }
            Status::Running => {}
        }

        let Some((report, _)) = netproc::latest() else {
            return card.text("助手没回数据（右键 → 进程明细 重试）");
        };

        let (items, total) = if up {
            (&report.up, report.up_total)
        } else {
            (&report.down, report.down_total)
        };

        // 面板上只放得下三行 + 一行「其他」：多的并到「其他」里。
        // 明细只列 0.1 M/s 以上的（助手那边就滤过了）；「其他」够门槛才露脸。
        // 分母跟着变：有「其他」时按全部流量算，没有时按列出来的这些算，
        // 保证画出来的百分比总是加起来 100%，不会出现「少了 40% 却没说去哪了」。
        let listed: Vec<&(String, f64)> = items.iter().take(3).collect();
        let shown: f64 = listed.iter().map(|(_, rate)| *rate).sum();
        let rest = total - shown;
        let show_rest = rest >= MIN_RATE;
        let denominator = if show_rest { total } else { shown };

        let mut card = card;
        for (name, rate) in &listed {
            let value = format!("{}  {:.0}%", format_speed(*rate), share(*rate, denominator));
            card = card.pair(shorten_name(name), value);
        }
        if show_rest {
            let value = format!("{}  {:.0}%", format_speed(rest), share(rest, denominator));
            card = card.pair("其他", value);
        }
        if card.lines.len() == 1 {
            // 这一秒全是零碎流量：说清楚为什么没有名字，别只留一个空标题
            card = card.text(format!("刚才没有超过 {} 的进程", format_speed(MIN_RATE)));
        }
        card
    }

    fn compute_gauge(&self) -> f32 {
        match self.settings.gauge_source.as_str() {
            // 内存占用百分比
            GAUGE_MEMORY => self.memory.used_ratio().clamp(0.0, 1.0),
            // Token 速度：0 ~ 250 t/s 铺满整条
            GAUGE_AI => (self.ai_snapshot.speed / 250.0).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        }
    }

    /// 面板被拖到另一块显示器（或那块屏的缩放比例被改了）：
    /// 字号、尺寸、缓冲区都要按新 DPI 重来，否则字会糊/面板大小不对。
    ///
    /// 尺寸由调用方按 `desired_width()` / `height()` 重新 SetWindowPos；
    /// 这里只把 DPI / 字体 / 配色换掉，并把缓冲区标记为待重建。
    pub fn set_dpi(&mut self, dpi: u32) {
        if self.dpi == dpi {
            return;
        }
        self.dpi = dpi;
        self.fonts = Fonts::create(dpi, self.settings.font_scale);
        self.colors = Colors::for_theme(crate::theme::taskbar_is_light(), 0x00F3F3F3);
        // 尺寸马上会变：底色缓存和内容指纹都得作废，缓冲区下次绘制按新尺寸重建
        self.bg_rect = (0, 0, 0, 0);
        self.bg_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        self.last_frame.clear();
        self.width = 0;
        self.height = 0;
    }

    /// 系统主题 / 任务栏外观变了：底色缓存作废，下一帧重新采样
    pub fn forget_background(&mut self) {
        self.bg_rect = (0, 0, 0, 0);
        self.bg_at = std::time::Instant::now() - std::time::Duration::from_secs(3600);
    }

    fn ensure_buffer(&mut self) {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe {
            GetClientRect(self.hwnd, &mut rect);
        }
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);

        if self.back_dc.is_null() || width != self.width || height != self.height {
            unsafe {
                if !self.back_dc.is_null() {
                    DeleteDC(self.back_dc);
                    DeleteObject(self.back_bitmap as *mut _);
                }

                // 用 DIB section 而不是 CreateCompatibleBitmap：后者的 alpha 通道恒为 0，
                // DWM 合成时会把 GDI 画的文字当成“透明”而显得极淡
                let screen = GetDC(std::ptr::null_mut());
                self.back_dc = CreateCompatibleDC(screen);

                let mut info: BITMAPINFO = std::mem::zeroed();
                info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                info.bmiHeader.biWidth = width;
                info.bmiHeader.biHeight = -height; // top-down
                info.bmiHeader.biPlanes = 1;
                info.bmiHeader.biBitCount = 32;
                info.bmiHeader.biCompression = 0; // BI_RGB

                let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
                self.back_bitmap = CreateDIBSection(
                    screen,
                    &info,
                    0,
                    &mut bits,
                    std::ptr::null_mut(),
                    0,
                );
                self.back_bits = bits as *mut u8;
                SelectObject(self.back_dc, self.back_bitmap as HGDIOBJ);
                ReleaseDC(std::ptr::null_mut(), screen);
            }

            self.width = width;
            self.height = height;
        }
    }

    /// 分层窗口：把背缓冲整块推给 DWM（不依赖 WM_PAINT，任务栏重绘也刷不掉）。
    pub fn paint(&mut self) {
        self.render(std::ptr::null_mut());
    }

    /// 画到任意 DC：WM_PAINT 和 WM_PRINTCLIENT（截图/打印）共用。
    pub fn render(&mut self, hdc: HDC) {
        unsafe {
            self.ensure_buffer();
            // 采样任务栏底色（取面板左右两侧的空隙，避开图标）。
            // 动画时一帧一次太亏：位置没变就复用 2 秒内的结果（主题切换靠 TTL 兜底）
            let mut frame = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetWindowRect(self.hwnd, &mut frame);
            let key = (frame.left, frame.top, frame.right, frame.bottom);
            let now = std::time::Instant::now();
            // 任务栏是半透明的：面板旁边那几个像素会随壁纸、悬停高亮、后面的窗口变来变去。
            // 以前每 2 秒重新采样一次，于是面板颜色会「时不时闪一下」。
            // 现在只有「面板挪了位置」或「过了很久」才采样，而且颜色差得不多就不认
            // （主题切换是明暗互换，差值很大，不会被这道门槛挡住）。
            let moved = key != self.bg_rect;
            let stale = now.duration_since(self.bg_at).as_secs() >= 30;
            if moved || stale {
                let light = taskbar_is_light();
                let fallback = if light { 0x00F3F3F3 } else { 0x001F1F1F };
                let sampled = sample_taskbar_color(self.hwnd).unwrap_or(fallback);
                // 没采过样（0）就用这次的；之后差得不够多就不认，避免面板颜色自己抖
                if self.bg_color == 0 || color_far_enough(sampled, self.bg_color) {
                    self.bg_color = sampled;
                }
                self.bg_rect = key;
                self.bg_at = now;
            }

            let light = taskbar_is_light();
            if self.colors.background != self.bg_color {
                self.colors = Colors::for_theme(light, self.bg_color);
            }


            let dc = self.back_dc;
            let width = self.width;
            let height = self.height;

            fill(dc, 0, 0, width, height, self.colors.background);

            match self.settings.view.as_str() {
                VIEW_MEMORY => {
                    self.row(dc, 0, "用", &self.used_text, false);
                    self.row(dc, 1, "总", &self.total_text, true);
                }
                VIEW_TEMPERATURE => {
                    self.row(dc, 0, "CPU", &self.temp_text, false);
                    self.row(dc, 1, "GPU", &self.temp_gpu_text, true);
                }
                VIEW_AI => {
                    self.row(dc, 0, "AI", &self.ai_speed_text, false);
                    self.row(dc, 1, "今日", &self.ai_today_text, true);
                }
                _ => {
                    self.row(dc, 0, "上", &self.up_text, false);
                    self.row(dc, 1, "下", &self.down_text, true);
                }
            }

            if self.settings.gauge_visible() {
                self.draw_gauge(dc, width, height);
            }


            // DIB 全量把 alpha 置 255，DWM 才会按不透明合成
            if !self.back_bits.is_null() {
                let count = (width * height) as usize;
                for index in 0..count {
                    *self.back_bits.add(index * 4 + 3) = 255;
                }
            }

            if hdc.is_null() {
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetWindowRect(self.hwnd, &mut rect);

                let destination = POINT {
                    x: rect.left,
                    y: rect.top,
                };
                let size = SIZE {
                    cx: width,
                    cy: height,
                };
                let source = POINT { x: 0, y: 0 };
                let blend = BlenFunction {
                    blend_op: AC_SRC_OVER,
                    blend_flags: 0,
                    source_constant_alpha: 255,
                    alpha_format: AC_SRC_ALPHA,
                };

                UpdateLayeredWindow(
                    self.hwnd,
                    std::ptr::null_mut(),
                    &destination,
                    &size,
                    dc,
                    &source,
                    0,
                    &blend as *const BlenFunction as *const _,
                    ULW_ALPHA,
                );
            } else {
                BitBlt(hdc, 0, 0, width, height, dc, 0, 0, SRCCOPY);
            }
        }
    }

    fn row(&self, dc: HDC, index: i32, label: &str, value: &str, is_down: bool) {
        let height = self.height;
        let gauge = if self.settings.gauge_visible() { scale(self.dpi, 3) } else { 0 };
        let row_height = (height - gauge) / 2;
        let top = index * row_height;
        let pad = scale(self.dpi, 7);

        let color = if is_down { self.colors.down } else { self.colors.up };
        unsafe {
            SetBkMode(dc, TRANSPARENT as i32);

            // 标签列不再写死 20px：雅黑比 Consolas 宽，写死会把 CPU/GPU 切掉
            let gap = scale(self.dpi, 4);
            let available = self.width - pad * 2;
            let mut label_font = self.fonts.label;
            let mut value_font = self.fonts.value;
            for (candidate_label, candidate_value) in [
                (self.fonts.label, self.fonts.value),
                (self.fonts.label, self.fonts.value_mid),
                (self.fonts.label_small, self.fonts.value_mid),
                (self.fonts.label_small, self.fonts.value_small),
            ] {
                let label_w = text_width(dc, label, candidate_label);
                let value_w = text_width(dc, value, candidate_value);
                label_font = candidate_label;
                value_font = candidate_value;
                if label_w + value_w + gap <= available {
                    break;
                }
            }

            let label_w = text_width(dc, label, label_font);
            let value_w = text_width(dc, value, value_font);
            if label_w + value_w + gap > available {
                // 最小的一档还塞不下（字号调大 + 面板又窄）：按比例继续缩，
                // 宁可字小一点也别被 DrawText 裁掉半截
                let ratio = (available as f32 / (label_w + value_w + gap) as f32).clamp(0.6, 1.0);
                let percent = self.settings.font_scale as f32 / 100.0;
                let scale = self.dpi as f32 / 96.0;
                label_font = crate::ui::font(9.5 * percent * ratio, 700, scale);
                value_font = crate::ui::font(11.0 * percent * ratio, 400, scale);
            }
            let font = SelectObject(dc, label_font as HGDIOBJ);
            SetTextColor(dc, color);
            draw_text(
                dc,
                &wide(label),
                pad,
                top,
                pad + label_w + 2,
                top + row_height,
                DT_LEFT,
            );

            SelectObject(dc, value_font as HGDIOBJ);
            SetTextColor(dc, self.colors.value);
            draw_text(
                dc,
                &wide(value),
                pad + label_w + gap,
                top,
                self.width - pad,
                top + row_height,
                DT_RIGHT,
            );
            SelectObject(dc, font);
        }
    }

    fn draw_gauge(&self, dc: HDC, width: i32, height: i32) {
        let pad = scale(self.dpi, 6);
        let bar_height = scale(self.dpi, 3);
        let top = height - bar_height - scale(self.dpi, 3);
        let track_width = width - pad * 2;

        unsafe {
            let track = CreateSolidBrush(blend(self.colors.background, 0x0000_0000, 0.12));
            let rect = RECT {
                left: pad,
                top,
                right: pad + track_width,
                bottom: top + bar_height,
            };
            FillRect(dc, &rect, track);
            DeleteObject(track as *mut _);

            let filled = ((track_width as f32) * self.gauge).round() as i32;
            if filled > 0 {
                let color = gauge_color(self.gauge);
                let brush = CreateSolidBrush(color);
                let rect = RECT {
                    left: pad,
                    top,
                    right: pad + filled,
                    bottom: top + bar_height,
                };
                FillRect(dc, &rect, brush);
                DeleteObject(brush as *mut _);
            }
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            if !self.back_dc.is_null() {
                DeleteDC(self.back_dc);
                DeleteObject(self.back_bitmap as *mut _);
            }
        }
    }
}

fn draw_text(dc: HDC, text: &[u16], left: i32, top: i32, right: i32, bottom: i32, align: u32) {
    // TextOutW 不支持对齐/裁剪，用 DrawTextW 更省事
    use windows_sys::Win32::Graphics::Gdi::{DrawTextW, DT_NOPREFIX};
    let mut rect = RECT {
        left,
        top,
        right,
        bottom,
    };
    let flags = align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    unsafe {
        DrawTextW(dc, text.as_ptr(), (text.len() - 1) as i32, &mut rect, flags);
    }
}

/// 量一段文字在当前字体下的像素宽（挑字号用）
fn text_width(dc: HDC, text: &str, font: HFONT) -> i32 {
    unsafe {
        let old = SelectObject(dc, font as HGDIOBJ);
        let mut size: SIZE = std::mem::zeroed();
        let units = wide(text);
        GetTextExtentPoint32W(dc, units.as_ptr(), (units.len() - 1) as i32, &mut size);
        SelectObject(dc, old);
        size.cx
    }
}

fn fill(dc: HDC, left: i32, top: i32, right: i32, bottom: i32, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        FillRect(dc, &rect, brush);
        DeleteObject(brush as *mut _);
    }
}

/// 从面板两侧的空隙取样任务栏底色：取多个点的中位数，避免采到图标/文字。
fn sample_taskbar_color(hwnd: HWND) -> Option<u32> {
    unsafe {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetWindowRect(hwnd, &mut rect);
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }

        let y = (rect.top + rect.bottom) / 2;
        let mut samples: Vec<u32> = Vec::new();
        // 离面板远一点再取：贴着边缘取容易采到面板自己的描边/阴影和旁边的悬停高亮
        for dx in [8, 12, 18, 26, 36, 50, 68] {
            for x in [rect.right + dx, rect.left - dx] {
                let pixel = GetPixel(screen, x, y);
                if pixel != u32::MAX && pixel != 0x00FF_FFFF {
                    samples.push(pixel);
                }
            }
        }

        ReleaseDC(std::ptr::null_mut(), screen);
        if samples.is_empty() {
            return None;
        }

        samples.sort_by_key(|color| {
            let r = (color & 0xFF) as u32;
            let g = ((color >> 8) & 0xFF) as u32;
            let b = ((color >> 16) & 0xFF) as u32;
            r * 299 + g * 587 + b * 114
        });

        samples.get(samples.len() / 2).copied()
    }
}

/// 两个颜色的最大通道差是否超过阈值（小于阈值就当没变，别为了一点点色差重画）
fn color_far_enough(a: u32, b: u32) -> bool {
    let channel = |shift: u32| {
        let av = ((a >> shift) & 0xFF) as i32;
        let bv = ((b >> shift) & 0xFF) as i32;
        (av - bv).abs()
    };
    channel(0).max(channel(8)).max(channel(16)) > 10
}

/// 进程名太长会撑爆气泡：超过 14 个字符就截断加省略号（中英文都按字符数算）
fn shorten_name(name: &str) -> String {
    const LIMIT: usize = 14;
    if name.chars().count() <= LIMIT {
        return name.to_string();
    }
    let mut text: String = name.chars().take(LIMIT - 1).collect();
    text.push('…');
    text
}

/// 占比（0~100）；合计为 0 时返回 0，别出现 NaN
fn share(rate: f64, total: f64) -> f64 {
    if total > 0.0 {
        (rate / total * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn blend(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |shift: u32| {
        let av = ((a >> shift) & 0xFF) as f32;
        let bv = ((b >> shift) & 0xFF) as f32;
        ((av + (bv - av) * t).round() as u32) & 0xFF
    };
    mix(0) | (mix(8) << 8) | (mix(16) << 16)
}

fn gauge_color(value: f32) -> u32 {
    let v = value.clamp(0.0, 1.0);
    // 绿 → 黄 → 红
    let (r, g, b) = if v < 0.55 {
        let t = v / 0.55;
        (62.0 + (242.0 - 62.0) * t, 207.0 + (179.0 - 207.0) * t, 142.0 + (61.0 - 142.0) * t)
    } else {
        let t = (v - 0.55) / 0.45;
        (242.0 + (255.0 - 242.0) * t, 179.0 + (92.0 - 179.0) * t, 61.0 + (92.0 - 61.0) * t)
    };
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

pub fn scale(dpi: u32, value: i32) -> i32 {
    (value as f32 * dpi as f32 / 96.0).round() as i32
}

/// 设置里的四个来源开关 → 采集器的来源掩码
fn sources_of(settings: &Settings) -> Sources {
    Sources {
        opencode: settings.ai_opencode,
        dsh: settings.ai_dsh,
        workbuddy: settings.ai_workbuddy,
        workbuddy_ai: settings.ai_workbuddy_ai,
        claude: settings.ai_claude,
        codex: settings.ai_codex,
    }
}

// ---------------------------------------------------------------- AI 采集线程

/// 采集放在后台线程：WorkBuddy 的 jsonl 首次解析要读几百 MB，不能卡 UI。
///
/// 只在设置里打开「AI 统计」时才真的起线程：关掉就立刻停，连文件都不读。
struct AiWorker {
    sources: Sources,
    stop: Arc<AtomicBool>,
    snapshot: Arc<Mutex<AiSnapshot>>,
    running: bool,
}

impl AiWorker {
    fn new(sources: Sources) -> Self {
        Self {
            sources,
            stop: Arc::new(AtomicBool::new(true)),
            snapshot: Arc::new(Mutex::new(AiSnapshot::default())),
            running: false,
        }
    }

    fn start(&mut self) {
        if self.running || !self.sources.any() {
            return;
        }
        // 每轮跑自己的 snapshot：老线程退出前写的是旧 Arc，不会污染新数据
        self.snapshot = Arc::new(Mutex::new(AiSnapshot::default()));
        self.stop = Arc::new(AtomicBool::new(false));

        let stop = Arc::clone(&self.stop);
        let snapshot = Arc::clone(&self.snapshot);
        let sources = self.sources;

        std::thread::spawn(move || {
            let mut engine = AiEngine::new(sources);
            // 排查采集开销用：设了 NST_AI_PROFILE 就每 10 秒往调试日志写一次均值
            let profile = std::env::var_os("NST_AI_PROFILE").is_some();
            let mut polls = 0_u32;
            let mut total_us = 0_u128;
            let mut max_us = 0_u128;
            let mut last_report = std::time::Instant::now();

            while !stop.load(Ordering::Relaxed) {
                let now = netspeed_core::ai::now_ms();
                let started = std::time::Instant::now();
                engine.poll(now);
                let cost = started.elapsed().as_micros();
                let next = engine.snapshot(now);

                if profile {
                    polls += 1;
                    total_us += cost;
                    max_us = max_us.max(cost);
                    if last_report.elapsed().as_secs() >= 10 {
                        crate::debug_log(&format!(
                            "ai poll avg={}us max={}us n={polls}",
                            total_us / polls.max(1) as u128,
                            max_us
                        ));
                        polls = 0;
                        total_us = 0;
                        max_us = 0;
                        last_report = std::time::Instant::now();
                    }
                }

                if let Ok(mut slot) = snapshot.lock() {
                    *slot = next;
                }

                // 在生成就 700ms 一轮（速度跟手），空闲退到 1.5s（少唤醒几次 CPU）
                let busy = next.generating || next.speed > 0.0;
                std::thread::sleep(std::time::Duration::from_millis(if busy { 700 } else { 1500 }));
            }
        });

        self.running = true;
    }

    fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        self.running = false;
        self.snapshot = Arc::new(Mutex::new(AiSnapshot::default()));
    }

    fn set_sources(&mut self, sources: Sources) {
        if self.sources == sources {
            return;
        }
        self.sources = sources;
        if self.running {
            self.stop();
            self.start();
        }
    }

    fn snapshot(&self) -> AiSnapshot {
        self.snapshot
            .lock()
            .map(|value| *value)
            .unwrap_or_default()
    }
}

impl Drop for AiWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
