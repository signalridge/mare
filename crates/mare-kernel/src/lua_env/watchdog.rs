//! The main VM's watchdog against runaway plugin callbacks.

use mlua::Lua;

/// Watchdog for the main VM: a runaway plugin callback (e.g. an infinite loop) would freeze
/// the single-threaded runtime. The runtime stamps `progress` from a tick task (which can only
/// run while Lua is yielded); the instruction hook aborts the running Lua code when the stamp
/// has stayed stale for `limit` while Lua kept executing.
///
/// The abort can't be swallowed: once it fires, the coroutine that ran away is latched — every
/// call and every instruction it executes raises again, so a `pcall` (or a loop of them) only
/// delays the error to its next instruction outside the protected call, up to the callback's
/// caller. The latch clears when the runtime makes progress again (Lua yielded). Implemented as
/// a raw Lua hook (mlua's hook can't change its own mask from inside), installed on the main
/// state and inherited by every coroutine.
pub fn install_watchdog(
    lua: &Lua,
    limit: std::time::Duration,
) -> mlua::Result<std::sync::Arc<std::sync::atomic::AtomicU64>> {
    use std::sync::atomic::Ordering;
    let sample_every = mare_shared::profile::lua_sample_every();
    let hook_count = if sample_every > 0 && (sample_every as std::ffi::c_int) < WATCHDOG_EVERY {
        sample_every as std::ffi::c_int
    } else {
        WATCHDOG_EVERY
    };
    let w = std::sync::Arc::new(Watchdog {
        start: std::time::Instant::now(),
        progress: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        limit_ms: limit.as_millis() as u64,
        stall_since: std::sync::atomic::AtomicU64::new(0),
        latched: std::sync::atomic::AtomicUsize::new(0),
        hook_count,
        sample_every,
        sample_budget: std::sync::atomic::AtomicU32::new(0),
        disarmed: std::sync::atomic::AtomicBool::new(false),
    });
    // Disarm any previously installed watchdog BEFORE calling `exec_raw`: if the previous
    // watchdog was latched with `LUA_MASKCALL`, `exec_raw`'s internal `lua_pcallk` would
    // otherwise invoke the old hook and abort re-installation before overwriting `WATCHDOG_KEY`.
    // Keep the old `WatchdogHold` alive in app_data until after `exec_raw` replaces `WATCHDOG_KEY`.
    let prev_disarmed =
        lua.app_data_ref::<WatchdogHold>().map(|prev| (prev.w.clone(), prev.w.disarmed.swap(true, Ordering::AcqRel)));
    let ptr = std::sync::Arc::as_ptr(&w) as *mut std::ffi::c_void;
    let install_res = unsafe {
        lua.exec_raw::<()>((), |state| {
            mlua::ffi::lua_pushlightuserdata(state, ptr);
            mlua::ffi::lua_rawsetp(state, mlua::ffi::LUA_REGISTRYINDEX, &WATCHDOG_KEY as *const u8 as *const _);
            let mask = if mare_shared::lua::has_cov_hook_raw(state) {
                mlua::ffi::LUA_MASKCOUNT | mlua::ffi::LUA_MASKLINE
            } else {
                mlua::ffi::LUA_MASKCOUNT
            };
            mlua::ffi::lua_sethook(state, Some(watchdog_hook), mask, hook_count);
        })
    };
    if let Err(err) = install_res {
        if let Some((prev_w, was_disarmed)) = prev_disarmed {
            prev_w.disarmed.store(was_disarmed, Ordering::Release);
        }
        return Err(err);
    }
    // the tick task stores `start.elapsed()` here (it only runs while Lua is yielded)
    let stamp = w.progress.clone();
    let start = w.start;
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = mare_shared::txn_scope::spawn_local(async move {
        #[cfg(test)]
        let _active = ActiveTaskGuard::new();
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => break,
                _ = iv.tick() => {
                    stamp.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
            }
        }
    });
    // the VM owns the watchdog: the raw pointer stays valid as long as the hook can run,
    // and dropping/replacing WatchdogHold stops the background tick task.
    lua.set_app_data(WatchdogHold { w: w.clone(), cancel, task: Some(task) });
    Ok(w.progress.clone())
}

