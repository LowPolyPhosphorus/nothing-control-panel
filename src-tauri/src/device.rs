//! Dedicated Bluetooth SPP/RFCOMM manager thread.
//!
//! Owns the single serial port, so all writes are implicitly serialised. Reads
//! are pumped continuously and fed to the protocol reassembler; `0xF0xx` set
//! commands wait for their `0x70xx` ACK (with one retry) before the UI is
//! allowed to show the new value, which is then confirmed by a read.

use std::{
    io::{Read, Write},
    sync::{mpsc, Mutex},
    time::{Duration, Instant},
};

use serialport::{available_ports, SerialPort, SerialPortType};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::{
    patch_runtime,
    platform,
    protocol::{self as proto, AncMode},
    settings::{self, Settings},
    state::{AppRuntime, BatteryState},
};

const BAUD: u32 = 9600;
const READ_SLICE: Duration = Duration::from_millis(80);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_millis(2000);
const BASE_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum Command {
    Connect,
    Disconnect,
    Reconnect,
    SetAnc {
        mode: String,
        strength: Option<u8>,
    },
    SetEq {
        preset: u8,
    },
    SetInEar {
        enabled: bool,
    },
}

pub fn spawn(app: AppHandle, commands: mpsc::Receiver<Command>) {
    std::thread::Builder::new()
        .name("cmf-device".to_string())
        .spawn(move || DeviceManager::new(app, commands).run())
        .expect("spawn device thread");
}

struct DeviceManager {
    app: AppHandle,
    commands: mpsc::Receiver<Command>,
    port: Option<Box<dyn SerialPort>>,
    port_name: String,
    reassembler: proto::FrameReassembler,
    op_id: u8,
    next_poll: Instant,
    retry_at: Option<Instant>,
    backoff: Duration,
    should_connect: bool,
    raw_log: bool,
    ack_target: Option<(u16, u8)>,
    ack_seen: bool,
    connected_at: Option<Instant>,
    last_frame_at: Option<Instant>,
    waiting_notified: bool,
}

impl DeviceManager {
    fn new(app: AppHandle, commands: mpsc::Receiver<Command>) -> Self {
        let raw_log = with_settings(&app, |settings| settings.debug);
        Self {
            app,
            commands,
            port: None,
            port_name: String::new(),
            reassembler: proto::FrameReassembler::new(),
            op_id: 0,
            next_poll: Instant::now(),
            retry_at: None,
            backoff: BASE_BACKOFF,
            should_connect: false,
            raw_log,
            ack_target: None,
            ack_seen: false,
            connected_at: None,
            last_frame_at: None,
            waiting_notified: false,
        }
    }

