//! Windows-specific helpers: taskbar-aware popup placement and the autostart
//! Run key. Kept isolated so the rest of the app stays platform-agnostic.

use std::mem::size_of;

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, HDEVINFO,
    SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, DIGCF_PRESENT, GUID_DEVCLASS_BLUETOOTH,
    GUID_DEVCLASS_PORTS, SPDRP_FRIENDLYNAME,
};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
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

/// A transparent WebView2 window has no desktop content in its own backdrop,
/// so CSS `backdrop-filter` samples only transparent pixels and does nothing.
/// The real blur has to be applied to the HWND by DWM.
///
/// We use the undocumented `SetWindowCompositionAttribute` accent policy rather
/// than Windows 11's `DWMWA_SYSTEMBACKDROP_TYPE`: the system backdrop is dropped
/// whenever the window loses activation (a notification is enough), which made
/// the glass collapse into a flat wallpaper-coloured rectangle. The accent
/// policy keeps sampling the desktop while the popup is inactive.
#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct CompositionAttributeData {
    attribute: u32,
    data: *mut std::ffi::c_void,
    data_size: usize,
}

type SetCompositionAttribute =
    unsafe extern "system" fn(*mut std::ffi::c_void, *mut CompositionAttributeData) -> i32;

extern "system" {
    fn LoadLibraryA(name: *const i8) -> *mut std::ffi::c_void;
    fn GetProcAddress(module: *mut std::ffi::c_void, name: *const i8) -> *mut std::ffi::c_void;
}

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;

unsafe fn set_accent(hwnd: isize, state: u32, gradient_color: u32) {
    // This API is undocumented and absent from user32.lib, so resolve it at
    // runtime instead of linking against it directly.
    let module = LoadLibraryA(b"user32.dll\0".as_ptr() as *const i8);
    if module.is_null() {
        return;
    }
    let address = GetProcAddress(
        module,
        b"SetWindowCompositionAttribute\0".as_ptr() as *const i8,
    );
    if address.is_null() {
        return;
    }
    let function: SetCompositionAttribute = std::mem::transmute(address);

    let mut policy = AccentPolicy {
        state,
        flags: 0,
        gradient_color,
        animation_id: 0,
    };
    let mut data = CompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut std::ffi::c_void,
        data_size: size_of::<AccentPolicy>(),
    };
    function(hwnd as *mut std::ffi::c_void, &mut data);
}

/// Ask DWM to round the top-level window, not just the web content. The accent
/// backdrop is painted by DWM and ignores the CSS corner radius, so without
/// this the acrylic fills the square HWND corners.
pub fn configure_popup_shape<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
    // Tauri's HWND type comes from a different `windows` crate version.
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let preference = DWMWCP_ROUND;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const std::ffi::c_void,
            size_of_val(&preference) as u32,
        )
        .map_err(|error| error.to_string())?
    }
    Ok(())
}

pub fn apply_popup_backdrop<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
    // Plain blur-behind is slightly weaker than full acrylic and keeps much
    // more background contrast, which makes the glass read as less blurry and
    // less milky. 0x08FFFFFF is only a faint tint in ABGR order; the rest of
    // the colour comes from the CSS veil.
    unsafe { set_accent(hwnd, ACCENT_ENABLE_BLURBEHIND, 0x08FFFFFF) }
    Ok(())
}

pub fn clear_popup_backdrop<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    if let Ok(hwnd) = window.hwnd() {
        unsafe { set_accent(hwnd.0 as isize, ACCENT_DISABLED, 0) }
    }
}

const POPUP_WIDTH: f32 = 360.0;
const POPUP_HEIGHT: f32 = 700.0;

