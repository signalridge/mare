//! Incremental `text/event-stream` decoder (WHATWG HTML, "Interpreting an event stream").
//!
//! The decoder core (`parser`, `parser/scan.rs`, `parser/data.rs`) and the stream adapter
//! ([`Blocks`]) are ported from sse-stream 0.3.0 (MIT OR Apache-2.0):
//! one linear memchr scan over each newly arrived chunk, data values appended straight into the
//! data buffer, at most 64 KiB of scratch capacity kept after an event. mare adds:
//! - caps on one line and on the bytes one event accumulates ([`Caps`], 16 MiB each by default):
//!   upstream's 64 KiB only bounds retained scratch space, not an event in flight, and the idle
//!   watchdog can't end a stream whose endless line keeps sending bytes;
//! - an `on_comment` callback (heartbeats feed the idle watchdog);
//! - [`dispatch`]: the spec's last-event-ID buffer and "a block without data dispatches nothing"
//!   on top of the raw blocks the parser returns.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use futures_util::{Stream, TryStream};

mod parser;
use parser::Parser;

#[cfg(test)]
thread_local! {
    /// Test seam: bytes handed to the line-end scans on this thread. The linear-time regression
    /// test counts this instead of timing the decoder, so machine load can't flip it.
    static SCANNED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count `n` bytes given to a line-end scan (test builds only).
#[inline]
fn note_scanned(n: usize) {
    #[cfg(test)]
    SCANNED.with(|c| c.set(c.get() + n as u64));
    #[cfg(not(test))]
    let _ = n;
}

/// A raw SSE event block as the parser returns it: fields are not inherited from earlier blocks
/// and metadata-only blocks are returned too (sse-stream `Sse`).
#[derive(Default, Debug, PartialEq, Eq, Hash, Clone)]
pub struct Sse {
    /// explicit event type
    pub event: Option<String>,
    /// data lines joined with LF; `None` and an empty data field are distinct
    pub data: Option<String>,
    /// explicit id (a value containing NUL is ignored)
    pub id: Option<String>,
    /// explicit reconnection delay, in milliseconds (ASCII digits only)
    pub retry: Option<u64>,
}

#[cfg(test)]
impl Sse {
    fn event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }
    fn data(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }
    fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    fn retry(mut self, retry: u64) -> Self {
        self.retry = Some(retry);
        self
    }
}

/// A terminal decoding error: after it the stream ends.
#[derive(Debug)]
pub enum Error {
    /// the underlying byte stream failed
    Body(Box<dyn std::error::Error + Send + Sync>),
    /// invalid UTF-8 in a recognized field value
    Utf8Parse(std::str::Utf8Error),
    LineTooLong(usize),
    EventTooLarge(usize),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Body(e) => write!(f, "body error: {e}"),
            Self::Utf8Parse(e) => write!(f, "utf8 parse error: {e}"),
            Self::LineTooLong(n) => write!(f, "an SSE line is longer than {n} bytes"),
            Self::EventTooLarge(n) => write!(f, "an SSE event is larger than {n} bytes"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Body(e) => Some(&**e),
            Self::Utf8Parse(e) => Some(e),
            _ => None,
        }
    }
}

/// Size caps (mare).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// bytes in one line, without its line ending
    pub line: usize,
    /// bytes in one event's lines (comments excluded) up to the blank line that ends it
    pub event: usize,
}

pub const DEFAULT_CAP: usize = 16 * 1024 * 1024;

impl Default for Caps {
    fn default() -> Self {
        Self { line: DEFAULT_CAP, event: DEFAULT_CAP }
    }
}

/// Counts line and event sizes ahead of the parser, with the same line endings (CR, LF, CRLF,
/// also split across chunks); linear in the input.
#[derive(Default)]
struct CapCounter {
    caps: Caps,
    line: usize,
    event: usize,
    after_cr: bool,
    comment: bool,
}

