//! 用量历史窗口：区间胶囊 + 四张卡片 + 按来源堆叠柱状图 + 每日明细表。
//!
//! 整窗自绘（一个 canvas 子窗口），数据在后台线程加载，弹簧动效和 AgentSpeed 同款参数。

use netspeed_core::ai::now_ms;
use netspeed_core::history::{
    self, all_sources, source_color, source_name, DailyRow, UsageRecord, DAY_MS,
};
use std::sync::mpsc::{channel, Receiver};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmFlush;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, LineTo, MoveToEx, InvalidateRect, ReleaseDC,
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
        // 对齐桌面合成的下一帧（≈垂直同步）。AgentSpeed 用 WPF 的
        // CompositionTarget.Rendering 做同样的事：节奏跟着合成器走，不会和
        // DWM 的呈现节拍错开（固定 sleep 16ms 会慢慢漂移，表现为偶发掉帧）。
        if unsafe { DwmFlush() } != 0 {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
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
    /// 逐日、逐来源的堆叠值：换区间/来源时按「同一天」对齐做过渡（同 AgentSpeed 的 Blend）
    from_stack: Vec<Vec<f32>>,
    to_stack: Vec<Vec<f32>>,
    now_stack: Vec<Vec<f32>>,
    /// 上一帧每个日期、每个来源的值
    previous: std::collections::HashMap<i64, Vec<f32>>,
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
    /// 图表卡片区域（动画期间只重画这块）
    chart_rect: RECT,
    /// 数据或尺寸变了，下一帧必须整页重画
    full_dirty: bool,
    /// 当前是否「按小时」视图（今天 / 昨天）
    hourly: bool,
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
            from_stack: Vec::new(),
            to_stack: Vec::new(),
            now_stack: Vec::new(),
            previous: std::collections::HashMap::new(),
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
            chart_rect: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            hourly: false,
            full_dirty: true,
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

    // 「今天 / 昨天」按小时看：横轴是 0-23 点
    let hourly = state.range <= 1;
    state.hourly = hourly;

    if hourly {
        let day = from;
        let hours = history::summarize_hours(&state.records, day, filter);
        state.days = hours
            .iter()
            .map(|row| DailyRow {
                day: row.hour as i64,
                output: row.output,
                input: row.input,
                requests: row.requests,
                measured_output: row.measured_output,
                measured_ms: row.measured_ms,
                peak_speed: row.peak_speed,
                main_model: row.main_model.clone(),
            })
            .collect();

        let mut rows = history::hourly_by_source(&state.records, day, &all_sources());
        if state.source != 0 {
            for row in rows.iter_mut() {
                for (slot, value) in row.iter_mut().enumerate() {
                    if slot + 1 != state.source {
                        *value = 0;
                    }
                }
            }
        }
        state.stack = rows;
    } else {
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
    }

    let target: Vec<f32> = state
        .days
        .iter()
        .map(|day| day.output as f32)
        .collect();

    // 过渡起点：按「同一天」从上一帧取值（AgentSpeed 的 HistoryBars.Blend 就是这么做的）。
    // 换区间时同一根柱子保持高度、只是重新排布，而不是全体从 0 长出来。
    // 小时的键用 day*24+hour，日视图用 day*24，两种模式不会互相污染。
    let slot_key = |index: usize, value: i64| -> i64 {
        if hourly {
            from * 24 + index as i64
        } else {
            value * 24
        }
    };
    let source_count = all_sources().len();
    state.from_stack = state
        .days
        .iter()
        .enumerate()
        .map(|(index, day)| {
            state
                .previous
                .get(&slot_key(index, day.day))
                .cloned()
                .unwrap_or_else(|| vec![0.0; source_count])
        })
        .collect();
    state.to_stack = state
        .stack
        .iter()
        .map(|row| row.iter().map(|value| *value as f32).collect())
        .collect();
    state.now_stack = state.from_stack.clone();
    state.previous = state
        .days
        .iter()
        .enumerate()
        .zip(state.to_stack.iter())
        .map(|((index, day), row)| (slot_key(index, day.day), row.clone()))
        .collect();

    state.from_heights = state.now_stack.iter().map(|row| row.iter().sum()).collect();
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
                    state.now_stack = state.to_stack.clone();
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
                // 逐日逐来源插值：换区间时同一根柱子按日期对齐，不会整体从 0 冒出来
                let mut rows = Vec::with_capacity(state.to_stack.len());
                for (index, to_row) in state.to_stack.iter().enumerate() {
                    let from_row = state.from_stack.get(index);
                    let mut row = Vec::with_capacity(to_row.len());
                    for (slot, to) in to_row.iter().enumerate() {
                        let from = from_row.and_then(|r| r.get(slot)).copied().unwrap_or(0.0);
                        row.push((from + (to - from) * t).max(0.0));
                    }
                    rows.push(row);
                }
                state.now_heights = rows.iter().map(|row| row.iter().sum()).collect();
                state.now_stack = rows;

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
            paint(hwnd, hdc, &ps);
            EndPaint(hwnd, &mut ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- 绘制

use std::cell::RefCell;
use std::collections::HashMap;

/// 复用的后台 DIB（避免每帧 CreateDIBSection 5 MB）
struct BackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    width: i32,
    height: i32,
}

thread_local! {
    static BACK: RefCell<Option<BackBuffer>> = RefCell::new(None);
    static FONTS: RefCell<HashMap<(i32, i32), HFONT>> = RefCell::new(HashMap::new());
}

/// 字体缓存：每帧现建现删 HFONT 是这窗口最贵的操作之一
fn font(points: f32, weight: i32) -> HFONT {
    let key = ((points * 100.0).round() as i32, weight);
    FONTS.with(|cache| {
        if let Some(handle) = cache.borrow().get(&key) {
            return *handle;
        }
        let handle = make_font(points, weight);
        cache.borrow_mut().insert(key, handle);
        handle
    })
}

fn back_buffer(width: i32, height: i32) -> Option<HDC> {
    BACK.with(|cell| {
        let mut slot = cell.borrow_mut();
        let resize = match slot.as_ref() {
            Some(buffer) => buffer.width != width || buffer.height != height,
            None => true,
        };
        if resize {
            if let Some(old) = slot.take() {
                unsafe {
                    SelectObject(old.dc, old.old);
                    DeleteObject(old.bitmap as *mut _);
                    DeleteDC(old.dc);
                }
            }
            unsafe {
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
                ReleaseDC(std::ptr::null_mut(), screen);
                if bitmap.is_null() || dc.is_null() {
                    return None;
                }
                let old = SelectObject(dc, bitmap as HGDIOBJ);
                *slot = Some(BackBuffer {
                    dc,
                    bitmap,
                    old,
                    width,
                    height,
                });
            }
        }
        slot.as_ref().map(|buffer| buffer.dc)
    })
}

// ---- 2026 视觉规范（0x00BBGGRR）----
const BG: u32 = 0x00F8_F6F5; // #F5F6F8
const SURFACE: u32 = 0x00FF_FFFF;
const BORDER: u32 = 0x00EF_EAE8; // #E8EAEF
const TEXT: u32 = 0x001A_1614; // #14161A
const TEXT2: u32 = 0x0080_726B; // #6B7280
const TEXT3: u32 = 0x00AF_A39A; // #9AA3AF
const CHIP: u32 = 0x00F3_F1EF; // 分段控件底色
const ROW_TINT: u32 = 0x00FC_FBFA;
const DARK: u32 = 0x001A_1614;

fn series_color(source: &str) -> u32 {
    match source {
        "opencode" => 0x00F6_823B,   // #3B82F6
        "dsh" => 0x00F6_5C8B,        // #8B5CF6
        "workbuddy" => 0x000B_9EF5,  // #F59E0B
        _ => 0x0081_B910,            // #10B981
    }
}

fn thousands(value: i64) -> String {
    let text = value.to_string();
    let mut out = String::new();
    let bytes = text.as_bytes();
    for (index, ch) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(*ch as char);
    }
    out
}

