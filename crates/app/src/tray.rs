//! 托盘图标：用 GDI 现画一个 32×32 图标（深蓝底 + 白色 ↕）。

use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, HGDIOBJ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICONINFO,
};

use crate::theme::wide;

const ICON_SIZE: i32 = 32;

pub fn create_icon() -> HICON {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return std::ptr::null_mut();
        }

        let dc = CreateCompatibleDC(screen);

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = ICON_SIZE;
        info.bmiHeader.biHeight = -ICON_SIZE;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = 0;

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(screen, &info, 0, &mut bits, std::ptr::null_mut(), 0);
        let previous = SelectObject(dc, color as HGDIOBJ);

        // 背景：任务栏绿蓝色（COLORREF = 0x00BBGGRR）
        let background = CreateSolidBrush(0x00D8_8A_10);
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: ICON_SIZE,
            bottom: ICON_SIZE,
        };
        FillRect(dc, &rect, background);
        DeleteObject(background as *mut _);

        // 白色 ↕
        let font = CreateFontW(
            -24,
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Segoe UI Symbol").as_ptr(),
        );
        let previous_font = SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, 1);
        SetTextColor(dc, 0x00FF_FFFF);
        let text = wide("\u{2195}");
        let mut text_rect = rect;
        DrawTextW(
            dc,
            text.as_ptr(),
            (text.len() - 1) as i32,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(dc, previous_font);
        DeleteObject(font as *mut _);
        SelectObject(dc, previous);
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);

        // 掩码全 0 = 整块不透明
        let mask = CreateBitmap(ICON_SIZE, ICON_SIZE, 1, 1, std::ptr::null());

        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&icon_info);

        DeleteObject(mask as *mut _);
        DeleteObject(color as *mut _);

        icon
    }
}

pub fn destroy_icon(icon: HICON) {
    if !icon.is_null() {
        unsafe {
            DestroyIcon(icon);
        }
    }
}
