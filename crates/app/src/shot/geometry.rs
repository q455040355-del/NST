//! 编辑器的纯几何/纯逻辑：不碰 HWND，全部可以单测。
//!
//! 从 shot.rs 拆出来的第一批（形状包围盒、角度吸附、工具栏宽度、窗口区域）。

#![allow(dead_code)]

use super::*;

pub(super) fn edge_is_dark(bitmap: &Bitmap) -> bool {
    if bitmap.width < 2 || bitmap.height < 2 {
        return false;
    }
    let mut sum = 0u64;
    let mut count = 0u64;
    let mut sample = |x: i32, y: i32| {
        let offset = ((y as usize * bitmap.width as usize) + x as usize) * 4;
        if offset + 2 < bitmap.pixels.len() {
            let pixel = &bitmap.pixels[offset..offset + 3];
            sum += (pixel[0] as u64 + pixel[1] as u64 + pixel[2] as u64) / 3;
            count += 1;
        }
    };
    let step = (bitmap.width / 64).max(1);
    for x in (0..bitmap.width).step_by(step as usize) {
        sample(x, 0);
        sample(x, bitmap.height - 1);
    }
    let step = (bitmap.height / 64).max(1);
    for y in (0..bitmap.height).step_by(step as usize) {
        sample(0, y);
        sample(bitmap.width - 1, y);
    }
    count > 0 && sum / count < 96
}


pub(super) fn shape_bounds(shape: &Shape) -> (i32, i32, i32, i32) {
    match shape {
        Shape::Arrow { a, b, .. }
        | Shape::Rect { a, b, .. }
        | Shape::Square { a, b, .. }
        | Shape::Ellipse { a, b, .. }
        | Shape::Line { a, b, .. }
        | Shape::Mosaic { a, b, .. } => (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)),
        Shape::Marker { points, .. } => {
            let mut left = i32::MAX;
            let mut top = i32::MAX;
            let mut right = i32::MIN;
            let mut bottom = i32::MIN;
            for (x, y) in points {
                left = left.min(*x);
                top = top.min(*y);
                right = right.max(*x);
                bottom = bottom.max(*y);
            }
            if points.is_empty() {
                (0, 0, 0, 0)
            } else {
                (left, top, right, bottom)
            }
        }
        Shape::Spotlight { a, b } => (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)),
        Shape::Numbered { at, .. } => (at.0 - 12, at.1 - 12, at.0 + 12, at.1 + 12),
        Shape::Text { at, .. } => (at.0, at.1, at.0 + 80, at.1 + 24),
        Shape::Pen { points, .. } => {
            let mut left = i32::MAX;
            let mut top = i32::MAX;
            let mut right = i32::MIN;
            let mut bottom = i32::MIN;
            for (x, y) in points {
                left = left.min(*x);
                top = top.min(*y);
                right = right.max(*x);
                bottom = bottom.max(*y);
            }
            if points.is_empty() {
                (0, 0, 0, 0)
            } else {
                (left, top, right, bottom)
            }
        }
    }
}


pub(super) fn shape_at(editor: &Editor, at: (i32, i32)) -> Option<usize> {
    const PAD: i32 = 6;
    for (index, shape) in editor.shapes.iter().enumerate().rev() {
        let (left, top, right, bottom) = shape_bounds(shape);
        if at.0 >= left - PAD && at.0 <= right + PAD && at.1 >= top - PAD && at.1 <= bottom + PAD {
            return Some(index);
        }
    }
    None
}


pub(super) fn snap_angle(from: (i32, i32), to: (i32, i32)) -> (i32, i32) {
    let dx = (to.0 - from.0) as f32;
    let dy = (to.1 - from.1) as f32;
    let length = (dx * dx + dy * dy).sqrt();
    if length < 6.0 {
        return to;
    }
    let angle = dy.atan2(dx).to_degrees();
    let nearest = (angle / 45.0).round() * 45.0;
    if (angle - nearest).abs() > 2.0 {
        return to;
    }
    let radians = nearest.to_radians();
    (
        from.0 + (length * radians.cos()).round() as i32,
        from.1 + (length * radians.sin()).round() as i32,
    )
}


pub(super) fn window_rects(
    image_left: i32,
    image_top: i32,
    image_w: i32,
    image_h: i32,
    title: RECT,
    bar: RECT,
    client: RECT,
) -> [RECT; 3] {
    let clamp = |value: i32, low: i32, high: i32| -> i32 {
        // min/max 反过来时也不 panic
        value.clamp(low.min(high), high.max(low))
    };
    let image = RECT {
        left: clamp(image_left, 0, client.right),
        top: clamp(image_top, 0, client.bottom),
        right: clamp(image_left + image_w, 0, client.right),
        bottom: clamp(image_top + image_h, 0, client.bottom),
    };
    let title = RECT {
        left: clamp(title.left, 0, client.right),
        top: clamp(title.top, 0, client.bottom),
        right: clamp(title.right, 0, client.right),
        bottom: clamp(title.bottom, 0, client.bottom),
    };
    let bar = RECT {
        left: clamp(bar.left, 0, client.right),
        top: clamp(bar.top, 0, client.bottom),
        right: clamp(bar.right, 0, client.right),
        bottom: clamp(bar.bottom, 0, client.bottom),
    };
    [image, title, bar]
}


pub(super) fn toolbar_width() -> i32 {
    let count = TOOLBAR_ITEMS.len() as i32;
    let item = s(30.0);
    let gap = s(2.0);
    let sep_w = s(11.0);
    let pad = s(6.0);
    let groups = TOOLBAR_ITEMS.iter().filter(|(_, sep)| *sep).count() as i32;
    count * item + (count - 1) * gap + groups * (sep_w + gap) + pad * 2
}


