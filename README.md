# CMF Tray

A lightweight Windows 11 system-tray controller for **CMF Buds Pro 2**, built with Tauri 2 and a
vanilla HTML/CSS/JS frontend. No Electron, no framework, no always-running webview: the popup is
created only when you left-click the tray icon and destroyed when it loses focus.

> Status: **Milestone 1** — tray + popup shell with fake data. Bluetooth arrives in Milestone 2.

## Layout

```
src/                     vanilla frontend (popup UI)
src-tauri/src/main.rs    tray, popup lifecycle, commands, settings wiring
src-tauri/src/platform.rs Windows helpers (taskbar/DPI placement, autostart Run key)
src-tauri/src/settings.rs %APPDATA%\cmf-tray\settings.json + optional file logging
src-tauri/src/state.rs   state pushed to the frontend via Tauri events
NOTES.md                 full protocol research and byte fixtures
tools/generate_icons.py  regenerates the original icons (requires Pillow)
```

## Build & run

Prerequisites:

- Windows 11 x64, WebView2 (preinstalled on Windows 11)
- Rust toolchain with the `x86_64-pc-windows-msvc` target and Visual Studio Build Tools
- Tauri CLI: `cargo install tauri-cli --version "^2" --locked`

Then, from the repository root:

```powershell
cargo tauri dev                  # build and run in development mode
cargo tauri build                # produce MSI + NSIS installers
cargo tauri dev -- --debug       # enable file logging + raw dumps (from M2)
```

Debug logs go to `%APPDATA%\cmf-tray\cmf-tray.log`. Setting `CMF_TRAY_DEBUG=1` does the same.

## Tray behaviour

- **Left click** — toggle a frameless 320×400 popup, anchored above the taskbar on the monitor
  containing the cursor (work area from `GetMonitorInfoW`, DPI from `GetDpiForMonitor`).
- **Right click** — menu: Connect / Disconnect, Start with Windows, Quit.
- **Esc** or clicking outside — destroy the popup.
- A second app launch is handed to the running instance by `tauri-plugin-single-instance` and simply
  opens the popup.

Settings (`%APPDATA%\cmf-tray\settings.json`) hold the last device, autostart flag, optional manual
COM port override, and debug flag. Autostart writes the `CMF Tray` value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

## Protocol summary (details in NOTES.md)

Transport is **Bluetooth SPP / RFCOMM**, not BLE. Control service UUID:
`aeac4a03-dff5-498f-843a-34487cf133eb`; Fast Pair identification UUID:
`df21fe2c-2515-4fdb-8886-f12c4d67927c`. Web Serial uses 9600 baud.

Frames:

```
55 60 01 [cmd lo hi] [len lo hi] [op id] [payload] [crc lo hi]
```

- CRC is CRC-16/Modbus (init `0xFFFF`, poly `0xA001`) when header byte 1 has bit `0x20` set.
- `0xC0xx` = read, `0xF0xx` = set (ACK `0x70xx`), `0x40xx` = response, `0xE0xx` = push.
- Battery `0xC007`, ANC `0xC01E`/`0xF00F`, in-ear `0xC00E`/`0xF004`,
  EQ presets `0xC050`/`0xF01D` (this model does *not* use the generic EQ commands).

Core command set: battery (L/R/case + charging), ANC mode and strength, in-ear detection toggle,
and the seven EQ presets. ANC strength wire values `01`–`04` are kept raw and shown as Level 1–4
until confirmed on hardware.

## License

Original source code is provided as-is. Icons in `src-tauri/icons/` were generated for this project
by `tools/generate_icons.py`. Protocol research is derived from the AGPL-3.0 licensed
[ear-web](https://github.com/radiance-project/ear-web) project; see NOTES.md for source references.
