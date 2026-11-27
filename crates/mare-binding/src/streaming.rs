//! Incremental byte buffers and event coalescing for streamed output.
//!
//! The coalescer keeps the event-driven 50 ms behavior of `agent/loop.lua`: a caller flushes
//! a changed key before appending the new event, and samples the clock again when it flushes.
//! It does not schedule a timer or dispatch events; those stay with the caller.

/// A growable Lua-compatible byte string. Snapshots borrow the existing bytes; materializing a
/// Lua string or taking the bytes is left to the caller at the point where they are needed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextBuffer(Vec<u8>);

impl mlua::UserData for TextBuffer {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("append", |_, this, bytes: mlua::LuaString| {
            this.append(bytes.as_bytes().as_ref());
            Ok(())
        });
        methods.add_method("snapshot", |lua, this, ()| lua.create_string(this.as_bytes()));
        methods.add_method("tail", |lua, this, (max_lines, max_bytes): (usize, usize)| {
            lua.create_string(this.tail(max_lines, max_bytes))
        });
        methods.add_method_mut("clear", |_, this, ()| {
            this.clear();
            Ok(())
        });
        methods.add_method("len", |_, this, ()| Ok(this.len()));
    }
}

/// Public VM-local stream values; providers decide when to materialize complete strings.
pub fn install(lua: &mlua::Lua, mare: &mlua::Table) -> mlua::Result<()> {
    let stream = lua.create_table()?;
    stream.set("buffer", lua.create_function(|_, ()| Ok(TextBuffer::new()))?)?;
    stream.set("coalescer", lua.create_function(|_, ()| Ok(LuaCoalescer::default()))?)?;
    mare.set("stream", stream)
}

impl TextBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }

    /// Return a bounded UTF-8-safe tail slice covering at most `max_lines` lines within the last
    /// `max_bytes` bytes of the buffer. Runs in O(min(len, max_bytes)) time.
    pub fn tail(&self, max_lines: usize, max_bytes: usize) -> &[u8] {
        if max_lines == 0 || max_bytes == 0 || self.0.is_empty() {
            return &[];
        }
        let truncated = self.0.len() > max_bytes;
        let window = if truncated { &self.0[self.0.len() - max_bytes..] } else { &self.0[..] };
        let scan_end = if window.last() == Some(&b'\n') { window.len() - 1 } else { window.len() };
        let mut lines = 0usize;
        let mut start = 0usize;
        for i in (0..scan_end).rev() {
            if window[i] == b'\n' {
                lines += 1;
                if lines >= max_lines {
                    start = i + 1;
                    break;
                }
            }
        }
        if start == 0 && truncated {
            // Avoid splitting a multi-byte UTF-8 sequence when sliced mid-buffer.
            while start < window.len() && (window[start] & 0xC0) == 0x80 {
                start += 1;
            }
        }
        &window[start..]
    }
}

/// The only event kinds combined by the current agent sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
    ToolCall,
}

/// One combined delta. An ID is optional independently of the event kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta<Id> {
    pub kind: DeltaKind,
    pub id: Option<Id>,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
struct Pending<Id> {
    kind: DeltaKind,
    id: Option<Id>,
    bytes: TextBuffer,
}

/// A delta belongs to another key and was not appended. The caller must flush the old key,
/// synchronously dispatch it, then retry with the returned ID and the same kind and bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct KeyChanged<Id> {
    pub id: Option<Id>,
}

/// At most one pending `(kind, id)`; no clocks, timers or callbacks are owned by this value.
///
/// Caller protocol for a delta:
/// 1. `append`; on `KeyChanged`, `flush`, dispatch it, and retry `append`.
/// 2. Call `due` with a fresh clock sample. If true, `flush` using another sample and dispatch.
///
/// For a non-delta, flush and dispatch any pending delta before dispatching the original event.
/// Flush once more when the stream returns, including on cancellation or errors as appropriate
/// for the caller. Never hold a mutable userdata borrow while invoking an external callback.
#[derive(Debug)]
pub struct DeltaCoalescer<Id> {
    pending: Option<Pending<Id>>,
    last_emit_ms: f64,
}

impl<Id> Default for DeltaCoalescer<Id> {
    fn default() -> Self {
        Self { pending: None, last_emit_ms: 0.0 }
    }
}

