use super::{execution::Session, limits::Limits, wire::Capability};
use mare_shared::lua::{nil_err, strict_lua_to_json};
use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
pub(super) struct Settings {
    pub limits: Limits,
    pub owner_cancel: tokio_util::sync::CancellationToken,
    pub expose: Capability,
    pub callbacks: Vec<Function>,
    pub on_output: Option<Function>,
    pub context: Option<Table>,
    pub generation: u64,
    pub persistent: bool,
}
struct Sandbox {
    base: Settings,
    persistent: bool,
    used: AtomicBool,
    running: AtomicBool,
    poisoned: AtomicBool,
    generation: AtomicU64,
    session: tokio::sync::Mutex<Option<Session>>,
    active: Mutex<Option<Limits>>,
    output: Arc<Mutex<String>>,
    executable: std::path::PathBuf,
}
fn capabilities(
    lua: &Lua,
    value: Value,
    callbacks: &mut Vec<Function>,
    stack: &mut Vec<usize>,
) -> mlua::Result<Capability> {
    if let Value::Function(function) = value {
        let id = callbacks.len();
        callbacks.push(function);
        return Ok(Capability::Function { id });
    }
    if let Ok(value) = strict_lua_to_json(lua, value.clone()) {
        return Ok(Capability::Value { value });
    }
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime("sandbox unsupported exposure"));
    };
    let id = table.to_pointer() as usize;
    if stack.len() > 32 || stack.contains(&id) || table.metatable().is_some() {
        return Err(mlua::Error::runtime("sandbox invalid exposure table"));
    }
    stack.push(id);
    let mut entries = vec![];
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        entries.push((name, capabilities(lua, value, callbacks, stack)?));
    }
    stack.pop();
    Ok(Capability::Table { entries })
}
fn failure(
    lua: &Lua,
    error: impl std::fmt::Display,
    output: String,
    cancelled: bool,
    unconfirmed: bool,
    lost: bool,
) -> mlua::Result<MultiValue> {
    let status = lua.create_table()?;
    status.set("output", output)?;
    status.set("cancelled", cancelled)?;
    status.set("stop_unconfirmed", unconfirmed)?;
    status.set("state_lost", lost)?;
    let mut values = nil_err(lua, error)?;
    values.push_back(Value::Table(status));
    Ok(values)
}
impl Sandbox {
    fn new(lua: &Lua, opts: Option<Table>) -> mlua::Result<Self> {
        let limits = Limits::new(&opts)?;
        let expose = opts
            .as_ref()
            .map(|o| o.get::<Option<Table>>("expose"))
            .transpose()?
            .flatten()
            .unwrap_or(lua.create_table()?);
        let mut callbacks = vec![];
        let expose = capabilities(lua, Value::Table(expose), &mut callbacks, &mut vec![])?;
        let persistent =
            opts.as_ref().map(|o| o.get::<Option<bool>>("persistent")).transpose()?.flatten().unwrap_or(false);
        let base = Settings {
            owner_cancel: limits.cancel.clone(),
            limits,
            expose,
            callbacks,
            on_output: opts.as_ref().map(|o| o.get::<Option<Function>>("on_output")).transpose()?.flatten(),
            context: mare_shared::exec_ctx::current(lua),
            generation: 0,
            persistent,
        };
        let executable = opts
            .as_ref()
            .map(|o| o.get::<Option<String>>("worker_executable"))
            .transpose()?
            .flatten()
            .map(std::path::PathBuf::from)
            .map(Ok)
            .unwrap_or_else(std::env::current_exe)
            .map_err(mlua::Error::external)?;
        Ok(Self {
            base,
            persistent,
            used: AtomicBool::new(false),
            running: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            session: tokio::sync::Mutex::new(None),
            active: Mutex::new(None),
            output: Arc::new(Mutex::new(String::new())),
            executable,
        })
    }
    fn settings(&self, lua: &Lua, options: &Option<Table>) -> mlua::Result<Settings> {
        let opts = lua.create_table()?;
        if self.base.limits.deadline_enabled {
            opts.set("timeout_ms", self.base.limits.timeout.as_millis() as u64)?;
        } else {
            opts.set("timeout_ms", false)?;
        }
        opts.set("memory_bytes", self.base.limits.memory)?;
        opts.set("max_bytes", self.base.limits.bytes)?;
        opts.set("max_message_bytes", self.base.limits.message_bytes)?;
        opts.set("max_output_bytes", self.base.limits.output_bytes)?;
        opts.set("max_pending_calls", self.base.limits.pending_calls)?;
        opts.set("stop_grace_ms", self.base.limits.stop_grace.as_millis() as u64)?;
        opts.set("grace_ms", self.base.limits.drain_grace.as_millis() as u64)?;
        opts.set("max_instructions", self.base.limits.instructions)?;
        opts.set("cancel", crate::task::Token(self.base.limits.cancel.clone()))?;
        if let Some(options) = options {
            for key in [
                "timeout_ms",
                "memory_bytes",
                "max_bytes",
                "max_message_bytes",
                "max_output_bytes",
                "max_pending_calls",
                "stop_grace_ms",
                "grace_ms",
                "max_instructions",
                "cancel",
            ] {
                let value = options.get::<Value>(key)?;
                if !matches!(value, Value::Nil) {
                    opts.set(key, value)?;
                }
            }
        }
        let (expose, callbacks) =
            if let Some(table) = options.as_ref().map(|o| o.get::<Option<Table>>("expose")).transpose()?.flatten() {
                let mut callbacks = vec![];
                let expose = capabilities(lua, Value::Table(table), &mut callbacks, &mut vec![])?;
                (expose, callbacks)
            } else {
                (self.base.expose.clone(), self.base.callbacks.clone())
            };
        Ok(Settings {
            limits: Limits::new(&Some(opts))?,
            owner_cancel: self.base.owner_cancel.clone(),
            expose,
            callbacks,
            on_output: options
                .as_ref()
                .map(|o| o.get::<Option<Function>>("on_output"))
                .transpose()?
                .flatten()
                .or_else(|| self.base.on_output.clone()),
            context: mare_shared::exec_ctx::current(lua).or_else(|| self.base.context.clone()),
            generation: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
            persistent: self.persistent,
        })
    }
}
impl UserData for Sandbox {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("stop", |_, this, ()| {
            if let Some(limits) = this.active.lock().unwrap().as_ref() {
                limits.stop("cancelled");
            } else if this.persistent {
                if let Ok(mut state) = this.session.try_lock() {
                    *state = None;
                }
                this.poisoned.store(true, Ordering::SeqCst);
            } else {
                this.base.owner_cancel.cancel();
            }
            Ok(())
        });
        methods.add_async_method("close", |_, this, ()| async move {
            if let Some(limits) = this.active.lock().unwrap().as_ref() {
                limits.stop("cancelled");
            }
            let mut state = this.session.lock().await;
            let confirmed = if let Some(session) = state.as_mut() {
                session.close(this.base.limits.stop_grace, this.base.limits.drain_grace).await.0
            } else {
                true
            };
            if confirmed {
                *state = None;
                this.poisoned.store(false, Ordering::SeqCst);
            }
            Ok(confirmed)
        });
        methods.add_async_method("run", |lua, this, (code, options): (String, Option<Table>)| async move {
            if this.poisoned.load(Ordering::SeqCst) {
                return failure(
                    &lua,
                    "sandbox: stop_unconfirmed; close the worker before reuse",
                    String::new(),
                    false,
                    true,
                    true,
                );
            }
            if this.running.swap(true, Ordering::SeqCst) {
                return nil_err(&lua, "sandbox: execution already running");
            }
            struct RunGuard<'a> {
                sandbox: &'a Sandbox,
                armed: std::cell::Cell<bool>,
                pid: std::cell::Cell<Option<u32>>,
            }
            impl Drop for RunGuard<'_> {
                fn drop(&mut self) {
                    if self.armed.get() {
                        crate::proc::kill_group(self.pid.get(), crate::platform::Signal::Kill);
                        self.sandbox.poisoned.store(true, Ordering::SeqCst);
                        if let Ok(mut state) = self.sandbox.session.try_lock() {
                            *state = None;
                        }
                    }
                    self.sandbox.running.store(false, Ordering::SeqCst);
                    *self.sandbox.active.lock().unwrap() = None;
                }
            }
            let guard =
                RunGuard { sandbox: &this, armed: std::cell::Cell::new(false), pid: std::cell::Cell::new(None) };
            if !this.persistent && this.used.swap(true, Ordering::SeqCst) {
                return nil_err(&lua, "sandbox: VM is single-use");
            }
            let settings = this.settings(&lua, &options)?;
            if let Err(error) = settings.limits.size(code.len()) {
                return failure(
                    &lua,
                    error,
                    String::new(),
                    settings.limits.cancel.is_cancelled() || settings.owner_cancel.is_cancelled(),
                    false,
                    false,
                );
            }
            *this.active.lock().unwrap() = Some(settings.limits.clone());
            this.output.lock().unwrap().clear();
            let mut state = this.session.lock().await;
            let previous = this.generation.load(Ordering::SeqCst) > 1;
            let lost = previous && state.is_none() && this.persistent;
            if state.is_none() {
                *state = Some(Session::spawn(&this.executable)?);
            }
            guard.pid.set(state.as_ref().and_then(Session::pid));
            guard.armed.set(true);
            let ledger = Arc::new(Mutex::new(super::output::Ledger::new(settings.limits.output_bytes)));
            let (mut result, completed) =
                state.as_mut().unwrap().run(&lua, &settings, code, &this.output, &ledger).await;
            let mut confirmed = true;
            if !this.persistent || !completed {
                let (stopped, drained) =
                    state.as_mut().unwrap().close(settings.limits.stop_grace, settings.limits.drain_grace).await;
                confirmed = stopped;
                if !drained {
                    result = Err(mlua::Error::runtime("sandbox worker output did not drain cleanly"));
                }
                if let Err(error) = &result {
                    let diagnostic = state.as_ref().unwrap().diagnostic();
                    if !diagnostic.is_empty() {
                        result = Err(mlua::Error::runtime(format!("{error}\nworker stderr: {diagnostic}")));
                    }
                }
                if confirmed {
                    *state = None;
                } else {
                    this.poisoned.store(true, Ordering::SeqCst);
                    result = Err(mlua::Error::runtime("sandbox: stop_unconfirmed (worker process not reaped)"));
                }
            }
            guard.armed.set(false);
            // Streaming is irrevocable. Final retained output is a fitting prefix; callers
            // that already received chunks must not interpret this as a retraction.
            let value = match &result {
                Ok(Value::Table(table)) => Some(strict_lua_to_json(&lua, table.get::<Value>("result")?)?),
                _ => None,
            };
            let settled = ledger.lock().unwrap().finish(value, result.as_ref().err().map(ToString::to_string));
            let retained: String = settled.items.iter().filter_map(serde_json::Value::as_str).collect();
            *this.output.lock().unwrap() = retained.clone();
            if let Some(error) = settled.error {
                return failure(
                    &lua,
                    error,
                    retained,
                    settings.limits.cancel.is_cancelled() || settings.owner_cancel.is_cancelled(),
                    !confirmed,
                    !completed || lost,
                );
            } else if let Ok(Value::Table(table)) = &result {
                table.set("output", retained)?;
            }
            match result {
                Ok(Value::Table(table)) => {
                    table.set("state_lost", lost)?;
                    Ok(MultiValue::from_vec(vec![Value::Table(table)]))
                }
                Ok(value) => Ok(MultiValue::from_vec(vec![value])),
                Err(error) => failure(
                    &lua,
                    error,
                    this.output.lock().unwrap().clone(),
                    settings.limits.cancel.is_cancelled() || settings.owner_cancel.is_cancelled(),
                    !confirmed,
                    !completed || lost,
                ),
            }
        });
    }
}
pub(super) fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let api = lua.create_table()?;
    super::output::install(lua, &api)?;
    api.set("complete", lua.create_function(|_, ()| Ok(super::completion::Complete))?)?;
    api.set("new", lua.create_function(|lua, opts: Option<Table>| Sandbox::new(lua, opts))?)?;
    mare.set("sandbox", api)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sandbox_rejects_invalid_exposure_and_enforces_close_poison_running_and_single_use() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = Lua::new();
                let ud_bad = lua.create_any_userdata(1u32).unwrap();
                lua.globals().set("ud_bad", ud_bad).unwrap();
                let bad_exp: Table = lua.load("return { expose = { x = ud_bad } }").eval().unwrap();
                assert!(Sandbox::new(&lua, Some(bad_exp)).is_err());
                let cyc_exp: Table =
                    lua.load("local c = {}; c.self = c; return { expose = { x = c } }").eval().unwrap();
                assert!(Sandbox::new(&lua, Some(cyc_exp)).is_err());

                let sb = Sandbox::new(&lua, None).unwrap();
                sb.poisoned.store(true, Ordering::SeqCst);
                let ud = lua.create_userdata(sb).unwrap();
                lua.globals().set("sb", ud).unwrap();

                let (_, poisoned_err): (Value, String) =
                    lua.load("return sb:run('return 1')").eval_async().await.unwrap();
                assert!(poisoned_err.contains("stop_unconfirmed"), "{poisoned_err}");

                // close() clears poisoned state and returns confirmed=true when idle.
                let closed: bool = lua.load("return sb:close()").eval_async().await.unwrap();
                assert!(closed);

                // stop() on non-persistent idle sandbox cancels owner_cancel cleanly.
                lua.load("sb:stop()").exec().unwrap();

                // Pre-marked running or already-used single-use sandbox returns nil + error.
                lua.globals()
                    .get::<mlua::AnyUserData>("sb")
                    .unwrap()
                    .borrow::<Sandbox>()
                    .unwrap()
                    .running
                    .store(true, Ordering::SeqCst);
                let (_, running_err): (Value, String) =
                    lua.load("return sb:run('return 1')").eval_async().await.unwrap();
                assert!(running_err.contains("already running"), "{running_err}");

                {
                    let ud = lua.globals().get::<mlua::AnyUserData>("sb").unwrap();
                    let s = ud.borrow::<Sandbox>().unwrap();
                    s.running.store(false, Ordering::SeqCst);
                    s.used.store(true, Ordering::SeqCst);
                }
                let (_, used_err): (Value, String) = lua.load("return sb:run('return 1')").eval_async().await.unwrap();
                assert!(used_err.contains("single-use"), "{used_err}");
            })
            .await;
    }
}