/// Stop the watchdog tick task and unhook Lua so nothing outlives a shut-down kernel.
pub(crate) async fn stop_watchdog(lua: &Lua) {
    use std::sync::atomic::Ordering;
    // Disarm FIRST while `WatchdogHold` is still alive: if the watchdog had latched with
    // `LUA_MASKCALL`, `exec_raw`'s internal `lua_pcallk` triggers `watchdog_hook` on entry.
    // With `disarmed == true`, the hook self-unhooks and returns cleanly instead of aborting
    // `exec_raw` and leaving a dangling raw pointer in `WATCHDOG_KEY` after `WatchdogHold` drops.
    if let Some(hold) = lua.app_data_ref::<WatchdogHold>() {
        hold.w.disarmed.store(true, Ordering::Release);
    }
    let mut restore_cov = false;
    let unhook_res = unsafe {
        lua.exec_raw::<()>((), |state| {
            mlua::ffi::lua_sethook(state, None, 0, 0);
            mlua::ffi::lua_pushnil(state);
            mlua::ffi::lua_rawsetp(state, mlua::ffi::LUA_REGISTRYINDEX, &WATCHDOG_KEY as *const u8 as *const _);
            restore_cov = mare_shared::lua::has_cov_hook_raw(state);
        })
    };
    debug_assert!(unhook_res.is_ok(), "stop_watchdog unhook failed: {unhook_res:?}");
    if restore_cov {
        mare_shared::lua::attach_cov_hook(lua, &lua.current_thread());
    }
    if let Some(mut hold) = lua.remove_app_data::<WatchdogHold>() {
        hold.w.disarmed.store(true, Ordering::Release);
        hold.cancel.cancel();
        if let Some(task) = hold.task.take() {
            let _ = task.await;
        }
        // If `exec_raw` failed (e.g. OOM under `lua_memory_mb`), `WATCHDOG_KEY` may still hold
        // the raw pointer in Lua's registry. Leak the Arc rather than freeing it under the hook.
        if unhook_res.is_err() {
            std::mem::forget(hold.w.clone());
        }
    }
}

const WATCHDOG_EVERY: std::ffi::c_int = 1_000_000;
static WATCHDOG_KEY: u8 = 0;

struct Watchdog {
    start: std::time::Instant,
    progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    limit_ms: u64,
    /// ms (since start) when Lua was first seen running without progress; 0 = not stalled
    stall_since: std::sync::atomic::AtomicU64,
    /// the coroutine (lua_State address) that was aborted, until the runtime makes progress
    latched: std::sync::atomic::AtomicUsize,
    /// active LUA_MASKCOUNT interval (`WATCHDOG_EVERY` normally, smaller under high-res sampling)
    hook_count: std::ffi::c_int,
    /// startup Lua stack sampling interval captured at install time (0 = disabled)
    sample_every: u32,
    /// instructions accumulated since last watchdog clock check when `hook_count < WATCHDOG_EVERY`
    sample_budget: std::sync::atomic::AtomicU32,
    /// set before unhooking/dropping so any re-entrant hook call self-unhooks without raising
    disarmed: std::sync::atomic::AtomicBool,
}

