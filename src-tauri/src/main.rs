#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod device;
mod platform;
mod protocol;
mod settings;
mod state;

use std::{
    sync::{mpsc::Sender, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use device::Command;
use settings::Settings;
use state::{AppRuntime, BatteryState, FrontendState};
use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, Position, RunEvent, Runtime, WebviewUrl,
    WebviewWindowBuilder, Wry,
};
use tracing::{info, warn};

type Handles = UiHandles<Wry>;

struct UiHandles<R: Runtime> {
    tray: TrayIcon<R>,
    connect_item: MenuItem<R>,
    autostart_item: CheckMenuItem<R>,
}

fn main() {
    let debug = std::env::args().any(|arg| arg.eq_ignore_ascii_case("--debug"))
        || std::env::var("CMF_TRAY_DEBUG")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

    let mut stored_settings = settings::load();
    if debug {
        stored_settings.debug = true;
    }
    settings::init_logging(stored_settings.debug);
    info!("cmf-tray starting (debug={})", stored_settings.debug);

    let (device_tx, device_rx) = std::sync::mpsc::channel::<Command>();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second launch just surfaces the existing app's popup.
            toggle_popup(app);
        }))
        .manage(Mutex::new(stored_settings))
        .manage(Mutex::new(AppRuntime::default()))
        .manage(Mutex::new(device_tx))
        .invoke_handler(tauri::generate_handler![
            get_state,
            connect,
            disconnect,
            set_anc,
            set_eq,
            set_in_ear,
            hide_popup,
            get_manual_com_port,
            set_manual_com_port,
        ])
        .setup(move |app| {
            // One dedicated thread owns the serial port and its write queue.
            device::spawn(app.app_handle().clone(), device_rx);

            let settings = app.state::<Mutex<Settings>>().lock().unwrap().clone();

            let connect_item = MenuItem::with_id(
                app,
                "connect",
                "Connect",
                true,
                None::<&str>,
            )?;
            let autostart_item = CheckMenuItem::with_id(
                app,
                "autostart",
                "Start with Windows",
                true,
                settings.autostart,
                None::<&str>,
            )?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&connect_item, &autostart_item, &quit_item])?;

            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(Image::from_bytes(include_bytes!("../icons/icon.png")).expect("tray icon"))
                .tooltip("CMF Buds Pro 2 — disconnected")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_popup(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "connect" => {
                        let connected =
                            app.state::<Mutex<AppRuntime>>().lock().unwrap().connected;
                        send_command(
                            app,
                            if connected {
                                Command::Disconnect
                            } else {
                                Command::Connect
                            },
                        );
                    }
                    "autostart" => toggle_autostart(app),
                    "quit" => app.exit(0),
                    other => warn!("unknown tray menu item: {other}"),
                })
                .build(app)?;

            app.manage(Handles {
                tray,
                connect_item,
                autostart_item,
            });
            emit_state(app.app_handle());

            // Auto-connect on launch using the remembered/last Bluetooth port.
            send_command(app.app_handle(), Command::Connect);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building cmf-tray");

    app.run(|_app, event| {
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            // The popup is intentionally transient. Closing it must leave the
            // tray icon alive; only an explicit `app.exit(code)` (Quit menu)
            // carries `Some(code)` and is allowed to terminate the process.
            api.prevent_exit();
        }
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn send_command(app: &AppHandle, command: Command) {
    let sender = app.state::<Mutex<Sender<Command>>>();
    let sender = sender.lock().unwrap();
    if let Err(error) = sender.send(command) {
        warn!("device command channel closed: {error}");
    }
}

/// Single funnel for runtime mutations from the device thread: refresh the
/// tray menu label and push a fresh snapshot to the frontend every time.
pub(crate) fn patch_runtime(app: &AppHandle, change: impl FnOnce(&mut AppRuntime)) {
    let connected = {
        let runtime_state = app.state::<Mutex<AppRuntime>>();
        let mut runtime = runtime_state.lock().unwrap();
        change(&mut runtime);
        runtime.connected
    };
    if let Some(handles) = app.try_state::<Handles>() {
        let _ = handles
            .connect_item
            .set_text(if connected { "Disconnect" } else { "Connect" });
    }
    emit_state(app);
}

fn toggle_popup<R: Runtime>(app: &AppHandle<R>) {
    let now = now_ms();
    {
        let runtime_state = app.state::<Mutex<AppRuntime>>();
        let mut runtime = runtime_state.lock().unwrap();
        // Native tray menu / focus transitions can generate a second click.
        if now.saturating_sub(runtime.last_tray_click_ms) < 300 {
            return;
        }
        runtime.last_tray_click_ms = now;
    }

    if let Some(window) = app.get_webview_window("popup") {
        let _ = window.close();
        return;
    }

    let (x, y) = platform::popup_position();
    let window = match WebviewWindowBuilder::new(app, "popup", WebviewUrl::App("index.html".into()))
        .title("CMF Buds Pro 2")
        .inner_size(360.0, 640.0)
        .min_inner_size(360.0, 640.0)
        .max_inner_size(360.0, 640.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()
    {
        Ok(window) => window,
        Err(error) => {
            warn!("failed to create popup window: {error}");
            return;
        }
    };

    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));

    let hwnd = window.hwnd().ok().map(|handle| handle.0 as isize);
    let watcher_app = app.clone();
    std::thread::spawn(move || {
        let Some(hwnd) = hwnd else {
            return;
        };
        let opened = now_ms();
        loop {
            std::thread::sleep(Duration::from_millis(40));
            if watcher_app.get_webview_window("popup").is_none() {
                break;
            }
            // Give WebView2 time to paint before accepting a close signal.
            if now_ms().saturating_sub(opened) < 350 {
                continue;
            }
            if platform::left_click_outside(hwnd) {
                info!("outside click -> close popup");
                if let Some(popup) = watcher_app.get_webview_window("popup") {
                    let _ = popup.close();
                }
                break;
            }
        }
    });

    let _ = window.show();
    let _ = window.set_focus();
    info!("popup opened at physical position ({x}, {y})");
}

