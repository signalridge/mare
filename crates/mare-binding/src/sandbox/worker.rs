//! No user config or credentials are loaded. Replies are routed by invocation id, so
//! cancelled timer callbacks cannot consume another callback's response.
use super::{
    local,
    wire::{self, Capability, HostFrame, Reply, Response},
};
use mare_shared::lua::{json_to_lua_lossless, strict_lua_to_json};
use mlua::{Lua, MultiValue, ObjectLike, Table, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};
use tokio::{
    io::{BufReader, Stdout},
    sync::oneshot,
};
pub(super) struct StreamOutput;
type Pending = Rc<RefCell<HashMap<u64, oneshot::Sender<Reply>>>>;
#[derive(Clone)]
struct Channel {
    output: Rc<tokio::sync::Mutex<Stdout>>,
    pending: Pending,
    sequence: Rc<Cell<u64>>,
    max: usize,
    pending_limit: Rc<Cell<usize>>,
}
struct PendingGuard(Pending, u64);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.borrow_mut().remove(&self.1);
    }
}
fn expose(lua: &Lua, cap: Capability, channel: &Channel, generation: u64) -> mlua::Result<Value> {
    match cap {
        Capability::Value { value } => json_to_lua_lossless(lua, &value),
        Capability::Table { entries } => {
            let table = lua.create_table()?;
            for (name, value) in entries {
                table.set(name, expose(lua, value, channel, generation)?)?;
            }
            Ok(Value::Table(table))
        }
        Capability::Function { id } => {
            let channel = channel.clone();
            Ok(Value::Function(lua.create_async_function(move |lua, args: MultiValue| {
                let channel = channel.clone();
                async move {
                    if channel.pending.borrow().len() >= channel.pending_limit.get() {
                        return Err(mlua::Error::runtime("sandbox pending callback limit exceeded"));
                    }
                    let args =
                        args.into_iter().map(|v| strict_lua_to_json(&lua, v)).collect::<mlua::Result<Vec<_>>>()?;
                    let call_id = channel.sequence.get() + 1;
                    channel.sequence.set(call_id);
                    let (tx, rx) = oneshot::channel();
                    channel.pending.borrow_mut().insert(call_id, tx);
                    let _guard = PendingGuard(channel.pending.clone(), call_id);
                    wire::write(
                        &mut *channel.output.lock().await,
                        &Response::Call { generation, call_id, id, args },
                        channel.max,
                    )
                    .await
                    .map_err(mlua::Error::external)?;
                    let reply = rx.await.map_err(mlua::Error::external)?;
                    if reply.complete {
                        let signal = lua.app_data_ref::<super::completion::Signal>().map(|signal| signal.0.clone());
                        if let Some(signal) = signal {
                            signal.cancel();
                            // Never resume Lua after completion, even through pcall. The owning
                            // run select drops this callback future and its pending guard.
                            return std::future::pending::<mlua::Result<MultiValue>>().await;
                        }
                        return Err(mlua::Error::runtime("sandbox completion outside execution"));
                    }
                    if let Some(error) = reply.error {
                        return Err(mlua::Error::runtime(error));
                    }
                    reply.values.iter().map(|v| json_to_lua_lossless(&lua, v)).collect::<mlua::Result<MultiValue>>()
                }
            })?))
        }
    }
}
pub(super) async fn run() -> anyhow::Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let first: HostFrame = wire::read(&mut input, 128 * 1024 * 1024).await?;
    let HostFrame::Start { request: first } = first else { anyhow::bail!("sandbox expected start frame") };
    let channel = Channel {
        output: Rc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
        pending: Rc::new(RefCell::new(HashMap::new())),
        sequence: Rc::new(Cell::new(0)),
        max: 128 * 1024 * 1024,
        pending_limit: Rc::new(Cell::new(128)),
    };
    let pending = channel.pending.clone();
    let (starts_tx, mut starts) = tokio::sync::mpsc::channel(1);
    let reader = mare_shared::txn_scope::spawn_local(async move {
        let mut buf = wire::ReadBuf::new();
        while let Ok(frame) = wire::read_with::<HostFrame>(&mut input, 128 * 1024 * 1024, &mut buf).await {
            match frame {
                HostFrame::Reply { reply } => {
                    if let Some(sender) = pending.borrow_mut().remove(&reply.call_id) {
                        let _ = sender.send(reply);
                    }
                }
                HostFrame::Start { request } => {
                    if starts_tx.send(request).await.is_err() {
                        break;
                    }
                }
            }
        }
        pending.borrow_mut().clear();
    });
    let lua = Lua::new();
    lua.set_app_data(StreamOutput);
    let mare = lua.create_table()?;
    local::install(&lua, &mare)?;
    let api: Table = mare.get("sandbox")?;
    let mut vm: Option<mlua::AnyUserData> = None;
    let mut request = first;
    loop {
        anyhow::ensure!(request.version == 4, "unsupported sandbox protocol");
        let max = request.max_output_bytes.max(request.max_message_bytes).saturating_mul(6).saturating_add(65536);
        channel.pending_limit.set(request.max_pending_calls);
        lua.set_app_data(super::completion::Signal::default());
        let opts = lua.create_table()?;
        opts.set("persistent", request.persistent)?;
        if let Some(timeout) = request.timeout_ms {
            opts.set("timeout_ms", timeout)?;
        } else {
            opts.set("timeout_ms", false)?;
        }
        opts.set("memory_bytes", request.memory_bytes)?;
        opts.set("max_bytes", request.max_bytes)?;
        opts.set("max_message_bytes", request.max_message_bytes)?;
        opts.set("max_output_bytes", request.max_output_bytes)?;
        opts.set("max_pending_calls", request.max_pending_calls)?;
        opts.set("max_instructions", request.max_instructions)?;
        opts.set("expose", expose(&lua, request.expose, &channel, request.generation)?)?;
        if let Some(vm) = &vm {
            vm.call_method::<()>("configure", opts)?;
        } else {
            vm = Some(api.get::<mlua::Function>("new")?.call(opts)?);
        }
        let (value, error): (Option<Table>, Option<String>) =
            vm.as_ref().unwrap().call_async_method("run", request.code).await?;
        let terminated: bool = vm.as_ref().unwrap().call_method("terminated", ())?;
        let value = value.map(|value| strict_lua_to_json(&lua, Value::Table(value))).transpose()?;
        channel.pending.borrow_mut().clear();
        wire::write(&mut *channel.output.lock().await, &Response::Done { value, error, terminated }, max).await?;
        if !request.persistent || terminated {
            break;
        }
        match starts.recv().await {
            Some(next) => request = next,
            None => break,
        }
    }
    reader.abort();
    channel.pending.borrow_mut().clear();
    Ok(())
}
