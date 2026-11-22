#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod device;
mod platform;
mod protocol;
mod rfcomm;
mod settings;
mod state;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender, Arc, Mutex,
    },
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
    WebviewWindow, WebviewWindowBuilder, Wry,
};
use tracing::{info, warn};

type Handles = UiHandles<Wry>;

struct UiHandles<R: Runtime> {
    tray: TrayIcon<R>,
    connect_item: MenuItem<R>,
    autostart_item: CheckMenuItem<R>,
}

#[derive(Default)]
struct PopupState {
    /// Prevents a tray click racing an in-flight slide animation.
    animating: Arc<AtomicBool>,
    /// True while the sheet is sliding out, so focus events cannot restore the
    /// backdrop and leave a blank glass rectangle behind it.
    closing: Arc<AtomicBool>,
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
        .manage(Mutex::new(PopupState::default()))
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
                .icon(Image::from_bytes(include_bytes!("../icons/tray-case.png")).expect("tray icon"))
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

            // Build the WebView once and keep it hidden. Showing a preloaded
            // window is instant; creating one per click made WebView2 cold-start
            // every time and felt like launching a separate app.
            create_popup_window(app.app_handle())?;
            spawn_outside_watcher(app.app_handle().clone());

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

fn create_popup_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    let window = WebviewWindowBuilder::new(app, "popup", WebviewUrl::App("index.html".into()))
        .title("CMF Buds Pro 2")
        .inner_size(360.0, 700.0)
        .min_inner_size(360.0, 700.0)
        .max_inner_size(360.0, 700.0)
        .resizable(false)
        .decorations(false)
        .shadow(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;

    match platform::apply_popup_backdrop(&window) {
        Ok(()) => info!("native acrylic backdrop applied to popup"),
        Err(error) => warn!("failed to apply acrylic backdrop: {error}"),
    }
    if let Err(error) = platform::configure_popup_shape(&window) {
        warn!("failed to round popup window: {error}");
    }

    // Windows swaps the transient/acrylic backdrop for a flat wallpaper-derived
    // material as soon as the popup loses focus (a notification is enough).
    // Re-selecting the backdrop on focus changes restores the live desktop blur.
    let focus_window = window.clone();
    let focus_app = app.clone();
    window.on_window_event(move |event| {
        if !matches!(event, tauri::WindowEvent::Focused(_))
            || !focus_window.is_visible().unwrap_or(false)
        {
            return;
        }
        let closing = focus_app
            .state::<Mutex<PopupState>>()
            .lock()
            .unwrap()
            .closing
            .clone();
        if closing.load(Ordering::SeqCst) {
            return;
        }
        platform::clear_popup_backdrop(&focus_window);
        let _ = platform::apply_popup_backdrop(&focus_window);
    });

    Ok(window)
}

/// Claim the animation slot so a second click cannot start a competing slide.
fn begin_animation<R: Runtime>(app: &AppHandle<R>) -> Option<Arc<AtomicBool>> {
    let flag = app
        .state::<Mutex<PopupState>>()
        .lock()
        .unwrap()
        .animating
        .clone();
    if flag
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return None;
    }
    Some(flag)
}

fn slide_popup<R: Runtime>(
    window: &WebviewWindow<R>,
    x: i32,
    from_y: i32,
    to_y: i32,
    hide_after: bool,
    flag: Arc<AtomicBool>,
) {
    let window = window.clone();
    std::thread::spawn(move || {
        const STEPS: u32 = 14;
        const STEP_MS: u64 = 11;
        for step in 0..=STEPS {
            let t = step as f32 / STEPS as f32;
            let eased = 1.0 - (1.0 - t).powi(3);
            let y = from_y + ((to_y - from_y) as f32 * eased).round() as i32;
            if window
                .set_position(Position::Physical(PhysicalPosition::new(x, y)))
                .is_err()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(STEP_MS));
        }
        if hide_after {
            let _ = window.hide();
        }
        flag.store(false, Ordering::SeqCst);
    });
}

/// The frontend has already animated the sheet away; just hide the native
/// window. Safe to call twice (the fallback timer races the JS callback).
fn hide_popup_window<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>) {
    if !window.is_visible().unwrap_or(false) {
        return;
    }
    let _ = window.hide();
    app.state::<Mutex<PopupState>>()
        .lock()
        .unwrap()
        .closing
        .store(false, Ordering::SeqCst);
}

/// Ask the frontend to play the slide-out, with a timer as a fallback in case
/// the webview is unresponsive. The sheet clips at the taskbar top, so the
/// native window never draws over the taskbar.
fn request_hide_popup<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("popup") {
        // The sheet slides out inside the window. Remove the window-level
        // backdrop first so no empty blurred rectangle is left behind while
        // the content animates away.
        app.state::<Mutex<PopupState>>()
            .lock()
            .unwrap()
            .closing
            .store(true, Ordering::SeqCst);
        platform::clear_popup_backdrop(&window);
    }
    let _ = app.emit("popup-close-request", ());
    let watcher_app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(260));
        if let Some(window) = watcher_app.get_webview_window("popup") {
            hide_popup_window(&watcher_app, &window);
        }
    });
}

/// One watcher for the lifetime of the app. The window stays alive while
/// hidden, so it must only close on an outside click when actually visible.
fn spawn_outside_watcher<R: Runtime>(app: AppHandle<R>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(40));
        let Some(window) = app.get_webview_window("popup") else {
            continue;
        };
        if !window.is_visible().unwrap_or(false) {
            continue;
        }
        let animating = app
            .state::<Mutex<PopupState>>()
            .lock()
            .unwrap()
            .animating
            .load(Ordering::SeqCst);
        if animating {
            continue;
        }
        let Some(hwnd) = window.hwnd().ok().map(|handle| handle.0 as isize) else {
            continue;
        };
        if platform::left_click_outside(hwnd) {
            info!("outside click -> close popup");
            request_hide_popup(&app);
        }
    });
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

    let Some(window) = app.get_webview_window("popup") else {
        return;
    };

    if window.is_visible().unwrap_or(false) {
        request_hide_popup(app);
        return;
    }

    let Some(flag) = begin_animation(app) else {
        return;
    };
    if let Err(error) = platform::apply_popup_backdrop(&window) {
        warn!("failed to reapply popup backdrop: {error}");
    }
    let (x, y, bottom) = platform::popup_geometry();
    // Start flush with the taskbar and slide up, like a Windows flyout.
    let start_y = bottom;
    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, start_y)));
    let _ = window.show();
    let _ = window.set_focus();
    // Windows may clamp a partially off-screen window when it is shown; use
    // the real position so the slide always starts where the panel actually is.
    let actual_y = window
        .outer_position()
        .map(|position| position.y)
        .unwrap_or(start_y);
    slide_popup(&window, x, actual_y, y, false, flag);
    // Let the frontend clear a previous close animation before the new one.
    let _ = app.emit("popup-opened", ());
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
        hide_popup_window(&app, &window);
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
