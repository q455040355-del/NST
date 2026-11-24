//! 编辑器底部工具栏：按钮布局、分组线、hover 标签。
//!
//! 按钮内容来自 `geometry::TOOLBAR_ITEMS`（唯一事实来源），宽度也按它算。

use super::*;

/// 画底部工具栏（26 个按钮 + 分组线 + hover 标签），返回它的矩形给窗口区域用
pub(super) unsafe fn draw_toolbar(dc: HDC, editor: &mut Editor, client: RECT) -> RECT {
    // 这一趟绘制共用一块 GDI+ 画布：底栏、图标、标签都是抗锯齿的
    let _aa = crate::aa::begin(dc);

    // 圆角矩形：优先抗锯齿，GDI+ 不可用时退回 GDI
    let round = |left: i32, top: i32, right: i32, bottom: i32, radius: i32, fill: u32, border: u32| {
        if crate::aa::round(dc, left, top, right, bottom, radius, fill, border) {
            return;
        }
        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(0, 1, border);
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        windows_sys::Win32::Graphics::Gdi::RoundRect(
            dc, left, top, right, bottom, radius, radius,
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(pen as *mut _);
        DeleteObject(brush as *mut _);
    };

    // 工具栏：微信那种一条圆角底栏，纯图标（文字靠 hover 小标签提示）
    let ui = crate::theme::ui();
    let bar_h = s(38.0);
    // 工具栏浮在图片下方那条带子里（带子由窗口区域裁掉，只剩工具栏）
    let strip_top = (client.bottom - editor.bottom_strip).max(0);
    let bar_top = strip_top + (((client.bottom - strip_top) - bar_h) / 2).max(0);
    let bar_bottom = bar_top + bar_h;
    let item = s(30.0);
    let gap = s(2.0);
    let sep_w = s(11.0);
    let pad = s(6.0);

    // bool = 这一格前面要不要画一条分组竖线

    // 没配置翻译 Key 时不显示「翻译」按钮
    let layout_items: Vec<(Action, bool)> = TOOLBAR_ITEMS
        .iter()
        .copied()
        .filter(|(action, _)| !matches!(action, Action::Tool(Tool::Translate)) || translate_enabled())
        .collect();
    let layout_items = layout_items.as_slice();
    let groups = layout_items.iter().filter(|(_, sep)| *sep).count() as i32;
    let measure = |item: i32, gap: i32, sep_w: i32| -> i32 {
        layout_items.len() as i32 * item
            + (layout_items.len() as i32 - 1) * gap
            + groups * (sep_w + gap)
            + pad * 2
    };
    let bar_w = measure(item, gap, sep_w);
    let bar_left = ((client.right - bar_w) / 2).max(s(8.0));
    // 记下底栏的真实范围：命中判定和窗口区域都用它
    editor.bar = RECT {
        left: bar_left,
        top: bar_top,
        right: bar_left + bar_w,
        bottom: bar_top + bar_h,
    };

    // 底栏投影：比背板大一圈的浅灰圆角，制造「浮起来」的感觉
    round(
        bar_left - s(2.0),
        bar_top - s(1.0),
        bar_left + bar_w + s(2.0),
        bar_bottom + s(3.0),
        s(15.0),
        0x00EE_F1F4,
        0x00EE_F1F4,
    );

    // 底栏背板：圆角 + 1px 描边
    round(
        bar_left,
        bar_top,
        bar_left + bar_w,
        bar_bottom,
        s(13.0),
        ui.surface,
        ui.border,
    );

    let icon_top = bar_top + (bar_h - item) / 2;
    let mut x = bar_left + pad;
    let mut hovered: Option<(Action, RECT)> = None;
    for &(action, new_group) in layout_items {
        if new_group {
            let pen = CreatePen(0, 1, ui.border);
            let old = SelectObject(dc, pen as HGDIOBJ);
            let sx = x + sep_w / 2;
            MoveToEx(dc, sx, bar_top + s(11.0), std::ptr::null_mut());
            LineTo(dc, sx, bar_bottom - s(11.0));
            SelectObject(dc, old);
            DeleteObject(pen as *mut _);
            x += sep_w + gap;
        }

        let rect = RECT {
            left: x,
            top: icon_top,
            right: x + item,
            bottom: icon_top + item,
        };
        let active = match action {
            Action::Tool(tool) => editor.tool == tool,
            Action::Topmost => editor.topmost,
            _ => false,
        };
        let pressed = editor.pressed == Some(action);
        let hover = editor.hover == Some(action);

        if active || pressed || hover {
            let fill = if active {
                ui.accent
            } else if pressed {
                ui.chip_hover
            } else {
                ui.chip
            };
            round(
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                s(8.0),
                fill,
                fill,
            );
        }

        let color = if active {
            ui.bg
        } else if matches!(action, Action::Done) {
            // 微信的「完成」是绿的 #07C160、取消是红的 #FA5151（这里存的是 BGR）
            0x0060_C107
        } else if matches!(action, Action::Close) {
            0x0051_51FA
        } else {
            ui.text2
        };
        // 选中（蓝底反白）和悬停才用单色；平时按设计规范上彩色
        let colored = !active && !hover;
        draw_action_icon(dc, action, rect, color, editor.stroke, colored);
        if hover {
            hovered = Some((action, rect));
        }
        editor.buttons.push((rect, action));
        x = rect.right + gap;
    }

    // 图标没有文字，靠 hover 时在底栏上方弹一个小标签（微信也是这么做的）
    if let Some((action, rect)) = hovered {
        draw_bar_tooltip(dc, action, rect, bar_top, editor.stroke);
    }

    editor.bar
}

/// 工具 / 动作的快捷键：tooltip 里显示，键盘处理也按这张表走
pub(super) fn shortcut_of(action: Action) -> &'static str {
    match action {
        Action::Tool(Tool::Arrow) => "A",
        Action::Tool(Tool::Rect) => "R",
        Action::Tool(Tool::Square) => "Shift+R",
        Action::Tool(Tool::Ellipse) => "O",
        Action::Tool(Tool::Line) => "L",
        Action::Tool(Tool::Marker) => "H",
        Action::Tool(Tool::Spotlight) => "P",
        Action::Tool(Tool::Numbered) => "N",
        Action::Tool(Tool::Eraser) => "E",
        Action::Tool(Tool::Pen) => "B",
        Action::Tool(Tool::Text) => "T",
        Action::Tool(Tool::Mosaic) => "M",
        Action::Tool(Tool::Ocr) => "Ctrl+Shift+O",
        Action::Tool(Tool::Translate) => "Ctrl+Shift+T",
        Action::Undo => "Ctrl+Z",
        Action::Redo => "Ctrl+Y",
        Action::Copy => "Ctrl+C",
        Action::Save => "Ctrl+S",
        Action::Rotate => "Ctrl+R",
        Action::Done => "Enter",
        Action::Close => "Esc",
        Action::Minimize => "-",
        _ => "",
    }
}

