//! Durable-start gate for tool execution: while a writable session journal is
//! active in this VM, a tool call enters controlled execution only after that journal fsyncs its
//! start. Acknowledgements belong to the journal that wrote them, never to the whole VM.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use mare_shared::lua::rt_err;

#[derive(Default)]
struct State {
    next: u64,
    active: Option<u64>,
    journals: HashMap<u64, HashSet<String>>,
    #[cfg(debug_assertions)]
    fail_next_completion: bool,
    #[cfg(debug_assertions)]
    fail_completion_wrap: bool,
    #[cfg(debug_assertions)]
    fail_activations: usize,
    #[cfg(debug_assertions)]
    fail_append: bool,
    #[cfg(debug_assertions)]
    fail_close: bool,
    #[cfg(debug_assertions)]
    fail_discard_before_release: bool,
}

pub struct Gate {
    state: RefCell<State>,
    barrier: Rc<mare_shared::session_barrier::SessionBarrier>,
}

/// This VM's gate (created on first use).
pub fn gate(lua: &Lua) -> Rc<Gate> {
    if let Some(g) = lua.app_data_ref::<Rc<Gate>>() {
        return g.clone();
    }
    let g =
        Rc::new(Gate { state: RefCell::new(State::default()), barrier: mare_shared::session_barrier::barrier(lua) });
    lua.set_app_data(g.clone());
    g
}

/// Permanently stop new work in this VM after a session transaction loses its invariants.
/// Recovery requires a new host and Lua VM; plugin reload cannot clear this state.
pub fn poison(lua: &Lua, reason: impl Into<String>) {
    mare_shared::session_barrier::barrier(lua).poison(reason);
}

pub fn poison_reason(lua: &Lua) -> Option<String> {
    mare_shared::session_barrier::barrier(lua).reason()
}

/// Test-only injection for the session completion task registration boundary.
#[cfg(debug_assertions)]
pub fn fail_next_session_completion(lua: &Lua) {
    gate(lua).state.borrow_mut().fail_next_completion = true;
}

#[cfg(debug_assertions)]
pub fn fail_next_session_completion_wrap(lua: &Lua) {
    gate(lua).state.borrow_mut().fail_completion_wrap = true;
}

#[cfg(debug_assertions)]
pub(crate) fn take_completion_wrap_failure(lua: &Lua) -> bool {
    std::mem::take(&mut gate(lua).state.borrow_mut().fail_completion_wrap)
}

#[cfg(debug_assertions)]
pub fn fail_session_journal_ops(lua: &Lua, activations: usize, append: bool, close: bool) {
    let gate = gate(lua);
    let mut state = gate.state.borrow_mut();
    state.fail_activations = activations;
    state.fail_append = append;
    state.fail_close = close;
}

#[cfg(debug_assertions)]
pub(crate) fn take_append_failure(lua: &Lua) -> bool {
    std::mem::take(&mut gate(lua).state.borrow_mut().fail_append)
}

#[cfg(debug_assertions)]
pub(crate) fn take_close_failure(lua: &Lua) -> bool {
    std::mem::take(&mut gate(lua).state.borrow_mut().fail_close)
}

#[cfg(debug_assertions)]
pub fn fail_next_discard_before_release(lua: &Lua) {
    gate(lua).state.borrow_mut().fail_discard_before_release = true;
}

#[cfg(debug_assertions)]
pub(crate) fn take_discard_before_release_failure(lua: &Lua) -> bool {
    std::mem::take(&mut gate(lua).state.borrow_mut().fail_discard_before_release)
}

#[cfg(debug_assertions)]
pub(crate) fn take_completion_failure(lua: &Lua) -> bool {
    std::mem::take(&mut gate(lua).state.borrow_mut().fail_next_completion)
}

pub use mare_shared::session_barrier::check_unpoisoned;

/// One open journal, or an auxiliary file which must not arm the execution gate.
pub struct JournalHold {
    gate: Rc<Gate>,
    id: Option<u64>,
    open: Cell<bool>,
}

/// Deferred acknowledgement capability for an async journal append: captures the journal id
/// validated before I/O and grants admission only if this journal is still active and open
/// when `append_batch` + `sync_data` finish on the blocking pool.
pub struct AckToken {
    gate: Rc<Gate>,
    id: u64,
}

impl AckToken {
    pub fn ack(self, call_id: String) -> mlua::Result<()> {
        let mut state = self.gate.state.borrow_mut();
        if state.active != Some(self.id) {
            return Err(rt_err("only the active session journal can acknowledge a tool call"));
        }
        state
            .journals
            .get_mut(&self.id)
            .ok_or_else(|| rt_err("session journal closed before acknowledgement"))?
            .insert(call_id);
        Ok(())
    }
}

