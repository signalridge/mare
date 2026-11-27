//! `mare.task`: spawn, channel, select, sleep, cancellation tokens.

use std::time::Duration;

use futures_util::future::select_all;
use mlua::{AnyUserData, Function, Lua, MultiValue, ObjectLike, Table, UserData, UserDataMethods, Value};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

pub use mare_shared::lua::{TaskHandle, Token};

/// A dropped select must not detach any of the tasks it created.
struct AbortBranches(Vec<AbortHandle>);

impl Drop for AbortBranches {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Extract an optional cancellation token from an options table field `cancel`.
pub fn opt_token(opts: &Option<Table>) -> Option<CancellationToken> {
    let t = opts.as_ref()?;
    let ud: AnyUserData = t.get::<Option<AnyUserData>>("cancel").ok().flatten()?;
    let tok = ud.borrow::<Token>().ok()?;
    Some(tok.0.clone())
}

/// Extract an optional cancellation token from an options table, rejecting non-token `cancel` values.
pub fn checked_token(opts: &Table) -> mlua::Result<Option<CancellationToken>> {
    opts.get::<Option<AnyUserData>>("cancel")?.map(|ud| Ok(ud.borrow::<Token>()?.0.clone())).transpose()
}

pub struct Sender {
    tx: mpsc::UnboundedSender<Value>,
    closed: CancellationToken,
}
pub struct Receiver {
    rx: AsyncMutex<mpsc::UnboundedReceiver<Value>>,
    closed: CancellationToken,
}
pub struct BoundedSender {
    tx: mpsc::Sender<Value>,
    closed: CancellationToken,
}
pub struct BoundedReceiver {
    rx: AsyncMutex<mpsc::Receiver<Value>>,
    closed: CancellationToken,
}

impl UserData for Sender {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("send", |_, this, v: Value| {
            if this.closed.is_cancelled() {
                return Ok(false);
            }
            Ok(this.tx.send(v).is_ok())
        });
        m.add_method("is_closed", |_, this, ()| Ok(this.closed.is_cancelled() || this.tx.is_closed()));
    }
}

impl UserData for Receiver {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("recv", |_, this, ()| async move {
            let mut rx = this.rx.lock().await;
            if this.closed.is_cancelled() {
                rx.close();
                return Ok(rx.try_recv().ok());
            }
            tokio::select! {
                biased;
                msg = rx.recv() => Ok(msg),
                _ = this.closed.cancelled() => {
                    rx.close();
                    Ok(rx.try_recv().ok())
                }
            }
        });
        m.add_method("try_recv", |_, this, ()| match this.rx.try_lock() {
            Ok(mut rx) => Ok(rx.try_recv().ok()),
            Err(_) => Ok(None),
        });
        m.add_method("close", |_, this, ()| {
            this.closed.cancel();
            if let Ok(mut rx) = this.rx.try_lock() {
                rx.close();
            }
            Ok(())
        });
    }
}

impl UserData for BoundedSender {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("send", |_, this, v: Value| {
            let tx = this.tx.clone();
            let closed = this.closed.clone();
            async move {
                if closed.is_cancelled() {
                    return Ok(false);
                }
                tokio::select! {
                    biased;
                    _ = closed.cancelled() => Ok(false),
                    res = tx.send(v) => Ok(res.is_ok()),
                }
            }
        });
        m.add_method("try_send", |_, this, v: Value| {
            if this.closed.is_cancelled() {
                return Ok(false);
            }
            Ok(this.tx.try_send(v).is_ok())
        });
        m.add_method("is_closed", |_, this, ()| Ok(this.closed.is_cancelled() || this.tx.is_closed()));
    }
}

