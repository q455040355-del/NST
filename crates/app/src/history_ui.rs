//! 用量历史窗口：区间胶囊 + 四张卡片 + 按来源堆叠柱状图 + 每日明细表。
//!
//! 整窗自绘（一个 canvas 子窗口），数据在后台线程加载，弹簧动效和 AgentSpeed 同款参数。

use netspeed_core::ai::now_ms;
use netspeed_core::history::{
    self, all_sources, source_color, source_name, DailyRow, UsageRecord, DAY_MS,
};
use std::sync::mpsc::{channel, Receiver};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, InvalidateRect, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    HBITMAP, HDC, HFONT, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};

const DT_NOPREFIX: u32 = 0x0800;
const DT_CALCRECT: u32 = 0x0400;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme::wide;

const CLASS_NAME: &str = "NetSpeedTaskbarRs.History";
const CANVAS_CLASS: &str = "NetSpeedTaskbarRs.HistoryCanvas";

const WM_APP_HISTORY_READY: u32 = WM_APP + 30;
const WM_APP_HISTORY_ANIM: u32 = WM_APP + 31;

const RANGE_LABELS: [&str; 5] = ["今天", "昨天", "7 天", "30 天", "全部"];
const SOURCE_LABELS: [&str; 5] = ["全部", "OpenCode", "DSH", "WorkBuddy", "WorkBuddy AI"];

const BG: u32 = 0x00FC_F9F8; // 0x00BBGGRR，浅灰蓝
const CARD_BG: u32 = 0x00FF_FFFF;
const CARD_LINE: u32 = 0x00F0_E8E4;
const TEXT_MAIN: u32 = 0x002A_2420;
const TEXT_MUTED: u32 = 0x00A0_9690;
const TEXT_HEAD: u32 = 0x00A8_9E96;
const ACCENT_BLUE: u32 = 0x00D8_8440;
const ACCENT_GREEN: u32 = 0x008C_A92F;
const ACCENT_PURPLE: u32 = 0x00FF_617B;
const ACCENT_ORANGE: u32 = 0x0032_8AE0;

/// 图表的阻尼弹簧：response = 一个来回的周期（秒），damping = 阻尼比（1 = 临界阻尼）。
/// 这两个值沿用 AgentSpeed / C# 版 `DampedSpring(0.30s / 0.86)` 的参数：
/// 0.30s 的周期够快、0.86 的阻尼几乎不过冲，收得干净。
const SPRING_RESPONSE: f64 = 0.30;
const SPRING_DAMPING: f64 = 0.86;
/// AgentSpeed 的 HistoryChart 里有个硬上限：0.45s 到点就收，不等速度衰减完，
/// 免得尾巴上还有肉眼几乎看不见的微动，看起来发飘。
const SPRING_MAX_DURATION: f64 = 0.45;
/// 判定「已经到位」的阈值（值 / 速度），和 AgentSpeed 的 IsSettled(1, 0.002) 一致
const SPRING_EPSILON: f64 = 0.002;

/// 每帧最多推进多少秒（掉帧时别让弹簧一步跳太远）
const MAX_STEP: f64 = 1.0 / 20.0;

/// 动画投递线程的代（每次重启动画就 +1，旧线程看到代变了就退出）
static ANIM_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 每 16ms 给画布投一次 WM_APP_HISTORY_ANIM。
/// 本进程 SetTimer/WM_TIMER 实测收不到消息，只能用线程投递（和面板刷新同因）。
fn start_animator(canvas: HWND) {
    let generation = ANIM_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let handle = canvas as usize;
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(16));
        if ANIM_GEN.load(std::sync::atomic::Ordering::SeqCst) != generation {
            break;
        }
        unsafe {
            let target = handle as HWND;
            if IsWindow(target) == 0 {
                break;
            }
            if PostMessageW(target, WM_APP_HISTORY_ANIM, 0, 0) == 0 {
                break;
            }
        }
    });
}

struct Spring {
    omega: f64,
    damping: f64,
    value: f64,
    velocity: f64,
}

impl Spring {
    fn new(response: f64, damping: f64) -> Self {
        Self {
            omega: 2.0 * std::f64::consts::PI / response.max(0.001),
            damping,
            value: 0.0,
            velocity: 0.0,
        }
    }

