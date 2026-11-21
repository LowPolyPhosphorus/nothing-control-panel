//! Wire protocol for the CMF Buds Pro 2 SPP/RFCOMM control channel.
//!
//! Derived from the `ear-web` reference implementation. This module is pure
//! (no I/O) so every frame shape can be tested against the verified byte
//! fixtures in `NOTES.md` with plain `cargo test`.

pub const SYNC: u8 = 0x55;
/// Outgoing control byte. Bit 5 (`0x20`) means "CRC present"; we always send
/// a CRC. Incoming frames may use `0x40` (no CRC) or `0x60` (CRC).
pub const OUT_CONTROL: u8 = 0x60;
pub const VERSION: u8 = 0x01;
pub const CRC_FLAG: u8 = 0x20;
pub const MAX_PAYLOAD: usize = 4096;

// ---- Command IDs ---------------------------------------------------------
pub const CMD_GET_BATTERY: u16 = 0xC007;
pub const CMD_GET_INEAR: u16 = 0xC00E;
pub const CMD_GET_ANC: u16 = 0xC01E;
pub const CMD_GET_EQ: u16 = 0xC050; // "listening mode" on these models
pub const CMD_GET_FIRMWARE: u16 = 0xC042;

pub const CMD_SET_ANC: u16 = 0xF00F;
pub const CMD_SET_INEAR: u16 = 0xF004;
pub const CMD_SET_EQ: u16 = 0xF01D;
pub const CMD_SET_RING: u16 = 0xF002;

pub const RESP_BATTERY: u16 = 0x4007;
pub const PUSH_BATTERY: u16 = 0xE001;
pub const RESP_INEAR: u16 = 0x400E;
pub const RESP_ANC: u16 = 0x401E;
pub const PUSH_ANC: u16 = 0xE003;
pub const RESP_EQ: u16 = 0x4050;
pub const RESP_FIRMWARE: u16 = 0x4042;

/// ear-web exposes ANC strengths as UI "levels" 3..6 in the odd order
/// low/high/mid/adaptive (wire strengths 3,1,2,4). The read mapping from wire
/// strength 1..4 to UI level is therefore `[4, 5, 3, 6]`. Keep the quirk in
/// this single table so it has one source of truth (and one test).
pub const ANC_STRENGTH_UI_LEVEL: [u8; 4] = [4, 5, 3, 6];

/// CRC-16/Modbus: init 0xFFFF, reflected poly 0xA001, no final XOR.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &byte in bytes {
        crc ^= byte as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Build an outgoing frame. The CRC is calculated over bytes 0..8+N and
/// appended little-endian.
pub fn build_frame(command: u16, op_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.push(SYNC);
    frame.push(OUT_CONTROL);
    frame.push(VERSION);
    frame.extend_from_slice(&command.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    frame.push(op_id);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc16(&frame).to_le_bytes());
    frame
}

// ---- Frame representation ------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub control: u8,
    pub version: u8,
    pub command: u16,
    pub op_id: u8,
    pub payload: Vec<u8>,
    /// `None` when the sender did not include a CRC; otherwise whether the
    /// received CRC matched. Frames with a bad CRC are dropped by the
    /// reassembler and never reach this struct.
    pub crc_valid: Option<bool>,
}

impl Frame {
    /// A set (`0xF0xx`) is acked by `0x70xx` with the same low 15 bits and the
    /// same operation ID. Responses and pushes have bit 15 set.
    pub fn is_ack_for(&self, command: u16, op_id: u8) -> bool {
        self.command & 0x8000 == 0
            && self.command & 0x7FFF == command & 0x7FFF
            && self.op_id == op_id
    }

    pub fn is_push(&self) -> bool {
        self.command & 0x8000 != 0
    }
}

/// Incremental reassembler. Accepts arbitrary byte chunks, resynchronises on
/// `0x55`, buffers partial frames, drops over-long payloads and bad CRCs.
#[derive(Debug, Default)]
pub struct FrameReassembler {
    buf: Vec<u8>,
}