struct Painter {
    dc: HDC,
    width: i32,
    height: i32,
    scale: f32,
}

impl Painter {
    fn s(&self, value: f32) -> i32 {
        (value * self.scale).round() as i32
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

    unsafe fn round(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
        fill: u32,
        border: u32,
    ) {
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, border);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            self.dc,
            left,
            top,
            right,
            bottom,
            radius,
            radius,
        );
        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }

    unsafe fn dashed(&self, x1: i32, y1: i32, x2: i32, y2: i32, color: u32) {
        let pen = CreatePen(2, 1, color); // PS_DOT
        let old = SelectObject(self.dc, pen as HGDIOBJ);
        MoveToEx(self.dc, x1, y1, std::ptr::null_mut());
        LineTo(self.dc, x2, y2);
        SelectObject(self.dc, old);
        DeleteObject(pen as *mut _);
    }

    unsafe fn solid_line(&self, x1: i32, y1: i32, x2: i32, y2: i32, color: u32) {
        let pen = CreatePen(0, 1, color);
        let old = SelectObject(self.dc, pen as HGDIOBJ);
        MoveToEx(self.dc, x1, y1, std::ptr::null_mut());
        LineTo(self.dc, x2, y2);
        SelectObject(self.dc, old);
        DeleteObject(pen as *mut _);
    }

    unsafe fn dot(&self, cx: i32, cy: i32, radius: i32, color: u32) {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(0, 1, color);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::Ellipse(
            self.dc,
            cx - radius,
            cy - radius,
            cx + radius,
            cy + radius,
        );
        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }

    unsafe fn text(
        &self,
        text: &str,
        points: f32,
        weight: i32,
        color: u32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        align: u32,
    ) {
        let handle = font(points, weight);
        let old = SelectObject(self.dc, handle as HGDIOBJ);
        SetBkMode(self.dc, 1);
        SetTextColor(self.dc, color);
        let mut rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        let wide_text = wide(text);
        DrawTextW(
            self.dc,
            wide_text.as_ptr(),
            (wide_text.len() - 1) as i32,
            &mut rect,
            align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(self.dc, old);
    }

    unsafe fn text_width(&self, text: &str, points: f32, weight: i32) -> i32 {
        let handle = font(points, weight);
        let old = SelectObject(self.dc, handle as HGDIOBJ);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let wide_text = wide(text);
        DrawTextW(
            self.dc,
            wide_text.as_ptr(),
            (wide_text.len() - 1) as i32,
            &mut rect,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(self.dc, old);
        rect.right - rect.left
    }
}

fn make_font(points: f32, weight: i32) -> HFONT {
    let height = -((points * crate::screen_scale() * 96.0 / 72.0).round() as i32);
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
            wide("Microsoft YaHei UI").as_ptr(),
        )
    }
}

