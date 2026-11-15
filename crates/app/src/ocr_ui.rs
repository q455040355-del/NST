//! 取字结果窗：多行可编辑文本框 + 复制 / 关闭（识别完自动复制一次）。

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetStockObject, DEFAULT_GUI_FONT, HGDIOBJ,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const COLOR_BTNFACE: u32 = 15;

use crate::theme::wide;

const CLASS_NAME: &str = "NetSpeedTaskbarRs.OcrResult";
const ID_EDIT: usize = 2001;
const ID_COPY: usize = 2002;
const ID_CLOSE: usize = 2003;
const ID_STATUS: usize = 2004;

pub fn open(_parent: HWND, text: &str, error: Option<&str>) {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::tray::create_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as *mut _,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        let scale = crate::screen_scale();
        let client_w = (560.0 * scale) as i32;
        let client_h = (420.0 * scale) as i32;

        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut _, 0);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0);
        let window_w = rect.right - rect.left;
        let window_h = rect.bottom - rect.top;
        let x = work.left + ((work.right - work.left) - window_w) / 2;
        let y = work.top + ((work.bottom - work.top) - window_h) / 3;

        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("取字结果").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            window_w,
            window_h,
            // 不挂任务栏面板当 owner（否则会变成置顶窗口，挡住别的软件）
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );

        if hwnd.is_null() {
            return;
        }

        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("EDIT").as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_TABSTOP
                | 0x0004  // ES_MULTILINE
                | 0x0040  // ES_AUTOVSCROLL
                | 0x0080, // ES_AUTOHSCROLL
            0,
            0,
            0,
            0,
            hwnd,
            ID_EDIT as usize as *mut _,
            instance,
            std::ptr::null(),
        );

        let font = GetStockObject(DEFAULT_GUI_FONT) as HGDIOBJ;
        SendMessageW(edit, WM_SETFONT, font as WPARAM, 1);

        let button_w = (78.0 * scale) as i32;
        let button_h = (28.0 * scale) as i32;
        let margin = (12.0 * scale) as i32;

        CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide("复制").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | 0x0000_0001,
            client_w - margin - button_w * 2 - margin,
            client_h - margin - button_h,
            button_w,
            button_h,
            hwnd,
            ID_COPY as usize as *mut _,
            instance,
            std::ptr::null(),
        );

        CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide("关闭").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | 0x0000_0001,
            client_w - margin - button_w,
            client_h - margin - button_h,
            button_w,
            button_h,
            hwnd,
            ID_CLOSE as usize as *mut _,
            instance,
            std::ptr::null(),
        );

        let status_text = match error {
            Some(message) => message.to_string(),
            None => format!(
                "已复制到剪贴板 · {} 字（可直接 Ctrl+V 粘贴）",
                text.chars().count()
            ),
        };

        CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(&status_text).as_ptr(),
            WS_CHILD | WS_VISIBLE,
            margin,
            client_h - margin - button_h,
            client_w - margin * 3 - button_w * 2,
            button_h,
            hwnd,
            ID_STATUS as usize as *mut _,
            instance,
            std::ptr::null(),
        );

        if error.is_none() {
            crate::ocr::copy_text(text);
        }

        // 子控件建好后才有尺寸：WM_SIZE 在创建子控件之前就到过了，这里补一次布局
        layout(hwnd);

        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(edit);
    }
}

unsafe fn layout(hwnd: HWND) {
    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    GetClientRect(hwnd, &mut client);

    let scale = crate::screen_scale();
    let margin = (12.0 * scale) as i32;
    let button_h = (28.0 * scale) as i32;
    let button_w = (78.0 * scale) as i32;

    let edit = GetDlgItem(hwnd, ID_EDIT as i32);
    if !edit.is_null() {
        MoveWindow(
            edit,
            margin,
            margin,
            client.right - margin * 2,
            client.bottom - margin * 2 - button_h - (8.0 * scale) as i32,
            1,
        );
    }

    let copy = GetDlgItem(hwnd, ID_COPY as i32);
    let close = GetDlgItem(hwnd, ID_CLOSE as i32);
    let status = GetDlgItem(hwnd, ID_STATUS as i32);
    let bottom = client.bottom - margin - button_h;

    if !copy.is_null() {
        MoveWindow(
            copy,
            client.right - margin - button_w * 2 - margin,
            bottom,
            button_w,
            button_h,
            1,
        );
    }
    if !close.is_null() {
        MoveWindow(close, client.right - margin - button_w, bottom, button_w, button_h, 1);
    }
    if !status.is_null() {
        MoveWindow(
            status,
            margin,
            bottom,
            client.right - margin * 3 - button_w * 2,
            button_h,
            1,
        );
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout(hwnd);
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            if id == ID_COPY {
                let edit = GetDlgItem(hwnd, ID_EDIT as i32);
                if !edit.is_null() {
                    let length = GetWindowTextLengthW(edit);
                    let mut buffer = vec![0u16; (length + 1) as usize];
                    GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
                    let text = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
                    crate::ocr::copy_text(&text);
                    let status = GetDlgItem(hwnd, ID_STATUS as i32);
                    if !status.is_null() {
                        SetWindowTextW(status, wide("已复制到剪贴板").as_ptr());
                    }
                }
                0
            } else if id == ID_CLOSE {
                DestroyWindow(hwnd);
                0
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
