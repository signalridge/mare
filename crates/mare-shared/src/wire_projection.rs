//! Zero-overhead out-of-process wire projection hook stored in Lua `app_data`.
//!
//! Populated once by `mare_binding::projection::install` and read by core out-of-process
//! exit points (the event log, RPC `Conn::send`, process streams, headless, and ACP).
//! When no sensitive values have ever been registered in the VM, `has_entries` is `false` so
//! callers skip locking and JSON traversal completely.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

use mlua::Lua;
use serde_json::Value;

type JsonProjectFn = Arc<dyn Fn(&mut Value) + Send + Sync>;
type JsonKeyCheckFn = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;
type StrProjectFn = Arc<dyn Fn(&str) -> String + Send + Sync>;
type BytesProjectFn = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;
type StreamFactoryFn = Arc<dyn Fn() -> Box<dyn ByteStreamProjector> + Send + Sync>;
type RevisionFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Bounded stateful byte-stream projector for chunked out-of-process pipes/PTYs.
pub trait ByteStreamProjector: Send {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8>;
    fn finish(&mut self) -> Vec<u8>;
}

/// Zero-overhead per-stream wrapper that stays dormant until any secret is registered.
pub struct WireByteStream {
    projector: Option<WireProjector>,
    inner: Option<Box<dyn ByteStreamProjector>>,
}

impl WireByteStream {
    pub fn new(projector: Option<WireProjector>) -> Self {
        Self { projector, inner: None }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if let Some(stream) = &mut self.inner {
            return stream.push(chunk);
        }
        if let Some(proj) = self.projector.as_ref().filter(|p| p.is_active()) {
            let mut stream = (proj.stream_fn)();
            let out = stream.push(chunk);
            self.inner = Some(stream);
            return out;
        }
        chunk.to_vec()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        self.inner.take().map(|mut s| s.finish()).unwrap_or_default()
    }
}

/// VM-local hook registered by `mare-binding` and invoked by kernel/CLI out-of-process boundaries.
#[derive(Clone)]
pub struct WireProjector {
    has_entries: Arc<AtomicBool>,
    json_fn: JsonProjectFn,
    key_check_fn: JsonKeyCheckFn,
    str_fn: StrProjectFn,
    bytes_fn: BytesProjectFn,
    stream_fn: StreamFactoryFn,
    revision_fn: RevisionFn,
}

impl WireProjector {
    pub fn new(
        has_entries: Arc<AtomicBool>,
        json_fn: JsonProjectFn,
        str_fn: StrProjectFn,
        bytes_fn: BytesProjectFn,
        stream_fn: StreamFactoryFn,
    ) -> Self {
        Self {
            has_entries,
            json_fn,
            key_check_fn: Arc::new(|_| Ok(())),
            str_fn,
            bytes_fn,
            stream_fn,
            revision_fn: Arc::new(|| 0),
        }
    }

    pub fn with_key_check(mut self, key_check_fn: JsonKeyCheckFn) -> Self {
        self.key_check_fn = key_check_fn;
        self
    }

    /// Track changes to the existing projection registry while an event waits to be committed.
    pub fn with_revision(mut self, revision_fn: RevisionFn) -> Self {
        self.revision_fn = revision_fn;
        self
    }

    pub fn revision(&self) -> u64 {
        (self.revision_fn)()
    }

    pub fn same_registry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.json_fn, &other.json_fn)
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.has_entries.load(Ordering::Acquire)
    }

    pub fn project_json(&self, value: &mut Value) {
        if self.is_active() {
            (self.json_fn)(value);
        }
    }

    pub fn check_json_keys(&self, value: &Value) -> Result<(), String> {
        if self.is_active() {
            (self.key_check_fn)(value)?;
        }
        Ok(())
    }

    pub fn project_str(&self, input: &str) -> String {
        if self.is_active() { (self.str_fn)(input) } else { input.to_owned() }
    }

    pub fn project_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        if self.is_active() { (self.bytes_fn)(bytes) } else { bytes.to_vec() }
    }

    pub fn byte_stream(&self) -> WireByteStream {
        WireByteStream::new(Some(self.clone()))
    }
}

/// Shared slot held by `Kernel` / `EventLog` so pure-Rust events bind eagerly regardless of
/// `projection::install` vs `Kernel::new` initialization order.
#[derive(Clone, Default)]
pub struct WireProjectorSlot(Arc<RwLock<Option<WireProjector>>>);

impl WireProjectorSlot {
    pub fn set(&self, projector: WireProjector) {
        *self.0.write().expect("wire projector slot lock") = Some(projector);
    }

    pub fn get(&self) -> Option<WireProjector> {
        self.0.read().expect("wire projector slot lock").clone()
    }

