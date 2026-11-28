//! Key bindings: resolving a key along a context lookup chain. The chain is walked
//! in priority order, skipping bindings whose `when` doesn't hold; an action that declines
//! (`run_action` → false) passes the key to the next binding, and when the chain runs out (or hits
//! an unbound mask, or stops after `text` on an unmodified printable key) the focus's fallback gets
//! it. Typing and unbound keys never reach Lua: only a key bound to a Lua action calls
//! into it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use mare_shared::bindings::{COMPLETION_OPEN, EDITOR_EMPTY, KITTY_KEYBOARD, PASTE_BURST, WORKING};

use crate::app::*;

/// A Lua action's part before its first yield should take well under 1 ms; past half a 60 Hz
/// frame it is logged, once per action (nothing interrupts it: Lua has no cheap preemption).
const SLOW_ACTION: Duration = Duration::from_millis(8);

thread_local! {
    static SLOW_LOGGED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchOutcome {
    Handled,
    Masked,
    Unhandled,
}

impl App {
    /// Run `key`'s bindings along `chain`, distinguishing handled, masked (`false`), and unhandled.
    fn dispatch_bindings(&mut self, chain: &[&str], key: &str, printable: bool, queue: bool) -> DispatchOutcome {
        let queue = queue && self.ui.queued.load(std::sync::atomic::Ordering::Relaxed) > 0;
        let state = if self.keymap.chain_needs_state(chain, key) { self.key_state() } else { 0 };
        let (raw, masked) = self.keymap.chain_resolve(chain, key, state, printable);
        let candidates: Vec<(String, String)> = raw.into_iter().map(|(c, a)| (c.to_string(), a.to_string())).collect();
        for (ctx, action) in &candidates {
            if queue && self.route_queue_action(Some(action)) {
                return DispatchOutcome::Handled;
            }
            if self.run_action_on(action, Some((key, ctx))) {
                if queue {
                    self.route_queue_action(None);
                }
                return DispatchOutcome::Handled;
            }
        }
        if queue {
            self.route_queue_action(None);
        }
        if masked { DispatchOutcome::Masked } else { DispatchOutcome::Unhandled }
    }

    /// Run `key`'s bindings along `chain`; true when handled or stopped by an `unbound` mask.
    pub(in crate::app) fn dispatch_bindings_stops(&mut self, chain: &[&str], key: &str, printable: bool) -> bool {
        self.dispatch_bindings(chain, key, printable, false) != DispatchOutcome::Unhandled
    }

    pub(super) fn dispatch_editor_bindings_stops(&mut self, chain: &[&str], key: &str) -> bool {
        self.dispatch_bindings(chain, key, false, true) != DispatchOutcome::Unhandled
    }

    /// Run `key`'s bindings along `chain`; false when none took it.
    pub(in crate::app) fn run_bindings(&mut self, chain: &[&str], key: &str, printable: bool) -> bool {
        self.dispatch_bindings(chain, key, printable, false) == DispatchOutcome::Handled
    }

    /// The `when` predicates that hold now, as bits (`mare_shared::bindings`).
    pub(in crate::app) fn key_state(&self) -> u8 {
        let bit = |on: bool, b: u8| if on { b } else { 0 };
        let kitty_keyboard =
            self.ui.caps.read().unwrap().get("kitty_keyboard").and_then(|v| v.as_bool()).unwrap_or(false);
        bit(self.ed().is_empty(), EDITOR_EMPTY)
            | bit(self.completion.is_open(), COMPLETION_OPEN)
            | bit(self.burst_enabled && self.burst.active(mare_tui::render::clock_now()), PASTE_BURST)
            | bit(self.working.is_some(), WORKING)
            | bit(kitty_keyboard, KITTY_KEYBOARD)
    }

    /// An action that isn't a core one: a Lua action (`mare.keymap.action`, or a function bound
    /// with `set`) runs now. False when the Lua action declined or no such action is registered.
    pub(super) fn lua_action(&mut self, action: &str, key: Option<(&str, &str)>) -> bool {
        let res = self.invoke_lua(action, key);
        self.keymap.sync_from(&self.ui.keymap.read().unwrap());
        self.sync_editor_state();
        match res {
            Ok(Some(taken)) => taken,
            // a function binding whose function is gone: the key is still taken
            Ok(None) if action.starts_with("fn:") => true,
            Ok(None) => false,
            Err(e) => {
                self.push_notice((format!("keymap: {e}"), "error".into(), Instant::now()));
                true
            }
        }
    }

    /// `mare.keymap._invoke(action, ev)`, run synchronously up to its first yield (poll once with
    /// a no-op waker): what it returned decides the key, like a Neovim `expr` mapping or a DOM
    /// handler's `preventDefault`. An action still pending (it waits on a dialog or a timer) has
    /// taken the key, and the rest of it runs as a task, which polls it again with a real waker.
    fn invoke_lua(&self, action: &str, key: Option<(&str, &str)>) -> mlua::Result<Option<bool>> {
        let lua = self.host.lua();
        let invoke = mare_shared::lua::core::keymap::invoke_fn(lua)?;
        let ev = lua.create_table()?;
        if let Some((k, context)) = key {
            ev.set("key", k)?;
            ev.set("context", context)?;
        }
        let started = Instant::now();
        let mut call = Box::pin(invoke.call_async::<Option<bool>>((action, ev)));
        let polled = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        let took = started.elapsed();
        if took > SLOW_ACTION && SLOW_LOGGED.with(|s| s.borrow_mut().insert(action.to_string())) {
            tracing::warn!("keymap action {action} took {took:?} before its first yield (keep it under 1 ms)");
        }
        match polled {
            Poll::Ready(r) => r,
            Poll::Pending => {
                tokio::task::spawn_local(async move {
                    if let Err(e) = call.await {
                        tracing::warn!("keymap action: {e}");
                    }
                });
                Ok(Some(true))
            }
        }
    }
}
