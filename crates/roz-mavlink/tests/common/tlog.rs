//! Minimal `.tlog` reader for MAVLink compliance fixtures.
//!
//! QGroundControl-style telemetry logs store an 8-byte big-endian timestamp
//! before each MAVLink packet. The helpers here intentionally decode real
//! MAVLink frames instead of comparing hand-authored structs.

use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use mavlink::{
    MAV_STX, MAV_STX_V2, ReadVersion, common::COMMAND_ACK_DATA, common::COMMAND_INT_DATA, common::COMMAND_LONG_DATA,
    common::MavCmd, common::MavMessage, peek_reader::PeekReader, read_versioned_msg,
};

#[derive(Debug, Clone)]
pub struct TlogFrame {
    pub timestamp_usec: u64,
    pub message: MavMessage,
}

pub fn read_tlog(path: &Path) -> Result<Vec<TlogFrame>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    parse_tlog_bytes(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn load_tlog(path: &Path) -> Result<Vec<TlogFrame>> {
    read_tlog(path)
}

pub fn find_command_long(frames: &[TlogFrame], cmd: MavCmd) -> Option<&COMMAND_LONG_DATA> {
    frames.iter().find_map(|frame| match &frame.message {
        MavMessage::COMMAND_LONG(data) if data.command == cmd => Some(data),
        _ => None,
    })
}

pub fn find_command_int(frames: &[TlogFrame], cmd: MavCmd) -> Option<&COMMAND_INT_DATA> {
    frames.iter().find_map(|frame| match &frame.message {
        MavMessage::COMMAND_INT(data) if data.command == cmd => Some(data),
        _ => None,
    })
}

pub fn find_command_ack(frames: &[TlogFrame], cmd: MavCmd) -> Option<&COMMAND_ACK_DATA> {
    frames.iter().find_map(|frame| match &frame.message {
        MavMessage::COMMAND_ACK(data) if data.command == cmd => Some(data),
        _ => None,
    })
}

pub fn command_long_equal(left: &COMMAND_LONG_DATA, right: &COMMAND_LONG_DATA) -> bool {
    same_f32(left.param1, right.param1)
        && same_f32(left.param2, right.param2)
        && same_f32(left.param3, right.param3)
        && same_f32(left.param4, right.param4)
        && same_f32(left.param5, right.param5)
        && same_f32(left.param6, right.param6)
        && same_f32(left.param7, right.param7)
        && left.command == right.command
        && left.target_system == right.target_system
        && left.target_component == right.target_component
        && left.confirmation == right.confirmation
}

pub fn command_long_payload_equal(left: &COMMAND_LONG_DATA, right: &COMMAND_LONG_DATA) -> bool {
    command_long_equal(left, right)
}

pub fn command_int_equal(left: &COMMAND_INT_DATA, right: &COMMAND_INT_DATA) -> bool {
    same_f32(left.param1, right.param1)
        && same_f32(left.param2, right.param2)
        && same_f32(left.param3, right.param3)
        && same_f32(left.param4, right.param4)
        && left.x == right.x
        && left.y == right.y
        && same_f32(left.z, right.z)
        && left.command == right.command
        && left.target_system == right.target_system
        && left.target_component == right.target_component
        && left.frame == right.frame
        && left.current == right.current
        && left.autocontinue == right.autocontinue
}

pub fn command_int_payload_equal(left: &COMMAND_INT_DATA, right: &COMMAND_INT_DATA) -> bool {
    command_int_equal(left, right)
}

fn parse_tlog_bytes(bytes: &[u8]) -> Result<Vec<TlogFrame>> {
    let mut offset = 0;
    let mut frames = Vec::new();

    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            bail!("truncated tlog timestamp at byte offset {offset}");
        }

        let timestamp_usec = u64::from_be_bytes(bytes[offset..offset + 8].try_into()?);
        offset += 8;

        let frame_len = mavlink_frame_len(bytes, offset)?;
        let raw = &bytes[offset..offset + frame_len];
        let mut reader = PeekReader::new(raw);
        let (_header, message) = read_versioned_msg::<MavMessage, _>(&mut reader, ReadVersion::Any)
            .with_context(|| format!("decode MAVLink frame at byte offset {offset}"))?;

        frames.push(TlogFrame {
            timestamp_usec,
            message,
        });
        offset += frame_len;
    }

    Ok(frames)
}

fn mavlink_frame_len(bytes: &[u8], offset: usize) -> Result<usize> {
    let stx = *bytes
        .get(offset)
        .with_context(|| format!("missing MAVLink frame at byte offset {offset}"))?;

    match stx {
        MAV_STX_V2 => {
            if bytes.len() - offset < 10 {
                bail!("truncated MAVLink v2 header at byte offset {offset}");
            }
            let payload_len = usize::from(bytes[offset + 1]);
            let incompat_flags = bytes[offset + 2];
            let signature_len = if incompat_flags & 0x01 == 0 { 0 } else { 13 };
            let frame_len = 1 + 9 + payload_len + 2 + signature_len;
            ensure_frame_available(bytes, offset, frame_len, "v2")?;
            Ok(frame_len)
        }
        MAV_STX => {
            if bytes.len() - offset < 6 {
                bail!("truncated MAVLink v1 header at byte offset {offset}");
            }
            let payload_len = usize::from(bytes[offset + 1]);
            let frame_len = 1 + 5 + payload_len + 2;
            ensure_frame_available(bytes, offset, frame_len, "v1")?;
            Ok(frame_len)
        }
        other => bail!("unexpected MAVLink magic 0x{other:02x} at byte offset {offset}"),
    }
}

fn ensure_frame_available(bytes: &[u8], offset: usize, frame_len: usize, version: &str) -> Result<()> {
    if bytes.len() - offset < frame_len {
        bail!(
            "truncated MAVLink {version} frame at byte offset {offset}: need {frame_len} bytes, have {}",
            bytes.len() - offset
        );
    }
    Ok(())
}

fn same_f32(left: f32, right: f32) -> bool {
    if left.is_nan() && right.is_nan() {
        return true;
    }
    left.to_bits() == right.to_bits()
}
