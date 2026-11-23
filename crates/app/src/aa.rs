//! 圆角 / 圆 / 斜线的抗锯齿。
//!
//! GDI 的 `RoundRect`、`Ellipse`、`LineTo` 都是硬边：圆角和斜线放大看全是台阶。
//! 这里用 GDI+ 的 `SmoothingModeAntiAlias` 画同一批图形——坐标、颜色照旧传进来，
//! 只是把「画」这一步换成带抗锯齿的版本；文字仍然走 GDI（ClearType 更清晰）。
//!
//! 颜色是 GDI 的 `0x00BBGGRR`，GDI+ 要 `0xAARRGGBB`，进门前统一换（见 [`argb`]）。
//!
//! 注意：GDI 的画笔压在图形**内侧**，GDI+ 的描边骑在路径上（内外各半像素）。
//! 1px 描边看上去只差半像素，换成抗锯齿之后反而更接近设计稿的「细而不断」。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use windows_sys::Win32::Graphics::Gdi::HDC;
use windows_sys::Win32::Graphics::GdiPlus::{
    GdipAddPathArc, GdipAddPathEllipse, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath,
    GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
    GdipDeletePen, GdipDrawLine, GdipDrawPath, GdipFillPath, GdipResetClip,
    GdipResetWorldTransform, GdipSetClipRectI, GdipSetSmoothingMode, GdipTranslateWorldTransform,
    GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, GpBrush, GpGraphics, GpPath, GpPen,
    GpSolidFill,
};

/// `SmoothingModeAntiAlias`
const SMOOTHING_ANTIALIAS: i32 = 4;
/// `FillModeAlternate`
const FILL_ALTERNATE: i32 = 0;
/// `UnitPixel`
const UNIT_PIXEL: i32 = 2;
/// `MatrixOrderAppend`
const MATRIX_ORDER_APPEND: i32 = 1;
/// `CombineModeReplace`
const COMBINE_MODE_REPLACE: i32 = 0;

static STARTED: AtomicBool = AtomicBool::new(false);

/// GDI+ 只需要启动一次；起不来就退回 GDI 的硬边画法，不影响功能。
fn started() -> bool {
    static START: Once = Once::new();
    START.call_once(|| unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: 0,
            SuppressExternalCodecs: 0,
        };
        let mut token: usize = 0;
        let mut output: GdiplusStartupOutput = std::mem::zeroed();
        let ok = GdiplusStartup(&mut token, &input, &mut output) == 0;
        STARTED.store(ok, Ordering::Relaxed);
    });
    STARTED.load(Ordering::Relaxed)
}