/// 版式：整页和「只重画图表」两条路径共用
struct Layout {
    pad: i32,
    row_top: i32,
    chips_h: i32,
    cards_top: i32,
    cards_h: i32,
    chart_top: i32,
    chart_bottom: i32,
    table_top: i32,
    table_bottom: i32,
}

/// 把轴上限凑成 1/2/2.5/5 × 10ⁿ 的整数刻度，返回 (轴上限, 单格步长)
fn nice_axis(max: f32, intervals: i32) -> (f64, f64) {
    let raw = (max / intervals as f32).max(1.0) as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let factor = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 2.5 {
        2.5
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    let step = factor * magnitude;
    (step * intervals as f64, step)
}


fn chart_ready(rect: &RECT) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

fn layout(p: &Painter) -> Layout {
    let pad = p.s(26.0);
    let row_top = p.s(86.0);
    let chips_h = p.s(30.0);
    let cards_top = p.s(130.0);
    let cards_h = p.s(92.0);
    let chart_top = p.s(238.0);
    let chart_bottom = chart_top + p.s(250.0);
    let table_top = chart_bottom + p.s(16.0);
    Layout {
        pad,
        row_top,
        chips_h,
        cards_top,
        cards_h,
        chart_top,
        chart_bottom,
        table_top,
        table_bottom: p.height - p.s(16.0),
    }
}

unsafe fn draw_chip(
    p: &Painter,
    rect: RECT,
    label: &str,
    active: bool,
    accent: Option<u32>,
) {
    if active {
        p.round(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            (rect.bottom - rect.top) / 2,
            DARK,
            DARK,
        );
    } else if accent.is_some() {
        // 来源按钮：透明底，只在悬停时才明显（这里保持轻量）
    }

    let color = if active { 0x00FF_FFFF } else { TEXT2 };
    if let Some(dot_color) = accent {
        let cx = rect.left + p.s(12.0);
        let cy = (rect.top + rect.bottom) / 2;
        p.dot(cx, cy, p.s(3.0), if active { 0x00FF_FFFF } else { dot_color });
    }
    p.text(
        label,
        8.5,
        if active { 600 } else { 400 },
        color,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom + 1,
        DT_CENTER,
    );
}

/// 整页绘制（数据变化、尺寸变化、露出时用）
unsafe fn draw_all(p: &Painter, state: &mut State) {
    let l = layout(p);
    p.fill(0, 0, p.width, p.height, BG);

    // ---- 标题
    p.text(
        "用量历史",
        17.0,
        700,
        TEXT,
        l.pad,
        p.s(18.0),
        p.width - l.pad,
        p.s(48.0),
        DT_LEFT,
    );

    let subtitle = if state.loading {
        "正在读取…".to_string()
    } else {
        let count = if state.source == 0 {
            thousands(state.records.len() as i64)
        } else {
            let wanted = all_sources()[state.source - 1];
            thousands(
                state
                    .records
                    .iter()
                    .filter(|record| {
                        record.source == wanted
                            && record.day >= state.from_day
                            && record.day <= state.to_day
                    })
                    .count() as i64,
            )
        };
        if state.from_day == state.to_day {
            format!("{}   ·   {} 条记录", format_day(state.from_day), count)
        } else {
            format!(
                "{} – {}   ·   {} 条记录",
                format_day(state.from_day),
                format_day(state.to_day),
                count
            )
        }
    };
    p.text(
        &subtitle,
        8.5,
        400,
        TEXT3,
        l.pad,
        p.s(50.0),
        p.width - l.pad,
        p.s(70.0),
        DT_LEFT,
    );

    // ---- 区间：分段控件
    state.pill_rects.clear();
    let chip_pad = p.s(14.0);
    let gap = p.s(4.0);
    let mut widths = [0_i32; 5];
    let mut total = 0;
    for (index, label) in RANGE_LABELS.iter().enumerate() {
        widths[index] = p.text_width(label, 8.5, 500) + chip_pad * 2;
        total += widths[index];
    }
    total += gap * (RANGE_LABELS.len() as i32 - 1);

    let group = RECT {
        left: l.pad,
        top: l.row_top - p.s(3.0),
        right: l.pad + total + p.s(6.0),
        bottom: l.row_top + l.chips_h + p.s(3.0),
    };
    p.round(
        group.left,
        group.top,
        group.right,
        group.bottom,
        (group.bottom - group.top) / 2,
        CHIP,
        CHIP,
    );

    let mut x = group.left + p.s(3.0);
    for (index, label) in RANGE_LABELS.iter().enumerate() {
        let rect = RECT {
            left: x,
            top: l.row_top,
            right: x + widths[index],
            bottom: l.row_top + l.chips_h,
        };
        draw_chip(p, rect, label, index == state.range, None);
        state.pill_rects.push(rect);
        x += widths[index] + gap;
    }

    // ---- 来源：右侧轻量按钮
    state.source_rects.clear();
    let labels = SOURCE_LABELS;
    let mut widths = [0_i32; 5];
    let mut total = 0;
    for (index, label) in labels.iter().enumerate() {
        widths[index] = p.text_width(label, 8.5, 500) + p.s(26.0) + if index == 0 { 0 } else { p.s(10.0) };
        total += widths[index];
    }
    total += gap * (labels.len() as i32 - 1);
    let label_width = p.text_width("来源", 8.5, 400) + p.s(14.0);
    let mut sx = p.width - l.pad - total - label_width - p.s(16.0);
    p.text(
        "来源",
        8.5,
        400,
        TEXT3,
        sx - label_width,
        l.row_top,
        sx - p.s(12.0),
        l.row_top + l.chips_h,
        DT_RIGHT,
    );

    for (index, label) in labels.iter().enumerate() {
        let rect = RECT {
            left: sx,
            top: l.row_top,
            right: sx + widths[index],
            bottom: l.row_top + l.chips_h,
        };
        let accent = if index == 0 {
            None
        } else {
            Some(series_color(all_sources()[index - 1]))
        };
        draw_chip(p, rect, label, index == state.source, accent);
        state.source_rects.push(rect);
        sx += widths[index] + gap;
    }

    // ---- 卡片
    let gap_card = p.s(14.0);
    let card_w = (p.width - l.pad * 2 - gap_card * 3) / 4;

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

    let per = if state.hourly { "时均" } else { "日均" };
    let cards: [(&str, String, String, u32); 4] = [
        (
            "输出 tokens",
            history::format_tokens(total_output),
            format!("{per} {}", history::format_tokens(total_output / days_count)),
            series_color("opencode"),
        ),
        (
            "输入 tokens",
            history::format_tokens(total_input),
            if total_output > 0 {
                format!("输入 / 输出 {}×", total_input / total_output.max(1))
            } else {
                "含缓存".to_string()
            },
            series_color("workbuddyAi"),
        ),
        (
            "请求数",
            thousands(total_requests),
            format!("{per} {}", thousands(total_requests / days_count)),
            series_color("dsh"),
        ),
        (
            "平均速度",
            if speed > 0.0 {
                format!("{speed:.1} t/s")
            } else {
                "—".to_string()
            },
            "按可测请求估算".to_string(),
            series_color("workbuddy"),
        ),
    ];

    for (index, (title, value, caption, accent)) in cards.iter().enumerate() {
        let left = l.pad + (card_w + gap_card) * index as i32;
        p.round(
            left,
            l.cards_top,
            left + card_w,
            l.cards_top + l.cards_h,
            p.s(14.0),
            SURFACE,
            BORDER,
        );

        let dot_y = l.cards_top + p.s(22.0);
        p.dot(left + p.s(16.0), dot_y, p.s(3.5), *accent);
        p.text(
            title,
            8.5,
            400,
            TEXT2,
            left + p.s(26.0),
            dot_y - p.s(10.0),
            left + card_w - p.s(14.0),
            dot_y + p.s(10.0),
            DT_LEFT,
        );
        p.text(
            value,
            17.5,
            700,
            TEXT,
            left + p.s(15.0),
            l.cards_top + p.s(32.0),
            left + card_w - p.s(12.0),
            l.cards_top + p.s(66.0),
            DT_LEFT,
        );
        p.text(
            caption,
            8.0,
            400,
            TEXT3,
            left + p.s(16.0),
            l.cards_top + p.s(64.0),
            left + card_w - p.s(12.0),
            l.cards_top + p.s(84.0),
            DT_LEFT,
        );
    }

    draw_chart(p, state);
    draw_table(p, state, &l);
}

/// 只重画图表卡片（动画期间用，其它区域保持不动）
unsafe fn draw_chart(p: &Painter, state: &mut State) {
    let l = layout(p);
    state.chart_rect = RECT {
        left: l.pad,
        top: l.chart_top,
        right: p.width - l.pad,
        bottom: l.chart_bottom,
    };

    p.round(
        l.pad,
        l.chart_top,
        p.width - l.pad,
        l.chart_bottom,
        p.s(14.0),
        SURFACE,
        BORDER,
    );

    p.text(
        if state.hourly { "每小时输出" } else { "每日输出" },
        9.5,
        600,
        TEXT,
        l.pad + p.s(18.0),
        l.chart_top + p.s(10.0),
        l.pad + p.s(200.0),
        l.chart_top + p.s(32.0),
        DT_LEFT,
    );

    // 图例
    let chart_title = if state.hourly { "每小时输出" } else { "每日输出" };
    let mut legend_x = l.pad + p.s(18.0) + p.text_width(chart_title, 9.5, 600) + p.s(18.0);
    let sources = all_sources();
    for source in sources {
        let index = sources.iter().position(|item| *item == source).unwrap_or(0);
        let has = state
            .stack
            .iter()
            .any(|row| row.get(index).copied().unwrap_or(0) > 0);
        if !has {
            continue;
        }
        let name = source_name(source);
        p.dot(
            legend_x + p.s(4.0),
            l.chart_top + p.s(21.0),
            p.s(3.5),
            series_color(source),
        );
        p.text(
            name,
            8.0,
            400,
            TEXT2,
            legend_x + p.s(12.0),
            l.chart_top + p.s(10.0),
            legend_x + p.s(140.0),
            l.chart_top + p.s(32.0),
            DT_LEFT,
        );
        legend_x += p.text_width(name, 8.0, 400) + p.s(28.0);
    }

    let plot_left = l.pad + p.s(54.0);
    let plot_right = p.width - l.pad - p.s(18.0);
    let plot_top = l.chart_top + p.s(46.0);
    let plot_bottom = l.chart_bottom - p.s(30.0);
    let plot_height = (plot_bottom - plot_top).max(10);
    let plot_width = (plot_right - plot_left).max(10);

    // 轴上限取「好看的」整数刻度（1/2/2.5/5 × 10ⁿ），柱子按它缩放
    let (axis_max, tick_step) = nice_axis(state.max_now.max(1.0), 3);

    // 网格 + 刻度
    for step in 0..=3 {
        let y = plot_bottom - plot_height * step / 3;
        p.dashed(plot_left, y, plot_right, y, 0x00E6_E9EE);
        let value = (tick_step * step as f64) as i64;
        let label = if value == 0 {
            "0".to_string()
        } else {
            history::format_tokens(value)
        };
        p.text(
            &label,
            7.5,
            400,
            TEXT3,
            l.pad,
            y - p.s(9.0),
            plot_left - p.s(10.0),
            y + p.s(9.0),
            DT_RIGHT,
        );
    }

    let count = state.days.len();
    if count == 0 {
        return;
    }

    let max_output = axis_max as f32;
    let slot = plot_width as f32 / count as f32;
    let bar_width = (slot * 0.58).clamp(4.0, 40.0);
    let radius = (bar_width * 0.28).clamp(2.0, 8.0) as i32;

    for index in 0..state.days.len() {
        let Some(row) = state.now_stack.get(index) else {
            continue;
        };
        let total: f32 = row.iter().sum();
        if total <= 0.5 {
            continue;
        }

        // 像素对齐，1px 的边不会糊
        let slot_left = plot_left as f32 + slot * index as f32;
        let left = (slot_left + (slot - bar_width) / 2.0).round() as i32;
        let right = (slot_left + (slot + bar_width) / 2.0).round() as i32;
        let mut bottom = plot_bottom as f32;

        for (slot_index, source) in sources.iter().enumerate() {
            let value = row.get(slot_index).copied().unwrap_or(0.0);
            if value <= 0.0 {
                continue;
            }

            // 段高直接按数值换算，和整根柱子的高度一致（逐来源过渡）
            let segment = plot_height as f32 * value / max_output;
            bottom -= segment;
            if segment < 1.0 {
                continue;
            }

            let color = series_color(source);
            let top = bottom.round() as i32;
            let bottom_px = (bottom + segment).round() as i32;
            // 只有整根柱子最上面一段带圆角，堆叠处保持平直
            let is_top = bottom_px >= plot_bottom - 1;
            if is_top {
                p.round(left, top, right, bottom_px, radius, color, color);
                if bottom_px - radius > top {
                    p.fill(left, bottom_px - radius, right, bottom_px, color);
                }
            } else {
                p.fill(left, top, right, bottom_px, color);
            }
        }
    }

    // X 轴
    let label_step = (count / 8).max(1);
    for (index, day) in state.days.iter().enumerate() {
        if index % label_step != 0 && index + 1 != count {
            continue;
        }
        let x = plot_left as f32 + slot * index as f32;
        p.text(
            &if state.hourly { format!("{:02}", day.day) } else { day_label(day.day) },
            7.5,
            400,
            TEXT3,
            (x - p.s(22.0) as f32) as i32,
            plot_bottom + p.s(6.0),
            (x + slot + p.s(22.0) as f32) as i32,
            plot_bottom + p.s(26.0),
            DT_CENTER,
        );
    }
}

unsafe fn draw_table(p: &Painter, state: &mut State, l: &Layout) {
    p.round(
        l.pad,
        l.table_top,
        p.width - l.pad,
        l.table_bottom,
        p.s(14.0),
        SURFACE,
        BORDER,
    );

    let columns = [0.17f32, 0.10, 0.12, 0.12, 0.10, 0.10, 0.28];
    let mut left = l.pad + p.s(18.0);
    let usable = p.width - l.pad * 2 - p.s(36.0);
    let mut column_left = [0_i32; 7];
    for (index, weight) in columns.iter().enumerate() {
        column_left[index] = left;
        left += (usable as f32 * weight) as i32;
    }

    let header_top = l.table_top + p.s(10.0);
    let header_bottom = header_top + p.s(22.0);
    let headers = [if state.hourly { "小时" } else { "日期" }, "请求", "输出", "输入", "速度", "峰值", "主要模型"];
    for (index, header) in headers.iter().enumerate() {
        let align = if index == 0 || index == 6 { DT_LEFT } else { DT_RIGHT };
        let right = if index == 6 {
            column_left[index] + p.s(220.0)
        } else {
            column_left[index] + (usable as f32 * columns[index]) as i32 - p.s(10.0)
        };
        p.text(
            header,
            8.0,
            500,
            TEXT3,
            column_left[index],
            header_top,
            right,
            header_bottom,
            align,
        );
    }

    p.solid_line(
        l.pad + p.s(14.0),
        header_bottom + p.s(4.0),
        p.width - l.pad - p.s(14.0),
        header_bottom + p.s(4.0),
        BORDER,
    );

    let row_height = p.s(30.0);
    let list_top = header_bottom + p.s(10.0);
    let available = ((l.table_bottom - list_top - p.s(6.0)) / row_height).max(1) as usize;
    let max_scroll = state
        .days
        .iter()
        .filter(|day| day.requests > 0)
        .count()
        .saturating_sub(available);
    if state.scroll > max_scroll {
        state.scroll = max_scroll;
    }

    let rows: Vec<&DailyRow> = state.days.iter().rev().filter(|day| day.requests > 0).collect();
    let mut y = list_top;
    for (index, day) in rows.iter().skip(state.scroll).take(available).enumerate() {
        if index % 2 == 1 {
            p.fill(
                l.pad + p.s(8.0),
                y,
                p.width - l.pad - p.s(8.0),
                y + row_height,
                ROW_TINT,
            );
        }

        let text_bottom = y + row_height;
        p.text(
            &if state.hourly { format!("{:02}:00", day.day) } else { day_full_label(day.day) },
            8.5,
            500,
            TEXT,
            column_left[0],
            y,
            column_left[0] + p.s(120.0),
            text_bottom,
            DT_LEFT,
        );

        let numbers = [
            (1, thousands(day.requests)),
            (2, history::format_tokens(day.output)),
            (3, history::format_tokens(day.input)),
            (
                4,
                if day.speed() > 0.0 {
                    format!("{:.0} t/s", day.speed())
                } else {
                    "—".to_string()
                },
            ),
            (
                5,
                if day.peak_speed > 0.0 {
                    format!("{:.0} t/s", day.peak_speed)
                } else {
                    "—".to_string()
                },
            ),
        ];

        for (column, text) in numbers {
            let right =
                column_left[column] + (usable as f32 * columns[column]) as i32 - p.s(12.0);
            p.text(
                &text,
                8.5,
                400,
                if column == 5 { TEXT3 } else { TEXT },
                column_left[column],
                y,
                right,
                text_bottom,
                DT_RIGHT,
            );
        }

        p.text(
            &day.main_model,
            8.5,
            400,
            TEXT3,
            column_left[6],
            y,
            column_left[6] + p.s(250.0),
            text_bottom,
            DT_LEFT,
        );

        y += row_height;
    }
}

fn paint(hwnd: HWND, hdc: HDC, ps: &PAINTSTRUCT) {
    unsafe {
        let parent = GetParent(hwnd);
        let state = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut State;
        if state.is_null() {
            return;
        }
        let state = &mut *state;

        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetClientRect(hwnd, &mut client);
        let Some(dc) = back_buffer(client.right, client.bottom) else {
            return;
        };

        let p = Painter {
            dc,
            width: client.right,
            height: client.bottom,
            scale: crate::screen_scale(),
        };

        let chart = state.chart_rect;
        let dirty = ps.rcPaint;
        let chart_only = !state.full_dirty
            && chart_ready(&chart)
            && dirty.left >= chart.left
            && dirty.top >= chart.top
            && dirty.right <= chart.right
            && dirty.bottom <= chart.bottom;

        if chart_only {
            draw_chart(&p, state);
        } else {
            draw_all(&p, state);
            state.full_dirty = false;
        }


        BitBlt(
            hdc,
            dirty.left,
            dirty.top,
            dirty.right - dirty.left,
            dirty.bottom - dirty.top,
            dc,
            dirty.left,
            dirty.top,
            SRCCOPY,
        );
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
