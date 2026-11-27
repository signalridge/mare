//! Framing between mare and a sandboxed PTY runner (ConPTY + stdio bridge, Apache-2.0).
//!
//! The confined command can't be attached to a pseudo-terminal that mare owns (the console's
//! ACL doesn't admit the restricted token / sandbox account), so the runner creates the ConPTY
//! itself and bridges it over its stdio: the terminal's output comes back raw on the runner's
//! stdout, and mare writes *frames* to the runner's stdin so input and resizes share one pipe:
//!
//! ```text
//! 0x00 len:u32le bytes…     input for the terminal
//! 0x01 cols:u16le rows:u16le resize
//! ```
//!
//! Pure code, tested on every platform.

const DATA: u8 = 0;
const RESIZE: u8 = 1;
/// A frame longer than this is a protocol error (protects the runner from a bad length).
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Resize(u16, u16),
}

pub fn encode_data(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 5);
    if bytes.is_empty() {
        out.extend_from_slice(&[DATA, 0, 0, 0, 0]);
    }
    for chunk in bytes.chunks(MAX_FRAME) {
        out.push(DATA);
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

pub fn encode_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut out = vec![RESIZE];
    out.extend_from_slice(&cols.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out
}

/// Incremental decoder: feed whatever arrived on the pipe, get the complete frames.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let rest = &self.buf[i..];
            let Some(&kind) = rest.first() else { break };
            match kind {
                DATA => {
                    if rest.len() < 5 {
                        break;
                    }
                    let n = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
                    if n > MAX_FRAME {
                        return Err(format!("pty frame too long ({n} bytes)"));
                    }
                    if rest.len() < 5 + n {
                        break;
                    }
                    out.push(Frame::Data(rest[5..5 + n].to_vec()));
                    i += 5 + n;
                }
                RESIZE => {
                    if rest.len() < 5 {
                        break;
                    }
                    let cols = u16::from_le_bytes([rest[1], rest[2]]);
                    let rows = u16::from_le_bytes([rest[3], rest[4]]);
                    out.push(Frame::Resize(cols.max(1), rows.max(1)));
                    i += 5;
                }
                k => return Err(format!("unknown pty frame type {k}")),
            }
        }
        self.buf.drain(..i);
        Ok(out)
    }
}

/// `--pty` value: `<cols>x<rows>`.
pub fn parse_size(s: &str) -> Option<(u16, u16)> {
    let (c, r) = s.split_once('x')?;
    let (c, r) = (c.parse::<u16>().ok()?, r.parse::<u16>().ok()?);
    (c > 0 && r > 0).then_some((c, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_across_arbitrary_splits() {
        let mut wire = encode_data(b"ls -la\r");
        wire.extend(encode_resize(120, 40));
        wire.extend(encode_data(b""));
        wire.extend(encode_data("é\x1b[A".as_bytes()));
        // every split point must decode to the same frames
        for cut in 0..wire.len() {
            let mut d = Decoder::default();
            let mut got = d.push(&wire[..cut]).unwrap();
            got.extend(d.push(&wire[cut..]).unwrap());
            assert_eq!(
                got,
                vec![
                    Frame::Data(b"ls -la\r".to_vec()),
                    Frame::Resize(120, 40),
                    Frame::Data(vec![]),
                    Frame::Data("é\x1b[A".as_bytes().to_vec()),
                ],
                "split at {cut}"
            );
        }
    }

    #[test]
    fn bad_frames_are_errors() {
        assert!(Decoder::default().push(&[9]).is_err());
        let mut long = vec![DATA];
        long.extend(((MAX_FRAME + 1) as u32).to_le_bytes());
        assert!(Decoder::default().push(&long).is_err());
        assert_eq!(parse_size("80x24"), Some((80, 24)));
        assert_eq!(parse_size("0x24"), None);
        assert_eq!(parse_size("80"), None);
    }
}