    fn run(mut self) {
        info!("device manager thread started");
        loop {
            match self.commands.recv_timeout(Duration::from_millis(40)) {
                Ok(Command::Connect) => self.command_connect(),
                Ok(Command::Disconnect) => self.command_disconnect(),
                Ok(Command::Reconnect) => {
                    self.disconnect("Reconnecting…");
                    self.command_connect();
                }
                Ok(Command::SetAnc { mode, strength }) => self.command_set_anc(mode, strength),
                Ok(Command::SetEq { preset }) => self.command_set_eq(preset),
                Ok(Command::SetInEar { enabled }) => self.command_set_inear(enabled),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if !self.should_connect {
                continue;
            }

            if self.port.is_none() {
                if self.retry_at.is_some_and(|at| Instant::now() < at) {
                    continue;
                }
                self.try_connect();
                continue;
            }

            self.pump_reads(READ_SLICE);
            if Instant::now() >= self.next_poll {
                self.refresh();
                self.next_poll = Instant::now() + POLL_INTERVAL;
                self.check_silent_device();
            }
        }
        info!("device manager thread stopped");
    }

    // ---- connection lifecycle --------------------------------------------

    fn command_connect(&mut self) {
        if self.port.is_some() {
            return;
        }
        self.should_connect = true;
        self.retry_at = None;
        self.backoff = BASE_BACKOFF;
        patch_runtime(&self.app, |runtime| {
            runtime.status = "Searching for CMF Buds Pro 2…".to_string();
        });
    }

    fn command_disconnect(&mut self) {
        self.should_connect = false;
        self.retry_at = None;
        self.disconnect("Disconnected");
    }

    fn disconnect(&mut self, reason: &str) {
        let was_connected = self.port.take().is_some();
        self.port_name.clear();
        self.ack_target = None;
        self.connected_at = None;
        self.last_frame_at = None;
        self.waiting_notified = false;
        self.reassembler = proto::FrameReassembler::new();
        patch_runtime(&self.app, |runtime| {
            runtime.connected = false;
            runtime.status = reason.to_string();
            runtime.port.clear();
            runtime.firmware.clear();
            runtime.left = BatteryState::absent();
            runtime.right = BatteryState::absent();
            runtime.case_battery = BatteryState::absent();
        });
        if was_connected {
            info!("disconnected: {reason}");
        }
    }

    fn schedule_retry(&mut self, reason: &str) {
        let delay = self.backoff;
        self.retry_at = Some(Instant::now() + delay);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        patch_runtime(&self.app, |runtime| {
            runtime.status = format!("{reason} — retrying in {}s", delay.as_secs());
        });
    }

    fn try_connect(&mut self) {
        let (manual, last_port) = with_settings(&self.app, |settings| {
            (settings.manual_com_port.clone(), settings.last_port.clone())
        });

        let ports = match available_ports() {
            Ok(ports) => ports,
            Err(error) => {
                warn!("cannot enumerate COM ports: {error}");
                self.schedule_retry("Cannot enumerate COM ports");
                return;
            }
        };
        info!(
            "available COM ports: {}",
            ports
                .iter()
                .map(|port| format!("{} ({:?})", port.port_name, port.port_type))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Manual override wins, then the remembered port, then any Bluetooth
        // SPP port Windows exposes for a paired headset.
        let has = |name: &String| {
            ports
                .iter()
                .any(|port| port.port_name.eq_ignore_ascii_case(name))
        };
        let chosen = manual
            .filter(|name| has(name))
            .or_else(|| last_port.filter(|name| has(name)))
            .or_else(|| {
                // Windows reports SPP ports as `Unknown`, so match the paired
                // device's name and Bluetooth address via SetupAPI.
                let discovered = platform::bluetooth_serial_port();
                info!("name-based SPP discovery: {discovered:?}");
                discovered.filter(|name| has(name))
            })
            .or_else(|| {
                ports
                    .iter()
                    .find(|port| port.port_type == SerialPortType::BluetoothPort)
                    .map(|port| port.port_name.clone())
            });

        let Some(name) = chosen else {
            self.schedule_retry("No Bluetooth serial port found — pair your buds first");
            return;
        };

        patch_runtime(&self.app, |runtime| {
            runtime.status = format!("Connecting to {name}…");
        });
        info!("opening {name} at {BAUD} baud");
        match serialport::new(&name, BAUD)
            .timeout(READ_SLICE)
            .open()
        {
            Ok(port) => {
                info!("connected to {name}");
                self.port = Some(port);
                self.port_name = name.clone();
                self.retry_at = None;
                self.backoff = BASE_BACKOFF;
                self.op_id = 0;
                self.connected_at = Some(Instant::now());
                self.last_frame_at = None;
                self.waiting_notified = false;
                self.next_poll = Instant::now() + POLL_INTERVAL;
                with_settings_mut(&self.app, |settings| {
                    settings.last_port = Some(name.clone());
                    settings.last_device = Some("CMF Buds Pro 2".to_string());
                    settings::save(settings);
                });
                patch_runtime(&self.app, |runtime| {
                    runtime.connected = true;
                    runtime.port = name;
                    runtime.status = "Connected".to_string();
                });
                self.initial_reads();
            }
            Err(error) => {
                warn!("failed to open {name}: {error}");
                self.schedule_retry(&format!("Could not open {name}"));
            }
        }
    }

    // ---- I/O --------------------------------------------------------------

    fn initial_reads(&mut self) {
        let frames = [
            proto::get_battery(self.next_op_id()),
            proto::get_eq(self.next_op_id()),
            proto::get_anc(self.next_op_id()),
            proto::get_firmware(self.next_op_id()),
            proto::get_ear(self.next_op_id()),
        ];
        for frame in frames {
            self.write_frame(&frame);
        }
        self.pump_reads(Duration::from_millis(500));
    }

    fn refresh(&mut self) {
        let frames = [
            proto::get_battery(self.next_op_id()),
            proto::get_anc(self.next_op_id()),
            proto::get_eq(self.next_op_id()),
            proto::get_ear(self.next_op_id()),
        ];
        for frame in frames {
            self.write_frame(&frame);
        }
        self.pump_reads(Duration::from_millis(400));
    }

    fn write_frame(&mut self, frame: &[u8]) -> bool {
        if self.raw_log {
            info!("TX {}", proto::hex(frame));
        }
        let result = match self.port.as_mut() {
            Some(port) => port.write_all(frame).map_err(|error| error.to_string()),
            None => Err("not connected".to_string()),
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                warn!("serial write failed: {error}");
                self.disconnect("Connection lost");
                false
            }
        }
    }

    fn pump_reads(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        let mut buffer = [0u8; 256];
        loop {
            let result = match self.port.as_mut() {
                Some(port) => port.read(&mut buffer),
                None => break,
            };
            match result {
                Ok(0) => {}
                Ok(count) => {
                    if self.raw_log {
                        info!("RX {}", proto::hex(&buffer[..count]));
                    }
                    for frame in self.reassembler.push(&buffer[..count]) {
                        self.handle_frame(frame);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    // A serial read timeout just means "nothing yet"; keep
                    // listening until the whole pump window has elapsed.
                    if Instant::now() >= deadline {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    warn!("serial read failed: {error}");
                    self.disconnect("Connection lost");
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }

    fn check_silent_device(&mut self) {
        if self.waiting_notified {
            return;
        }
        let silent = self.connected_at.is_some_and(|at| at.elapsed() > Duration::from_secs(5))
            && self.last_frame_at.is_none();
        if silent {
            self.waiting_notified = true;
            warn!("port opened but no protocol frames received — is the headset busy with another device?");
            let port = self.port_name.clone();
            patch_runtime(&self.app, |runtime| {
                runtime.status = format!("Connected to {port} — waiting for buds…");
            });
        }
    }

    fn handle_frame(&mut self, frame: proto::Frame) {
        self.last_frame_at = Some(Instant::now());
        let name = proto::command_name(frame.command);
        info!(
            "frame cmd=0x{:04X} ({name}) op={} len={} push={} crc={:?}",
            frame.command,
            frame.op_id,
            frame.payload.len(),
            frame.is_push(),
            frame.crc_valid
        );

        if let Some((command, op_id)) = self.ack_target {
            if frame.is_ack_for(command, op_id) {
                info!("ACK for 0x{command:04X} op {op_id}");
                self.ack_seen = true;
                return;
            }
        }
        // Ignore unrelated/unexpected ACKs.
        if frame.command & 0x8000 == 0
            && (0x7000..=0x70FF).contains(&(frame.command & 0x7FFF))
        {
            return;
        }

        match frame.command {
            proto::RESP_BATTERY | proto::PUSH_BATTERY => {
                match proto::parse_battery(&frame.payload) {
                    Some(reading) => {
                        // The web app treats a stereo (single-body) reading as
                        // applying to all three indicators.
                        let (left, right, case_battery) = match reading.stereo {
                            Some(stereo) => (Some(stereo), Some(stereo), Some(stereo)),
                            None => (reading.left, reading.right, reading.case_battery),
                        };
                        patch_runtime(&self.app, |runtime| {
                            runtime.left = BatteryState::from_reading(left);
                            runtime.right = BatteryState::from_reading(right);
                            runtime.case_battery = BatteryState::from_reading(case_battery);
                        });
                    }
                    None => warn!("malformed battery payload: {}", proto::hex(&frame.payload)),
                }
            }
            proto::RESP_ANC | proto::PUSH_ANC => match proto::parse_anc(&frame.payload) {
                Some(status) => {
                    info!(
                        "ANC mode={:?} wire strength={} (UI level {})",
                        status.mode,
                        status.strength,
                        proto::anc_ui_level(status.strength)
                    );
                    let mode = match status.mode {
                        AncMode::Off => "off",
                        AncMode::Transparency => "transparency",
                        AncMode::Anc => "anc",
                    };
                    patch_runtime(&self.app, |runtime| {
                        runtime.anc_mode = mode.to_string();
                        runtime.anc_strength = status.strength;
                    });
                }
                None => warn!("malformed ANC payload: {}", proto::hex(&frame.payload)),
            },
            proto::RESP_EQ => {
                if let Some(preset) = proto::parse_eq(&frame.payload) {
                    patch_runtime(&self.app, |runtime| runtime.eq_preset = preset);
                }
            }
            proto::RESP_INEAR => {
                if let Some(enabled) = proto::parse_in_ear(&frame.payload) {
                    patch_runtime(&self.app, |runtime| runtime.in_ear_detection = enabled);
                }
            }
            proto::RESP_FIRMWARE => {
                let firmware = proto::parse_firmware(&frame.payload);
                info!("firmware: {firmware}");
                patch_runtime(&self.app, |runtime| runtime.firmware = firmware);
            }
            other => warn!("unknown command id 0x{other:04X}"),
        }
    }

    // ---- commands ---------------------------------------------------------

    fn next_op_id(&mut self) -> u8 {
        self.op_id = if self.op_id >= 254 { 1 } else { self.op_id + 1 };
        self.op_id
    }

    /// Serialised set: one frame in flight, ACK with a short timeout, one
    /// retry, then a read to confirm the value actually stuck.
    fn send_set(
        &mut self,
        command: u16,
        make_frame: impl Fn(u8) -> Vec<u8>,
        label: &str,
    ) {
        if self.port.is_none() {
            warn!("{label}: not connected");
            return;
        }
        for attempt in 1..=2 {
            let op_id = self.next_op_id();
            let frame = make_frame(op_id);
            if !self.write_frame(&frame) {
                return;
            }
            self.ack_target = Some((command, op_id));
            self.ack_seen = false;
            let deadline = Instant::now() + ACK_TIMEOUT;
            while Instant::now() < deadline && !self.ack_seen {
                self.pump_reads(Duration::from_millis(40));
            }
            self.ack_target = None;
            if self.ack_seen {
                info!("{label} acknowledged");
                self.confirm_after_set(command);
                return;
            }
            warn!("{label} ACK timeout (attempt {attempt}/2)");
            if self.port.is_none() {
                return;
            }
        }
        warn!("{label} failed after 2 attempts");
    }

    fn confirm_after_set(&mut self, command: u16) {
        let frame = match command {
            proto::CMD_SET_ANC => proto::get_anc(self.next_op_id()),
            proto::CMD_SET_EQ => proto::get_eq(self.next_op_id()),
            proto::CMD_SET_INEAR => proto::get_ear(self.next_op_id()),
            _ => return,
        };
        self.write_frame(&frame);
        self.pump_reads(Duration::from_millis(300));
    }

    fn command_set_anc(&mut self, mode: String, strength: Option<u8>) {
        let mode = match mode.as_str() {
            "anc" => AncMode::Anc,
            "transparency" => AncMode::Transparency,
            _ => AncMode::Off,
        };
        let strength = strength.unwrap_or_else(|| {
            self.app
                .state::<Mutex<AppRuntime>>()
                .lock()
                .map(|runtime| runtime.anc_strength)
                .unwrap_or(1)
        });
        self.send_set(
            proto::CMD_SET_ANC,
            |op_id| proto::set_anc(op_id, mode, strength),
            "ANC",
        );
    }

    fn command_set_eq(&mut self, preset: u8) {
        let preset = preset.min(6);
        self.send_set(
            proto::CMD_SET_EQ,
            |op_id| proto::set_eq(op_id, preset),
            "EQ preset",
        );
    }

    fn command_set_inear(&mut self, enabled: bool) {
        self.send_set(
            proto::CMD_SET_INEAR,
            |op_id| proto::set_in_ear(op_id, enabled),
            "In-ear detection",
        );
    }
}

fn with_settings<R>(app: &AppHandle, use_settings: impl FnOnce(&Settings) -> R) -> R {
    let state = app.state::<Mutex<Settings>>();
    let settings = state.lock().expect("settings mutex poisoned");
    use_settings(&settings)
}

fn with_settings_mut<R>(app: &AppHandle, change: impl FnOnce(&mut Settings) -> R) -> R {
    let state = app.state::<Mutex<Settings>>();
    let mut settings = state.lock().expect("settings mutex poisoned");
    change(&mut settings)
}