    fn step(&mut self, target: f64, dt: f64) {
        if dt <= 0.0 {
            return;
        }

        let x0 = self.value - target;
        let v0 = self.velocity;
        let w = self.omega;
        let z = self.damping;

        let (x, v) = if z < 1.0 {
            let wd = w * (1.0 - z * z).sqrt();
            let e = (-z * w * dt).exp();
            let cos = (wd * dt).cos();
            let sin = (wd * dt).sin();
            (
                e * (x0 * cos + (v0 + z * w * x0) / wd * sin),
                e * (v0 * cos - (z * w * v0 + w * w * x0) / wd * sin),
            )
        } else {
            let c = v0 + w * x0;
            let e = (-w * dt).exp();
            ((x0 + c * dt) * e, (v0 - w * c * dt) * e)
        };

        self.value = target + x;
        self.velocity = v;
    }

    fn settled(&self, target: f64) -> bool {
        (self.value - target).abs() < SPRING_EPSILON && self.velocity.abs() < SPRING_EPSILON * 10.0
    }
}

struct State {
    records: Vec<UsageRecord>,
    range: usize,
    loading: bool,
    days: Vec<DailyRow>,
    stack: Vec<Vec<i64>>,
    from_day: i64,
    to_day: i64,
    spring: Spring,
    from_heights: Vec<f32>,
    to_heights: Vec<f32>,
    now_heights: Vec<f32>,
    /// Y 轴最大值也用同一根弹簧，柱子和刻度一起动
    max_from: f32,
    max_to: f32,
    max_now: f32,
    animating: bool,
    animation_elapsed: f64,
    last_tick: std::time::Instant,
    loader: Option<Receiver<Vec<UsageRecord>>>,
    canvas: HWND,
    pill_rects: Vec<RECT>,
    /// 0 = 全部来源，1..=4 = all_sources() 里对应的一路
    source: usize,
    source_rects: Vec<RECT>,
    scroll: usize,
}

pub fn open(_parent: HWND) {
    unsafe {
        if let Some(existing) = find_existing() {
            ShowWindow(existing, SW_RESTORE);
            SetForegroundWindow(existing);
            return;
        }

        let instance = GetModuleHandleW(std::ptr::null());
        let scale = crate::screen_scale();

        // wide() 的 Vec 必须活到 RegisterClassW 之后
        let class_name = wide(CLASS_NAME);
        let canvas_name = wide(CANVAS_CLASS);
        let window_title = wide("用量历史");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::tray::create_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: (15 + 1) as usize as *mut _, // COLOR_BTNFACE
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);


        let canvas_wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(canvas_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::tray::create_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: canvas_name.as_ptr(),
        };
        RegisterClassW(&canvas_wc);

        let width = (900.0 * scale) as i32;
        let height = (760.0 * scale) as i32;

        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
        let left = work.left + ((work.right - work.left) - width) / 2;
        let top = work.top + ((work.bottom - work.top) - height) / 3;

        let state = Box::into_raw(Box::new(State {
            records: Vec::new(),
            range: 3,
            loading: true,
            days: Vec::new(),
            stack: Vec::new(),
            from_day: 0,
            to_day: 0,
            spring: Spring::new(SPRING_RESPONSE, SPRING_DAMPING),
            from_heights: Vec::new(),
            to_heights: Vec::new(),
            now_heights: Vec::new(),
            max_from: 1.0,
            max_to: 1.0,
            max_now: 1.0,
            animating: false,
            animation_elapsed: 0.0,
            last_tick: std::time::Instant::now(),
            loader: None,
            canvas: std::ptr::null_mut(),
            pill_rects: Vec::new(),
            source: 0,
            source_rects: Vec::new(),
            scroll: 0,
        }));

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            left,
            top,
            width,
            height,
            // 不要挂到任务栏面板（TOPMOST 的拥有者会把拥有窗口一起变成置顶，挡住别的软件）
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            state as *const _,
        );

        if hwnd.is_null() {
            let _ = Box::from_raw(state);
            return;
        }

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);

        let canvas = CreateWindowExW(
            0,
            canvas_name.as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            width,
            height,
            hwnd,
            std::ptr::null_mut(),
            instance,
            hwnd as *const _,
        );
        // 存进 State（GWLP_USERDATA+8 是别的槽位，不能随便用）
        {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                (*state).canvas = canvas;
            }
        }

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetForegroundWindow(hwnd);

        // 后台加载
        start_loading(hwnd);
    }
}

fn start_loading(hwnd: HWND) {
    let (sender, receiver) = channel::<Vec<UsageRecord>>();
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if state.is_null() {
            return;
        }

        (*state).loader = Some(receiver);
        (*state).loading = true;
    }

    let handle = hwnd as usize;
    std::thread::spawn(move || {
        // 第一批：OpenCode 数据库（毫秒级），先让界面有数
        let opencode = history::load_opencode();

        let _ = sender.send(opencode);
        unsafe {
            PostMessageW(handle as HWND, WM_APP_HISTORY_READY, 0, 0);
        }

        // 第二批：DSH / WorkBuddy 文件（可能几十秒）
        let files = history::load_files_only(400);
        let _ = sender.send(files);
        unsafe {
            PostMessageW(handle as HWND, WM_APP_HISTORY_READY, 0, 0);
        }
    });
}