impl FrameReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();

        loop {
            // Discard noise until the next sync byte.
            if let Some(start) = self.buf.iter().position(|&byte| byte == SYNC) {
                if start > 0 {
                    self.buf.drain(..start);
                }
            } else {
                self.buf.clear();
                break;
            }

            // Need at least the fixed header: sync..op_id (8 bytes).
            if self.buf.len() < 8 {
                break;
            }
            let control = self.buf[1];
            let version = self.buf[2];
            let command = u16::from_le_bytes([self.buf[3], self.buf[4]]);
            let length = u16::from_le_bytes([self.buf[5], self.buf[6]]) as usize;
            let op_id = self.buf[7];

            if length > MAX_PAYLOAD {
                // Not a plausible frame length: skip this sync and resync.
                self.buf.drain(..1);
                continue;
            }

            let has_crc = control & CRC_FLAG != 0;
            let total = 8 + length + if has_crc { 2 } else { 0 };
            if self.buf.len() < total {
                break; // wait for the rest of the frame
            }

            let payload = self.buf[8..8 + length].to_vec();
            let crc_valid = if has_crc {
                let expected =
                    u16::from_le_bytes([self.buf[total - 2], self.buf[total - 1]]);
                Some(expected == crc16(&self.buf[..total - 2]))
            } else {
                None
            };
            self.buf.drain(..total);

            if crc_valid == Some(false) {
                continue; // silently drop corrupt frames; caller can log via raw dump
            }
            frames.push(Frame {
                control,
                version,
                command,
                op_id,
                payload,
                crc_valid,
            });
        }

        frames
    }
}

// ---- State decoders ------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatteryValue {
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatteryReading {
    pub left: Option<BatteryValue>,
    pub right: Option<BatteryValue>,
    pub case_battery: Option<BatteryValue>,
    /// 0x06 = single stereo body; the web app treats this as all three.
    pub stereo: Option<BatteryValue>,
}

impl BatteryValue {
    fn from_byte(byte: u8) -> Self {
        Self {
            percent: byte & 0x7F,
            charging: byte & 0x80 != 0,
        }
    }
}