/// Physical-pixel position for the popup, anchored just above the taskbar on
/// the monitor containing the cursor. `rcWork` already accounts for taskbar
/// position and auto-hide, and `GetDpiForMonitor` handles scaling. The third
/// value is the work-area bottom, used as the slide-out/down destination.
pub fn popup_geometry() -> (i32, i32, i32) {
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
                // Clamp both axes so a small display can never push the panel
                // off screen (the old code only subtracted, no lower bound).
                let x = (info.rcWork.right - width - margin).max(info.rcWork.left + margin);
                // No vertical margin: the sheet's bottom edge sits exactly on
                // the taskbar top, which is where the slide-out clips it.
                let y = (info.rcWork.bottom - height).max(info.rcWork.top);
                return (x, y, info.rcWork.bottom);
            }
        }
    }

    // Fallback: primary monitor, no DPI info available.
    unsafe {
        let width = POPUP_WIDTH as i32;
        let height = POPUP_HEIGHT as i32;
        let screen_x = GetSystemMetrics(SM_CXSCREEN);
        let screen_y = GetSystemMetrics(SM_CYSCREEN);
        (
            (screen_x - width - 8).max(8),
            (screen_y - height).max(0),
            screen_y,
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

/// Find the outgoing SPP COM port for a paired CMF/Nothing headset.
///
/// `serialport::available_ports()` reports Bluetooth ports as `Unknown` on
/// Windows, so we pair two SetupAPI enumerations instead:
///   1. the Bluetooth device whose friendly name is "CMF Buds Pro 2" gives us
///      its 12-digit address from `BTHENUM\DEV_<address>\...`;
///   2. the port device whose instance id contains that address and the SPP
///      service class GUID gives us the COM number from its friendly name.
pub fn bluetooth_serial_port() -> Option<String> {
    let addresses: Vec<String> = enumerate_devices(&GUID_DEVCLASS_BLUETOOTH)
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            name.contains("cmf buds pro 2")
                || name.contains("cmf buds")
                || name.contains("b172")
                || name.contains("b187")
                || name.contains("b195")
        })
        .filter_map(|(_, instance)| bluetooth_address(instance))
        .collect();

    if addresses.is_empty() {
        return None;
    }

    let mut spp_ports = Vec::new();
    for (name, instance) in enumerate_devices(&GUID_DEVCLASS_PORTS) {
        let upper = instance.to_ascii_uppercase();
        if !upper.contains("{00001101-0000-1000-8000-00805F9B34FB}") {
            continue;
        }
        let Some(com) = com_name(&name) else {
            continue;
        };
        if addresses.iter().any(|address| upper.contains(address)) {
            return Some(com);
        }
        spp_ports.push(com);
    }

    // Only use an unmatched SPP port when there is exactly one; otherwise it
    // could belong to a different paired device.
    if spp_ports.len() == 1 {
        spp_ports.pop()
    } else {
        None
    }
}

fn enumerate_devices(class: &GUID) -> Vec<(String, String)> {
    let mut devices = Vec::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(Some(class), PCWSTR::null(), None, DIGCF_PRESENT)
        else {
            return devices;
        };
        let mut index = 0u32;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, index, &mut data).is_err() {
                break;
            }
            index += 1;
            devices.push((
                registry_string(set, &data, SPDRP_FRIENDLYNAME),
                instance_string(set, &data),
            ));
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    devices
}

unsafe fn registry_string(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    property: SETUP_DI_REGISTRY_PROPERTY,
) -> String {
    let mut required = 0u32;
    let _ = SetupDiGetDeviceRegistryPropertyW(set, data, property, None, None, Some(&mut required));
    if required == 0 {
        return String::new();
    }
    let mut buffer = vec![0u8; required as usize];
    if SetupDiGetDeviceRegistryPropertyW(
        set,
        data,
        property,
        None,
        Some(&mut buffer),
        Some(&mut required),
    )
    .is_err()
    {
        return String::new();
    }
    let words: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&words)
        .trim_end_matches('\0')
        .to_string()
}

unsafe fn instance_string(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> String {
    let mut required = 0u32;
    let _ = SetupDiGetDeviceInstanceIdW(set, data, None, Some(&mut required));
    if required == 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; required as usize];
    if SetupDiGetDeviceInstanceIdW(set, data, Some(&mut buffer), Some(&mut required)).is_err() {
        return String::new();
    }
    String::from_utf16_lossy(&buffer)
        .trim_end_matches('\0')
        .to_string()
}

fn bluetooth_address(instance_id: &str) -> Option<String> {
    let upper = instance_id.to_ascii_uppercase();
    let start = upper.find("DEV_")? + 4;
    let address = &upper[start..];
    let end = address.find('\\')?;
    let address = &address[..end];
    (address.len() == 12 && address.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| address.to_string())
}

fn com_name(text: &str) -> Option<String> {
    let upper = text.to_ascii_uppercase();
    let start = upper.find("COM")? + 3;
    let digits: String = upper[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then(|| format!("COM{digits}"))
}