    #[inline]
    pub fn active(&self) -> Option<WireProjector> {
        self.0.read().expect("wire projector slot lock").as_ref().filter(|p| p.is_active()).cloned()
    }
}

/// Return (or initialize) the VM's shared wire projector slot in `lua.app_data`.
pub fn hook_slot(lua: &Lua) -> WireProjectorSlot {
    if let Some(slot) = lua.app_data_ref::<WireProjectorSlot>() {
        return (*slot).clone();
    }
    let slot = WireProjectorSlot::default();
    lua.set_app_data(slot.clone());
    slot
}

/// Register the VM-local wire projector hook in `lua.app_data`.
pub fn install(lua: &Lua, projector: WireProjector) {
    hook_slot(lua).set(projector);
}

/// Clone the VM's projector handle (preserving the zero-cost `is_active` atomic flag).
pub fn hook(lua: &Lua) -> Option<WireProjector> {
    lua.app_data_ref::<WireProjectorSlot>().and_then(|slot| slot.get())
}

/// Return the VM's projector only when sensitive entries are registered; `None` otherwise.
#[inline]
pub fn active_projector(lua: &Lua) -> Option<WireProjector> {
    lua.app_data_ref::<WireProjectorSlot>().and_then(|slot| slot.active())
}

/// Project `value` in place if sensitive entries exist in `lua`, or return without locking.
#[inline]
pub fn project_json(lua: &Lua, value: &mut Value) {
    if let Some(p) = active_projector(lua) {
        p.project_json(value);
    }
}

/// Project `input` if sensitive entries exist in `lua`, returning a fresh string when active.
pub fn project_str(lua: &Lua, input: &str) -> String {
    match active_projector(lua) {
        Some(p) => p.project_str(input),
        None => input.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestStream(Vec<u8>);
    impl ByteStreamProjector for TestStream {
        fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
            self.0.extend_from_slice(chunk);
            Vec::new()
        }
        fn finish(&mut self) -> Vec<u8> {
            let s = String::from_utf8_lossy(&self.0).replace("SEC", "[REDACTED]");
            self.0.clear();
            s.into_bytes()
        }
    }

    #[test]
    fn inactive_and_active_wire_projector_hooks_behave_as_expected() {
        let lua = Lua::new();
        let slot = hook_slot(&lua);
        assert!(hook(&lua).is_none());
        assert!(slot.active().is_none());
        assert!(active_projector(&lua).is_none());
        let mut plain = serde_json::json!({ "text": "raw" });
        project_json(&lua, &mut plain);
        assert_eq!(plain["text"], "raw");
        assert_eq!(project_str(&lua, "raw"), "raw");

        let flag = Arc::new(AtomicBool::new(false));
        let proj = WireProjector::new(
            Arc::clone(&flag),
            Arc::new(|v| {
                if let Some(s) = v.get("text").and_then(Value::as_str).map(str::to_owned) {
                    v["text"] = Value::String(s.replace("SEC", "[REDACTED]"));
                }
            }),
            Arc::new(|s| s.replace("SEC", "[REDACTED]")),
            Arc::new(|b| String::from_utf8_lossy(b).replace("SEC", "[REDACTED]").into_bytes()),
            Arc::new(|| Box::new(TestStream(Vec::new()))),
        );
        install(&lua, proj.clone());
        assert!(hook(&lua).is_some());
        assert!(active_projector(&lua).is_none());
        assert_eq!(proj.project_bytes(b"SEC"), b"SEC");
        let mut dormant = proj.byte_stream();
        assert_eq!(dormant.push(b"plain"), b"plain");
        assert!(dormant.finish().is_empty());

        flag.store(true, Ordering::Release);
        assert!(slot.active().is_some());
        assert!(active_projector(&lua).is_some());
        let mut msg = serde_json::json!({ "text": "keep SEC" });
        project_json(&lua, &mut msg);
        assert_eq!(msg["text"], "keep [REDACTED]");
        assert_eq!(project_str(&lua, "keep SEC"), "keep [REDACTED]");
        assert_eq!(proj.project_bytes(b"keep SEC"), b"keep [REDACTED]");
        let mut active_stream = proj.byte_stream();
        assert!(active_stream.push(b"SE").is_empty());
        assert!(active_stream.push(b"C").is_empty());
        assert_eq!(active_stream.finish(), b"[REDACTED]");

        let checked = proj.with_key_check(Arc::new(|v| {
            if v.as_object().is_some_and(|m| m.keys().any(|k| k.contains("SEC"))) {
                Err("sensitive key".to_string())
            } else {
                Ok(())
            }
        }));
        assert!(checked.check_json_keys(&serde_json::json!({ "ok": "SEC" })).is_ok());
        assert!(checked.check_json_keys(&serde_json::json!({ "k-SEC": 1 })).is_err());
    }
}
