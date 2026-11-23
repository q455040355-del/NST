//! 编辑器顶部的窗口栏：标题 + 最小化 / 关闭 / 完成，整条用来拖动窗口。

use super::*;

/// 画顶部窗口栏（标题 + 最小化/关闭/完成），返回它的矩形供窗口区域使用
pub(super) unsafe fn draw_title_bar(dc: HDC, editor: &mut Editor, client: RECT) -> RECT {
    let ui = crate::theme::ui();
    let _ = &ui;

        let ui = crate::theme::ui();
        // 窗口栏宽度跟图片一致（图很窄时至少放得下三个按钮）
        let image_w = (editor.base.width as f32 * editor.scale) as i32;
        let bar_w = image_w.max(s(96.0));
        let bar_left = ((client.right - bar_w) / 2).max(0);
        let strip_rect = RECT {
            left: bar_left,
            top: 0,
            right: bar_left + bar_w,
            bottom: editor.title_h,
        };
        {
            let brush = CreateSolidBrush(ui.surface);
            FillRect(dc, &strip_rect, brush);
            DeleteObject(brush as *mut _);
            let pen = CreatePen(0, 1, ui.border);
            let old = SelectObject(dc, pen as HGDIOBJ);
            MoveToEx(dc, strip_rect.left, editor.title_h - 1, std::ptr::null_mut());
            LineTo(dc, strip_rect.right, editor.title_h - 1);
            SelectObject(dc, old);
            DeleteObject(pen as *mut _);
        }
        editor.title_rect = strip_rect;

        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, ui.text2);
        let font = cached_font(9.0, 600);
        let old_font = SelectObject(dc, font as HGDIOBJ);
        let mut text_rect = RECT {
            left: strip_rect.left + s(10.0),
            top: 0,
            right: strip_rect.right - s(84.0),
            bottom: editor.title_h,
        };
        let title = title_text(editor.tool);
        DrawTextW(
            dc,
            wide(&title).as_ptr(),
            -1,
            &mut text_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);

        // 右上角三个按钮：最小化 / 关闭 / 完成
        let btn = s(24.0);
        let mut bx = strip_rect.right - s(6.0) - btn * 3 - s(4.0) * 2;
        for action in [Action::Minimize, Action::Close, Action::Done] {
            let rect = RECT {
                left: bx,
                top: (editor.title_h - btn) / 2,
                right: bx + btn,
                bottom: (editor.title_h - btn) / 2 + btn,
            };
            draw_action_icon(dc, action, rect, ui.text2, editor.stroke, true);
            editor.buttons.push((rect, action));
            bx += btn + s(4.0);
        }
    editor.title_rect
}

