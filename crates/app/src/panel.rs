//! 任务栏子窗口面板：GDI 绘制、视图切换、指示条。

use netspeed_core::ai::{format_speed as format_ai_speed, format_tokens, AiEngine, AiSnapshot};
use netspeed_core::memory::read_memory;
use netspeed_core::temperature::{append_watts, format_temperature as format_celsius, TemperatureMonitor};
use netspeed_core::net::{NetSample, NetSampler};
use netspeed_core::settings::{Settings, GAUGE_AI, GAUGE_CPU, GAUGE_MEMORY, GAUGE_SPEED};
use netspeed_core::{format_bytes_gb, format_speed, format_temperature, MemorySnapshot};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetDC, GetPixel, InvalidateRect, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, DT_LEFT, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, SRCCOPY, TRANSPARENT,
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
    cpu_celsius: f64,
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
    /// 上一次真正采样/重绘的时间（用来按设置的间隔限流）
    last_refresh: std::time::Instant,
}

impl Panel {
    pub fn new(hwnd: HWND, settings: Settings) -> Self {
        let dpi = unsafe { crate::dpi_for_window(hwnd) };
        let light = taskbar_is_light();
        let colors = Colors::for_theme(light, 0x00F3F3F3);

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
            cpu_celsius: f64::NAN,
            temperatures: None,
            ai_speed_text: "0.0 t/s".to_string(),
            ai_today_text: "0".to_string(),
            ai: AiWorker::new(),
            ai_snapshot: AiSnapshot::default(),
            dpi,
            colors,
            fonts: Fonts::create(dpi),
            back_dc: std::ptr::null_mut(),
            back_bitmap: std::ptr::null_mut(),
            back_bits: std::ptr::null_mut(),
            width: 0,
            height: 0,
            gauge: 0.0,
            last_refresh: std::time::Instant::now(),
        };