impl CapCounter {
    /// `Err((offset, error))`: the bytes before `offset` are within the caps.
    fn scan(&mut self, b: &[u8]) -> Result<(), (usize, Error)> {
        let mut pos = 0;
        while pos < b.len() {
            if std::mem::take(&mut self.after_cr) && b[pos] == b'\n' {
                pos += 1;
                continue;
            }
            if self.line == 0 {
                self.comment = b[pos] == b':';
            }
            note_scanned(b.len() - pos);
            let end = memchr::memchr2(b'\n', b'\r', &b[pos..]);
            let n = end.unwrap_or(b.len() - pos);
            if self.line + n > self.caps.line {
                return Err((pos, Error::LineTooLong(self.caps.line)));
            }
            if !self.comment {
                if self.event + n > self.caps.event {
                    return Err((pos, Error::EventTooLarge(self.caps.event)));
                }
                self.event += n;
            }
            self.line += n;
            let Some(i) = end else { break };
            if self.line == 0 {
                self.event = 0; // a blank line ends the event
            }
            self.line = 0;
            self.after_cr = b[pos + i] == b'\r';
            pos += i + 1;
        }
        Ok(())
    }
}

/// Called with each comment line's text (heartbeats feed the idle watchdog).
pub type OnComment = Box<dyn FnMut(&[u8]) + Send>;

/// Raw SSE blocks decoded from a stream of byte buffers (sse-stream `SseByteStream`, plus the
/// caps and the comment callback). Input buffers may hold several events or pieces of one. A
/// decoding error (events completed before it come first) or a body error is returned once,
/// then the stream ends; EOF discards an unfinished final block.
pub struct Blocks<S> {
    stream: S,
    pending_input: Option<Bytes>,
    parser: Parser,
    caps: CapCounter,
    /// an over-cap error, returned once the input before it is parsed
    over_cap: Option<Error>,
    on_comment: OnComment,
    finished: bool,
}

impl<S> Blocks<S> {
    pub fn new(stream: S, caps: Caps, on_comment: OnComment) -> Self {
        Self {
            stream,
            pending_input: None,
            parser: Parser::default(),
            caps: CapCounter { caps, ..Default::default() },
            over_cap: None,
            on_comment,
            finished: false,
        }
    }

    pub fn is_terminated(&self) -> bool {
        self.finished
    }
}

impl<S> Stream for Blocks<S>
where
    S: TryStream + Unpin,
    S::Ok: Buf,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<Sse, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.finished {
            return Poll::Ready(None);
        }
        let error = loop {
            if let Some(data) = this.pending_input.as_mut() {
                match this.parser.parse_buf(data, &mut *this.on_comment) {
                    Ok(Some(event)) => {
                        if !data.has_remaining() {
                            this.pending_input = None;
                        }
                        return Poll::Ready(Some(Ok(event)));
                    }
                    Ok(None) => this.pending_input = None,
                    Err(error) => break Some(error),
                }
            }
            if let Some(error) = this.over_cap.take() {
                break Some(error);
            }
            match ready!(Pin::new(&mut this.stream).try_poll_next(cx)) {
                Some(Ok(mut data)) => {
                    // one contiguous buffer (zero-copy for `Bytes`), so the caps can look ahead
                    let mut data = data.copy_to_bytes(data.remaining());
                    if let Err((at, error)) = this.caps.scan(&data) {
                        data.truncate(at);
                        this.over_cap = Some(error);
                    }
                    this.pending_input = Some(data);
                }
                Some(Err(error)) => break Some(Error::Body(Box::new(error))),
                None => break None,
            }
        };
        this.finished = true;
        this.pending_input = None;
        this.parser = Parser::default();
        Poll::Ready(error.map(Err))
    }
}

/// A dispatched event (WHATWG "dispatch the event").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// `message` when the block had no (or an empty) `event:` field
    pub event: String,
    pub data: String,
    /// the last event ID buffer: the latest `id:` of this or an earlier block
    pub id: String,
}

/// Apply one raw block to the last event ID buffer and turn it into an event, unless it has no
/// data (then nothing is dispatched; its `id:` still counts).
pub fn dispatch(last_id: &mut String, block: Sse) -> Option<Event> {
    if let Some(id) = block.id {
        *last_id = id;
    }
    let data = block.data?;
    let event = block.event.filter(|e| !e.is_empty()).unwrap_or_else(|| "message".to_string());
    Some(Event { event, data, id: last_id.clone() })
}

#[cfg(test)]
mod tests;
