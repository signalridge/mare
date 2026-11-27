//! The Rust emitters' entry: the blocked-turn check, the event record and the taps, then dispatch
//! on the cordis tree's bus (dispatch/bus.rs, mare_kernel/bus.lua); recent event names. A Rust host
//! listens with Kernel::host_on / host_voter (bus.rs).

use std::sync::Mutex;

use mlua::{MultiValue, Value};

use crate::Kernel;
use mare_shared::lua::{json_to_lua, lua_to_json};

impl Kernel {
    pub fn policy_metadata(&self, event: &str) -> crate::PolicyMetadata {
        self.st().policy_metadata.get(event).cloned().unwrap_or_default()
    }

    pub fn is_policy_point(&self, event: &str) -> bool {
        self.st().policy_points.contains(event)
    }

    pub fn is_rewrite_point(&self, event: &str) -> bool {
        self.st().rewrite_points.contains(event)
    }

    pub fn policy_points(&self) -> Vec<String> {
        self.st().policy_points.iter().cloned().collect()
    }

    pub fn rewrite_points(&self) -> Vec<String> {
        self.st().rewrite_points.iter().cloned().collect()
    }

    pub fn policy_epoch(&self) -> u64 {
        self.0.policy_epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn bump_policy_epoch(&self) {
        self.0.policy_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Live listeners of `event`: on the bus, plus the policy service's voters of a decision
    /// point.
    pub fn listener_count(&self, event: &str) -> usize {
        self.bus_listener_count(event) + self.voter_count(event)
    }

    pub(crate) fn report(&self, event: &str, plugin: &str, e: &mlua::Error) {
        self.handler_failed(plugin, &format!("listener for `{event}`"), Some(event), &e.to_string());
    }

    /// Notify every listener in order. Oversized or unencodable events never enter the bounded log
    /// and never throw an error back to the emitter.
    pub async fn emit(&self, event: &str, args: MultiValue) {
        record_event(event);
        let ev = self.tap_event(event, &args).await;
        self.tree_emit(event, &ev, args).await;
    }

    /// Like `emit`, but stop and return an error when a listener fails. Use this for preparation
    /// whose failure must prevent the caller from proceeding (ordinary notifications remain best
    /// effort).
    pub async fn emit_checked(&self, event: &str, args: MultiValue) -> mlua::Result<()> {
        self.validate_event_dispatch("emit_checked", event, None)?;
        record_event(event);
        let ev = self.tap_event(event, &args).await;
        self.tree_emit_checked(event, &ev, args).await
    }

    /// First listener returning a non-nil value wins.
    pub async fn bail(&self, event: &str, args: MultiValue) -> Value {
        record_event(event);
        let ev = self.tap_event(event, &args).await;
        self.tree_bail(event, &ev, None, args).await
    }

    /// Each listener receives the current value (plus extra args) and may pass on a replacement.
    pub async fn waterfall(&self, event: &str, value: Value, extra: MultiValue) -> Value {
        self.waterfall_dispatch(event, value.clone(), extra, false).await.unwrap_or(value)
    }

    /// Like `waterfall`, but stop and return an error when a listener fails so callers can fail
    /// closed instead of keeping a pre-hook value.
    pub async fn waterfall_checked(&self, event: &str, value: Value, extra: MultiValue) -> mlua::Result<Value> {
        self.waterfall_dispatch(event, value, extra, true).await
    }

    async fn waterfall_dispatch(
        &self,
        event: &str,
        value: Value,
        extra: MultiValue,
        checked: bool,
    ) -> mlua::Result<Value> {
        if checked {
            self.validate_event_dispatch("waterfall_checked", event, None)?;
        }
        record_event(event);
        let mut tapped = MultiValue::new();
        tapped.push_back(value.clone());
        let ev = self.tap_event(event, &tapped).await;
        self.tree_waterfall(event, &ev, value, extra, checked).await
    }

    /// Convenience for Rust frontends: emit with a JSON payload.
    pub async fn emit_json(&self, event: &str, payload: serde_json::Value) {
        match json_to_lua(self.lua(), &payload) {
            Ok(v) => self.emit(event, MultiValue::from_vec(vec![v])).await,
            Err(e) => tracing::warn!("emit_json {event}: {e}"),
        }
    }

    pub async fn bail_json(&self, event: &str, payload: serde_json::Value) -> Option<serde_json::Value> {
        let v = json_to_lua(self.lua(), &payload).ok()?;
        match self.bail(event, MultiValue::from_vec(vec![v])).await {
            Value::Nil => None,
            v => lua_to_json(self.lua(), v).ok(),
        }
    }
}

// recent event names, for crash reports
static RECENT: Mutex<std::collections::VecDeque<String>> = Mutex::new(std::collections::VecDeque::new());

pub(crate) fn record_event(name: &str) {
    if let Ok(mut r) = RECENT.lock() {
        if r.len() >= 50 {
            r.pop_front();
        }
        r.push_back(name.to_string());
    }
}

/// The last ~50 kernel event names (oldest first).
pub fn recent_events() -> Vec<String> {
    RECENT.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default()
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[tokio::test]
    async fn bus_and_policy_pass_single_payload_and_event_envelope() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
                kernel
                    .lua()
                    .load("mare = mare or {}; mare.task = mare.task or { clock = function() return 0 end }; mare.log = mare.log or function() end")
                    .exec()
                    .unwrap();
                let contracts: mlua::Table = kernel
                    .lua()
                    .load(
                        r#"return {
                            org = {
                                methods = {},
                                events = {
                                    check = { mode = "policy", doc = "policy point" },
                                    rw = { mode = "rewrite", doc = "rewrite point" },
                                },
                            },
                        }"#,
                    )
                    .eval()
                    .unwrap();
                kernel.install_service_contracts(contracts).unwrap();
                let _ = kernel.cordis_boot().unwrap();
                tokio::task::yield_now().await;
                kernel.set_session_identity(Some("sess-42".into()));
                let (_, expected_epoch) = kernel.0.event_log.session_identity();
                let mut cursor = kernel
                    .0
                    .event_log
                    .open_cursor(std::sync::Arc::new(|name| (name == "host/notice").then_some(true)));
                kernel.taps_changed();
                let ctx = kernel.host_ctx().unwrap();
                let owner = kernel.host_owner().unwrap();

                let captured: mlua::Table = kernel
                    .lua()
                    .load(
                        r#"
                        CAPTURED = {}
                        return CAPTURED
                    "#,
                    )
                    .eval()
                    .unwrap();

                let notice_fn = kernel
                    .lua()
                    .load(
                        r#"return function(data, ev)
                            CAPTURED.host_notice = { data = data, ev = ev }
                        end"#,
                    )
                    .eval()
                    .unwrap();
                kernel.host_on(&ctx, owner, "host/notice", notice_fn, crate::HostOn::default()).unwrap();

                let internal_fn = kernel
                    .lua()
                    .load(
                        r#"return function(data, ev)
                            CAPTURED.internal = { has_data = data ~= nil, ev = ev }
                        end"#,
                    )
                    .eval()
                    .unwrap();
                kernel.host_on(&ctx, owner, "internal/demo", internal_fn, crate::HostOn::default()).unwrap();

                let wf_fn = kernel
                    .lua()
                    .load(
                        r#"return function(data, ev)
                            CAPTURED.wf = { data = data, ev = ev, has_next = type(ev) == "table" and type(ev.next) == "function" }
                            local inner = ev.next({ val = data.val .. "+step" })
                            return inner
                        end"#,
                    )
                    .eval()
                    .unwrap();
                kernel.host_on(&ctx, owner, "demo/wf", wf_fn, crate::HostOn::default()).unwrap();

                kernel
                    .emit_json(
                        "host/notice",
                        serde_json::json!({ "level": "info", "text": "hi", "turn_id": "t-7" }),
                    )
                    .await;
                kernel.emit("internal/demo", MultiValue::new()).await;

                let wf_in: mlua::Table = kernel.lua().load("return { val = 'base' }").eval().unwrap();
                let wf_out = kernel.waterfall("demo/wf", Value::Table(wf_in), MultiValue::new()).await;
                let wf_val: String = wf_out.as_table().unwrap().get("val").unwrap();
                assert_eq!(wf_val, "base+step");

                let _ = cursor.try_recv().unwrap().expect("cursor must receive host/notice");

                let notice_ev: mlua::Table = captured.get::<mlua::Table>("host_notice").unwrap().get("ev").unwrap();
                assert_eq!(notice_ev.get::<String>("type").unwrap(), "host/notice");
                assert_eq!(notice_ev.get::<Option<String>>("session_id").unwrap().as_deref(), Some("sess-42"));
                assert_eq!(notice_ev.get::<String>("session_epoch").unwrap(), expected_epoch);
                assert_eq!(notice_ev.get::<Option<u64>>("seq").unwrap(), Some(1));
                assert_eq!(notice_ev.get::<Option<String>>("turn_id").unwrap().as_deref(), Some("t-7"));

                let internal_rec: mlua::Table = captured.get("internal").unwrap();
                assert!(!internal_rec.get::<bool>("has_data").unwrap());
                let internal_ev: mlua::Table = internal_rec.get("ev").unwrap();
                assert_eq!(internal_ev.get::<String>("type").unwrap(), "internal/demo");
                assert_eq!(internal_ev.get::<Option<u64>>("seq").unwrap(), None);

                let wf_rec: mlua::Table = captured.get("wf").unwrap();
                assert!(wf_rec.get::<bool>("has_next").unwrap());
                let wf_ev: mlua::Table = wf_rec.get("ev").unwrap();
                assert_eq!(wf_ev.get::<String>("type").unwrap(), "demo/wf");
            })
            .await;
    }
}