        panel.ensure_buffer();
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
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
        if refresh_us > 5_000 {
            crate::debug_log(&format!("tick refresh={refresh_us}us"));
        }
    }

    /// 改刷新间隔（毫秒）：立刻生效，下一次 tick 就会采一次样。
    pub fn set_refresh_ms(&mut self, ms: i32) {
        self.settings.refresh_ms = netspeed_core::settings::normalize_refresh(ms);
        self.settings.save();
        self.last_refresh = std::time::Instant::now() - std::time::Duration::from_secs(10);
        self.tick();
    }

    pub fn cycle_view(&mut self) {
        self.settings.cycle_view();
        self.settings.save();
        self.refresh();
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    pub fn set_view(&mut self, view: &str) {
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

    /// 改面板宽度：更新设置、保存、立刻改窗口尺寸并重绘。
    pub fn set_panel_width(&mut self, width: i32) {
        self.settings.panel_width = width.clamp(72, 360);
        self.settings.save();
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

        if self.settings.view == VIEW_MEMORY {
            self.memory = read_memory();
            self.used_text = format_bytes_gb(self.memory.used_bytes());
            self.total_text = format_bytes_gb(self.memory.total_bytes);
        } else if self.settings.view == VIEW_TEMPERATURE {
            if self.temperatures.is_none() {
                self.temperatures = Some(TemperatureMonitor::start());
            }

            if let Some(monitor) = self.temperatures.as_ref() {
                let snapshot = monitor.read();
                self.cpu_celsius = snapshot.cpu_celsius;
                self.temp_text = append_watts(format_celsius(snapshot.cpu_celsius), f64::NAN);
                self.temp_gpu_text =
                    append_watts(format_celsius(snapshot.gpu_celsius), snapshot.gpu_watts);
            }
        }

        // AI 采集懒启动：只有 AI 视图或 AI 指示条才让后台线程读文件/数据库
        let need_ai = self.settings.view == VIEW_AI || self.settings.gauge_source == GAUGE_AI;
        self.ai.set_enabled(need_ai);
        if need_ai {
            self.ai_snapshot = self.ai.snapshot();
            self.ai_speed_text = format_ai_speed(self.ai_snapshot.speed);
            self.ai_today_text = format_tokens(self.ai_snapshot.today_output);
        }

        self.gauge = self.compute_gauge();
    }

    fn compute_gauge(&self) -> f32 {
        let source = if self.settings.gauge_source == "view" {
            match self.settings.view.as_str() {
                VIEW_MEMORY => GAUGE_MEMORY,
                VIEW_TEMPERATURE => GAUGE_CPU,
                VIEW_AI => GAUGE_AI,
                _ => GAUGE_SPEED,
            }
        } else {
            self.settings.gauge_source.as_str()
        };

        match source {
            GAUGE_SPEED => {
                let bytes = self
                    .sample
                    .up_bytes_per_sec
                    .max(self.sample.down_bytes_per_sec);
                // 1KB/s → 0，100MB/s → 1，对数刻度
                let floor = 1024.0_f64;
                let ceiling = 100.0 * 1024.0 * 1024.0;
                if bytes <= floor {
                    0.0
                } else {
                    let ratio = (bytes / floor).ln() / (ceiling / floor).ln();
                    (ratio as f32).clamp(0.0, 1.0)
                }
            }
            GAUGE_MEMORY => self.memory.used_ratio(),
            GAUGE_CPU => {
                if self.cpu_celsius.is_finite() {
                    (((self.cpu_celsius - 30.0) / 60.0) as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            GAUGE_AI => (self.ai_snapshot.speed / 50.0) as f32,
            _ => 0.0,
        }
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

    /// 采样任务栏背景色，让面板“融进”任务栏；顺带用亮度判断明暗主题，
    /// 比读注册表更可靠（注册表读取在某些精简系统上会失败）。
    fn sample_background(&mut self) {
        unsafe {
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetWindowRect(self.hwnd, &mut rect);
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return;
            }

            let y = (rect.top + rect.bottom) / 2;
            let mut pixel = u32::MAX;
            // 往左找一段任务栏底色（避开图标）
            for step in [12, 24, 40, 60, 90] {
                let x = (rect.left - step).max(2);
                let candidate = GetPixel(screen, x, y);
                if candidate != u32::MAX && candidate != 0xFFFF_FFFF {
                    pixel = candidate;
                    break;
                }
            }

            ReleaseDC(std::ptr::null_mut(), screen);
            if pixel == u32::MAX || pixel == 0xFFFF_FFFF {
                return;
            }

            // 主题以注册表为准；采样只用来微调底色，避免采到图标导致整块变黑/变白
            let light = taskbar_is_light();
            let standard = if light { 0x00F3F3F3 } else { 0x001F1F1F };
            let r = (pixel & 0xFF) as f32;
            let g = ((pixel >> 8) & 0xFF) as f32;
            let b = ((pixel >> 16) & 0xFF) as f32;
            let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
            let plausible = if light { luminance > 150.0 } else { luminance < 110.0 };
            self.colors = Colors::for_theme(light, if plausible { pixel } else { standard });
        }
    }

    /// 分层窗口：把背缓冲整块推给 DWM（不依赖 WM_PAINT，任务栏重绘也刷不掉）。
    pub fn paint(&mut self) {
        unsafe {
            self.render(std::ptr::null_mut());
        }
    }

    /// 画到任意 DC：WM_PAINT 和 WM_PRINTCLIENT（截图/打印）共用。
    pub fn render(&mut self, hdc: HDC) {
        unsafe {
            self.ensure_buffer();
            // 采样任务栏底色（取面板左右两侧的空隙，避开图标）
            let light = taskbar_is_light();
            let fallback = if light { 0x00F3F3F3 } else { 0x001F1F1F };
            let background = sample_taskbar_color(self.hwnd).unwrap_or(fallback);
            if self.colors.background != background {
                self.colors = Colors::for_theme(light, background);
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
            let font = SelectObject(dc, self.fonts.label as HGDIOBJ);
            SetTextColor(dc, color);
            draw_text(dc, &wide(label), pad, top, pad + scale(self.dpi, 20), top + row_height, DT_LEFT);

            SelectObject(dc, self.fonts.value as HGDIOBJ);
            SetTextColor(dc, self.colors.value);
            draw_text(
                dc,
                &wide(value),
                pad,
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
            let mut rect = RECT {
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
                let mut rect = RECT {
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

fn fill(dc: HDC, left: i32, top: i32, right: i32, bottom: i32, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let mut rect = RECT {
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
        for dx in [4, 7, 10, 14] {
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

pub fn font_is_alive(font: HFONT) -> bool {
    !font.is_null()
}

// ---------------------------------------------------------------- AI 采集线程

/// 采集放在后台线程：WorkBuddy 的 jsonl 首次解析要读几百 MB，不能卡 UI。
struct AiWorker {
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    snapshot: Arc<Mutex<AiSnapshot>>,
}

impl AiWorker {
    fn new() -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(AiSnapshot::default()));

        let thread_enabled = Arc::clone(&enabled);
        let thread_stop = Arc::clone(&stop);
        let thread_snapshot = Arc::clone(&snapshot);

        std::thread::spawn(move || {
            let mut engine = AiEngine::new();
            while !thread_stop.load(Ordering::Relaxed) {
                if thread_enabled.load(Ordering::Relaxed) {
                    let now = netspeed_core::ai::now_ms();
                    engine.poll(now);
                    let next = engine.snapshot(now);

                    if let Ok(mut slot) = thread_snapshot.lock() {
                        *slot = next;
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(700));
            }
        });

        Self {
            enabled,
            stop,
            snapshot,
        }
    }

    fn set_enabled(&self, value: bool) {
        self.enabled.store(value, Ordering::Relaxed);
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