unsafe fn canvas_of(hwnd: HWND) -> HWND {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if state.is_null() {
        std::ptr::null_mut()
    } else {
        (*state).canvas
    }
}

unsafe fn find_existing() -> Option<HWND> {
    let mut found: HWND = std::ptr::null_mut();
    let class = wide(CLASS_NAME);
    EnumThreadWindows(
        windows_sys::Win32::System::Threading::GetCurrentThreadId(),
        Some(enum_history),
        &mut found as *mut HWND as isize,
    );
    let _ = class;
    if found.is_null() {
        None
    } else {
        Some(found)
    }
}

unsafe extern "system" fn enum_history(hwnd: HWND, lparam: LPARAM) -> i32 {
    let mut class = [0u16; 128];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    let name = String::from_utf16_lossy(&class[..length.max(0) as usize]);
    if name == CLASS_NAME && IsWindowVisible(hwnd) != 0 {
        *(lparam as *mut HWND) = hwnd;
        return 0;
    }

    1
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            let canvas = canvas_of(hwnd);
            if !canvas.is_null() {
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetClientRect(hwnd, &mut rect);
                SetWindowPos(
                    canvas,
                    std::ptr::null_mut(),
                    0,
                    0,
                    rect.right,
                    rect.bottom,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            0
        }
        WM_APP_HISTORY_READY => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                let state = &mut *state;

                if let Some(loader) = state.loader.as_ref() {
                    if let Ok(mut records) = loader.try_recv() {
                        // 第一批是 OpenCode（直接覆盖），第二批是文件来源（追加）
                        if state.records.is_empty() {
                            state.records = records;
                            state.loading = true;
                        } else {
                            state.records.append(&mut records);
                            state.loading = false;
                            state.loader = None;
                        }

                        recalc(state);

                        let canvas = canvas_of(hwnd);
                        if !canvas.is_null() {
                            start_animator(canvas);
                        }
                    }
                }
            }

            let canvas = canvas_of(hwnd);
            if !canvas.is_null() {
                InvalidateRect(canvas, std::ptr::null(), 0);
                UpdateWindow(canvas);
            }
            0
        }
        WM_DESTROY => {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                drop(Box::from_raw(state));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn range_days(range: usize, today: i64) -> (i64, i64) {
    match range {
        0 => (today, today),
        1 => (today - 1, today - 1),
        2 => (today - 6, today),
        3 => (today - 29, today),
        _ => {
            // 全部：由调用方用最早一条记录覆盖
            (today - 29, today)
        }
    }
}

fn recalc(state: &mut State) {
    let now = now_ms();
    let today = history::today(now);
    let (mut from, to) = range_days(state.range, today);

    let filter = if state.source == 0 {
        None
    } else {
        all_sources().get(state.source - 1).copied()
    };

    if state.range == 4 {
        from = state
            .records
            .iter()
            .filter(|record| filter.map_or(true, |wanted| record.source == wanted))
            .map(|record| record.day)
            .min()
            .unwrap_or(today);
    }

    state.from_day = from;
    state.to_day = to;

    state.days = history::summarize_filtered(&state.records, from, to, filter);
    // daily_by_source 是按「区间内每一天」返回的，而 days 只含「有记录的天」，
    // 直接按下标取会错位（柱子会全部落空），这里按日期重新对齐。
    let mut rows = history::daily_by_source(&state.records, from, to, &all_sources());
    // 只看单一来源时把其他来源的列清零：这样图例、堆叠柱都只剩选中的那一路
    if state.source != 0 {
        for row in rows.iter_mut() {
            for (slot, value) in row.iter_mut().enumerate() {
                if slot + 1 != state.source {
                    *value = 0;
                }
            }
        }
    }
    state.stack = state
        .days
        .iter()
        .map(|day| {
            let index = (day.day - from).max(0) as usize;
            rows.get(index).cloned().unwrap_or_default()
        })
        .collect();

    let target: Vec<f32> = state
        .days
        .iter()
        .map(|day| day.output as f32)
        .collect();

    state.from_heights = if state.now_heights.len() == target.len() {
        state.now_heights.clone()
    } else {
        vec![0.0; target.len()]
    };
    state.to_heights = target;
    state.now_heights = state.from_heights.clone();

    // Y 轴最大值跟着一起弹，柱子和刻度不会各跳各的
    let target_max = state
        .days
        .iter()
        .map(|day| day.output)
        .max()
        .unwrap_or(0)
        .max(1) as f32;
    state.max_from = if state.max_now > 0.0 { state.max_now } else { target_max };
    state.max_to = target_max;
    state.max_now = state.max_from;

    state.spring = Spring::new(SPRING_RESPONSE, SPRING_DAMPING);
    state.animating = true;
    state.animation_elapsed = 0.0;
    state.last_tick = std::time::Instant::now();
}

unsafe extern "system" fn canvas_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_APP_HISTORY_ANIM => {
            let parent = GetParent(hwnd);
            let state = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                let state = &mut *state;
                let now = std::time::Instant::now();
                let dt = now
                    .duration_since(state.last_tick)
                    .as_secs_f64()
                    .clamp(0.0, MAX_STEP);
                state.last_tick = now;
                state.animation_elapsed += dt;

                state.spring.step(1.0, dt);

                // 和 AgentSpeed 的 HistoryChart 一样：0.45s 到点、或者判定已到位，就收尾吸附
                if state.animation_elapsed >= SPRING_MAX_DURATION || state.spring.settled(1.0) {
                    state.now_heights = state.to_heights.clone();
                    state.max_now = state.max_to;
                    state.animating = false;
                    // 让投递线程自然退出
                    ANIM_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    UpdateWindow(hwnd);
                    return 0;
                }

                // 柱子用原始 t（允许回弹），坐标轴最大值把 t 夹在 [0, 1.1]，别让刻度跟着过冲
                let t = state.spring.value as f32;
                let axis_t = t.clamp(0.0, 1.1);
                state.max_now = state.max_from + (state.max_to - state.max_from) * axis_t;
                for index in 0..state.now_heights.len() {
                    let from = *state.from_heights.get(index).unwrap_or(&0.0);
                    let to = *state.to_heights.get(index).unwrap_or(&0.0);
                    state.now_heights[index] = from + (to - from) * t;
                }

                InvalidateRect(hwnd, std::ptr::null(), 0);
                UpdateWindow(hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            let x = (lparam as i32) & 0xFFFF;
            let y = ((lparam as i32) >> 16) & 0xFFFF;
            let parent = GetParent(hwnd);
            let state = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                let state = &mut *state;

                for (index, rect) in state.source_rects.iter().enumerate() {
                    if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                        if state.source != index {
                            state.source = index;
                            recalc(state);
                            start_animator(hwnd);
                        }
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                        return 0;
                    }
                }

                for (index, rect) in state.pill_rects.iter().enumerate() {
                    if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                        if state.range != index {
                            state.range = index;
                            recalc(state);
                            start_animator(hwnd);
                        }
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                        break;
                    }
                }
            }
            0
        }
        WM_MOUSEWHEEL => {
            let parent = GetParent(hwnd);
            let state = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                let state = &mut *state;
                let delta = ((wparam >> 16) as i16) as i32;
                if delta < 0 {
                    state.scroll += 1;
                } else if state.scroll > 0 {
                    state.scroll -= 1;
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint(hwnd, hdc);
            EndPaint(hwnd, &mut ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- 绘制

struct Painter {
    dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
    scale: f32,
    old_bitmap: HGDIOBJ,
}

impl Painter {
    unsafe fn new(hdc: HDC, width: i32, height: i32) -> Self {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width.max(1);
        info.bmiHeader.biHeight = -height.max(1);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = 0;

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        let old_bitmap = SelectObject(dc, bitmap as HGDIOBJ);
        ReleaseDC(std::ptr::null_mut(), screen);

        Self {
            dc,
            bitmap,
            bits: bits as *mut u8,
            width: width.max(1),
            height: height.max(1),
            scale: crate::screen_scale(),
            old_bitmap,
        }
    }

    fn s(&self, value: f32) -> i32 {
        (value * self.scale).round() as i32
    }

    unsafe fn finish(mut self, target: HDC) {
        // DIB 的 alpha 全置 255，DWM 才会按不透明合成
        if !self.bits.is_null() {
            let count = (self.width * self.height) as usize;
            for index in 0..count {
                *self.bits.add(index * 4 + 3) = 255;
            }
        }

        BitBlt(target, 0, 0, self.width, self.height, self.dc, 0, 0, SRCCOPY);
        SelectObject(self.dc, self.old_bitmap);
        DeleteObject(self.bitmap as *mut _);
        DeleteDC(self.dc);
    }

    unsafe fn fill(&self, left: i32, top: i32, right: i32, bottom: i32, color: u32) {
        let brush = CreateSolidBrush(color);
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        FillRect(self.dc, &rect, brush);
        DeleteObject(brush as *mut _);
    }

    unsafe fn round_rect(&self, left: i32, top: i32, right: i32, bottom: i32, radius: i32, fill: u32, border: u32) {
        use windows_sys::Win32::Graphics::Gdi::{CreatePen, RoundRect, PS_SOLID};

        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(PS_SOLID as i32, 1, border);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);

        RoundRect(self.dc, left, top, right, bottom, radius * 2, radius * 2);

        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }

    unsafe fn text(&self, text: &str, font: HFONT, color: u32, left: i32, top: i32, right: i32, bottom: i32, align: u32) {
        let old = SelectObject(self.dc, font as HGDIOBJ);
        SetBkMode(self.dc, 1);
        SetTextColor(self.dc, color);
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        let text = wide(text);
        DrawTextW(
            self.dc,
            text.as_ptr(),
            (text.len() - 1) as i32,
            &mut rect,
            align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(self.dc, old);
    }

    /// 量一段文字在当前字号下的像素宽度（用来把胶囊宽度贴合文字）。
    fn measure(&self, text: &str, font: HFONT) -> i32 {
        unsafe {
            let old = SelectObject(self.dc, font as HGDIOBJ);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let text = wide(text);
            DrawTextW(
                self.dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut rect,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(self.dc, old);
            rect.right - rect.left
        }
    }
}

fn make_font(scale: f32, points: f32, weight: i32, face: &str) -> HFONT {
    let height = -((points * scale * 96.0 / 72.0).round() as i32);
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide(face).as_ptr(),
        )
    }
}

fn paint(hwnd: HWND, hdc: HDC) {
    unsafe {
        let parent = GetParent(hwnd);
        let state = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut State;
        if state.is_null() {
            return;
        }

        let state = &mut *state;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetClientRect(hwnd, &mut rect);

        let p = Painter::new(hdc, rect.right, rect.bottom);
        p.fill(0, 0, p.width, p.height, BG);



        let title_font = make_font(p.scale, 15.0, 700, "Microsoft YaHei UI");
        let sub_font = make_font(p.scale, 8.5, 400, "Microsoft YaHei UI");
        let label_font = make_font(p.scale, 8.5, 400, "Microsoft YaHei UI");
        let value_font = make_font(p.scale, 15.0, 700, "Microsoft YaHei UI");
        let num_font = make_font(p.scale, 8.5, 400, "Consolas");
        let head_font = make_font(p.scale, 8.0, 400, "Microsoft YaHei UI");

        let pad = p.s(24.0);
        p.text("用量历史", title_font, TEXT_MAIN, pad, p.s(10.0), p.width - pad, p.s(48.0), DT_LEFT);

        let mut subtitle = if state.loading {
            "读取中…".to_string()
        } else if state.source == 0 {
            format!("{} 条记录", state.records.len())
        } else {
            let wanted = all_sources()[state.source - 1];
            let count = state
                .records
                .iter()
                .filter(|record| {
                    record.source == wanted && record.day >= state.from_day && record.day <= state.to_day
                })
                .count();
            format!("{count} 条记录 · 仅 {}", history::source_name(wanted))
        };

        if !state.loading {
            subtitle = format!("{} – {} · {}", format_day(state.from_day), format_day(state.to_day), subtitle);
        }

        p.text(&subtitle, sub_font, TEXT_MUTED, pad, p.s(44.0), p.width - pad, p.s(70.0), DT_LEFT);

        // 区间胶囊
        state.pill_rects.clear();
        let pill_width = p.s(72.0);
        let pill_height = p.s(26.0);
        let mut x = pad;
        for (index, label) in RANGE_LABELS.iter().enumerate() {
            let pill = RECT {
                left: x,
                top: p.s(78.0),
                right: x + pill_width,
                bottom: p.s(78.0) + pill_height,
            };

            let active = index == state.range;
            p.round_rect(
                pill.left,
                pill.top,
                pill.right,
                pill.bottom,
                p.s(12.0),
                if active { 0x00F7_EBE3 } else { CARD_BG },
                if active { 0x00D8_C4B8 } else { 0x00E8_DFD9 },
            );
            p.text(
                label,
                sub_font,
                if active { TEXT_MAIN } else { TEXT_MUTED },
                pill.left,
                pill.top,
                pill.right,
                pill.bottom + 1,
                DT_CENTER,
            );

            state.pill_rects.push(pill);
            x += pill_width + p.s(8.0);
        }

        // 来源筛选：和区间同一行，右对齐
        state.source_rects.clear();
        {
            let gap_pill = p.s(5.0);
            let source_height = pill_height;
            let mut widths = [0_i32; 5];
            let mut total = 0;
            for (index, label) in SOURCE_LABELS.iter().enumerate() {
                widths[index] = p.measure(label, sub_font) + p.s(16.0);
                total += widths[index];
            }
            total += gap_pill * (SOURCE_LABELS.len() as i32 - 1);

            let label_width = p.measure("来源", head_font) + p.s(8.0);
            let mut sx = (p.width - pad - total - label_width).max(x + p.s(24.0)) + label_width;
            p.text("来源", head_font, TEXT_HEAD, sx - label_width, p.s(78.0), sx, p.s(78.0) + source_height, DT_RIGHT);

            for (index, label) in SOURCE_LABELS.iter().enumerate() {
                let pill = RECT {
                    left: sx,
                    top: p.s(78.0),
                    right: sx + widths[index],
                    bottom: p.s(78.0) + source_height,
                };

                let active = index == state.source;
                p.round_rect(
                    pill.left,
                    pill.top,
                    pill.right,
                    pill.bottom,
                    p.s(12.0),
                    if active { 0x00F7_EBE3 } else { CARD_BG },
                    if active { 0x00D8_C4B8 } else { 0x00E8_DFD9 },
                );
                p.text(
                    label,
                    sub_font,
                    if active { TEXT_MAIN } else { TEXT_MUTED },
                    pill.left,
                    pill.top,
                    pill.right,
                    pill.bottom + 1,
                    DT_CENTER,
                );

                state.source_rects.push(pill);
                sx += widths[index] + gap_pill;
            }
        }

        // 卡片
        let cards_top = p.s(118.0);
        let card_height = p.s(76.0);
        let gap = p.s(12.0);
        let card_width = (p.width - pad * 2 - gap * 3) / 4;

        let total_output: i64 = state.days.iter().map(|day| day.output).sum();
        let total_input: i64 = state.days.iter().map(|day| day.input).sum();
        let total_requests: i64 = state.days.iter().map(|day| day.requests).sum();
        let measured_output: i64 = state.days.iter().map(|day| day.measured_output).sum();
        let measured_ms: i64 = state.days.iter().map(|day| day.measured_ms).sum();
        let speed = if measured_ms > 0 {
            measured_output as f64 * 1000.0 / measured_ms as f64
        } else {
            0.0
        };

        let days_count = state.days.len().max(1) as i64;
        let cards: [(&str, String, String, u32); 4] = [
            ("输出 tokens", history::format_tokens(total_output), format!("日均 {}", history::format_tokens(total_output / days_count)), ACCENT_BLUE),
            ("输入 tokens", history::format_tokens(total_input), if total_output > 0 { format!("输入/输出 {}×", total_input / total_output.max(1)) } else { "含缓存".to_string() }, ACCENT_GREEN),
            ("请求数", total_requests.to_string(), format!("日均 {}", total_requests / days_count), ACCENT_PURPLE),
            ("平均速度", if speed > 0.0 { format!("{speed:.1} t/s") } else { "--".to_string() }, "按可测请求估算".to_string(), ACCENT_ORANGE),
        ];

        for (index, (title, value, caption, accent)) in cards.iter().enumerate() {
            let left = pad + (card_width + gap) * index as i32;
            p.round_rect(left, cards_top, left + card_width, cards_top + card_height, p.s(10.0), CARD_BG, CARD_LINE);

            p.fill(left + p.s(14.0), cards_top + p.s(14.0), left + p.s(20.0), cards_top + p.s(20.0), *accent);
            p.text(title, label_font, TEXT_MUTED, left + p.s(26.0), cards_top + p.s(8.0), left + card_width - p.s(12.0), cards_top + p.s(30.0), DT_LEFT);
            p.text(value, value_font, TEXT_MAIN, left + p.s(13.0), cards_top + p.s(24.0), left + card_width - p.s(12.0), cards_top + p.s(58.0), DT_LEFT);
            p.text(caption, head_font, TEXT_MUTED, left + p.s(14.0), cards_top + p.s(54.0), left + card_width - p.s(12.0), cards_top + p.s(72.0), DT_LEFT);
        }

        // 图表
        let chart_top = cards_top + card_height + p.s(14.0);
        let chart_bottom = chart_top + p.s(230.0);
        p.round_rect(pad, chart_top, p.width - pad, chart_bottom, p.s(10.0), CARD_BG, CARD_LINE);

        let chart_title = format!("每日输出");
        p.text(&chart_title, label_font, TEXT_MAIN, pad + p.s(16.0), chart_top + p.s(8.0), p.width - pad, chart_top + p.s(30.0), DT_LEFT);

        // 图例
        let mut legend_x = pad + p.s(80.0);
        for source in all_sources() {
            if state.stack.iter().any(|row| {
                let index = all_sources().iter().position(|item| *item == source).unwrap_or(0);
                row.get(index).copied().unwrap_or(0) > 0
            }) {
                p.fill(legend_x, chart_top + p.s(15.0), legend_x + p.s(9.0), chart_top + p.s(24.0), source_color(source));
                p.text(source_name(source), head_font, TEXT_MUTED, legend_x + p.s(13.0), chart_top + p.s(8.0), legend_x + p.s(120.0), chart_top + p.s(28.0), DT_LEFT);
                legend_x += p.s(13.0) + p.s(80.0);
            }
        }

        let plot_left = pad + p.s(60.0);
        let plot_right = p.width - pad - p.s(20.0);
        let plot_top = chart_top + p.s(42.0);
        let plot_bottom = chart_bottom - p.s(28.0);
        let plot_height = (plot_bottom - plot_top).max(10);
        let plot_width = (plot_right - plot_left).max(10);

        // Y 轴最大值来自弹簧（切换区间/来源时和柱子一起动）
        let max_output = state.max_now.max(1.0);

        // 网格 + Y 轴
        for step in 0..=2 {
            let y = plot_bottom - plot_height * step / 2;
            p.fill(plot_left, y, plot_right, y + 1, 0x00F0_E8E4);
            let value = (max_output * step as f32 / 2.0) as i64;
            p.text(&history::format_tokens(value), head_font, TEXT_MUTED, pad, y - p.s(10.0), plot_left - p.s(8.0), y + p.s(10.0), DT_RIGHT);
        }

        let count = state.days.len();
        if count > 0 {
            let slot = plot_width as f32 / count as f32;
            let bar_width = (slot * 0.62).clamp(3.0, 42.0);
            let sources = all_sources();

            for (index, day) in state.days.iter().enumerate() {
                let total = *state.now_heights.get(index).unwrap_or(&0.0);
                if total <= 0.0 {
                    continue;
                }

                let height = (plot_height as f32 * total / max_output).max(1.0);
                let x = plot_left as f32 + slot * index as f32 + (slot - bar_width) / 2.0;
                let mut bottom = plot_bottom as f32;

                let day_stack = state.stack.get(index);
                let day_total: i64 = day_stack.map(|row| row.iter().sum()).unwrap_or(0).max(1);
                let day_output = day.output as f32;

                for (slot_index, source) in sources.iter().enumerate() {
                    let value = day_stack
                        .and_then(|row| row.get(slot_index).copied())
                        .unwrap_or(0) as f32;
                    if value <= 0.0 {
                        continue;
                    }

                    let share = value / day_total as f32;
                    let segment = (height * share).max(0.6);
                    bottom -= segment;
                    p.fill(
                        x as i32,
                        bottom as i32,
                        (x + bar_width) as i32,
                        (bottom + segment) as i32,
                        source_color(source),
                    );
                }

                let _ = day_output;
            }

            // X 轴标签
            let label_step = (count / 8).max(1);
            for (index, day) in state.days.iter().enumerate() {
                if index % label_step != 0 && index + 1 != count {
                    continue;
                }

                let x = plot_left as f32 + slot * index as f32;
                p.text(
                    &day_label(day.day),
                    head_font,
                    TEXT_MUTED,
                    (x - p.s(20.0) as f32) as i32,
                    plot_bottom + p.s(6.0),
                    (x + slot + p.s(20.0) as f32) as i32,
                    plot_bottom + p.s(26.0),
                    DT_CENTER,
                );
            }
        }

        // 表格
        let table_top = chart_bottom + p.s(14.0);
        let table_bottom = p.height - p.s(14.0);
        p.round_rect(pad, table_top, p.width - pad, table_bottom, p.s(10.0), CARD_BG, CARD_LINE);

        let columns = [0.16f32, 0.10, 0.13, 0.13, 0.12, 0.12, 0.24];
        let mut left = pad + p.s(14.0);
        let usable = p.width - pad * 2 - p.s(28.0);
        let mut column_left = [0_i32; 7];
        for (index, weight) in columns.iter().enumerate() {
            column_left[index] = left;
            left += (usable as f32 * weight) as i32;
        }

        let headers = ["日期", "请求", "输出", "输入", "平均速度", "峰值", "主要模型"];
        for (index, header) in headers.iter().enumerate() {
            p.text(header, head_font, TEXT_HEAD, column_left[index], table_top + p.s(8.0), column_left[index] + p.s(120.0), table_top + p.s(28.0), DT_LEFT);
        }

        let row_height = p.s(26.0);
        let available = ((table_bottom - table_top - p.s(34.0)) / row_height).max(1) as usize;
        let max_scroll = state.days.iter().filter(|day| day.requests > 0).count().saturating_sub(available);
        if state.scroll > max_scroll {
            state.scroll = max_scroll;
        }

        let rows: Vec<&DailyRow> = state.days.iter().rev().filter(|day| day.requests > 0).collect();
        let mut y = table_top + p.s(32.0);
        for (index, day) in rows.iter().skip(state.scroll).take(available).enumerate() {
            if index % 2 == 1 {
                p.fill(pad + p.s(6.0), y - p.s(1.0), p.width - pad - p.s(6.0), y + row_height - p.s(3.0), 0x00FA_F6F2);
            }

            let text_top = y;
            let text_bottom = y + row_height;
            p.text(
                &day_full_label(day.day),
                label_font,
                TEXT_MAIN,
                column_left[0],
                text_top,
                column_left[0] + p.s(110.0),
                text_bottom,
                DT_LEFT,
            );
            p.text(&day.requests.to_string(), num_font, TEXT_MAIN, column_left[1], text_top, column_left[1] + p.s(60.0), text_bottom, DT_LEFT);
            p.text(&history::format_tokens(day.output), num_font, TEXT_MAIN, column_left[2], text_top, column_left[2] + p.s(70.0), text_bottom, DT_LEFT);
            p.text(&history::format_tokens(day.input), num_font, TEXT_MAIN, column_left[3], text_top, column_left[3] + p.s(70.0), text_bottom, DT_LEFT);
            p.text(
                &if day.speed() > 0.0 { format!("{:.0} t/s", day.speed()) } else { "--".to_string() },
                num_font,
                TEXT_MAIN,
                column_left[4],
                text_top,
                column_left[4] + p.s(70.0),
                text_bottom,
                DT_LEFT,
            );
            p.text(
                &if day.peak_speed > 0.0 { format!("{:.0} t/s", day.peak_speed) } else { "--".to_string() },
                num_font,
                TEXT_MUTED,
                column_left[5],
                text_top,
                column_left[5] + p.s(70.0),
                text_bottom,
                DT_LEFT,
            );
            p.text(
                &day.main_model,
                label_font,
                TEXT_MUTED,
                column_left[6],
                text_top,
                column_left[6] + p.s(200.0),
                text_bottom,
                DT_LEFT,
            );

            y += row_height;
        }

        let _ = (&label_font, &head_font, &num_font);
        for font in [title_font, sub_font, label_font, value_font, num_font, head_font] {
            DeleteObject(font as *mut _);
        }

        p.finish(hdc);
    }
}

fn format_day(day: i64) -> String {
    let date = day_to_date(day);
    format!("{:04}-{:02}-{:02}", date.0, date.1, date.2)
}

fn day_label(day: i64) -> String {
    let date = day_to_date(day);
    format!("{:02}-{:02}", date.1, date.2)
}

fn day_full_label(day: i64) -> String {
    let date = day_to_date(day);
    let today = history::today(now_ms());
    if day == today {
        "今天".to_string()
    } else if day == today - 1 {
        "昨天".to_string()
    } else {
        format!("{:02}/{:02} ({})", date.1, date.2, weekday_name(date.3))
    }
}

fn weekday_name(weekday: i64) -> &'static str {
    match weekday {
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        6 => "周六",
        _ => "周日",
    }
}

/// day 序号 → (年, 月, 日, 星期)
fn day_to_date(day: i64) -> (i32, u32, u32, i64) {
    // 1970-01-01 是周四（weekday 4）
    let days = day;
    let weekday = ((days % 7) + 11) % 7; // 3..9 映射到 0..6 太绕，直接算：
    let weekday = ((days + 3).rem_euclid(7)) + 1; // 1=周一 … 7=周日
    let _ = weekday;

    let date = civil_from_days(days);
    let wd = ((days + 3).rem_euclid(7) + 1).rem_euclid(7);
    (date.0, date.1, date.2, wd + 1)
}

/// 天数（1970-01-01 起）转公历。
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m as u32, d as u32)
}

#[allow(dead_code)]
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[allow(dead_code)]
fn day_from_ms(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}
