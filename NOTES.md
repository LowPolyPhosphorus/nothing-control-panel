# Research notes: CMF Buds Pro 2 control protocol

Research date: 2026-11-20. Sources:

- <https://github.com/radiance-project/ear-web> (cloned locally into `%TEMP%\ear-web-research` for inspection)
- <https://github.com/radiance-project/ear-pc> (frontend + frozen `main.pyc` only; backend is closed source)
- <https://earweb.bttl.xyz/index.html>

All line references below refer to the `main` branch of `ear-web` as cloned on the date above.

## 1. Transport — VERIFIED: Bluetooth SPP / RFCOMM, not BLE

The web app uses the **Web Serial API** on top of a paired Bluetooth **Serial Port Profile (SPP / RFCOMM)** connection. There is no BLE GATT anywhere in the control path.

Evidence (`res/js/bluetooth_socket.js`):

- Line 275: `const SPP_UUID = "aeac4a03-dff5-498f-843a-34487cf133eb";`
- Line 276: `const FASTPAIR_UUID = "df21fe2c-2515-4fdb-8886-f12c4d67927c";`
- Lines 278–281: `navigator.serial.requestPort({ allowedBluetoothServiceClassIds: [SPP_UUID], filters: [{ bluetoothServiceClassId: SPP_UUID }] })`
- Line 286: `sppPort.open({ baudRate: 9600, bufferSize: 2048 })`
- Lines 311–328: a byte stream is read continuously and fed to a frame reassembler.

The ear-pc repo corroborates this: its README lists `Pybluez` as the backend dependency, i.e. classic Bluetooth RFCOMM sockets, not BLE.

Service IDs to use on Windows:

| Purpose | UUID |
|---|---|
| Control/data SPP service | `aeac4a03-dff5-498f-843a-34487cf133eb` |
| Fast Pair identification service (used once during "scan" to read the model ID) | `df21fe2c-2515-4fdb-8886-f12c4d67927c` |

