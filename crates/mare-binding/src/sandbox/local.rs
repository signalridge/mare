//! Worker-local bounded VM; only the process protocol is exposed to the parent.
use super::{bridge, limits::Limits};
use mare_shared::lua::{json_to_lua_lossless, nil_err, strict_lua_to_json};
use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Table, UserData, UserDataMethods, Value,
    VmState,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

#[derive(Default)]
struct OutputBuffer {
    buffer: String,
    streamed_bytes: usize,
    streaming: bool,
}

impl OutputBuffer {
    fn new(streaming: bool) -> Self {
        Self { buffer: String::new(), streamed_bytes: 0, streaming }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.streamed_bytes = 0;
    }
}

pub(super) struct Sandbox {
    vm: Lua,
    env: Table,
    output: Arc<Mutex<OutputBuffer>>,
    limits: Arc<Mutex<Limits>>,
    persistent: bool,
    exposed: Mutex<Vec<String>>,
    epoch: Arc<std::sync::atomic::AtomicU64>,
    used: AtomicBool,
    terminated: AtomicBool,
}
impl Sandbox {
    fn new(parent: &Lua, opts: Option<Table>) -> mlua::Result<Self> {
        let limits = Limits::new(&opts)?;
        let vm = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
            LuaOptions::new(),
        )?;
        vm.set_memory_limit(limits.memory)?;
        let g = vm.globals();
        let env = vm.create_table()?;
        for name in [
            "assert",
            "error",
            "ipairs",
            "next",
            "pairs",
            "pcall",
            "xpcall",
            "select",
            "tonumber",
            "tostring",
            "type",
            "rawequal",
            "rawget",
            "rawset",
            "rawlen",
            "setmetatable",
            "getmetatable",
            "_VERSION",
            "table",
            "string",
            "math",
            "utf8",
            "coroutine",
        ] {
            env.set(name, g.get::<Value>(name)?)?;
        }
        let stream_output = parent.app_data_ref::<super::worker::StreamOutput>().is_some();
        let output = Arc::new(Mutex::new(OutputBuffer::new(stream_output)));
        let out2 = output.clone();
        let shared_limits = Arc::new(Mutex::new(limits.clone()));
        let print_limits = shared_limits.clone();
        env.set(
            "print",
            vm.create_function(move |_, args: mlua::Variadic<Value>| {
                let print_limits = print_limits.lock().unwrap().clone();
                print_limits.check()?;
                let mut o = out2.lock().unwrap();
                if o.streaming {
                    o.buffer.clear();
                    for (index, v) in args.iter().enumerate() {
                        let part = v.to_string()?;
                        let tab = usize::from(index > 0);
                        let next = o
                            .streamed_bytes
                            .saturating_add(o.buffer.len())
                            .saturating_add(tab)
                            .saturating_add(part.len());
                        print_limits.output_size(next)?;
                        if index > 0 {
                            o.buffer.push('\t');
                        }
                        o.buffer.push_str(&part);
                    }
                    let next = o.streamed_bytes.saturating_add(o.buffer.len()).saturating_add(1);
                    print_limits.output_size(next)?;
                    o.buffer.push('\n');
                    super::wire::output(&o.buffer, print_limits.output_bytes.saturating_mul(6).saturating_add(65536))?;
                    o.streamed_bytes = o.streamed_bytes.saturating_add(o.buffer.len());
                    o.buffer.clear();
                } else {
                    for (index, v) in args.iter().enumerate() {
                        let part = v.to_string()?;
                        let tab = usize::from(index > 0);
                        print_limits.output_size(o.buffer.len().saturating_add(tab).saturating_add(part.len()))?;
                        if index > 0 {
                            o.buffer.push('\t');
                        }
                        o.buffer.push_str(&part);
                    }
                    print_limits.output_size(o.buffer.len().saturating_add(1))?;
                    o.buffer.push('\n');
                }
                Ok(())
            })?,
        )?;
        let json = vm.create_table()?;
        let encode_limits = shared_limits.clone();
        json.set(
            "encode",
            vm.create_function(move |lua, v: Value| {
                let encode_limits = encode_limits.lock().unwrap().clone();
                let text = serde_json::to_string(&strict_lua_to_json(lua, v)?).map_err(mlua::Error::external)?;
                encode_limits.message_size(text.len())?;
                Ok(text)
            })?,
        )?;
        let decode_limits = shared_limits.clone();
        json.set(
            "decode",
            vm.create_function(move |lua, s: String| {
                let decode_limits = decode_limits.lock().unwrap().clone();
                decode_limits.message_size(s.len())?;
                let value: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
                json_to_lua_lossless(lua, &value)
            })?,
        )?;
        json.set("null", vm.null())?;
        json.set(
            "array",
            vm.create_function(|lua, t: Option<Table>| {
                let t = t.unwrap_or(lua.create_table()?);
                t.set_metatable(Some(lua.array_metatable()))?;
                Ok(t)
            })?,
        )?;
        env.set("json", json)?;
        let mut exposed_names = vec![];
        if let Some(exp) = opts.as_ref().map(|o| o.get::<Option<Table>>("expose")).transpose()?.flatten() {
            let exposed = bridge::expose(parent, &vm, &exp, &limits)?;
            for p in exposed.pairs::<String, Value>() {
                let (name, value) = p?;
                if name == "_G" || !matches!(env.raw_get::<Value>(name.as_str())?, Value::Nil) {
                    return Err(mlua::Error::runtime(format!("sandbox: reserved exposure name {name}")));
                }
                exposed_names.push(name.clone());
                env.set(name, value)?;
            }
        }

