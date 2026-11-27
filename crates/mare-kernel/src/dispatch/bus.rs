//! The Rust emitters' half of the one event bus: after the
//! blocked-turn check, `record_event` and the taps, every `Kernel::emit*` / `bail*` / `waterfall*`
//! dispatches on the cordis tree's root events service through `mare_kernel/bus.lua`, the same bus
//! every plugin listens on. Until the tree is booted
//! there is nothing to dispatch to.

use mlua::{Function, MultiValue, Table, Value};

use crate::Kernel;

const BOOTED: &str = "mare.cordis_boot";
/// A Rust host listener's options (`Kernel::host_on`).
#[derive(Default, Clone, Copy)]
pub struct HostOn {
    /// before the listeners registered earlier (cordis `prepend`, no numeric order)
    pub prepend: bool,
}

/// A Rust host voter's options (`Kernel::host_voter`).
#[derive(Default, Clone, Copy)]
pub struct HostVoter {
    pub prepend: bool,
    /// the answer time: `None` = the default policy timeout, `Some(Duration::ZERO)` = none (a
    /// host that waits for a person)
    pub timeout: Option<std::time::Duration>,
}

impl Kernel {
    /// `booted.hostFiber` (mare_kernel/host_fiber.lua): the host's own fiber on the tree.
    fn host_fiber(&self) -> mlua::Result<Table> {
        self.cordis_boot()?.get("hostFiber")
    }

    /// The host fiber's cordis ctx (`<root>`): the native successor of the old root Ctx.
    pub fn host_ctx(&self) -> mlua::Result<Table> {
        self.host_fiber()?.get::<Function>("ctx")?.call(())
    }

    /// The host's owner record (`<root>`): what the host's listeners and registrations are
    /// attributed to.
    pub fn host_owner(&self) -> mlua::Result<u64> {
        self.host_fiber()?.get::<Function>("owner")?.call(())
    }

    /// Provide a contracted host service on the host fiber via `mare.Service`, subject to the
    /// service's contract admission, typed guards, and session barrier.
    pub fn host_provide_service(&self, name: &str, methods: Table) -> mlua::Result<()> {
        self.host_fiber()?.get::<Function>("provide")?.call((name, methods))
    }

    /// A Rust host listener: `ctx:on` on `ctx` — the host's fiber
    /// ([`Kernel::host_ctx`]) or an RPC connection's — made as `owner`. It runs in the context it
    /// registered in, whoever emits and goes with its fiber. Returns its release.
    pub fn host_on(
        &self,
        ctx: &Table,
        owner: u64,
        event: &str,
        func: Function,
        opts: HostOn,
    ) -> mlua::Result<Function> {
        let o = self.lua().create_table()?;
        o.set("prepend", opts.prepend)?;
        self.host_fiber()?.get::<Function>("on")?.call((ctx.clone(), owner, event, func, o))
    }

    /// A Rust host voter: `policy:voter(point, ...)` read through `ctx`, made as `owner`. Returns
    /// its release.
    pub fn host_voter(
        &self,
        ctx: &Table,
        owner: u64,
        point: &str,
        func: Function,
        opts: HostVoter,
    ) -> mlua::Result<Function> {
        let o = self.lua().create_table()?;
        o.set("prepend", opts.prepend)?;
        if let Some(t) = opts.timeout {
            o.set("timeout_ms", t.as_millis() as f64)?;
        }
        self.host_fiber()?.get::<Function>("voter")?.call((ctx.clone(), owner, point, func, o))
    }

    /// `booted.bus[method]`, once the tree is booted (an emit never boots it).
    pub(crate) fn bus(&self, method: &str) -> Option<Function> {
        let booted: Table = self.lua().named_registry_value(BOOTED).ok()?;
        booted.get::<Table>("bus").ok()?.get(method).ok()
    }

    /// The plugins taking part in `event`, in the order they run: a decision point's voters /
    /// rewriters, else its bus listeners (diagnostics and order tests).
    pub fn listener_owners(&self, event: &str) -> Vec<String> {
        let kind = if self.is_policy_point(event) {
            Some("voters")
        } else if self.is_rewrite_point(event) {
            Some("rewriters")
        } else {
            None
        };
        match kind {
            Some(kind) => super::policy_prims::service_list(self, kind, event)
                .map(|list| list.into_iter().map(|v| v.owner).collect())
                .unwrap_or_default(),
            None => self.bus("owners").and_then(|f| f.call::<Vec<String>>(event).ok()).unwrap_or_default(),
        }
    }

    /// `bus.listener_count(event)`: 0 before the tree is booted.
    pub(crate) fn bus_listener_count(&self, event: &str) -> usize {
        self.bus("listener_count").and_then(|f| f.call::<usize>(event).ok()).unwrap_or(0)
    }

    /// Who would vote on decision point `point`: the policy service's voters only. Unlike
    /// `listener_count`, this excludes ordinary event listeners, including on listed points.
    pub fn voter_count(&self, point: &str) -> usize {
        super::policy_prims::service_list(self, "voters", point).map(|v| v.len()).unwrap_or(0)
    }