impl UserData for BoundedReceiver {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("recv", |_, this, ()| async move {
            let mut rx = this.rx.lock().await;
            if this.closed.is_cancelled() {
                rx.close();
                return Ok(rx.try_recv().ok());
            }
            tokio::select! {
                biased;
                msg = rx.recv() => Ok(msg),
                _ = this.closed.cancelled() => {
                    rx.close();
                    Ok(rx.try_recv().ok())
                }
            }
        });
        m.add_method("try_recv", |_, this, ()| match this.rx.try_lock() {
            Ok(mut rx) => Ok(rx.try_recv().ok()),
            Err(_) => Ok(None),
        });
        m.add_method("close", |_, this, ()| {
            this.closed.cancel();
            if let Ok(mut rx) = this.rx.try_lock() {
                rx.close();
            }
            Ok(())
        });
    }
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set("spawn", lua.create_function(|lua, (f, args): (Function, MultiValue)| TaskHandle::spawn(lua, f, args))?)?;
    // Public synchronous boundary: a synchronous Lua call cannot suspend across this Rust
    // frame. Used where yielding would expose half-published state.
    t.set("call_sync", lua.create_function(|_, (f, args): (Function, MultiValue)| f.call::<MultiValue>(args))?)?;
    // Public host-only publication: poison before returning any thrown error or yield.
    t.set(
        "publish",
        lua.create_function(|lua, (f, args): (Function, MultiValue)| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(mlua::Error::runtime("session publication is unavailable inside a tool call"));
            }
            crate::exec_gate::check_unpoisoned(lua)?;
            match f.call::<MultiValue>(args) {
                Ok(value) => Ok(value),
                Err(err) => {
                    crate::exec_gate::poison(lua, format!("agent publication failed: {err}"));
                    Err(err)
                }
            }
        })?,
    )?;
    t.set(
        "fail_session",
        lua.create_function(|lua, reason: String| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(mlua::Error::runtime("session poisoning is unavailable inside a tool call"));
            }
            crate::exec_gate::poison(lua, reason);
            Ok(())
        })?,
    )?;
    t.set("session_error", lua.create_function(|lua, ()| Ok(crate::exec_gate::poison_reason(lua)))?)?;
    // Register completion with the plugin owner. Poison synchronously on failure so no
    // later frontend or tool admission can observe the committed state as ready.
    t.set(
        "spawn_completion",
        lua.create_function(|lua, (ctx, f): (Value, Function)| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(mlua::Error::runtime("session completion is unavailable inside a tool call"));
            }
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_completion_failure(lua) {
                let err = mlua::Error::runtime("injected session completion spawn failure");
                crate::exec_gate::poison(lua, err.to_string());
                return Err(err);
            }
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_completion_wrap_failure(lua) {
                mare_shared::lua::fail_next_task_wrap(lua);
            }
            // the plugin's ctx: a cordis ctx (or the transition layer's compatible one, both
            // tables whose `spawn` is the `task` host plugin's) or a kernel-held ctx userdata
            let spawned = match &ctx {
                Value::Table(t) => t.get::<Function>("spawn").and_then(|spawn| spawn.call::<Value>((t.clone(), f))),
                Value::UserData(ud) => ud.call_method::<Value>("spawn", f),
                other => Err(mlua::Error::runtime(format!(
                    "spawn_completion needs the plugin's ctx, got {}",
                    other.type_name()
                ))),
            };
            match spawned {
                Ok(task) => Ok(task),
                Err(err) => {
                    crate::exec_gate::poison(lua, format!("session completion could not start: {err}"));
                    Err(err)
                }
            }
        })?,
    )?;
    // sleep(ms [, token]) -> true if the full time elapsed, false if cancelled
    t.set(
        "sleep",
        lua.create_async_function(|_, (ms, tok): (f64, Option<mlua::UserDataRef<Token>>)| {
            let tok = tok.map(|t| t.0.clone());
            async move {
                let d = Duration::from_millis(ms.max(0.0) as u64);
                match tok {
                    Some(t) => Ok(tokio::select! {
                        biased;
                        _ = t.cancelled() => false,
                        _ = tokio::time::sleep(d) => true,
                    }),
                    None => {
                        tokio::time::sleep(d).await;
                        Ok(true)
                    }
                }
            }
        })?,
    )?;
    t.set(
        "channel",
        lua.create_function(|lua, cap: Option<i64>| match cap {
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                let closed = CancellationToken::new();
                Ok((
                    lua.create_userdata(Sender { tx, closed: closed.clone() })?,
                    lua.create_userdata(Receiver { rx: AsyncMutex::new(rx), closed })?,
                ))
            }
            Some(c) if c > 0 => {
                let (tx, rx) = mpsc::channel(c as usize);
                let closed = CancellationToken::new();
                Ok((
                    lua.create_userdata(BoundedSender { tx, closed: closed.clone() })?,
                    lua.create_userdata(BoundedReceiver { rx: AsyncMutex::new(rx), closed })?,
                ))
            }
            Some(_) => Err(mlua::Error::runtime("mare.task.channel: capacity must be a positive integer")),
        })?,
    )?;
    t.set("token", lua.create_function(|_, ()| Ok(Token(CancellationToken::new())))?)?;
    t.set(
        "is_token",
        lua.create_function(|_, value: Value| Ok(matches!(value, Value::UserData(ud) if ud.is::<Token>())))?,
    )?;
    // select(f1, f2, ...) -> index, results of the first function to finish; others are aborted
    t.set(
        "select",
        lua.create_async_function(|lua, fs: mlua::Variadic<Function>| {
            let ectx = mare_shared::exec_ctx::current(&lua);
            async move {
                if fs.is_empty() {
                    return Ok(MultiValue::new());
                }
                let handles: Vec<JoinHandle<mlua::Result<MultiValue>>> = fs
                    .into_iter()
                    .map(|f| {
                        let (lua, ectx) = (lua.clone(), ectx.clone());
                        mare_shared::txn_scope::spawn_local(async move {
                            mare_shared::exec_ctx::call_in::<MultiValue>(&lua, ectx, f, MultiValue::new()).await
                        })
                    })
                    .collect();
                let abort = AbortBranches(handles.iter().map(JoinHandle::abort_handle).collect());
                let (res, idx, rest) = select_all(handles).await;
                for h in &rest {
                    h.abort();
                }
                // Completion of a losing Lua task precedes the result, though separately spawned
                // tasks, blocking work, remote requests and OS side effects may outlive it.
                for h in rest {
                    let _ = h.await;
                }
                drop(abort);
                let vals = res.map_err(|e| mlua::Error::runtime(e.to_string()))??;
                let mut out = MultiValue::new();
                out.push_back(Value::Integer(idx as i64 + 1));
                out.extend(vals);
                Ok(out)
            }
        })?,
    )?;
    // timeout(ms, fn, ...) -> true, results | false
    t.set(
        "timeout",
        lua.create_async_function(|lua, (ms, f, args): (f64, Function, MultiValue)| {
            let ectx = mare_shared::exec_ctx::current(&lua);
            async move {
                let call = mare_shared::exec_ctx::call_in::<MultiValue>(&lua, ectx, f, args);
                match tokio::time::timeout(Duration::from_millis(ms as u64), call).await {
                    Ok(r) => {
                        let mut out = MultiValue::new();
                        out.push_back(Value::Boolean(true));
                        out.extend(r?);
                        Ok(out)
                    }
                    Err(_) => Ok(MultiValue::from_vec(vec![Value::Boolean(false)])),
                }
            }
        })?,
    )?;
    t.set("now", lua.create_function(|_, ()| Ok(now_ms()))?)?;
    // monotonic clock in ms (for durations)
    let start = std::time::Instant::now();
    t.set("clock", lua.create_function(move |_, ()| Ok(start.elapsed().as_secs_f64() * 1000.0))?)?;
    t.set("id", lua.create_function(|_, ()| Ok(tokio::task::try_id().map(|id| id.to_string())))?)?;
    t.set(
        "yield",
        lua.create_async_function(|_, ()| async move {
            tokio::task::yield_now().await;
            Ok(())
        })?,
    )?;
    mare.set("task", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn call_sync_returns_values_and_refuses_a_yield() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        assert!(mare.get::<mlua::Table>("task").unwrap().get::<mlua::Value>("_call_sync").unwrap().is_nil());
        lua.globals().set("mare", mare).unwrap();
        assert_eq!(lua.load("return mare.task.call_sync(function(a) return a + 1 end, 3)").eval::<i64>().unwrap(), 4);
        let err = lua.load("return mare.task.call_sync(function() coroutine.yield() end)").exec().unwrap_err();
        assert!(err.to_string().contains("yield"), "{err}");
    }

    #[test]
    fn public_publication_preserves_values_and_poison_on_error_or_yield() {
        for (body, diagnostic) in
            [("error('publication sentinel')", "publication sentinel"), ("coroutine.yield()", "yield")]
        {
            let lua = mlua::Lua::new();
            let mare = lua.create_table().unwrap();
            super::install(&lua, &mare).unwrap();
            let task: mlua::Table = mare.get("task").unwrap();
            for old in ["_session_guard", "_session_poison", "_session_poison_reason", "_session_spawn"] {
                assert!(task.get::<mlua::Value>(old).unwrap().is_nil(), "old alias {old}");
            }
            lua.globals().set("mare", mare).unwrap();
            let values: (i64, String) =
                lua.load("return mare.task.publish(function(a, b) return a, b end, 7, 'published')").eval().unwrap();
            assert_eq!(values, (7, "published".into()));
            let error = lua.load(format!("mare.task.publish(function() {body} end)")).exec().unwrap_err();
            assert!(error.to_string().contains(diagnostic), "{error}");
            let reason: String = lua.load("return mare.task.session_error()").eval().unwrap();
            assert!(reason.contains(diagnostic), "{reason}");
            let blocked = lua.load("mare.task.publish(function() ran_after_poison = true end)").exec();
            assert!(blocked.is_err());
            assert!(lua.globals().get::<mlua::Value>("ran_after_poison").unwrap().is_nil());
        }
    }

    #[tokio::test]
    async fn channel_close_wakes_concurrent_waiting_recv() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = mlua::Lua::new();
                let mare = lua.create_table().unwrap();
                super::install(&lua, &mare).unwrap();
                lua.globals().set("mare", mare).unwrap();

                let ok: bool = lua
                    .load(
                        r#"
                        for _, cap in ipairs({ false, 2 }) do
                            local tx, rx = mare.task.channel(cap or nil)
                            local entered_tx, entered_rx = mare.task.channel()
                            local recv_done = false
                            local recv_val = "sentinel"
                            local waiter = mare.task.spawn(function()
                                entered_tx:send(true)
                                recv_val = rx:recv()
                                recv_done = true
                            end)

                            assert(entered_rx:recv() == true)
                            for _ = 1, 10 do
                                mare.task.yield()
                            end
                            assert(not recv_done)

                            rx:close()
                            assert(tx:is_closed() == true, "is_closed must be true after close()")
                            assert(tx:send("after") == false, "send after close must return false")

                            local finished = mare.task.timeout(200, function()
                                waiter:wait()
                            end)
                            assert(finished, "close() must wake waiting recv()")
                            assert(recv_done and recv_val == nil)
                        end
                        return true
                        "#,
                    )
                    .eval_async()
                    .await
                    .unwrap();
                assert!(ok);
            })
            .await;
    }

    /// a plugin's ctx is a cordis ctx (a table whose `spawn` is the task host plugin's), so
    /// session completion starts through any ctx's `spawn`; something that is no ctx poisons.
    #[test]
    fn spawn_completion_starts_through_a_table_ctx_and_poisons_without_one() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        let started: String = lua
            .load(
                r#"local ctx = { spawn = function(self, f) self.started = f() return "task" end }
                local task = mare.task.spawn_completion(ctx, function() return "ran" end)
                return task .. ":" .. ctx.started"#,
            )
            .eval()
            .unwrap();
        assert_eq!(started, "task:ran");
        assert!(lua.load("return mare.task.session_error()").eval::<mlua::Value>().unwrap().is_nil());
        let err = lua.load("mare.task.spawn_completion(42, function() end)").exec().unwrap_err();
        assert!(err.to_string().contains("needs the plugin's ctx"), "{err}");
        let reason: String = lua.load("return mare.task.session_error()").eval().unwrap();
        assert!(reason.contains("could not start"), "{reason}");
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_prefers_cancel_when_deadline_and_cancel_are_both_ready() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let lua = mlua::Lua::new();
                let mare = lua.create_table().unwrap();
                super::install(&lua, &mare).unwrap();
                lua.globals().set("mare", mare).unwrap();
                let sleep_fn: mlua::Function = lua.load("return mare.task.sleep").eval().unwrap();
                for i in 0..30 {
                    let tok = super::CancellationToken::new();
                    let ud = lua.create_userdata(super::Token(tok.clone())).unwrap();
                    let f = sleep_fn.clone();
                    let handle =
                        tokio::task::spawn_local(async move { f.call_async::<bool>((50.0, ud)).await.unwrap() });
                    tokio::task::yield_now().await;
                    tok.cancel();
                    tokio::time::advance(super::Duration::from_millis(100)).await;
                    let completed = handle.await.unwrap();
                    assert!(!completed, "iteration {i}: sleep reported elapsed despite simultaneous cancel");
                }
            })
            .await;
    }
}
