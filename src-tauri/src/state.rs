use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct BatteryState {
    pub level: u8,
    pub charging: bool,
    pub present: bool,
}

impl BatteryState {
    pub fn absent() -> Self {
        Self {
            level: 0,
            charging: false,
            present: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FrontendState {
    pub device_name: String,
    pub connected: bool,
    pub left: BatteryState,
    pub right: BatteryState,
    pub case_battery: BatteryState,
    pub anc_mode: String,
    pub anc_strength: u8,
    pub eq_preset: u8,
    pub in_ear_detection: bool,
    pub autostart: bool,
}

/// Mutable runtime state. Milestone 1 uses fake values; later milestones
/// replace these fields with values pushed from the Bluetooth task.
pub struct AppRuntime {
    pub connected: bool,
    pub last_tray_click_ms: u64,
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
            anc_mode: "off".to_string(),
            anc_strength: 1,
            eq_preset: 0,
            in_ear_detection: true,
        }
    }
}