impl<Id: PartialEq> DeltaCoalescer<Id> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes if the key matches, otherwise return the unconsumed ID without mutation.
    pub fn append(&mut self, kind: DeltaKind, id: Option<Id>, bytes: &[u8]) -> Result<(), KeyChanged<Id>> {
        if self.pending.as_ref().is_some_and(|p| p.kind != kind || p.id != id) {
            return Err(KeyChanged { id });
        }
        let p = self.pending.get_or_insert_with(|| Pending { kind, id, bytes: TextBuffer::new() });
        p.bytes.append(bytes);
        Ok(())
    }

    /// Called after an append with the current monotonic time, not by a background timer.
    pub fn due(&self, now_ms: f64) -> bool {
        self.pending.is_some() && now_ms - self.last_emit_ms >= 50.0
    }

    /// Remove the pending delta, updating the last emit time before the caller dispatches it.
    /// The clock is not sampled when no delta is pending.
    pub fn flush(&mut self, clock: impl FnOnce() -> f64) -> Option<Delta<Id>> {
        let delta = self.take_pending()?;
        self.last_emit_ms = clock();
        Some(delta)
    }

    /// Transfer pending data without emitting or sampling the clock so the caller can dispatch it.
    pub fn take_pending(&mut self) -> Option<Delta<Id>> {
        let mut p = self.pending.take()?;
        Some(Delta { kind: p.kind, id: p.id, bytes: p.bytes.take() })
    }

    /// Match the existing Lua sink after a change-triggered flush: a synchronous listener may
    /// have installed a new pending event while the flush was dispatched.
    pub fn append_after_flush(&mut self, kind: DeltaKind, id: Option<Id>, bytes: &[u8]) {
        if let Some(p) = &mut self.pending {
            p.bytes.append(bytes);
        } else {
            self.pending = Some(Pending { kind, id, bytes: TextBuffer::new() });
            self.pending.as_mut().unwrap().bytes.append(bytes);
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Record a clock sampled by the caller after it has taken a pending event.
    pub fn mark_emitted(&mut self, now_ms: f64) {
        self.last_emit_ms = now_ms;
    }
}

/// Lua-facing coalescer for the declared string-or-nil stream-event ID contract. The caller
/// dispatches callbacks itself so no mutable userdata borrow is held during listener execution.
#[derive(Default)]
struct LuaCoalescer(DeltaCoalescer<Vec<u8>>);

fn lua_delta(lua: &mlua::Lua, delta: Delta<Vec<u8>>) -> mlua::Result<mlua::Table> {
    let event = lua.create_table()?;
    event.set(
        "type",
        match delta.kind {
            DeltaKind::Text => "text_delta",
            DeltaKind::Reasoning => "reasoning_delta",
            DeltaKind::ToolCall => "tool_call_delta",
        },
    )?;
    if let Some(id) = delta.id {
        event.set("tool_call_id", lua.create_string(id)?)?;
    }
    event.set("delta", lua.create_string(delta.bytes)?)?;
    Ok(event)
}

fn lua_kind(kind: &str) -> mlua::Result<DeltaKind> {
    match kind {
        "text_delta" => Ok(DeltaKind::Text),
        "reasoning_delta" => Ok(DeltaKind::Reasoning),
        "tool_call_delta" => Ok(DeltaKind::ToolCall),
        _ => Err(mlua::Error::runtime("unsupported coalesced delta type")),
    }
}

impl mlua::UserData for LuaCoalescer {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut(
            "append",
            |_, this, (kind, id, bytes): (String, Option<mlua::LuaString>, mlua::LuaString)| {
                let kind = lua_kind(&kind)?;
                let id = id.map(|s| s.as_bytes().to_vec());
                Ok(this.0.append(kind, id, bytes.as_bytes().as_ref()).is_ok())
            },
        );
        methods.add_method_mut(
            "append_after_flush",
            |_, this, (kind, id, bytes): (String, Option<mlua::LuaString>, mlua::LuaString)| {
                this.0.append_after_flush(
                    lua_kind(&kind)?,
                    id.map(|s| s.as_bytes().to_vec()),
                    bytes.as_bytes().as_ref(),
                );
                Ok(())
            },
        );
        methods.add_method("has_pending", |_, this, ()| Ok(this.0.has_pending()));
        methods.add_method("due", |_, this, now_ms: f64| Ok(this.0.due(now_ms)));
        methods.add_method_mut("mark_emitted", |_, this, now_ms: f64| {
            this.0.mark_emitted(now_ms);
            Ok(())
        });
        methods.add_method_mut("flush", |lua, this, now_ms: f64| {
            let Some(delta) = this.0.flush(|| now_ms) else { return Ok(None) };
            Ok(Some(lua_delta(lua, delta)?))
        });
        methods.add_method_mut("take_pending", |lua, this, ()| {
            this.0.take_pending().map(|delta| lua_delta(lua, delta)).transpose()
        });
    }
}

#[cfg(test)]
mod lua_tests;
#[cfg(test)]
mod tests;
