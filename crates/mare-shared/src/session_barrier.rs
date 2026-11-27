//! VM-owned fail-closed barrier for an indeterminate session transaction.
//!
//! There is deliberately no reset: recovery requires a new host/Lua VM.
use std::cell::RefCell;
use std::rc::Rc;

use mlua::Lua;

#[derive(Default)]
pub struct SessionBarrier(RefCell<Option<String>>);

pub fn barrier(lua: &Lua) -> Rc<SessionBarrier> {
    if let Some(value) = lua.app_data_ref::<Rc<SessionBarrier>>() {
        return value.clone();
    }
    let value = Rc::new(SessionBarrier::default());
    lua.set_app_data(value.clone());
    value
}

impl SessionBarrier {
    pub fn poison(&self, reason: impl Into<String>) {
        let mut state = self.0.borrow_mut();
        if state.is_none() {
            *state = Some(reason.into());
        }
    }

    pub fn reason(&self) -> Option<String> {
        self.0
            .borrow()
            .as_ref()
            .map(|reason| format!("session transaction failed: {reason}; restart mare and resume a known-good session"))
    }

    pub fn check(&self) -> mlua::Result<()> {
        match self.reason() {
            Some(reason) => Err(mlua::Error::runtime(reason)),
            None => Ok(()),
        }
    }
}

/// Fail if this VM's session barrier has been poisoned.
pub fn check_unpoisoned(lua: &Lua) -> mlua::Result<()> {
    barrier(lua).check()
}
