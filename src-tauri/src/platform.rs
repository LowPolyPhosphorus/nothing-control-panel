//! Windows-specific helpers: taskbar-aware popup placement and the autostart
//! Run key. Kept isolated so the rest of the app stays platform-agnostic.

use std::mem::size_of;

use windows::core::w;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_ALL_ACCESS, REG_SZ,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN,
};

const POPUP_WIDTH: f32 = 360.0;
const POPUP_HEIGHT: f32 = 640.0;

/// Physical-pixel position for the popup, anchored just above the taskbar on
/// the monitor containing the cursor. `rcWork` already accounts for taskbar
/// position and auto-hide, and `GetDpiForMonitor` handles scaling.
pub fn popup_position() -> (i32, i32) {
    unsafe {
        let mut cursor = POINT::default();
        if GetCursorPos(&mut cursor).is_ok() {
            let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                rcMonitor: RECT::default(),
                rcWork: RECT::default(),
                dwFlags: 0,
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
                let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
                let width = scale(POPUP_WIDTH, dpi_x);
                let height = scale(POPUP_HEIGHT, dpi_y);
                let margin = scale(8.0, dpi_x);
                return (
                    info.rcWork.right - width - margin,
                    info.rcWork.bottom - height - margin,
                );
            }
        }
    }

    // Fallback: primary monitor, no DPI info available.
    unsafe {
        let width = POPUP_WIDTH as i32;
        let height = POPUP_HEIGHT as i32;
        (
            GetSystemMetrics(SM_CXSCREEN) - width - 8,
            GetSystemMetrics(SM_CYSCREEN) - height - 8,
        )
    }
}

fn scale(logical: f32, dpi: u32) -> i32 {
    (logical * dpi as f32 / 96.0).round() as i32
}

/// True when a left mouse button press just happened outside the popup.
/// Used instead of `Focused(false)`: WebView2 emits unpredictable blur bursts
/// while its content process starts, which made the popup vanish in the user's
/// face. An actual outside click is an unambiguous close signal.
pub fn left_click_outside(hwnd: isize) -> bool {
    unsafe {
        if (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) == 0 {
            return false;
        }
        let mut cursor = POINT::default();
        if GetCursorPos(&mut cursor).is_err() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(HWND(hwnd as *mut std::ffi::c_void), &mut rect).is_err() {
            return false;
        }
        cursor.x < rect.left
            || cursor.x >= rect.right
            || cursor.y < rect.top
            || cursor.y >= rect.bottom
    }
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    unsafe {
        let mut key: HKEY = HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            None,
            KEY_ALL_ACCESS,
            &mut key,
        )
        .ok()
        .map_err(|error| error.to_string())?;

        let result = if enabled {
            let exe = std::env::current_exe()
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned();
            let mut wide: Vec<u16> = exe.encode_utf16().collect();
            wide.push(0);
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            RegSetValueExW(key, w!("CMF Tray"), None, REG_SZ, Some(bytes))
                .ok()
                .map_err(|error| error.to_string())
        } else {
            RegDeleteValueW(key, w!("CMF Tray"))
                .ok()
                .map_err(|error| error.to_string())
        };

        let _ = RegCloseKey(key);
        result
    }
}
