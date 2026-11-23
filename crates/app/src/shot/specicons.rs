//! 按《截图与标注工具彩色图标设计规范》重画的图标。
//!
//! 只处理和旧画法**形状不同**的那几个（保存是软盘、撤销/重做是 U 型回转箭头、
//! 聚光灯是手电、贴图是图钉、美化是魔术棒…），其余仍走原来的几何画法。
//! 所有坐标按 24×24 网格给，再缩放到实际矩形。

use super::*;

/// 在 24 单位网格里画：`u` 是一格对应的像素
struct Grid {
    dc: HDC,
    cx: i32,
    cy: i32,
    u: f32,
    ink: u32,
    tint: u32,
    pen: i32,
}

impl Grid {
    fn x(&self, value: f32) -> i32 {
        self.cx + ((value - 12.0) * self.u) as i32
    }
    fn y(&self, value: f32) -> i32 {
        self.cy + ((value - 12.0) * self.u) as i32
    }

    unsafe fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32) {
        icon_line(self.dc, self.x(x1), self.y(y1), self.x(x2), self.y(y2), self.pen, self.ink);
    }

    unsafe fn poly(&self, points: &[(f32, f32)]) {
        let converted: Vec<POINT> = points
            .iter()
            .map(|(x, y)| POINT {
                x: self.x(*x),
                y: self.y(*y),
            })
            .collect();
        icon_polyline(self.dc, &converted, self.pen, self.ink, false);
    }

    unsafe fn round(&self, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32, fill: bool) {
        let (brush, owned) = if fill {
            (CreateSolidBrush(self.tint), true)
        } else {
            (
                windows_sys::Win32::Graphics::Gdi::GetStockObject(
                    windows_sys::Win32::Graphics::Gdi::NULL_BRUSH,
                ) as windows_sys::Win32::Graphics::Gdi::HBRUSH,
                false,
            )
        };
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let pen = CreatePen(0, self.pen, self.ink);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        let diameter = (radius * 2.0 * self.u) as i32;
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            self.dc,
            self.x(x0),
            self.y(y0),
            self.x(x1),
            self.y(y1),
            diameter,
            diameter,
        );
        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        if owned {
            DeleteObject(brush as *mut _);
        }
    }

    unsafe fn fill_box(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: u32) {
        let brush = CreateSolidBrush(color);
        let rect = RECT {
            left: self.x(x0),
            top: self.y(y0),
            right: self.x(x1),
            bottom: self.y(y1),
        };
        FillRect(self.dc, &rect, brush);
        DeleteObject(brush as *mut _);
    }

    unsafe fn dots(&self, x0: f32, y0: f32, x1: f32, y1: f32) {
        icon_dot(self.dc, self.x(x0), self.y(y0), self.x(x1), self.y(y1), self.ink);
    }

    /// 箭头三角（tip 是尖，angle 指向角度：0=右 90=下 180=左 270=上）
    unsafe fn head(&self, tip: (f32, f32), angle: f32, size: f32) {
        // 箭尖 + 两侧各偏 28 度的翼
        let back = size * 1.6;
        let wing = size * 0.95;
        let rad = angle.to_radians();
        let base = (tip.0 - back * rad.cos(), tip.1 - back * rad.sin());
        let left = (angle + 118.0).to_radians();
        let right = (angle - 118.0).to_radians();
        let points = [
            (tip.0, tip.1),
            (base.0 + wing * left.cos(), base.1 + wing * left.sin()),
            (tip.0 - back * 0.55 * rad.cos(), tip.1 - back * 0.55 * rad.sin()),
            (base.0 + wing * right.cos(), base.1 + wing * right.sin()),
        ];
        let converted: Vec<POINT> = points
            .iter()
            .map(|(x, y)| POINT {
                x: self.x(*x),
                y: self.y(*y),
            })
            .collect();
        let brush = CreateSolidBrush(self.ink);
        let old_brush = SelectObject(self.dc, brush as HGDIOBJ);
        let pen = CreatePen(0, 1, self.ink);
        let old_pen = SelectObject(self.dc, pen as HGDIOBJ);
        Polygon(self.dc, converted.as_ptr(), converted.len() as i32);
        SelectObject(self.dc, old_pen);
        SelectObject(self.dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    }
}

