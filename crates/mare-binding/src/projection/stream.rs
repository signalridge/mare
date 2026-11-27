//! Independent bounded literal projection. Retain max(value UTF-8 length)-1 bytes so no
//! incomplete known value can start in the emitted prefix. Never operates on encoded JSON.
use super::{Registry, State, error, snapshot_if_nonempty, state};
use mlua::{Lua, Table, UserData, UserDataMethods};
use std::sync::{Arc, Mutex};

pub(super) struct ByteStream {
    registry: Arc<Mutex<Registry>>,
    pending: Vec<u8>,
}

impl ByteStream {
    pub(super) fn new(registry: Arc<Mutex<Registry>>) -> Self {
        Self { registry, pending: Vec::new() }
    }

    fn take(&mut self, finish: bool) -> Vec<u8> {
        let Some(snapshot) = snapshot_if_nonempty(&self.registry) else {
            return std::mem::take(&mut self.pending);
        };
        let longest = snapshot.entries.iter().map(|e| e.value.len()).max().unwrap_or(1);
        let raw_limit =
            if finish { self.pending.len() } else { self.pending.len().saturating_sub(longest.saturating_sub(1)) };
        let mut pos = 0usize;
        let mut consumed = 0usize;
        let mut out = Vec::new();
        for chunk in self.pending.utf8_chunks() {
            if pos >= raw_limit {
                break;
            }
            let valid = chunk.valid();
            let invalid = chunk.invalid();
            if !valid.is_empty() {
                let valid_end = pos + valid.len();
                if valid_end > raw_limit {
                    let mut local_cut = raw_limit - pos;
                    while !valid.is_char_boundary(local_cut) {
                        local_cut -= 1;
                    }
                    let (projected, used) = snapshot.project_prefix(valid, local_cut);
                    out.extend_from_slice(projected.as_bytes());
                    consumed = pos + used;
                    break;
                }
                let (projected, used) = snapshot.project_prefix(valid, valid.len());
                out.extend_from_slice(projected.as_bytes());
                consumed = pos + used;
            }
            let pos_inv = pos + valid.len();
            if !invalid.is_empty() {
                let at_end = pos_inv + invalid.len() == self.pending.len();
                let truncated_utf8 =
                    !finish && at_end && std::str::from_utf8(invalid).is_err_and(|e| e.error_len().is_none());
                if truncated_utf8 || pos_inv >= raw_limit {
                    break;
                }
                out.extend_from_slice(invalid);
                consumed = pos_inv + invalid.len();
            }
            pos = pos_inv + invalid.len();
        }
        self.pending.drain(..consumed);
        out
    }
}

impl mare_shared::wire_projection::ByteStreamProjector for ByteStream {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        self.take(false)
    }

    fn finish(&mut self) -> Vec<u8> {
        self.take(true)
    }
}