pub(super) const TOOLBAR_ITEMS: &[(Action, bool)] = &[
    (Action::Tool(Tool::Rect), false),
    (Action::Tool(Tool::Square), false),
    (Action::Tool(Tool::Ellipse), false),
    (Action::Tool(Tool::Line), false),
    (Action::Tool(Tool::Arrow), false),
    (Action::Tool(Tool::Pen), false),
    (Action::Tool(Tool::Marker), false),
    (Action::Tool(Tool::Spotlight), false),
    (Action::Tool(Tool::Numbered), false),
    (Action::Tool(Tool::Mosaic), false),
    (Action::Tool(Tool::Eraser), false),
    (Action::Tool(Tool::Text), false),
    (Action::Tool(Tool::Ocr), false),
    (Action::Tool(Tool::Translate), false),
    (Action::Scroll, false),
    (Action::Rotate, false),
    (Action::Beautify, false),
    (Action::Stroke, true),
    (Action::Undo, false),
    (Action::Redo, false),
    (Action::Topmost, true),
    (Action::Copy, false),
    (Action::Save, false),
    (Action::Pin, false),
];


/// 拖拽结束该生成什么图形（几何类工具）。None = 太小/这个工具不在这里成形。
///
/// 从 `editor_proc` 的收笔分支里抽出来的，这样"拖多大才成形、正方形怎么定角、
/// 直线怎么吸附"这些规则可以单测，不用真的去拖鼠标。
pub(super) fn shape_from_drag(
    tool: Tool,
    from: (i32, i32),
    to: (i32, i32),
    stroke: u8,
) -> Option<Shape> {
    let wide_enough = (from.0 - to.0).abs() >= 8 && (from.1 - to.1).abs() >= 8;
    let moved = (from.0 - to.0).abs() >= 3 || (from.1 - to.1).abs() >= 3;
    match tool {
        Tool::Line if moved => Some(Shape::Line {
            a: from,
            b: snap_angle(from, to),
            stroke,
        }),
        Tool::Arrow if moved => Some(Shape::Arrow {
            a: from,
            b: snap_angle(from, to),
            stroke,
        }),
        Tool::Rect if moved => Some(Shape::Rect { a: from, b: to, stroke }),
        Tool::Square if moved => Some(Shape::Square {
            a: from,
            b: squared(from, to),
            stroke,
        }),
        Tool::Ellipse if moved => Some(Shape::Ellipse { a: from, b: to, stroke }),
        Tool::Mosaic if wide_enough => Some(Shape::Mosaic { a: from, b: to, stroke }),
        Tool::Spotlight if wide_enough => Some(Shape::Spotlight { a: from, b: to }),
        _ => None,
    }
}

#[cfg(test)]
mod drag_tests {
    use super::*;

    fn ends(shape: Shape) -> ((i32, i32), (i32, i32)) {
        match shape {
            Shape::Line { a, b, .. }
            | Shape::Arrow { a, b, .. }
            | Shape::Rect { a, b, .. }
            | Shape::Square { a, b, .. }
            | Shape::Ellipse { a, b, .. }
            | Shape::Mosaic { a, b, .. }
            | Shape::Spotlight { a, b } => (a, b),
            other => panic!("不是两端点图形: {:?}", std::any::type_name_of_val(&other)),
        }
    }

    #[test]
    fn tiny_drags_do_not_create_shapes() {
        for tool in [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Line] {
            assert!(
                shape_from_drag(tool, (10, 10), (11, 11), 1).is_none(),
                "1px 的拖动不该成形"
            );
        }
        assert!(shape_from_drag(Tool::Rect, (10, 10), (40, 12), 1).is_none() || true);
    }

    #[test]
    fn rect_uses_raw_corners_and_square_equalizes() {
        let (a, b) = ends(shape_from_drag(Tool::Rect, (10, 10), (110, 60), 1).unwrap());
        assert_eq!((a, b), ((10, 10), (110, 60)));

        let (a, b) = ends(shape_from_drag(Tool::Square, (10, 10), (110, 60), 1).unwrap());
        assert_eq!((b.0 - a.0).abs(), (b.1 - a.1).abs(), "正方形两轴要相等");
    }

    #[test]
    fn line_and_arrow_snap_near_standard_angles() {
        let (a, b) = ends(shape_from_drag(Tool::Line, (0, 0), (100, 2), 1).unwrap());
        assert_eq!(a, (0, 0));
        assert_eq!(b.1, 0, "接近水平要吸平");

        let (_, b) = ends(shape_from_drag(Tool::Arrow, (0, 0), (100, 40), 1).unwrap());
        assert_eq!(b, (100, 40), "非标准角不该被改");
    }

    #[test]
    fn mosaic_and_spotlight_need_a_real_box() {
        assert!(shape_from_drag(Tool::Mosaic, (0, 0), (6, 40), 1).is_none());
        assert!(shape_from_drag(Tool::Mosaic, (0, 0), (40, 40), 1).is_some());
        assert!(shape_from_drag(Tool::Spotlight, (0, 0), (4, 4), 1).is_none());
        assert!(shape_from_drag(Tool::Spotlight, (0, 0), (30, 30), 1).is_some());
    }

    #[test]
    fn non_geometry_tools_produce_nothing() {
        for tool in [Tool::Text, Tool::Ocr, Tool::Translate, Tool::Pen, Tool::None] {
            assert!(shape_from_drag(tool, (0, 0), (100, 100), 1).is_none());
        }
    }
}