/// 画规范版图标；不认识的返回 false，交回老画法
pub(super) unsafe fn draw(dc: HDC, action: Action, rect: RECT) -> bool {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return false;
    }
    let (ink, tint) = icon_color(action);
    let grid = Grid {
        dc,
        cx: (rect.left + rect.right) / 2,
        cy: (rect.top + rect.bottom) / 2,
        u: width.min(height) as f32 / 24.0,
        ink,
        tint,
        pen: ((width.min(height) as f32 / 24.0) * 2.0).round().max(2.0) as i32,
    };

    match action {
        // 软盘
        Action::Save => {
            grid.round(4.0, 3.0, 20.0, 21.0, 2.0, true);
            grid.fill_box(8.0, 3.6, 16.0, 9.0, 0x00FF_FFFF);
            grid.fill_box(8.0, 14.0, 16.0, 20.4, 0x00FF_FFFF);
        }
        // U 型回转箭头（左）
        Action::Undo => {
            grid.poly(&[(18.0, 7.0), (18.0, 12.0), (10.0, 12.0)]);
            grid.line(10.0, 12.0, 10.0, 15.0);
            grid.head((10.0, 16.5), 90.0, 3.2);
        }
        // U 型回转箭头（右）
        Action::Redo => {
            grid.poly(&[(6.0, 7.0), (6.0, 12.0), (14.0, 12.0)]);
            grid.line(14.0, 12.0, 14.0, 15.0);
            grid.head((14.0, 16.5), 90.0, 3.2);
        }
        // 马赛克：深藏青 3×3 棋盘
        Action::Tool(Tool::Mosaic) => {
            grid.round(3.0, 3.0, 21.0, 21.0, 3.0, false);
            for row in 0..3 {
                for column in 0..3 {
                    if (row + column) % 2 == 0 {
                        let x0 = 5.0 + column as f32 * 5.0;
                        let y0 = 5.0 + row as f32 * 5.0;
                        grid.fill_box(x0, y0, x0 + 4.0, y0 + 4.0, 0x008A_3A1D);
                    }
                }
            }
        }
        // 聚光灯：手电 + 光线
        Action::Tool(Tool::Spotlight) => {
            // 灯身：指向左上方的圆角方块
            grid.poly(&[
                (4.0, 12.0),
                (4.0, 6.0),
                (10.0, 6.0),
                (14.0, 10.0),
                (10.0, 14.0),
                (4.0, 14.0),
            ]);
            grid.line(6.5, 8.5, 11.0, 8.5);
            // 三束光
            grid.line(14.5, 6.0, 19.5, 3.0);
            grid.line(16.5, 10.0, 21.0, 10.0);
            grid.line(14.5, 13.0, 19.5, 16.5);
        }
        // 贴图：图钉
        Action::Pin => {
            // 图钉：钉子头（实心圆）+ 针身 + 底座
            grid.dots(9.0, 5.0, 15.0, 11.0);
            grid.line(12.0, 11.0, 12.0, 16.5);
            grid.line(8.5, 18.0, 15.5, 18.0);
        }
        // 美化：魔术棒 + 星芒
        Action::Beautify => {
            grid.line(5.5, 18.5, 13.5, 10.5);
            grid.line(13.5, 10.5, 16.0, 8.0);
            for (x, y, size) in [(17.5f32, 6.0f32, 2.6f32), (20.0, 11.0, 2.2), (13.0, 4.5, 2.0)] {
                grid.line(x - size, y, x + size, y);
                grid.line(x, y - size, x, y + size);
            }
        }
        // 滚动截图：虚线框 + 上下箭头
        Action::Scroll => {
            let pen = CreatePen(1 /* PS_DOT */, grid.pen, grid.ink);
            let old = SelectObject(dc, pen as HGDIOBJ);
            let brush = SelectObject(
                dc,
                windows_sys::Win32::Graphics::Gdi::GetStockObject(
                    windows_sys::Win32::Graphics::Gdi::NULL_BRUSH,
                ) as HGDIOBJ,
            );
            windows_sys::Win32::Graphics::Gdi::RoundRect(
                dc,
                grid.x(3.0),
                grid.y(3.0),
                grid.x(21.0),
                grid.y(21.0),
                (4.0 * grid.u) as i32,
                (4.0 * grid.u) as i32,
            );
            SelectObject(dc, brush);
            SelectObject(dc, old);
            DeleteObject(pen as *mut _);

            grid.line(12.0, 7.0, 12.0, 17.0);
            grid.head((12.0, 6.0), 270.0, 3.0);
            grid.head((12.0, 18.0), 90.0, 3.0);
        }
        // 橡皮擦：斜方块 + 底线
        Action::Tool(Tool::Eraser) => {
            grid.poly(&[(6.0, 14.0), (13.0, 7.0), (18.0, 11.0), (11.0, 18.0), (6.0, 14.0)]);
            grid.line(9.5, 10.5, 14.5, 14.5);
            grid.line(6.0, 20.5, 18.0, 20.5);
        }
        // 高亮：荧光笔 + 底线
        Action::Tool(Tool::Marker) => {
            grid.poly(&[(8.0, 15.0), (12.5, 10.5), (15.5, 13.5), (11.0, 18.0), (8.0, 15.0)]);
            grid.line(12.5, 10.5, 16.0, 7.0);
            grid.line(7.0, 20.5, 17.0, 20.5);
        }
        // 翻译：两张卡片（A / 文）
        Action::Tool(Tool::Translate) => {
            grid.round(11.0, 3.0, 21.0, 13.0, 2.0, false);
            grid.round(3.0, 9.0, 13.0, 19.0, 2.0, true);
            let rect = RECT {
                left: grid.x(5.0),
                top: grid.y(11.0),
                right: grid.x(11.0),
                bottom: grid.y(17.0),
            };
            draw_glyph_sized(dc, "A", rect, ink, (grid.u * 7.0).max(6.0) / 3.0);
        }
        // OCR：四角取景框 + A
        Action::Tool(Tool::Ocr) => {
            let corner = 5.0;
            grid.poly(&[(3.0, 3.0 + corner), (3.0, 3.0), (3.0 + corner, 3.0)]);
            grid.poly(&[(21.0 - corner, 3.0), (21.0, 3.0), (21.0, 3.0 + corner)]);
            grid.poly(&[(3.0, 21.0 - corner), (3.0, 21.0), (3.0 + corner, 21.0)]);
            grid.poly(&[(21.0 - corner, 21.0), (21.0, 21.0), (21.0, 21.0 - corner)]);
            let rect = RECT {
                left: grid.x(8.0),
                top: grid.y(8.0),
                right: grid.x(16.0),
                bottom: grid.y(16.0),
            };
            draw_glyph_sized(dc, "A", rect, ink, (grid.u * 8.0).max(6.0) / 3.0);
        }
        // 浓淡：三条粗细递增的横线
        Action::Stroke => {
            let steps = [(7.0f32, 1.4f32), (12.0f32, 2.4f32), (17.0f32, 3.4f32)];
            for (y, weight) in steps {
                let pen = CreatePen(0, (weight * grid.u).max(2.0) as i32, grid.ink);
                let old = SelectObject(dc, pen as HGDIOBJ);
                MoveToEx(dc, grid.x(5.0), grid.y(y), std::ptr::null_mut());
                LineTo(dc, grid.x(19.0), grid.y(y));
                SelectObject(dc, old);
                DeleteObject(pen as *mut _);
            }
        }
        _ => return false,
    }
    true
}