struct Stream {
    state: State,
    snapshot: Registry,
    pending: String,
    longest: usize,
    finished: bool,
}
impl Stream {
    fn new(state: State) -> mlua::Result<Self> {
        let snapshot = state.0.lock().map_err(|_| error("projection registry unavailable"))?.clone();
        let longest = snapshot.entries.iter().map(|e| e.value.len()).max().unwrap_or(1);
        Ok(Self { state, snapshot, pending: String::new(), longest, finished: false })
    }
    fn take(&mut self, finish: bool) -> mlua::Result<String> {
        if self.state.0.lock().map_err(|_| error("projection registry unavailable"))?.revision != self.snapshot.revision
        {
            self.pending.clear();
            self.finished = true;
            return Err(error("PROJECTION_CHANGED: registrations changed during streaming"));
        }
        let mut cut =
            if finish { self.pending.len() } else { self.pending.len().saturating_sub(self.longest.saturating_sub(1)) };
        while !self.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        let (out, consumed) = self.snapshot.project_prefix(&self.pending, cut);
        self.pending.drain(..consumed);
        Ok(out)
    }
    fn push(&mut self, chunk: String) -> mlua::Result<String> {
        if self.finished {
            return Err(error("PROJECTION_CLOSED: stream already closed"));
        }
        self.pending.push_str(&chunk);
        self.take(false)
    }
    fn finish(&mut self) -> mlua::Result<String> {
        if self.finished {
            return Ok(String::new());
        }
        let out = self.take(true)?;
        self.finished = true;
        Ok(out)
    }
}
impl UserData for Stream {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method_mut("push", |_, this, chunk: String| this.push(chunk));
        m.add_method_mut("finish", |_, this, ()| this.finish());
        m.add_method_mut("close", |_, this, ()| {
            this.pending.clear();
            this.finished = true;
            Ok(())
        });
    }
}
pub(super) fn install(lua: &Lua, api: &Table) -> mlua::Result<()> {
    api.set("stream", lua.create_function(|lua, ()| Stream::new(state(lua)))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_utf8_boundary_matches_whole_projection_and_stays_bounded() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        super::super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            "keep=mare.projection.register('FAKE_秘密_secret');other=mare.projection.register('FAKE_秘密_secret_long')",
        )
        .exec()
        .unwrap();
        let state = state(&lua);
        let input = "long harmless prefix FAKE_秘密_secret_long / FAKE_秘密_secret / trailing text";
        let expected = state.0.lock().unwrap().project(input);
        for split in input.char_indices().map(|(i, _)| i).chain(std::iter::once(input.len())) {
            let mut stream = Stream::new(state.clone()).unwrap();
            let mut out = stream.push(input[..split].into()).unwrap();
            assert!(stream.pending.len() <= stream.longest + 3);
            out.push_str(&stream.push(input[split..].into()).unwrap());
            assert!(stream.pending.len() <= stream.longest + 3);
            out.push_str(&stream.finish().unwrap());
            assert_eq!(out, expected, "split={split}");
            assert!(stream.finish().unwrap().is_empty());
            assert!(stream.push("late".into()).is_err());
        }
        let mut stream = Stream::new(state.clone()).unwrap();
        assert!(
            !stream
                .push("a harmless prefix longer than the registered maximum length ................".into())
                .unwrap()
                .is_empty()
        );
        lua.load("keep:close()").exec().unwrap();
        assert!(stream.push("later".into()).is_err());

        // Exercise `UserData::close` and its closed state.
        let ud = lua.create_userdata(Stream::new(state).unwrap()).unwrap();
        lua.globals().set("s_ud", ud).unwrap();
        let closed_ok: bool = lua.load("s_ud:close(); return s_ud:finish() == ''").eval().unwrap();
        assert!(closed_ok);
    }

    #[test]
    fn byte_stream_projects_across_chunks_and_handles_invalid_utf8() {
        use mare_shared::wire_projection::ByteStreamProjector;
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        super::super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        let state = state(&lua);

        let mut bs = ByteStream::new(state.0.clone());
        assert_eq!(bs.push(b"plain "), b"plain ");
        assert_eq!(bs.finish(), b"");

        lua.load("_keep=mare.projection.register('FAKE_SECRET_VAL')").exec().unwrap();
        // Exercise invalid UTF-8 in the middle, truncated UTF-8 across chunks, split secret,
        // and `raw_limit` landing inside a multi-byte UTF-8 sequence (`local_cut` backtrack).
        for pad in ["a", "ab", "abc", "abcd"] {
            let mut out = Vec::new();
            let mut bs = ByteStream::new(state.0.clone());
            out.extend(bs.push(b"head \xff\xe4\xb8\xad\xe4\xb8"));
            let mid = [b"\xad".as_slice(), pad.as_bytes(), b"0123456789\xe4\xb8\xadFAKE_SEC"].concat();
            out.extend(bs.push(&mid));
            out.extend(bs.push(b"RET_VAL tail"));
            out.extend(bs.finish());
            let text = String::from_utf8_lossy(&out);
            assert!(!text.contains("FAKE_SECRET_VAL"), "pad={pad}: {text}");
            assert!(out.contains(&0xff));
        }
        // `raw_limit` inside a multi-byte char immediately followed by a shorter chunk.
        let mut out = Vec::new();
        out.extend(bs.push(b"prefix_ok_\xe4\xb8\xad\xe4\xb8\xadFAKE_SECRET_V"));
        out.extend(bs.push(b"AL"));
        out.extend(bs.finish());
        assert!(!String::from_utf8_lossy(&out).contains("FAKE_SECRET_VAL"));
    }
}
