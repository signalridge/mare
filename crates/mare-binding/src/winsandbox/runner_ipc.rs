#![cfg_attr(not(windows), allow(dead_code))]
//! Framing for the enhanced account runner, modified for mare's stdio bridge.
//! Copyright 2025 OpenAI (Apache-2.0).

use std::io::{Read, Write};

pub(super) const MAX_FRAME: usize = 16 * 1024 * 1024;
pub(super) const VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct SpawnRequest {
    pub version: u32,
    pub args: Vec<String>,
    // Preserve Windows environment, including non-Unicode UTF-16 code units.
    pub env: Vec<(Vec<u16>, Vec<u16>)>,
}

pub(super) fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> std::io::Result<()> {
    if data.len() > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "runner frame too large"));
    }
    w.write_all(&[kind])?;
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    w.write_all(data)
}

pub(super) fn read_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut kind = [0];
    r.read_exact(&mut kind)?;
    let mut size = [0; 4];
    r.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    if size > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "runner frame too large"));
    }
    let mut data = vec![0; size];
    r.read_exact(&mut data)?;
    Ok((kind[0], data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_request_and_lossless_environment_roundtrip() {
        let request = SpawnRequest {
            version: VERSION,
            args: vec!["powershell.exe".into(), "x世界".repeat(4096)],
            env: vec![(vec![65], vec![0xd800, 0x4e16, 0x754c])],
        };
        let payload = serde_json::to_vec(&request).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, 0, &payload).unwrap();
        let (kind, data) = read_frame(&mut wire.as_slice()).unwrap();
        assert_eq!(kind, 0);
        let decoded: SpawnRequest = serde_json::from_slice(&data).unwrap();
        assert_eq!(decoded.args, request.args);
        assert_eq!(decoded.env, request.env);
    }

    #[test]
    fn oversized_and_truncated_requests_are_rejected() {
        let mut wire = vec![0];
        wire.extend_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        assert_eq!(read_frame(&mut wire.as_slice()).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert!(read_frame(&mut &[0, 3, 0, 0, 0, 1][..]).is_err());
    }
}