struct WatchdogHold {
    w: std::sync::Arc<Watchdog>,
    cancel: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WatchdogHold {
    fn drop(&mut self) {
        self.w.disarmed.store(true, std::sync::atomic::Ordering::Release);
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
static ACTIVE_WATCHDOG_TASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
struct ActiveTaskGuard;

#[cfg(test)]
impl ActiveTaskGuard {
    fn new() -> Self {
        ACTIVE_WATCHDOG_TASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for ActiveTaskGuard {
    fn drop(&mut self) {
        ACTIVE_WATCHDOG_TASKS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

unsafe extern "C-unwind" fn watchdog_hook(state: *mut mlua::ffi::lua_State, ar: *mut mlua::ffi::lua_Debug) {
    use mlua::ffi;
    use std::sync::atomic::Ordering;
    unsafe {
        if !ar.is_null() && (*ar).event == ffi::LUA_HOOKLINE as std::ffi::c_int {
            mare_shared::lua::dispatch_cov_line_hook(state, ar);
            return;
        }
        if ffi::lua_checkstack(state, 2) == 0 {
            return;
        }
        ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, &WATCHDOG_KEY as *const u8 as *const _);
        let p = ffi::lua_touserdata(state, -1) as *const Watchdog;
        ffi::lua_settop(state, -2);
        if p.is_null() {
            ffi::lua_sethook(state, None, 0, 0);
            return;
        }
        let w = &*p;
        if w.disarmed.load(Ordering::Acquire) {
            ffi::lua_sethook(state, None, 0, 0);
            return;
        }
        // When `--startuptime` enabled Lua-layer sampling, capture the active Lua frame here
        // (reusing the watchdog count hook, zero extra hooks when disabled).
        if w.sample_every > 0 && mare_shared::profile::lua_sample_every() > 0 {
            let mut dbg: ffi::lua_Debug = std::mem::zeroed();
            if ffi::lua_getstack(state, 0, &mut dbg) == 1 && ffi::lua_getinfo(state, c"nSl".as_ptr(), &mut dbg) != 0 {
                let source = if !dbg.source.is_null() {
                    std::ffi::CStr::from_ptr(dbg.source).to_string_lossy()
                } else {
                    std::borrow::Cow::Borrowed("?")
                };
                let line = if dbg.currentline > 0 { dbg.currentline } else { dbg.linedefined };
                let name = if !dbg.name.is_null() {
                    std::ffi::CStr::from_ptr(dbg.name).to_string_lossy()
                } else if dbg.linedefined > 0 {
                    std::borrow::Cow::Owned(format!("<L{}>", dbg.linedefined))
                } else {
                    std::borrow::Cow::Borrowed("<chunk>")
                };
                mare_shared::profile::record_lua_sample(&source, &name, line);
            }
            if w.hook_count < WATCHDOG_EVERY && w.latched.load(Ordering::Relaxed) != state as usize {
                let acc = w.sample_budget.fetch_add(w.hook_count as u32, Ordering::Relaxed) + w.hook_count as u32;
                if acc < WATCHDOG_EVERY as u32 {
                    return;
                }
                w.sample_budget.store(0, Ordering::Relaxed);
            }
        }
        let now = (w.start.elapsed().as_millis() as u64).max(1);
        let last = w.progress.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 1000 {
            // the runtime got control recently: nothing ran away (or it's over)
            w.stall_since.store(0, Ordering::Relaxed);
            w.latched.store(0, Ordering::Relaxed);
            let desired_mask = if mare_shared::lua::has_cov_hook_raw(state) {
                ffi::LUA_MASKCOUNT | ffi::LUA_MASKLINE
            } else {
                ffi::LUA_MASKCOUNT
            };
            let desired_count =
                if mare_shared::profile::lua_sample_every() > 0 { w.hook_count } else { WATCHDOG_EVERY };
            if ffi::lua_gethookmask(state) != desired_mask || ffi::lua_gethookcount(state) != desired_count {
                ffi::lua_sethook(state, Some(watchdog_hook), desired_mask, desired_count);
            }
            return;
        }
        let me = state as usize;
        let tripped = if w.latched.load(Ordering::Relaxed) == me {
            true
        } else {
            let since = match w.stall_since.load(Ordering::Relaxed) {
                0 => {
                    w.stall_since.store(now, Ordering::Relaxed);
                    now
                }
                t => t,
            };
            let limit_ms =
                if mare_shared::lua::has_cov_hook_raw(state) { w.limit_ms.saturating_mul(6) } else { w.limit_ms };
            if now - since > limit_ms {
                // the next stalled callback gets a full limit of its own
                w.stall_since.store(0, Ordering::Relaxed);
                w.latched.store(me, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        if !tripped {
            return;
        }
        // latched: raise on every call and instruction of this coroutine from now on
        ffi::lua_sethook(state, Some(watchdog_hook), ffi::LUA_MASKCOUNT | ffi::LUA_MASKCALL, 1);
        let reported_limit =
            if mare_shared::lua::has_cov_hook_raw(state) { w.limit_ms.saturating_mul(6) } else { w.limit_ms };
        let msg = format!("watchdog: Lua code ran for more than {reported_limit}ms without yielding (aborted)\0");
        ffi::lua_pushstring(state, msg.as_ptr() as *const std::ffi::c_char);
        drop(msg);
        ffi::lua_error(state);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    #[tokio::test]
    async fn watchdog_task_exits_on_stop_replace_and_vm_drop() {
        tokio::task::LocalSet::new()
            .run_until(async {
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 0);

                // 1. Installing starts one task; stop_watchdog awaits and cleans it up.
                let lua = Lua::new();
                install_watchdog(&lua, std::time::Duration::from_secs(5)).unwrap();
                tokio::task::yield_now().await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 1);

                stop_watchdog(&lua).await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 0);

                // 2. Re-installing replaces the previous WatchdogHold and stops the old task.
                install_watchdog(&lua, std::time::Duration::from_secs(5)).unwrap();
                tokio::task::yield_now().await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 1);

                install_watchdog(&lua, std::time::Duration::from_secs(5)).unwrap();
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 1);

                // 3. Dropping the Lua VM without stop_watchdog aborts and cleans up the task.
                drop(lua);
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn kernel_shutdown_stops_watchdog_task() {
        tokio::task::LocalSet::new()
            .run_until(async {
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 0);
                let k = crate::Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                install_watchdog(k.lua(), std::time::Duration::from_secs(5)).unwrap();
                tokio::task::yield_now().await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 1);

                k.shutdown().await;
                assert_eq!(ACTIVE_WATCHDOG_TASKS.load(Ordering::SeqCst), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn watchdog_preserves_lua_cov_line_hook() {
        tokio::task::LocalSet::new()
            .run_until(async {
                use std::sync::{Arc, Mutex};

                fn record(ctx: *mut std::ffi::c_void, src: &str, line: usize) {
                    let log = unsafe { &*(ctx as *const Mutex<Vec<(String, usize)>>) };
                    log.lock().unwrap().push((src.to_string(), line));
                }

                let lua = Lua::new();
                let log: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
                let ctx = Arc::as_ptr(&log) as *mut std::ffi::c_void;
                unsafe {
                    mare_shared::lua::install_cov_hook(&lua, record, ctx).unwrap();
                }

                install_watchdog(&lua, std::time::Duration::from_secs(5)).unwrap();

                // Main thread execution after install_watchdog must still record lines.
                lua.load("local x = 1\nlocal y = 2\nreturn x + y\n")
                    .set_name("@builtin/lua/agent/loop.lua")
                    .exec()
                    .unwrap();
                assert!(
                    log.lock().unwrap().iter().any(|(s, _)| s == "@builtin/lua/agent/loop.lua"),
                    "expected coverage lines after install_watchdog, got {:?}",
                    *log.lock().unwrap()
                );

                // Stopping watchdog must restore the coverage line hook.
                log.lock().unwrap().clear();
                stop_watchdog(&lua).await;
                lua.load("local z = 3\nreturn z\n").set_name("@builtin/lua/agent/service.lua").exec().unwrap();
                assert!(
                    log.lock().unwrap().iter().any(|(s, _)| s == "@builtin/lua/agent/service.lua"),
                    "expected coverage lines after stop_watchdog, got {:?}",
                    *log.lock().unwrap()
                );
            })
            .await;
    }

    /// An infinite pure-Lua loop is aborted promptly by the
    /// watchdog with a `watchdog:` error.
    #[tokio::test]
    async fn watchdog_aborts_infinite_lua_loop() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = Lua::new();
                install_watchdog(&lua, std::time::Duration::from_millis(200)).unwrap();
                tokio::task::yield_now().await;
                let started = std::time::Instant::now();
                let err = lua.load("while true do end").exec().unwrap_err();
                assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
                let msg = err.to_string();
                assert!(msg.contains("watchdog:"), "expected watchdog error, got: {msg}");
                stop_watchdog(&lua).await;
                // Regression check: stopping a latched watchdog must clear the Lua hook and
                // registry pointer (otherwise subsequent Lua calls or `Lua::drop` UAFs).
                let mut hook_cleared = false;
                let mut reg_cleared = false;
                unsafe {
                    lua.exec_raw::<()>((), |state| {
                        hook_cleared = mlua::ffi::lua_gethook(state).is_none();
                        mlua::ffi::lua_rawgetp(
                            state,
                            mlua::ffi::LUA_REGISTRYINDEX,
                            &WATCHDOG_KEY as *const u8 as *const _,
                        );
                        reg_cleared = mlua::ffi::lua_isnil(state, -1) != 0;
                        mlua::ffi::lua_settop(state, -2);
                    })
                    .unwrap();
                }
                assert!(hook_cleared, "stop_watchdog must unhook Lua even when latched");
                assert!(reg_cleared, "stop_watchdog must clear WATCHDOG_KEY even when latched");
                assert_eq!(lua.load("return 42").eval::<i64>().unwrap(), 42);
            })
            .await;
    }

    #[tokio::test]
    async fn watchdog_samples_lua_frames_when_startuptime_sampling_enabled() {
        tokio::task::LocalSet::new()
            .run_until(async {
                mare_shared::profile::enable_lua_sampling(100);
                let lua = Lua::new();
                install_watchdog(&lua, std::time::Duration::from_secs(5)).unwrap();
                lua.load(
                    r#"
                    local function hot_startup_fn()
                        local acc = 0
                        for i = 1, 2000 do acc = acc + i end
                        return acc
                    end
                    local r = hot_startup_fn()
                    return r
                    "#,
                )
                .set_name("@kernel/lua/mare_kernel/sample_probe.lua")
                .exec()
                .unwrap();
                stop_watchdog(&lua).await;
                mare_shared::profile::disable_lua_sampling();

                let samples = mare_shared::profile::lua_samples();
                assert!(
                    samples.iter().any(|s| {
                        s.bucket == "mk/"
                            && s.source.contains("sample_probe.lua")
                            && s.name == "hot_startup_fn"
                            && s.hits > 0
                    }),
                    "expected sampled mk/ frame from hot_startup_fn, got {samples:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn watchdog_sampling_handles_anonymous_chunks_budget_rollover_and_hook_restore() {
        tokio::task::LocalSet::new()
            .run_until(async {
                mare_shared::profile::enable_lua_sampling(100);
                let lua = Lua::new();
                install_watchdog(&lua, std::time::Duration::from_secs(5)).unwrap();
                // Top-level chunk + anonymous function exceed WATCHDOG_EVERY to cover <chunk>, <L..>,
                // and sample_budget rollover.
                lua.load(
                    r#"
                    local acc = 0
                    for i = 1, 80000 do acc = acc + i end
                    local anon = (function()
                        local s = 0
                        for i = 1, 80000 do s = s + i end
                        return s
                    end)
                    return acc + anon()
                    "#,
                )
                .set_name("@kernel/lua/mare_kernel/anon_probe.lua")
                .exec()
                .unwrap();

                // Disabling sampling while the watchdog remains active triggers hook count restoration.
                mare_shared::profile::disable_lua_sampling();
                lua.load("local s = 0; for i = 1, 150000 do s = s + i end; return s").exec().unwrap();

                // Clearing WATCHDOG_KEY while the hook is active causes watchdog_hook to self-unhook cleanly.
                unsafe {
                    lua.exec_raw::<()>((), |state| {
                        mlua::ffi::lua_pushnil(state);
                        mlua::ffi::lua_rawsetp(
                            state,
                            mlua::ffi::LUA_REGISTRYINDEX,
                            &WATCHDOG_KEY as *const u8 as *const _,
                        );
                    })
                    .unwrap();
                }
                lua.load("local s = 0; for i = 1, 150000 do s = s + i end; return s").exec().unwrap();
                stop_watchdog(&lua).await;
            })
            .await;
    }
}