fn build_state(app: &AppHandle) -> FrontendState {
    let settings = app.state::<Mutex<Settings>>().lock().unwrap().clone();
    let runtime = app
        .state::<Mutex<AppRuntime>>()
        .lock()
        .unwrap()
        .clone();

    FrontendState {
        device_name: runtime.device_name,
        connected: runtime.connected,
        status: runtime.status,
        port: runtime.port,
        firmware: runtime.firmware,
        left: runtime.left,
        right: runtime.right,
        case_battery: runtime.case_battery,
        anc_mode: runtime.anc_mode,
        anc_strength: runtime.anc_strength,
        eq_preset: runtime.eq_preset,
        in_ear_detection: runtime.in_ear_detection,
        autostart: settings.autostart,
    }
}

fn emit_state(app: &AppHandle) {
    let state = build_state(app);
    let _ = app.emit("state-changed", &state);
    update_tooltip(app, &state);
}

fn percent(battery: &BatteryState) -> String {
    if battery.present {
        format!("{}%", battery.level)
    } else {
        "--".to_string()
    }
}

fn update_tooltip(app: &AppHandle, state: &FrontendState) {
    let tooltip = if state.connected {
        format!(
            "CMF Buds Pro 2  |  L {}  R {}  Case {}",
            percent(&state.left),
            percent(&state.right),
            percent(&state.case_battery)
        )
    } else {
        format!("CMF Buds Pro 2 — {}", state.status)
    };
    if let Some(handles) = app.try_state::<Handles>() {
        let _ = handles.tray.set_tooltip(Some(tooltip));
    }
}

fn toggle_autostart(app: &AppHandle) {
    let enabled = {
        let settings_state = app.state::<Mutex<Settings>>();
        let mut settings = settings_state.lock().unwrap();
        settings.autostart = !settings.autostart;
        settings::save(&settings);
        settings.autostart
    };
    if let Err(error) = platform::set_autostart(enabled) {
        warn!("failed to update autostart registry value: {error}");
    }
    if let Some(handles) = app.try_state::<Handles>() {
        let _ = handles.autostart_item.set_checked(enabled);
    }
    info!("start with Windows: {enabled}");
    emit_state(app);
}

#[tauri::command]
fn get_state(app: AppHandle) -> FrontendState {
    build_state(&app)
}

#[tauri::command]
fn connect(app: AppHandle) {
    send_command(&app, Command::Connect);
}

#[tauri::command]
fn disconnect(app: AppHandle) {
    send_command(&app, Command::Disconnect);
}

#[tauri::command]
fn set_anc(app: AppHandle, mode: String, strength: Option<u8>) {
    send_command(&app, Command::SetAnc { mode, strength });
}

#[tauri::command]
fn set_eq(app: AppHandle, preset: u8) {
    send_command(&app, Command::SetEq { preset });
}

#[tauri::command]
fn set_in_ear(app: AppHandle, enabled: bool) {
    send_command(&app, Command::SetInEar { enabled });
}

#[tauri::command]
fn hide_popup(app: AppHandle) {
    if let Some(window) = app.get_webview_window("popup") {
        let _ = window.close();
    }
}

#[tauri::command]
fn get_manual_com_port(app: AppHandle) -> Option<String> {
    app.state::<Mutex<Settings>>()
        .lock()
        .unwrap()
        .manual_com_port
        .clone()
}

#[tauri::command]
fn set_manual_com_port(app: AppHandle, port: Option<String>) {
    {
        let settings_state = app.state::<Mutex<Settings>>();
        let mut settings = settings_state.lock().unwrap();
        settings.manual_com_port = port.filter(|value| !value.trim().is_empty());
        settings::save(&settings);
    }
    // Re-run discovery immediately so a manual override takes effect now.
    send_command(&app, Command::Reconnect);
}