    async fn bus_call(&self, method: &str, args: MultiValue) -> mlua::Result<Option<MultiValue>> {
        let Some(f) = self.bus(method) else { return Ok(None) };
        mare_shared::lua::call_async::<MultiValue>(self.lua(), &f, args).await.map(Some)
    }

    fn with_ev_event(&self, ev: &super::tap::EventEnvelope, event: &str, args: MultiValue) -> mlua::Result<MultiValue> {
        let mut all = MultiValue::new();
        all.push_back(Value::Table(ev.to_lua(self.lua())?));
        all.push_back(Value::String(self.lua().create_string(event)?));
        all.extend(args);
        Ok(all)
    }

    /// Report the `{ {plugin, message}, ... }` failures a bus dispatch returned.
    fn report_failures(&self, event: &str, failures: Value) {
        let Value::Table(failures) = failures else { return };
        for failure in failures.sequence_values::<Table>().flatten() {
            let plugin: String = failure.get(1).unwrap_or_default();
            let message: String = failure.get(2).unwrap_or_default();
            self.report(event, &plugin, &mlua::Error::runtime(message));
        }
    }

    /// `emit`: every tree listener, a failing one reported and the rest still run.
    pub(crate) async fn tree_emit(&self, event: &str, ev: &super::tap::EventEnvelope, args: MultiValue) {
        let result = match self.with_ev_event(ev, event, args) {
            Ok(args) => self.bus_call("emit_ev", args).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(Some(mut out)) => self.report_failures(event, out.pop_front().unwrap_or(Value::Nil)),
            Ok(None) => {}
            Err(e) => self.report(event, "bus", &e),
        }
    }

    /// `emit_checked`: the first failing tree listener fails the dispatch.
    pub(crate) async fn tree_emit_checked(
        &self,
        event: &str,
        ev: &super::tap::EventEnvelope,
        args: MultiValue,
    ) -> mlua::Result<()> {
        let Some(mut out) = self.bus_call("emit_checked_ev", self.with_ev_event(ev, event, args)?).await? else {
            return Ok(());
        };
        let Value::String(plugin) = out.pop_front().unwrap_or(Value::Nil) else { return Ok(()) };
        let plugin = plugin.to_string_lossy();
        let message = match out.pop_front() {
            Some(Value::String(m)) => m.to_string_lossy(),
            _ => String::new(),
        };
        let error = mlua::Error::runtime(message);
        self.report(event, &plugin, &error);
        Err(mlua::Error::runtime(format!("{event} listener {plugin} failed: {error}")))
    }

    /// `bail`: the first non-nil result of the tree's (synchronous) listeners.
    pub(crate) async fn tree_bail(
        &self,
        event: &str,
        ev: &super::tap::EventEnvelope,
        before: Option<Function>,
        args: MultiValue,
    ) -> Value {
        let result = match self.with_ev_event(ev, event, args) {
            Ok(mut args) => {
                // `bail_each_ev(ev, before, event, ...)` -> keep `ev` at slot 1 and `before` at slot 2.
                let ev_val = args.pop_front().unwrap_or(Value::Nil);
                args.push_front(before.map(Value::Function).unwrap_or(Value::Nil));
                args.push_front(ev_val);
                self.bus_call("bail_each_ev", args).await
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(Some(mut out)) => {
                let value = out.pop_front().unwrap_or(Value::Nil);
                self.report_failures(event, out.pop_front().unwrap_or(Value::Nil));
                value
            }
            Ok(None) => Value::Nil,
            Err(e) => {
                self.report(event, "bus", &e);
                Value::Nil
            }
        }
    }

    /// `waterfall`: the value through the tree's listeners. Unchecked, a failure is reported and
    /// the value as it came in is kept; checked, it fails the dispatch.
    pub(crate) async fn tree_waterfall(
        &self,
        event: &str,
        ev: &super::tap::EventEnvelope,
        value: Value,
        extra: MultiValue,
        checked: bool,
    ) -> mlua::Result<Value> {
        let mut args = MultiValue::new();
        args.push_back(value.clone());
        args.extend(extra);
        let method = if checked { "waterfall_checked_ev" } else { "waterfall_ev" };
        let out = match self.bus_call(method, self.with_ev_event(ev, event, args)?).await {
            Ok(Some(out)) => out,
            Ok(None) => return Ok(value),
            Err(e) if checked => return Err(e),
            Err(e) => {
                self.report(event, "bus", &e);
                return Ok(value);
            }
        };
        let mut out = out.into_iter();
        let result = out.next().unwrap_or(Value::Nil);
        let failure = out.next().unwrap_or(Value::Nil);
        if failure.is_nil() {
            // a nil result keeps the value the dispatch was given
            return Ok(if result.is_nil() { value } else { result });
        }
        if checked {
            let plugin = match failure {
                Value::String(p) => p.to_string_lossy(),
                _ => "?".into(),
            };
            let message = match out.next() {
                Some(Value::String(m)) => m.to_string_lossy(),
                _ => String::new(),
            };
            let error = mlua::Error::runtime(message);
            self.report(event, &plugin, &error);
            return Err(mlua::Error::runtime(format!("{event} listener {plugin} failed: {error}")));
        }
        // unchecked: the failing listeners were skipped, the rest of the chain's value is kept
        self.report_failures(event, failure);
        Ok(if result.is_nil() { value } else { result })
    }
}