        env.set("_G", env.clone())?;
        Ok(Self {
            vm,
            env,
            output,
            limits: shared_limits,
            persistent: opts
                .as_ref()
                .map(|o| o.get::<Option<bool>>("persistent"))
                .transpose()?
                .flatten()
                .unwrap_or(false),
            exposed: Mutex::new(exposed_names),
            epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            used: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
        })
    }
}
impl UserData for Sandbox {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("configure", |parent, this, opts: Table| {
            let limits = Limits::new(&Some(opts.clone()))?;
            this.vm.set_memory_limit(limits.memory)?;
            for name in this.exposed.lock().unwrap().drain(..) {
                this.env.raw_remove(name)?;
            }
            if let Some(exp) = opts.get::<Option<Table>>("expose")? {
                let exposed = bridge::expose(parent, &this.vm, &exp, &limits)?;
                for pair in exposed.pairs::<String, Value>() {
                    let (name, value) = pair?;
                    if name == "_G"
                        || matches!(name.as_str(), "setTimeout" | "clearTimeout" | "sleep")
                        || !matches!(this.env.raw_get::<Value>(name.as_str())?, Value::Nil)
                    {
                        return Err(mlua::Error::runtime("reserved exposure name"));
                    }
                    this.exposed.lock().unwrap().push(name.clone());
                    this.env.set(name, value)?;
                }
            }
            *this.limits.lock().unwrap() = limits;
            Ok(())
        });
        m.add_method("stop", |_, this, ()| {
            this.terminated.store(true, Ordering::SeqCst);
            this.limits.lock().unwrap().stop("cancelled");
            Ok(())
        });
        m.add_method("terminated", |_, this, ()| Ok(this.terminated.load(Ordering::SeqCst)));
        m.add_async_method("run", |lua, this, code: String| async move {
            this.terminated.store(false, Ordering::SeqCst);
            if !this.persistent && this.used.swap(true, Ordering::SeqCst) {
                return nil_err(&lua, "sandbox: VM is single-use");
            }
            let limits = this.limits.lock().unwrap().clone();
            let completion =
                lua.app_data_ref::<super::completion::Signal>().map(|signal| signal.0.clone()).unwrap_or_default();
            this.output.lock().unwrap().reset();
            let epoch = this.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let timers = super::timers::install(&this.vm, &this.env, limits.clone(), this.epoch.clone(), epoch)?;
            struct TimerScope(super::timers::Timers);
            impl Drop for TimerScope {
                fn drop(&mut self) {
                    self.0.clear();
                }
            }
            let _timers = TimerScope(timers);
            if let Err(e) = limits.size(code.len()) {
                return nil_err(&lua, e);
            }
            let deadline = Instant::now() + limits.timeout;
            let hook_limits = limits.clone();
            let instructions = Arc::new(std::sync::atomic::AtomicU64::new(0));
            this.vm.set_global_hook(HookTriggers::new().every_nth_instruction(1_000), move |_, debug| {
                if instructions.fetch_add(1_000, Ordering::Relaxed) >= hook_limits.instructions {
                    hook_limits.stop("instruction limit exceeded");
                }
                if hook_limits.deadline_enabled && Instant::now() >= deadline {
                    hook_limits.stop("time limit exceeded");
                }
                if hook_limits.cancel.is_cancelled() {
                    hook_limits.stop("cancelled");
                }
                // Never hook-yield inside mlua's `=__mlua_async_poll` wrapper while still running:
                // yielding between `get_future`/`poll` returning `Poll::Pending` and `yield(PENDING)`
                // lets the underlying future wake the task while the hook-yield wake is already
                // queued, coalescing both wakes so the subsequent `yield(PENDING)` parks forever.
                if !hook_limits.stopped.is_cancelled() && debug.source().source.as_deref() == Some("=__mlua_async_poll")
                {
                    return Ok(VmState::Continue);
                }
                // Yield even when stopped: a Lua pcall must not catch a terminal limit and
                // keep executing forever. The host observes the sticky token before resume.
                Ok(VmState::Yield)
            })?;
            let f: Function = match this
                .vm
                .load(&code)
                .set_mode(mlua::chunk::ChunkMode::Text)
                .set_name("=sandbox")
                .set_environment(this.env.clone())
                .into_function()
            {
                Ok(f) => f,
                Err(e) => {
                    this.vm.remove_global_hook();
                    return nil_err(&lua, e);
                }
            };
            let timeout = async {
                if limits.deadline_enabled {
                    tokio::time::sleep(limits.timeout).await
                } else {
                    std::future::pending::<()>().await
                }
            };
            let result = tokio::select! {biased;
                _=limits.cancel.cancelled()=>{limits.stop("cancelled");Err(mlua::Error::runtime(limits.message()))},
                _=limits.stopped.cancelled()=>Err(mlua::Error::runtime(limits.message())),
                _=timeout=>{limits.stop("time limit exceeded");Err(mlua::Error::runtime(limits.message()))},
                _=completion.cancelled()=>Ok(MultiValue::new()),
                result=f.call_async::<MultiValue>(())=>result,
            };
            this.vm.remove_global_hook();
            let output = {
                let o = this.output.lock().unwrap();
                (!o.streaming).then(|| o.buffer.clone())
            };
            if let Err(e) = limits.check() {
                this.terminated.store(true, Ordering::SeqCst);
                return nil_err(&lua, e);
            }
            if matches!(result, Err(mlua::Error::MemoryError(_))) {
                this.terminated.store(true, Ordering::SeqCst);
            }
            match result {
                Ok(vals) => {
                    let mut values = Vec::new();
                    for v in vals {
                        let value = match if matches!(v, Value::Nil) {
                            Ok(serde_json::Value::Null)
                        } else {
                            strict_lua_to_json(&this.vm, v)
                        } {
                            Ok(v) => v,
                            Err(e) => return nil_err(&lua, e),
                        };
                        values.push(value);
                    }
                    let value = match values.len() {
                        0 => serde_json::Value::Null,
                        1 => values.remove(0),
                        _ => serde_json::Value::Array(values),
                    };
                    let t = lua.create_table()?;
                    t.set("result", json_to_lua_lossless(&lua, &value)?)?;
                    if let Some(output) = output {
                        t.set("output", output)?;
                    }
                    Ok(MultiValue::from_vec(vec![Value::Table(t)]))
                }
                Err(e) => nil_err(&lua, e),
            }
        });
    }
}
pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set("new", lua.create_function(|lua, opts: Option<Table>| Sandbox::new(lua, opts))?)?;
    mare.set("sandbox", t)
}
