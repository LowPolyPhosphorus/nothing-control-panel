use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct BatteryState {
    pub level: u8,
    pub charging: bool,
    pub present: bool,
}

impl BatteryState {
    pub fn absent() -> Self {
        Self::default()
    }

    pub fn from_reading(value: Option<crate::protocol::BatteryValue>) -> Self {
        match value {
            Some(value) => Self {
                level: value.percent.min(100),
                charging: value.charging,
                present: true,
            },
            None => Self::absent(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FrontendState {
    pub device_name: String,
    pub connected: bool,
    /// Short human-readable connection status shown in the popup.
    pub status: String,
    pub port: String,
    pub firmware: String,
    pub left: BatteryState,
    pub right: BatteryState,
    pub case_battery: BatteryState,
    pub anc_mode: String,
    pub anc_strength: u8,
    pub eq_preset: u8,
    pub in_ear_detection: bool,
    pub autostart: bool,
}

/// Mutable runtime state. All battery/ANC/EQ values come from frames decoded
/// by the Bluetooth manager thread; the UI never writes them directly.
#[derive(Clone)]
pub struct AppRuntime {
    pub connected: bool,
    pub last_tray_click_ms: u64,
    pub device_name: String,
    pub port: String,
    pub firmware: String,
    pub status: String,
    pub left: BatteryState,
    pub right: BatteryState,
    pub case_battery: BatteryState,
    pub anc_mode: String,
    pub anc_strength: u8,
    pub eq_preset: u8,
    pub in_ear_detection: bool,
}

impl Default for AppRuntime {
    fn default() -> Self {
        Self {
            connected: false,
            last_tray_click_ms: 0,
            device_name: "CMF Buds Pro 2".to_string(),
            port: String::new(),
            firmware: String::new(),
            status: "Disconnected".to_string(),
            left: BatteryState::absent(),
            right: BatteryState::absent(),
            case_battery: BatteryState::absent(),
            anc_mode: "off".to_string(),
            anc_strength: 1,
            eq_preset: 0,
            in_ear_detection: true,
        }
    }
}