**Windows implication:** after pairing, Windows normally exposes the outbound SPP service as a virtual COM port (the *outgoing* COM port listed under the device's Advanced Services). Baud is irrelevant for RFCOMM but the web app uses 9600; `serialport` should open at 9600 for parity. If no COM port exists or it is unreliable, fall back to `Windows.Devices.Bluetooth.Rfcomm` + `Windows.Networking.Sockets.StreamSocket` via the `windows` crate, connecting directly to the SPP service UUID above.

### Device identification

`res/js/control.js` maps Fast Pair model IDs to device names/bases. CMF Buds Pro 2 entries (lines 170–200, 317–399):

- `F29566`, `CA36A6`, `A7B220`, `2B353E` → base `B172` (older revision)
- `2F45F5`, `E1BE45`, `1253C0`, `0F1A4F` → base `B172`, `altBase: B187`
- `F51D3D`, `A544CE`, `9EAC04`, `1D07D4`, `53BFE6`, `072714` → base `B195` (Bluetrum-chip revision; 3-stage bass instead of 5)

`res/js/nothing_connected.js` lines 56–95 show the Fast Pair identification handshake: open the Fast Pair service at 9600 baud and read a **7-byte** reply; the model ID is `hex.substring(8, 14)` (i.e. bytes 4–6 of the reply). `res/js/nothing_connected.js` line 19 routes `B172`/`B195` to the `espeon` control page; `espeon` is the CMF Buds Pro 2 UI.

`res/js/bluetooth_socket.js` lines 264–270: `modelBase = model.base` and `modelConfigID = model.altBase || model.base`. So even for `B187` hardware, `modelBase` stays `B172` — all protocol branches in `bluetooth_socket.js` use `B172`. `B195` has a few small differences (3-stage bass; same core ANC/battery/EQ commands).

We do not need the Fast Pair handshake for the tray app if we discover paired devices by name and/or use the remembered last device. It is only needed for first-time model identification. Paired-device name matching should accept names containing `CMF Buds Pro 2` (and the base/alt-base IDs above if available).

## 2. Packet framing (`res/js/bluetooth_socket.js` lines 39–63, 146–219)

Every frame is:

```
offset  size  field
0       1     sync byte = 0x55
1       1     control byte; outgoing = 0x60. Bit 5 (0x20) means "CRC present"
                 in incoming frames. Outgoing always has it set.
2       1     protocol version = 0x01
3       2     command ID, LITTLE endian (e.g. 0xC007 -> 07 C0)
5       2     payload length, little endian
7       1     operation ID (1..255, wraps to 1)
8       N     payload (N = payload length)
8+N    2     CRC16, LITTLE endian (only if control bit 0x20 set)
```

CRC: **CRC-16/Modbus** — init `0xFFFF`, polynomial `0xA001` (reflected), no final XOR, byte-wise LSB-first. Computed over bytes `0 .. 8+N-1` (header + payload, excluding the two CRC bytes). Implementation at lines 210–219.

`createFrameReassembler` (lines 157–208) resynchronises on `0x55`, rejects payload lengths > 4096, honours the CRC-present flag, and drops frames whose CRC fails. Our Rust parser must do the same. Incoming frames are *not* guaranteed to have a CRC (control byte 0x40 vs 0x60).

### Command numbering

- Requests: `0xC0xx` = read/get; `0xF0xx` = set (must be ACKed).
- Responses: `0x40xx` (bit 15 clear).
- Pushes/unsolicited: `0x60xx` / `0xE0xx` (bit 15 set).
- ACK for a set: `0x70xx` with the same low 15 bits and same operation ID. `isAckFor()` at lines 74–77: `(received & 0x8000) === 0 && (received & 0x7FFF) === (requested & 0x7FFF)`.

`initDevice()` (lines 221–250) issues these reads at 100 ms intervals right after connecting, in this order: battery, EQ/listening mode, ANC, firmware, UTC time, in-ear, low-latency, personalized ANC (B155 only), gestures, ANC read (again), advanced EQ status, bass enhance.

Set commands are serialised: only one `0xF0xx` is in flight at a time; the next waits for its ACK with a 2 s timeout (`SET_ACK_TIMEOUT_MS`, lines 79–109). We should mirror that.

### Verified byte fixtures (generated with the exact JS CRC algorithm)

```
GET_BATTERY       cmd 0xC007 op 1 : 55 60 01 07 C0 00 00 01 AC DF
GET_ANC           cmd 0xC01E op 2 : 55 60 01 1E C0 00 00 02 F1 1C
GET_LISTENINGMODE cmd 0xC050 op 3 : 55 60 01 50 C0 00 00 03 58 D2
GET_INEAR         cmd 0xC00E op 4 : 55 60 01 0E C0 00 00 04 B0 DD
GET_FIRMWARE      cmd 0xC042 op 10: 55 60 01 42 C0 00 00 0A 20 D7
SET_ANC_OFF       cmd 0xF00F op 5 : 55 60 01 0F F0 03 00 05 01 05 00 FA 27
SET_ANC_TRANS     cmd 0xF00F op 6 : 55 60 01 0F F0 03 00 06 01 07 00 FB 03
SET_ANC_ANCHIGH   cmd 0xF00F op 7 : 55 60 01 0F F0 03 00 07 01 01 00 F9 5F
SET_EQ_ROCK(1)    cmd 0xF01D op 8 : 55 60 01 1D F0 02 00 08 01 00 2A FB
SET_RING_LEFT     cmd 0xF002 op 9 : 55 60 01 02 F0 02 00 09 02 01 54 CA
```

These are ideal unit-test vectors for `protocol::build_frame` and `protocol::crc16`.

## 3. Core commands and response parsing

### Battery — VERIFIED (`bluetooth_socket.js` lines 431–465, handler keys 0xE001 and 0xC007 → response 0x4007)

Read: `0xC007`, empty payload. Also pushed as `0xE001`.

Response payload:

```
byte 0            = count N of following entries
bytes 1..         = N entries of [device_id(1), battery_byte(1)]
```

`device_id`: `0x02` = left, `0x03` = right, `0x04` = case, `0x06` = stereo/single-body.
`battery_byte`: low 7 bits (`& 0x7F`) = percentage 0–100; bit 7 (`& 0x80`) = charging.

If a `stereo` (0x06) entry exists, the web app uses that percentage for all; otherwise left/right/case individually. We'll expose all three plus charging.

### ANC / Noise control — VERIFIED (`bluetooth_socket.js` lines 466–549, handler keys 0xC01E and 0xE003 → response 0x401E)

Read: `0xC01E`, empty payload. Also pushed as `0xE003`.

Response payload is a sequence of 3-byte records `[type(1), value(1), unused(1)]`:

- `type = 0x01` (mode): `value 1..4` = ANC on, strength level 1..4; `value 0 or 5` = off; `value 7 or 0xFE` = transparency.
- `type = 0x02` (strength only): `value 1..4` = strength level.

The web app's UI levels and their wire values (`setANC_BT`, lines 534–550) are:

| UI level | Meaning | wire byte (`payload[1]`) |
|---|---|---|
| 1 | Off | `0x05` |
| 2 | Transparency | `0x07` |
| 3 | ANC, strength 3 (low) | `0x03` |
| 4 | ANC, strength 1 (high) | `0x01` |
| 5 | ANC, strength 2 (mid) | `0x02` |
| 6 | ANC, strength 4 (adaptive) | `0x04` |

Read mapping back to UI level (`readANC`, lines 485–509): ANC strength 1→4, 2→5, 3→3, 4→6.

Set: `0xF00F` with payload `[0x01, wire_byte, 0x00]`. This is a `0xF0xx` set command → wait for `0x70xx` ACK.

The config for all CMF Buds Pro 2 variants has `ancLevel = 63` (`0b111111`), meaning: transparency + strength control + mid strength + adaptive strength are all supported (`device_common.js` lines 5–24). So expose Off / Transparency / ANC with 4 strengths (High, Mid, Low, Adaptive) — but the user's requested core UI is the 3-way segmented control (Off / Transparency / ANC) plus optional strength selector.

### In-ear detection — VERIFIED with one caveat (`bluetooth_socket.js` lines 1205–1221, handler key 0xC00E → response 0x400E)

Read: `0xC00E`, empty payload. Skipped for models B174/B185/B175/B189/B186; **B172/B195 do support it**.

Response: `inEarStatus = payload[2]`; the web app treats `status === 1` as "in-ear enabled", anything else as disabled. This is the *feature toggle state*, not a live per-ear "bud is in my ear" sensor value. The web app never exposes individual left/right in-ear occupancy, so we cannot derive that from this source. We'll mirror the single boolean and label it accordingly. Set: `0xF004` payload `[0x01, 0x01, enable?1:0]`.

Caveat: if the user expects per-ear wear detection, that is **not determinable from this source** and should not be guessed.

### EQ presets — VERIFIED (`bluetooth_socket.js` lines 563–593, `eq_espeon.js`, `MainControl_espeon.html` lines 200–230)

Important: for B172/B195 the web app does **not** use the generic `0xC01F`/`0xF010` EQ commands. It uses the "listening mode" commands instead:

- Read current preset: `0xC050` (empty payload); response `0x4050` → `readEQ(payload)`, preset = `payload[0]`.
- Set preset: `0xF01D` with payload `[preset, 0x00]` (`setListeningMode`), wait for `0x701D` ACK.

Preset IDs and names from `MainControl_espeon.html` + `eq_espeon.js` lines 34–73:

| ID | Preset name |
|---|---|
| 0 | Dirac OPTEO |
| 1 | Rock |
| 2 | Electronic |
| 3 | POP |
| 4 | Enhance vocals |
| 5 | Classical |
| 6 | Custom |

Custom EQ (ID 6) uses `0xC044` read / `0xF041` write with a 3-band float packet built by `createEQPacket` (lines 876–942): `[bandCount=3][totalGain f32 LE-ish][band0 13 bytes][band1 13 bytes][band2 13 bytes][9 zero bytes]`, each band = `[filterType][gain f32][frequency f32][quality f32]` where floats are big-endian bytes reversed. Per-model band frequencies from config `B172.customEQ`: low 140 Hz / Q 0.8, peak 980 Hz / Q 0.7, high 6900 Hz / Q 1.0. The web app's value array is `[Bass, Medium, Treble]` but `setCustomEQ_BT` receives `[custom_values[1], custom_values[2], custom_values[0]]` — i.e. wire band order is Mid, Treble, Bass (PEAK, HIGH_SHELF, LOW_SHELF). Custom EQ is out of scope for milestone 1; the dropdown will expose the 7 presets.

### Other things the web app exposes (supported by this model, NOT in the initial core set)

| Feature | Read cmd | Set cmd | Notes |
|---|---|---|---|
| Firmware version | `0xC042` | — | ASCII string payload (`readFirmware`, lines 1057–1064) |
| Low latency mode | `0xC041` | `0xF040` payload `[1 on / 2 off, 0]` | `readLatency` payload[0] 1=on, 2=off |
| Find my buds (ring) | `0xC002` (state) | `0xF002` payload `[0x02 left / 0x03 right, 1/0]` | `readFindState` payload `[count, position, ringing]...` |
| Gestures | `0xC018` | `0xF003` payload `[1, device(2/3), typebutton, type, action]` | `readGesture`: `[count, device, common, type, action]...` |
| Bass enhance | `0xC04E` | `0xF051` payload `[enabled, level]` | B195 uses 3-stage 0/5/10 wire levels; B172 uses level*2 |
| Advanced EQ | `0xC04C`/`0xC04D` | `0xF04F`/`0xF050` | 8-band float packets |
| Spatial audio | `0xC04F` | — | Config `spatialAudio = 6` |
| Ear tip fit test | — | `0xF014` payload `[0x01]` | Result push `0xE00D` |
| Dual connection / device list | `0xC027`, `0xC028` | `0xF042`, `0xF043` | — |
| High quality audio / codec | `0xC04F` | `0xF044` | — |
| Smart knob / dial | see `smart_knob.js` / `smart_dial.js` | — | Config `smartKnob = 1` |
| UTC time sync | — | `0xF032` payload 4-byte big-endian epoch | Sent during init |
| Mimi / Audiodo, Super Mic, call transparency, walkie-talkie, long power, mic mode, anti-leakage, flat EQ, 3rd-driver EQ | various | various | See `FRAME_HANDLERS` at lines 356–394 and the `send(...)` calls at lines 1468–1786 |

Only the core set (battery, ANC mode/strength, in-ear toggle, EQ presets) is planned for the first implementation, plus Find My Buds as a cheap extra if time permits.

## 4. Ambiguities / things we deliberately will not guess

1. **Per-ear in-ear occupancy** is not exposed by the web app; only a single toggle byte. We will surface "In-ear detection: on/off", not left/right wear state.
2. **Exact semantic labels for ANC strengths 1–4** are UI-internal (High / Mid / Low / Adaptive implied by `setAncStrengthHigh/Mid/Low/Adaptive` and `ancLevel=63`). The wire bytes are unambiguous; the labels are inferred from function names only.
3. **Windows COM-port naming** cannot be verified without the hardware. The implementation must discover the port by paired-device name / hardware IDs and allow manual override via settings; the RFCOMM fallback avoids depending on a COM port existing.
4. **Firmware-dependent branches** (`B172` vs `B187` vs `B195`) mostly affect bass EQ and gestures; core commands listed above are shared. We will log the detected base and firmware so real-hardware captures can confirm.

## 5. Debugging plan

The user can test on real hardware, so add a debug view/log:

- `?debug` / `--debug` flag enables `tracing` file logging in `%APPDATA%\cmf-tray\cmf-tray.log`.
- Dump every sent and received frame as hex with timestamps, plus the decoded command ID, op ID, payload length and CRC result.
- The popup's debug panel should show the last N TX/RX frames and raw payload bytes (mirroring ear-web's `?debug` console logging at lines 15–18, 119–122, 324–326).
- Validate incoming frames against the CRC flag and log dropped frames rather than failing silently.