/// Payload: `[count, (id, byte) * count]`.
/// IDs: 0x02 left, 0x03 right, 0x04 case, 0x06 stereo.
pub fn parse_battery(payload: &[u8]) -> Option<BatteryReading> {
    let count = *payload.first()? as usize;
    if payload.len() < 1 + count * 2 {
        return None;
    }
    let mut reading = BatteryReading::default();
    for index in 0..count {
        let id = payload[1 + index * 2];
        let value = BatteryValue::from_byte(payload[2 + index * 2]);
        match id {
            0x02 => reading.left = Some(value),
            0x03 => reading.right = Some(value),
            0x04 => reading.case_battery = Some(value),
            0x06 => reading.stereo = Some(value),
            _ => {} // unknown battery id: ignore, raw frame is logged separately
        }
    }
    Some(reading)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AncMode {
    Off,
    Transparency,
    Anc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AncStatus {
    pub mode: AncMode,
    /// Raw wire strength 1..4 (High, Mid, Low, Adaptive respectively, per
    /// the reference app's function names).
    pub strength: u8,
}

/// Response payload is a sequence of 3-byte records `[type, value, unused]`:
/// type 0x01 = mode, type 0x02 = strength.
pub fn parse_anc(payload: &[u8]) -> Option<AncStatus> {
    let mut raw_mode = None;
    let mut raw_strength = None;
    let mut index = 0;
    while index + 2 < payload.len() {
        match payload[index] {
            0x01 => raw_mode = Some(payload[index + 1]),
            0x02 => raw_strength = Some(payload[index + 1]),
            _ => {}
        }
        index += 3;
    }

    let value = raw_mode?;
    let (mode, derived_strength) = match value {
        1..=4 => (AncMode::Anc, value),
        0 | 5 => (AncMode::Off, 1),
        7 | 0xFE => (AncMode::Transparency, 1),
        _ => return None,
    };
    let strength = raw_strength
        .map(|value| value.clamp(1, 4))
        .unwrap_or(derived_strength);
    Some(AncStatus { mode, strength })
}

/// Wire byte used by `0xF00F`: off=0x05, transparency=0x07, ANC=strength 1..4.
pub fn anc_wire_byte(mode: AncMode, strength: u8) -> u8 {
    match mode {
        AncMode::Off => 0x05,
        AncMode::Transparency => 0x07,
        AncMode::Anc => strength.clamp(1, 4),
    }
}

pub fn anc_ui_level(strength: u8) -> u8 {
    ANC_STRENGTH_UI_LEVEL
        .get(strength.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(ANC_STRENGTH_UI_LEVEL[0])
}

#[allow(dead_code)] // kept as the inverse of the table for future UI-level input
pub fn anc_strength_from_ui_level(level: u8) -> Option<u8> {
    ANC_STRENGTH_UI_LEVEL
        .iter()
        .position(|&candidate| candidate == level)
        .map(|index| index as u8 + 1)
}

/// EQ response payload: first byte is preset 0..6.
pub fn parse_eq(payload: &[u8]) -> Option<u8> {
    let preset = *payload.first()?;
    (preset <= 6).then_some(preset)
}

/// In-ear toggle response: `payload[2] == 1` means enabled.
pub fn parse_in_ear(payload: &[u8]) -> Option<bool> {
    Some(*payload.get(2)? == 1)
}

pub fn parse_firmware(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload)
        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
        .to_string()
}

// ---- Command builders ----------------------------------------------------

pub fn get_battery(op_id: u8) -> Vec<u8> {
    build_frame(CMD_GET_BATTERY, op_id, &[])
}
pub fn get_anc(op_id: u8) -> Vec<u8> {
    build_frame(CMD_GET_ANC, op_id, &[])
}
pub fn get_ear(op_id: u8) -> Vec<u8> {
    build_frame(CMD_GET_INEAR, op_id, &[])
}
pub fn get_eq(op_id: u8) -> Vec<u8> {
    build_frame(CMD_GET_EQ, op_id, &[])
}
pub fn get_firmware(op_id: u8) -> Vec<u8> {
    build_frame(CMD_GET_FIRMWARE, op_id, &[])
}
pub fn set_anc(op_id: u8, mode: AncMode, strength: u8) -> Vec<u8> {
    build_frame(
        CMD_SET_ANC,
        op_id,
        &[0x01, anc_wire_byte(mode, strength), 0x00],
    )
}
pub fn set_eq(op_id: u8, preset: u8) -> Vec<u8> {
    build_frame(CMD_SET_EQ, op_id, &[preset, 0x00])
}
pub fn set_in_ear(op_id: u8, enabled: bool) -> Vec<u8> {
    build_frame(
        CMD_SET_INEAR,
        op_id,
        &[0x01, 0x01, if enabled { 1 } else { 0 }],
    )
}
#[allow(dead_code)] // Find My Buds is a planned extra, not part of the core set
pub fn set_ring(op_id: u8, side: u8, enabled: bool) -> Vec<u8> {
    build_frame(
        CMD_SET_RING,
        op_id,
        &[side, if enabled { 1 } else { 0 }],
    )
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        let _ = write!(text, "{byte:02X} ");
    }
    text.trim_end().to_string()
}

pub fn command_name(command: u16) -> &'static str {
    match command {
        CMD_GET_BATTERY | RESP_BATTERY | PUSH_BATTERY => "battery",
        CMD_GET_ANC | RESP_ANC | PUSH_ANC => "anc",
        CMD_GET_INEAR | RESP_INEAR => "in-ear",
        CMD_GET_EQ | RESP_EQ => "eq/listening-mode",
        CMD_GET_FIRMWARE | RESP_FIRMWARE => "firmware",
        CMD_SET_ANC => "set-anc",
        CMD_SET_INEAR => "set-in-ear",
        CMD_SET_EQ => "set-eq",
        CMD_SET_RING => "set-ring",
        command if (0x7000..=0x70FF).contains(&(command & 0x7FFF)) => "set-ack",
        _ => "unknown",
    }
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_hex(text: &str) -> Vec<u8> {
        text.split_whitespace()
            .map(|part| u8::from_str_radix(part, 16).expect("valid hex byte"))
            .collect()
    }

    #[test]
    fn crc16_matches_reference_vectors() {
        // The CRC of a frame is the last two bytes, little-endian.
        let frame = parse_hex("55 60 01 07 C0 00 00 01 AC DF");
        assert_eq!(crc16(&frame[..8]), 0xDFAC);
        assert_eq!(frame[8..], [0xAC, 0xDF]);
    }

    #[test]
    fn build_frame_matches_verified_fixtures() {
        let cases: &[(Vec<u8>, &str)] = &[
            (
                get_battery(1),
                "55 60 01 07 C0 00 00 01 AC DF",
            ),
            (get_anc(2), "55 60 01 1E C0 00 00 02 F1 1C"),
            (get_eq(3), "55 60 01 50 C0 00 00 03 58 D2"),
            (get_ear(4), "55 60 01 0E C0 00 00 04 B0 DD"),
            (get_firmware(10), "55 60 01 42 C0 00 00 0A 20 D7"),
            (
                set_anc(5, AncMode::Off, 1),
                "55 60 01 0F F0 03 00 05 01 05 00 FA 27",
            ),
            (
                set_anc(6, AncMode::Transparency, 1),
                "55 60 01 0F F0 03 00 06 01 07 00 FB 03",
            ),
            (
                set_anc(7, AncMode::Anc, 1),
                "55 60 01 0F F0 03 00 07 01 01 00 F9 5F",
            ),
            (set_eq(8, 1), "55 60 01 1D F0 02 00 08 01 00 2A FB"),
            (set_ring(9, 2, true), "55 60 01 02 F0 02 00 09 02 01 54 CA"),
        ];
        for (built, expected) in cases {
            assert_eq!(hex(built), *expected, "frame mismatch for {expected}");
        }
    }

    #[test]
    fn reassembler_handles_partial_frames_and_resync() {
        let fixture = parse_hex("55 60 01 07 C0 00 00 01 AC DF");
        let mut reassembler = FrameReassembler::new();
        assert!(reassembler.push(&[0x00, 0xFF]).is_empty());
        assert!(reassembler.push(&fixture[..5]).is_empty());
        let frames = reassembler.push(&fixture[5..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].command, CMD_GET_BATTERY);
        assert_eq!(frames[0].op_id, 1);
        assert!(frames[0].payload.is_empty());
        assert_eq!(frames[0].crc_valid, Some(true));
    }

    #[test]
    fn reassembler_accepts_frames_without_crc_and_drops_bad_crc() {
        let no_crc = parse_hex("55 40 01 07 C0 00 00 01");
        let mut reassembler = FrameReassembler::new();
        let frames = reassembler.push(&no_crc);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].control, 0x40);
        assert_eq!(frames[0].crc_valid, None);

        let mut corrupt = parse_hex("55 60 01 07 C0 00 00 01 AC DF");
        corrupt[8] ^= 0xFF;
        let mut reassembler = FrameReassembler::new();
        assert!(reassembler.push(&corrupt).is_empty());
    }

    #[test]
    fn ack_matching_ignores_high_bit_and_requires_op_id() {
        let mut ack = Frame {
            control: 0x40,
            version: 1,
            command: 0x700F,
            op_id: 7,
            payload: vec![],
            crc_valid: None,
        };
        assert!(ack.is_ack_for(CMD_SET_ANC, 7));
        assert!(!ack.is_ack_for(CMD_SET_ANC, 8));
        ack.command = 0xF00F;
        assert!(!ack.is_ack_for(CMD_SET_ANC, 7));
    }

    #[test]
    fn parses_battery_payload() {
        let reading = parse_battery(&[3, 0x02, 87, 0x03, 0xF2, 0x04, 45]).unwrap();
        assert_eq!(reading.left.unwrap().percent, 87);
        assert!(!reading.left.unwrap().charging);
        assert_eq!(reading.right.unwrap().percent, 114);
        assert!(reading.right.unwrap().charging);
        assert_eq!(reading.case_battery.unwrap().percent, 45);
        assert!(parse_battery(&[1, 0x02]).is_none());
    }

    #[test]
    fn parses_anc_and_applies_odd_ui_level_table() {
        let status = parse_anc(&[0x01, 0x01, 0x00, 0x02, 0x03, 0x00]).unwrap();
        assert_eq!(status.mode, AncMode::Anc);
        assert_eq!(status.strength, 3);
        assert_eq!(parse_anc(&[0x01, 0x05, 0x00]).unwrap().mode, AncMode::Off);
        assert_eq!(
            parse_anc(&[0x01, 0x07, 0x00]).unwrap().mode,
            AncMode::Transparency
        );
        assert_eq!(anc_ui_level(1), 4);
        assert_eq!(anc_ui_level(2), 5);
        assert_eq!(anc_ui_level(3), 3);
        assert_eq!(anc_ui_level(4), 6);
        for strength in 1..=4 {
            assert_eq!(anc_strength_from_ui_level(anc_ui_level(strength)), Some(strength));
        }
        assert_eq!(anc_wire_byte(AncMode::Anc, 4), 0x04);
    }

    #[test]
    fn parses_eq_inear_and_firmware() {
        assert_eq!(parse_eq(&[6, 0]), Some(6));
        assert_eq!(parse_eq(&[7, 0]), None);
        assert_eq!(parse_in_ear(&[0, 1, 1]), Some(true));
        assert_eq!(parse_in_ear(&[0, 1, 0]), Some(false));
        assert_eq!(parse_in_ear(&[0, 1]), None);
        assert_eq!(parse_firmware(b"1.2.3\0"), "1.2.3");
    }
}