/// 字母键 → 工具（没按修饰键时生效）
pub(super) fn tool_for_letter(key: u32, shift: bool) -> Option<Tool> {
    Some(match key {
        0x41 => Tool::Arrow,      // A
        0x52 => {
            if shift {
                Tool::Square
            } else {
                Tool::Rect
            }
        } // R / Shift+R
        0x4F => Tool::Ellipse,    // O
        0x4C => Tool::Line,       // L
        0x48 => Tool::Marker,     // H
        0x50 => Tool::Spotlight,  // P
        0x4E => Tool::Numbered,   // N
        0x45 => Tool::Eraser,     // E
        0x42 => Tool::Pen,        // B
        0x54 => Tool::Text,       // T
        0x4D => Tool::Mosaic,     // M
        _ => return None,
    })
}

pub(super) fn draw_bar_tooltip(dc: HDC, action: Action, anchor: RECT, bar_top: i32, stroke: u8) {
    unsafe {
        let name = match action {
            Action::Stroke => STROKE_LABELS[stroke.min(2) as usize],
            _ => text_of_action(action),
        };
        if name.is_empty() {
            return;
        }
        // 「矩形  R」：图标没文字，光看名字也认不出快捷键
        let key = shortcut_of(action);
        let label = if key.is_empty() {
            name.to_string()
        } else {
            format!("{name}   {key}")
        };
        let font = cached_font(8.5, 500);
        let old_font = SelectObject(dc, font as HGDIOBJ);
        let width = wide_width(dc, &label) + s(20.0);
        let height = s(20.0);
        // 编辑窗只有「图 + 底栏 + 提示」那么高：图很矮时标签会顶出客户区被裁掉，
        // 所以左右上下都夹在窗口内，宁可压在图上也要看得见。
        let mut client: RECT = std::mem::zeroed();
        let owner = windows_sys::Win32::Graphics::Gdi::WindowFromDC(dc);
        if !owner.is_null() {
            GetClientRect(owner, &mut client);
        }
        let max_left = (client.right - width - s(4.0)).max(s(4.0));
        let left = ((anchor.left + anchor.right) / 2 - width / 2).clamp(s(4.0), max_left);
        let top = (bar_top - height - s(5.0)).max(s(2.0));
        let rect = RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };

        // 和底栏同色 + 1px 描边：浅色主题下是白底灰边，深色主题下是深底浅字，
        // 弹在图片上方（图片大多偏亮）也看得清
        let ui = crate::theme::ui();
        let (fill, ink) = (ui.surface, ui.text);
        round_rect_aa(dc, &rect, s(8.0), fill, ui.border);

        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, ink);
        let mut target = rect;
        DrawTextW(
            dc,
            wide(&label).as_ptr(),
            -1,
            &mut target,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);
    }
}
