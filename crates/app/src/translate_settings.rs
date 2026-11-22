//! 翻译设置：一个独立的小窗（不碰原设置窗那套手排版面）。
//!
//! 用系统的 STATIC / EDIT / COMBOBOX / BUTTON 控件拼，省得再手绘一套。
//! 保存时写回 settings.ini 的 Translate* 几项。

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDlgItem, GetWindowTextLengthW,
    GetWindowTextW, LoadCursorW, RegisterClassW, SendMessageW, SetWindowPos, ShowWindow,
    CS_DROPSHADOW, HWND_TOPMOST, IDC_ARROW, SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE, SW_SHOW, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WNDCLASSW, WS_CHILD, WS_EX_DLGMODALFRAME, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WS_BORDER,
};

use crate::theme::wide;

const CLASS_NAME: &str = "NetSpeedTaskbarRs.TranslateSettings";
const ID_LABEL_PROVIDER: usize = 5201;
const ID_PROVIDER: usize = 5202;
const ID_LABEL_KEY: usize = 5203;
const ID_KEY: usize = 5204;
const ID_LABEL_MODEL: usize = 5205;
const ID_MODEL: usize = 5206;
const ID_LABEL_BASE: usize = 5207;
const ID_BASE: usize = 5208;
const ID_HINT: usize = 5209;
const ID_SAVE: usize = 5210;
const ID_CANCEL: usize = 5211;

thread_local! {
    static OPEN: RefCell<isize> = const { RefCell::new(0) };
}

fn s(value: i32) -> i32 {
    crate::ui::s(value as f32, crate::screen_scale())
}

fn read_text(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }
}

unsafe fn child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    id: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> HWND {
    let instance = GetModuleHandleW(std::ptr::null());
    CreateWindowExW(
        0,
        wide(class).as_ptr(),
        wide(text).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        width,
        height,
        parent,
        id as *mut _,
        instance,
        std::ptr::null(),
    )
}

unsafe fn save_and_close(hwnd: HWND) {
    let provider_item = SendMessageW(GetDlgItem(hwnd, ID_PROVIDER as i32), 0x0147, 0, 0); // CB_GETCURSEL
    let mut settings = netspeed_core::settings::Settings::load();
    settings.translate_provider = if provider_item == 1 { "deepseek" } else { "openai" }.to_string();
    settings.translate_key = read_text(GetDlgItem(hwnd, ID_KEY as i32)).trim().to_string();
    settings.translate_model = read_text(GetDlgItem(hwnd, ID_MODEL as i32)).trim().to_string();
    settings.translate_base = read_text(GetDlgItem(hwnd, ID_BASE as i32)).trim().to_string();
    settings.save();
    DestroyWindow(hwnd);
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as usize;
            match id {
                ID_SAVE => {
                    save_and_close(hwnd);
                    0
                }
                ID_CANCEL => {
                    DestroyWindow(hwnd);
                    0
                }
                _ => 0,
            }
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            OPEN.with(|cell| cell.replace(0));
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 打开设置窗；已经开着就提到前面
pub fn open(owner: HWND) {
    unsafe {
        let existing = OPEN.with(|cell| *cell.borrow());
        if existing != 0 {
            ShowWindow(existing as HWND, SW_SHOW);
            return;
        }
        let settings = netspeed_core::settings::Settings::load();
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::tray::shared_icon(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: (windows_sys::Win32::Graphics::Gdi::COLOR_WINDOW + 1) as *mut _,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        let pad = s(18);
        let label_w = s(72);
        let field_x = pad + label_w + s(10);
        let width = s(430);
        let height = s(310);
        let field_w = width - field_x - pad;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            wide("翻译设置").as_ptr(),
            WS_POPUP | WS_VISIBLE | WS_BORDER,
            s(160),
            s(160),
            width,
            height,
            owner,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        OPEN.with(|cell| cell.replace(hwnd as isize));

        let mut y = pad;
        child(hwnd, "STATIC", "服务商", 0, ID_LABEL_PROVIDER, pad, y + s(4), label_w, s(20));
        let combo = child(hwnd, "COMBOBOX", "", 0x3 /* CBS_DROPDOWNLIST */ | 0x0040 /* WS_VSCROLL */, ID_PROVIDER, field_x, y, field_w, s(200));
        let openai = wide("ChatGPT（OpenAI）");
        let deepseek = wide("DeepSeek");
        SendMessageW(combo, 0x0143, 0, openai.as_ptr() as isize); // CB_ADDSTRING
        SendMessageW(combo, 0x0143, 0, deepseek.as_ptr() as isize);
        let index = if settings.translate_provider.eq_ignore_ascii_case("deepseek") { 1 } else { 0 };
        SendMessageW(combo, 0x014E, index as usize, 0); // CB_SETCURSEL

        y += s(46);
        child(hwnd, "STATIC", "API Key", 0, ID_LABEL_KEY, pad, y + s(4), label_w, s(20));
        child(hwnd, "EDIT", &settings.translate_key, 0x0080 /* ES_PASSWORD */ | 0x0004 /* ES_AUTOHSCROLL */, ID_KEY, field_x, y, field_w, s(24));

        y += s(46);
        child(hwnd, "STATIC", "模型", 0, ID_LABEL_MODEL, pad, y + s(4), label_w, s(20));
        child(hwnd, "EDIT", &settings.translate_model, 0x0004, ID_MODEL, field_x, y, field_w, s(24));

        y += s(46);
        child(hwnd, "STATIC", "Base URL", 0, ID_LABEL_BASE, pad, y + s(4), label_w, s(20));
        child(hwnd, "EDIT", &settings.translate_base, 0x0004, ID_BASE, field_x, y, field_w, s(24));

        y += s(44);
        child(
            hwnd,
            "STATIC",
            "模型 / Base URL 留空就用默认值；Key 只存在本机。",
            0,
            ID_HINT,
            pad,
            y,
            width - pad * 2,
            s(20),
        );

        let button_w = s(88);
        let button_y = height - s(50);
        child(hwnd, "BUTTON", "保存", 0x0001 /* BS_DEFPUSHBUTTON */, ID_SAVE, width - pad - button_w * 2 - s(10), button_y, button_w, s(30));
        child(hwnd, "BUTTON", "取消", 0, ID_CANCEL, width - pad - button_w, button_y, button_w, s(30));

        crate::ui::round_corners(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}
