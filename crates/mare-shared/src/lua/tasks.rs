use mlua::{AnyUserData, Function, Lua, MultiValue, UserData, UserDataMethods};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

pub struct TaskHandle {
    handle: AsyncMutex<Option<JoinHandle<()>>>,
    pub(super) abort: tokio::task::AbortHandle,
    result: std::sync::Mutex<Option<mlua::Result<MultiValue>>>,
    result_rx: AsyncMutex<Option<tokio::sync::oneshot::Receiver<mlua::Result<MultiValue>>>>,
}

#[cfg(debug_assertions)]
struct FailNextTaskWrap(std::cell::Cell<bool>);

#[cfg(debug_assertions)]
pub fn fail_next_task_wrap(lua: &Lua) {
    lua.set_app_data(FailNextTaskWrap(std::cell::Cell::new(true)));
}

impl TaskHandle {
    pub fn wrap(lua: &Lua, h: JoinHandle<()>) -> mlua::Result<AnyUserData> {
        let abort = h.abort_handle();
        Self::finish(
            lua,
            Self {
                handle: AsyncMutex::new(Some(h)),
                abort: abort.clone(),
                result: std::sync::Mutex::new(None),
                result_rx: AsyncMutex::new(None),
            },
            &abort,
        )
    }

    // the one allocation boundary of both paths: dropping a JoinHandle detaches its task, so if
    // allocating the Lua handle fails (injected or real) a registered task must not continue
    // without an owner able to observe it
    fn finish(lua: &Lua, task: Self, abort: &tokio::task::AbortHandle) -> mlua::Result<AnyUserData> {
        #[cfg(debug_assertions)]
        let fail = lua.app_data_ref::<FailNextTaskWrap>().is_some_and(|fail| fail.0.replace(false));
        #[cfg(not(debug_assertions))]
        let fail = false;
        let result = if fail {
            drop(task);
            Err(mlua::Error::runtime("injected task handle allocation failure"))
        } else {
            lua.create_userdata(task)
        };
        if result.is_err() {
            abort.abort();
        }
        result
    }

    pub fn spawn(lua: &Lua, f: Function, args: MultiValue) -> mlua::Result<AnyUserData> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        // work spawned from a tool call stays inside its execution context
        let inherited = match crate::exec_ctx::current(lua) {
            Some(ctx) => Some(crate::exec_ctx::scoped_call(lua, Some(ctx), f.clone(), args.clone())?),
            None => None,
        };
        let h = crate::txn_scope::spawn_local(async move {
            let r = match inherited {
                Some(call) => call.await,
                None => f.call_async::<MultiValue>(args).await,
            };
            if let Err(e) = &r {
                tracing::debug!("task error: {e}");
            }
            let _ = tx.send(r);
        });
        let abort = h.abort_handle();
        // the cordis tree's `ctx:spawn` comes here through `mare.task.spawn`
        Self::finish(
            lua,
            Self {
                handle: AsyncMutex::new(Some(h)),
                abort: abort.clone(),
                result: std::sync::Mutex::new(None),
                result_rx: AsyncMutex::new(Some(rx)),
            },
            &abort,
        )
    }
}

impl UserData for TaskHandle {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // task:wait() -> results... (raises the task's error)
        m.add_async_method("wait", |_, this, ()| async move {
            // every waiter gets the result: the first takes the receiver and stores it, the others
            // (waiting on that lock meanwhile) read what it stored
            if let Some(rx) = this.result_rx.lock().await.take() {
                let r = rx.await.unwrap_or_else(|_| Err(mlua::Error::runtime("cancelled")));
                *this.result.lock().unwrap() = Some(r);
            } else {
                let stored = this.result.lock().unwrap().is_some();
                if !stored && let Some(h) = this.handle.lock().await.take() {
                    let _ = h.await;
                    return Ok(MultiValue::new());
                }
            }
            match this.result.lock().unwrap().as_ref() {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Ok(MultiValue::new()),
            }
        });
        m.add_method("abort", |_, this, ()| {
            this.abort.abort();
            Ok(())
        });
        m.add_method("done", |_, this, ()| Ok(this.abort.is_finished()));
    }
}

/// Lua-exposed wrapper around a `tokio_util::sync::CancellationToken`.
#[derive(Clone)]
pub struct Token(pub tokio_util::sync::CancellationToken);

impl UserData for Token {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("cancel", |_, this, ()| {
            this.0.cancel();
            Ok(())
        });
        m.add_method("is_cancelled", |_, this, ()| Ok(this.0.is_cancelled()));
        m.add_method("child", |_, this, ()| Ok(Self(this.0.child_token())));
        m.add_async_method("wait", |_, this, ()| async move {
            this.0.cancelled().await;
            Ok(())
        });
    }
}
