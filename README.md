# CMF Tray

A lightweight Windows 11 system-tray controller for **CMF Buds Pro 2**, built with Tauri 2 and a
vanilla HTML/CSS/JS frontend. No Electron, no framework, no always-running webview: the popup is
created only when you left-click the tray icon and destroyed when you click outside it.

> Status: **Milestone 2** - real Bluetooth SPP transport, protocol parser, live state, ANC/EQ/in-ear
> control, auto-discovery and reconnect. Core features are implemented; hardware-specific ANC
> strength labels and firmware variants still need real-device confirmation.

## Layout

```
src/                        vanilla frontend (compact Nothing X-style popup)
  assets/                   product renders copied from ear-web
src-tauri/src/main.rs       tray, popup lifecycle, commands, settings wiring
src-tauri/src/device.rs     dedicated SPP thread: discovery, reconnect, ACK queue
src-tauri/src/protocol.rs   pure frame build/parse + fixture unit tests
src-tauri/src/platform.rs   Windows helpers: DPI placement, SetupAPI BT discovery, autostart
src-tauri/src/settings.rs   %APPDATA%\cmf-tray\settings.json + optional file logging
src-tauri/src/state.rs      state pushed to the frontend via Tauri events
NOTES.md                    full protocol research and byte fixtures
tools/generate_icons.py     regenerates the original icons (requires Pillow)
```

## Build & run

Prerequisites:

- Windows 11 x64, WebView2 (preinstalled on Windows 11)
- Rust toolchain with the `x86_64-pc-windows-msvc` target and Visual Studio Build Tools
- Tauri CLI: `cargo install tauri-cli --version "^2" --locked`

From the repository root:

```powershell
cargo test                             # 8 protocol fixture/parser tests
cargo tauri dev                        # build and run in development mode
cargo tauri build                      # produce MSI + NSIS installers
cargo tauri dev -- --debug             # file logging + raw TX/RX hex dumps
```

Debug logs go to `%APPDATA%\cmf-tray\cmf-tray.log`. Setting `CMF_TRAY_DEBUG=1` does the same.

## Tray behaviour

- **Left click** - toggle a frameless 360x640 popup, anchored above the taskbar on the monitor
  containing the cursor (work area from `GetMonitorInfoW`, DPI from `GetDpiForMonitor`).
- **Right click** - menu: Connect / Disconnect, Start with Windows, Quit.
- **Esc** or clicking outside - destroy the popup.
- A second app launch is handed to the running instance by `tauri-plugin-single-instance` and simply
  opens the popup.

Settings (`%APPDATA%\cmf-tray\settings.json`) hold the last device/port, autostart flag, optional
manual COM port override, and debug flag. Autostart writes the `CMF Tray` value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

## Bluetooth transport

- One dedicated thread owns the single `serialport` handle, so all writes are serialised.
- On startup it reuses the remembered COM port, then the manual override, then discovers the
  **outgoing SPP port** by matching the paired headset's name/Bluetooth address through Windows
  SetupAPI (`BTHENUM\DEV_<address>` -> `Standard Serial over Bluetooth link (COMn)`). Windows
  reports these ports as `Unknown` via `serialport`, hence the SetupAPI lookup.
- Reconnect uses exponential backoff from 2 s to 60 s.
- `0xF0xx` set commands wait for their `0x70xx` ACK (2 s timeout, one retry), then a read confirms
  the value before the UI updates. Poll reads run every 2 s.
- Unsolicited `0x60/0xE0xx` pushes update battery and ANC immediately; unknown command IDs are logged.
- With `--debug`, every TX/RX frame is dumped as hex with decoded command, op ID, length and CRC
  result.

A direct WinRT `RfcommDeviceService`/`StreamSocket` fallback (for setups with no exposed COM port)
is not implemented yet; Windows normally exposes an outgoing SPP COM port for paired headsets, which
is what this build uses. The manual COM override covers unusual layouts.

## Protocol summary (details in NOTES.md)

Transport is **Bluetooth SPP / RFCOMM**, not BLE. Control service UUID:
`aeac4a03-dff5-498f-843a-34487cf133eb`; Fast Pair identification UUID:
`df21fe2c-2515-4fdb-8886-f12c4d67927c`. Web Serial uses 9600 baud.

```
55 60 01 [cmd lo hi] [len lo hi] [op id] [payload] [crc lo hi]
```

- CRC is CRC-16/Modbus (init `0xFFFF`, poly `0xA001`) when header byte 1 has bit `0x20` set.
  Incoming frames without a CRC are accepted; bad-CRC and over-long frames are dropped.
- `0xC0xx` = read, `0xF0xx` = set (ACK `0x70xx`), `0x40xx` = response, `0xE0xx` = push.
- Battery `0xC007`, ANC `0xC01E`/`0xF00F`, in-ear `0xC00E`/`0xF004`,
  EQ presets `0xC050`/`0xF01D` (this model does *not* use the generic EQ commands).
- ANC wire values `01`-`04` are kept raw and shown as Level 1-4. The odd ear-web UI-level mapping
  (`4,5,3,6`) lives in a single table in `protocol.rs` with a unit test.

## License

Original source code is provided as-is. Icons in `src-tauri/icons/` were generated for this project
by `tools/generate_icons.py`. Protocol research and product renders are derived from the AGPL-3.0
licensed [ear-web](https://github.com/radiance-project/ear-web) project; see NOTES.md for sources.