/// GDI 的 `0x00BBGGRR` → GDI+ 的 `0xAARRGGBB`
pub fn argb(color: u32) -> u32 {
    let r = color & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = (color >> 16) & 0xFF;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// 一次绘制期间共用的 GDI+ 画布（对应一个 DC）
pub struct Canvas {
    graphics: *mut GpGraphics,
}

impl Canvas {
    /// 建画布；GDI+ 起不来或者这个 DC 建不出画布时返回 None（调用方退回 GDI）
    pub unsafe fn new(dc: HDC) -> Option<Self> {
        if !started() {
            return None;
        }
        let mut graphics: *mut GpGraphics = std::ptr::null_mut();
        if GdipCreateFromHDC(dc, &mut graphics) != 0 || graphics.is_null() {
            return None;
        }
        GdipSetSmoothingMode(graphics, SMOOTHING_ANTIALIAS);
        Some(Self { graphics })
    }

    /// 圆角矩形（填充 + 1px 描边）。`radius` 是圆角直径，和 GDI `RoundRect` 一致。
    pub unsafe fn round(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
        fill: u32,
        border: u32,
    ) {
        let Some(path) = self.round_path(left, top, right, bottom, radius) else {
            return;
        };
        self.fill_path(path, fill);
        self.stroke_path(path, border, 1.0);
        GdipDeletePath(path);
    }

    /// 空心圆角框（焦点框）：只描边
    pub unsafe fn ring(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
        color: u32,
        width: i32,
    ) {
        let Some(path) = self.round_path(left, top, right, bottom, radius) else {
            return;
        };
        self.stroke_path(path, color, width.max(1) as f32);
        GdipDeletePath(path);
    }

    /// 圆（填充 + 1px 描边）
    pub unsafe fn circle(&self, cx: i32, cy: i32, radius: i32, fill: u32, border: u32) {
        if radius <= 0 {
            return;
        }
        let mut path: *mut GpPath = std::ptr::null_mut();
        if GdipCreatePath(FILL_ALTERNATE, &mut path) != 0 || path.is_null() {
            return;
        }
        let d = (radius * 2) as f32;
        GdipAddPathEllipse(path, (cx - radius) as f32, (cy - radius) as f32, d, d);
        self.fill_path(path, fill);
        self.stroke_path(path, border, 1.0);
        GdipDeletePath(path);
    }

    /// 直线（1~2px 的细线也有了渐变的边缘）
    pub unsafe fn line(&self, x1: i32, y1: i32, x2: i32, y2: i32, color: u32, width: i32) {
        let Some(pen) = self.pen(color, width.max(1) as f32) else {
            return;
        };
        GdipDrawLine(self.graphics, pen, x1 as f32, y1 as f32, x2 as f32, y2 as f32);
        GdipDeletePen(pen);
    }

    /// 平移：GDI 那套用 `SetViewportOrgEx` 做滚动，但 GDI+ 不认视口原点，
    /// 得在这里补一笔（内容滚了多少，就把画布往上挪多少）。
    pub unsafe fn translate(&self, dx: i32, dy: i32) {
        GdipTranslateWorldTransform(self.graphics, dx as f32, dy as f32, MATRIX_ORDER_APPEND);
    }

    /// 加一块裁剪区（世界坐标，和画图用的一致）：GDI 的 `IntersectClipRect`
    /// 同样管不到 GDI+，滚动时得自己切一刀。
    pub unsafe fn clip_rect(&self, left: i32, top: i32, right: i32, bottom: i32) {
        GdipSetClipRectI(
            self.graphics,
            left,
            top,
            right - left,
            bottom - top,
            COMBINE_MODE_REPLACE,
        );
    }

    /// 回到单位变换 + 整块画布（画完滚动区、要接着画页脚时调）
    pub unsafe fn reset(&self) {
        GdipResetWorldTransform(self.graphics);
        GdipResetClip(self.graphics);
    }

    unsafe fn round_path(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        radius: i32,
    ) -> Option<*mut GpPath> {
        let width = right - left;
        let height = bottom - top;
        if width <= 0 || height <= 0 {
            return None;
        }
        let mut path: *mut GpPath = std::ptr::null_mut();
        if GdipCreatePath(FILL_ALTERNATE, &mut path) != 0 || path.is_null() {
            return None;
        }
        // GDI 的圆角直径 → GDI+ 的角直径；不能超过短边
        let d = (radius.max(0) as f32).min(width as f32).min(height as f32);
        let (l, t) = (left as f32, top as f32);
        let (r, b) = (right as f32, bottom as f32);
        // 四个角：左上 180°、右上 270°、右下 0°、左下 90°，各扫 90°
        GdipAddPathArc(path, l, t, d, d, 180.0, 90.0);
        GdipAddPathArc(path, r - d, t, d, d, 270.0, 90.0);
        GdipAddPathArc(path, r - d, b - d, d, d, 0.0, 90.0);
        GdipAddPathArc(path, l, b - d, d, d, 90.0, 90.0);
        GdipClosePathFigure(path);
        Some(path)
    }

    unsafe fn fill_path(&self, path: *mut GpPath, color: u32) {
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        if GdipCreateSolidFill(argb(color), &mut brush) != 0 || brush.is_null() {
            return;
        }
        GdipFillPath(self.graphics, brush as *mut GpBrush, path);
        GdipDeleteBrush(brush as *mut GpBrush);
    }

    unsafe fn stroke_path(&self, path: *mut GpPath, color: u32, width: f32) {
        let Some(pen) = self.pen(color, width) else {
            return;
        };
        GdipDrawPath(self.graphics, pen, path);
        GdipDeletePen(pen);
    }

    unsafe fn pen(&self, color: u32, width: f32) -> Option<*mut GpPen> {
        let mut pen: *mut GpPen = std::ptr::null_mut();
        if GdipCreatePen1(argb(color), width, UNIT_PIXEL, &mut pen) != 0 || pen.is_null() {
            return None;
        }
        Some(pen)
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            if !self.graphics.is_null() {
                GdipDeleteGraphics(self.graphics);
            }
        }
    }
}

// ---------------------------------------------------------------- 一趟绘制共用一块画布

thread_local! {
    /// 当前这一趟绘制用的画布（画什么 DC 就建什么 DC 的）
    static ACTIVE: RefCell<Option<(HDC, Canvas)>> = const { RefCell::new(None) };
}

/// 一次绘制开始：这一趟里的 [`round`] / [`circle`] / [`line`] 都复用同一个 GDI+ 画布，
/// 省掉每个图形建一次画布的开销（工具栏一次要画上百条线）。drop 时自动清掉。
pub struct Scope;

pub unsafe fn begin(dc: HDC) -> Scope {
    ACTIVE.with(|cell| *cell.borrow_mut() = Canvas::new(dc).map(|canvas| (dc, canvas)));
    Scope
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|cell| *cell.borrow_mut() = None);
    }
}

/// 有活动画布就用它画；没有就现建一块（一帧只有几颗胶囊的场合够用了）。
fn with<F>(dc: HDC, draw: F) -> bool
where
    F: Fn(&Canvas),
{
    let reused = ACTIVE.with(|cell| {
        let borrowed = cell.borrow();
        match borrowed.as_ref() {
            Some((active, canvas)) if *active == dc => {
                draw(canvas);
                true
            }
            _ => false,
        }
    });
    if reused {
        return true;
    }
    unsafe {
        match Canvas::new(dc) {
            Some(canvas) => {
                draw(&canvas);
                true
            }
            None => false,
        }
    }
}

/// 圆角矩形；GDI+ 起不来时返回 false，调用方退回 GDI 的 `RoundRect`
pub unsafe fn round(
    dc: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    fill: u32,
    border: u32,
) -> bool {
    with(dc, |canvas| {
        canvas.round(left, top, right, bottom, radius, fill, border)
    })
}

/// 圆角描边（空心，只画边）；返回 false 表示没画成
pub unsafe fn ring(
    dc: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    color: u32,
    width: i32,
) -> bool {
    with(dc, |canvas| {
        canvas.ring(left, top, right, bottom, radius, color, width)
    })
}

/// 直线；返回 false 表示没画成（调用方退回 `MoveToEx/LineTo`）
pub unsafe fn line(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: u32, width: i32) -> bool {
    with(dc, |canvas| canvas.line(x1, y1, x2, y2, color, width))
}