impl JournalHold {
    pub fn new(lua: &Lua, auxiliary: bool, defer_activation: bool) -> mlua::Result<Self> {
        let gate = gate(lua);
        let id = if auxiliary {
            None
        } else {
            let mut state = gate.state.borrow_mut();
            let id = state.next.checked_add(1).ok_or_else(|| rt_err("too many session journal handles"))?;
            state.next = id;
            // A single journal keeps the existing plugin API: it becomes active on open. When
            // another is already open, the session owner explicitly selects it after switching.
            if state.journals.is_empty() && !defer_activation {
                state.active = Some(id);
            }
            state.journals.insert(id, HashSet::new());
            Some(id)
        };
        Ok(Self { gate, id, open: Cell::new(true) })
    }

    /// Select the journal used by this session; stale acks from the former session cannot cross.
    pub fn activate(&self) -> mlua::Result<()> {
        #[cfg(debug_assertions)]
        {
            let mut state = self.gate.state.borrow_mut();
            if state.fail_activations > 0 {
                state.fail_activations -= 1;
                return Err(rt_err("injected journal activation failure"));
            }
        }
        let id = self
            .id
            .filter(|_| self.open.get())
            .ok_or_else(|| rt_err("cannot activate an auxiliary or closed journal"))?;
        let mut state = self.gate.state.borrow_mut();
        if !state.journals.contains_key(&id) {
            return Err(rt_err("cannot activate a closed journal"));
        }
        if state.active != Some(id) {
            if let Some(old) = state.active
                && let Some(acks) = state.journals.get_mut(&old)
            {
                acks.clear();
            }
            state.active = Some(id);
        }
        Ok(())
    }

    pub fn is_poisoned(&self) -> bool {
        self.gate.barrier.reason().is_some()
    }

    /// Check before writing an acknowledgement: another file cannot vouch for this session.
    pub fn check_ack(&self) -> mlua::Result<()> {
        let id = self
            .id
            .filter(|_| self.open.get())
            .ok_or_else(|| rt_err("an auxiliary or closed journal cannot acknowledge a tool call"))?;
        if self.gate.state.borrow().active != Some(id) {
            return Err(rt_err("only the active session journal can acknowledge a tool call"));
        }
        Ok(())
    }

    /// Validate that this journal may acknowledge a tool call and return an [`AckToken`] that
    /// grants admission once the async write + fsync complete.
    pub fn begin_ack(&self) -> mlua::Result<AckToken> {
        self.check_ack()?;
        let id = self.id.expect("check_ack accepted a tracked journal");
        Ok(AckToken { gate: Rc::clone(&self.gate), id })
    }

    /// Called only after the active journal's append and fsync both succeeded.
    pub fn ack(&self, call_id: String) -> mlua::Result<()> {
        self.check_ack()?;
        let id = self.id.expect("check_ack accepted a tracked journal");
        let mut state = self.gate.state.borrow_mut();
        state
            .journals
            .get_mut(&id)
            .ok_or_else(|| rt_err("session journal closed before acknowledgement"))?
            .insert(call_id);
        Ok(())
    }

    pub fn release(&self) {
        if !self.open.replace(false) {
            return;
        }
        if let Some(id) = self.id {
            let mut state = self.gate.state.borrow_mut();
            state.journals.remove(&id);
            if state.active == Some(id) {
                state.active = None;
            }
        }
    }
}

impl Drop for JournalHold {
    fn drop(&mut self) {
        self.release();
    }
}

fn refuse(why: String) -> mlua::Error {
    rt_err(format!("EXEC_NOT_DURABLE: {why}"))
}

/// Admit a call once against the selected session, or fail closed during a journal switch.
pub fn admit(lua: &Lua, ctx: &Table) -> mlua::Result<()> {
    check_unpoisoned(lua)?;
    let gate = gate(lua);
    let mut state = gate.state.borrow_mut();
    if state.journals.is_empty() {
        return Ok(());
    }
    let active = state.active.ok_or_else(|| refuse("no active session journal during a session switch".into()))?;
    let id = match ctx.raw_get::<Value>("tool_call_id")? {
        Value::String(s) => s.to_string_lossy(),
        _ => return Err(refuse("a tool call needs a tool_call_id while the session is being saved".into())),
    };
    if state.journals.get_mut(&active).is_some_and(|acks| acks.remove(id.as_str())) {
        return Ok(());
    }
    Err(refuse(format!(
        "call {id} has no durable start record in the session, so it may not run \
         (its `started` record must be written with ack = call id first)"
    )))
}
