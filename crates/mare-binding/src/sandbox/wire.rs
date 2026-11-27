//! Bounded, versioned JSON protocol shared only by the trusted runtime processes.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Capability {
    Function { id: usize },
    Table { entries: Vec<(String, Self)> },
    Value { value: Value },
}
#[derive(Serialize, Deserialize)]
pub(super) struct Start {
    pub version: u32,
    pub generation: u64,
    pub persistent: bool,
    pub code: String,
    pub timeout_ms: Option<u64>,
    pub memory_bytes: usize,
    pub max_bytes: usize,
    pub max_message_bytes: usize,
    pub max_output_bytes: usize,
    pub max_pending_calls: usize,
    pub max_instructions: u64,
    pub expose: Capability,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Response {
    Output {
        text: String,
    },
    Call {
        generation: u64,
        call_id: u64,
        id: usize,
        args: Vec<Value>,
    },
    Done {
        value: Option<Value>,
        error: Option<String>,
        #[serde(default)]
        terminated: bool,
    },
}
#[derive(Serialize, Deserialize)]
pub(super) struct Reply {
    pub call_id: u64,
    pub complete: bool,
    pub values: Vec<Value>,
    pub error: Option<String>,
}

const RETAINED_READ_CAPACITY: usize = 64 * 1024;

pub(super) struct ReadBuf(Vec<u8>);

impl ReadBuf {
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }

    fn reset(&mut self) {
        self.0.clear();
        if self.0.capacity() > RETAINED_READ_CAPACITY {
            self.0.shrink_to(RETAINED_READ_CAPACITY);
        }
    }
}

pub(super) async fn write<T: Serialize>(
    out: &mut (impl AsyncWrite + Unpin),
    value: &T,
    max: usize,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > max {
        return Err(std::io::Error::other("sandbox protocol byte limit exceeded"));
    }
    bytes.push(b'\n');
    out.write_all(&bytes).await?;
    out.flush().await
}
pub(super) async fn read_with<T: for<'de> Deserialize<'de>>(
    input: &mut (impl AsyncBufRead + Unpin),
    max: usize,
    buf: &mut ReadBuf,
) -> std::io::Result<T> {
    buf.reset();
    input.take((max + 1) as u64).read_until(b'\n', &mut buf.0).await?;
    if buf.0.len() > max || buf.0.last() != Some(&b'\n') {
        buf.reset();
        return Err(std::io::Error::other("sandbox protocol EOF or byte limit exceeded"));
    }
    let parsed = serde_json::from_slice(&buf.0);
    buf.reset();
    Ok(parsed?)
}
pub(super) async fn read<T: for<'de> Deserialize<'de>>(
    input: &mut (impl AsyncBufRead + Unpin),
    max: usize,
) -> std::io::Result<T> {
    read_with(input, max, &mut ReadBuf::new()).await
}

pub(super) fn output(text: &str, max: usize) -> mlua::Result<()> {
    use std::io::Write;
    #[derive(Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum OutgoingOutput<'a> {
        Output { text: &'a str },
    }
    let mut data = serde_json::to_vec(&OutgoingOutput::Output { text }).map_err(mlua::Error::external)?;
    if data.len() > max {
        return Err(mlua::Error::runtime("sandbox output frame limit exceeded"));
    }
    data.push(b'\n');
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&data).map_err(mlua::Error::external)?;
    stdout.flush().map_err(mlua::Error::external)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum HostFrame {
    Start { request: Start },
    Reply { reply: Reply },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn read_buf_reuses_allocation_and_shrinks_oversized_frame() {
        let small = serde_json::to_string(&Response::Output { text: "ok".into() }).unwrap() + "\n";
        let big_text = "x".repeat(RETAINED_READ_CAPACITY * 2);
        let big = serde_json::to_string(&Response::Output { text: big_text.clone() }).unwrap() + "\n";
        let payload = format!("{small}{big}{small}");
        let mut reader = BufReader::new(payload.as_bytes());
        let mut buf = ReadBuf::new();

        let first: Response = read_with(&mut reader, 1024 * 1024, &mut buf).await.unwrap();
        assert!(matches!(first, Response::Output { text } if text == "ok"));
        assert!(buf.0.capacity() <= RETAINED_READ_CAPACITY);

        let second: Response = read_with(&mut reader, 1024 * 1024, &mut buf).await.unwrap();
        assert!(matches!(second, Response::Output { text } if text == big_text));
        assert!(buf.0.capacity() <= RETAINED_READ_CAPACITY, "oversized capacity must shrink back");

        let third: Response = read_with(&mut reader, 1024 * 1024, &mut buf).await.unwrap();
        assert!(matches!(third, Response::Output { text } if text == "ok"));
    }
}
