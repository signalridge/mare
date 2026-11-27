//! Open `mare.ui` dialog waits (`ui/wait-started` / `ui/wait-completed`), so a sub-agent's approval
//! never replaces a dialog that is already open.

use anyhow::Result;
use mlua::{MultiValue, Table, Value};

use crate::Host;

#[derive(Default)]
pub(crate) struct UiWaits {
    open: std::cell::Cell<usize>,
    /// waits started so far: a dialog that ended without an answer while another one started
    /// was replaced, not answered
    pub(crate) started: std::cell::Cell<u64>,
    idle: tokio::sync::Notify,
}

impl UiWaits {
    pub(crate) async fn until_idle(&self) {
        loop {
            let n = self.idle.notified();
            if self.open.get() == 0 {
                return;
            }
            n.await;
        }
    }
}

pub(crate) fn service_table_for_kernel(kernel: &mare_kernel::Kernel, name: &str) -> Option<Table> {
    match kernel.service(name) {
        Some(Value::Table(t)) => Some(t),
        _ => None,
    }
}

fn active_session_id(kernel: &mare_kernel::Kernel) -> Option<String> {
    let info = crate::startup::session_info_for_kernel(kernel).ok().flatten()?;
    info.get::<Option<String>>("session_id").ok().flatten()
}

pub(crate) fn ui_print_payload(text: &str) -> serde_json::Value {
    ui_print_payload_with_node(text, None)
}

pub(crate) fn ui_print_payload_with_node(text: &str, node: Option<serde_json::Value>) -> serde_json::Value {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    match node {
        Some(n) => serde_json::json!({ "text": text, "lines": lines, "node": n }),
        None => serde_json::json!({ "text": text, "lines": lines }),
    }
}

impl Host {
    /// `mare.ui.select/input/confirm` (whoever implements them) report "waiting for the user" as
    /// the public events `ui/wait-started` / `ui/wait-completed` with `{kind, title}`, plus the paired
    /// `interaction/requested` / `interaction/resolved` observation events and `ui/print`.
    pub(crate) fn install_wait_hook(&self) -> Result<()> {
        let lua = self.lua();
        let k = self.kernel.clone();
        let hook_waits = self.waits.clone();
        let hook = lua.create_async_function(move |_, (phase, info, interaction): (String, Value, Value)| {
            let k = k.clone();
            let is_print = phase == "print";
            let print_payload = if is_print {
                info.as_string().and_then(|s| s.to_str().ok().map(|v| ui_print_payload(&v)))
            } else {
                None
            };
            let emit_phase = if phase == "end_emit" { "end" } else { phase.as_str() };
            if !is_print {
                if phase == "start" {
                    hook_waits.open.set(hook_waits.open.get() + 1);
                    hook_waits.started.set(hook_waits.started.get() + 1);
                } else if phase == "end" {
                    hook_waits.open.set(hook_waits.open.get().saturating_sub(1));
                    if hook_waits.open.get() == 0 {
                        hook_waits.idle.notify_waiters();
                    }
                }
                if let Value::Table(ref it) = interaction
                    && it.get::<Option<String>>("session_id").ok().flatten().is_none()
                    && let Some(sid) = active_session_id(&k)
                {
                    let _ = it.set("session_id", sid);
                }
            }
            let is_start = emit_phase == "start";
            async move {
                if is_print {
                    if let Some(payload) = print_payload {
                        k.emit_json("ui/print", payload).await;
                    }
                    return Ok(());
                }
                let wait_args = MultiValue::from_vec(vec![info]);
                if is_start {
                    k.emit("ui/wait-started", wait_args).await;
                } else {
                    k.emit("ui/wait-completed", wait_args).await;
                }
                if matches!(interaction, Value::Table(_)) {
                    let args = MultiValue::from_vec(vec![interaction]);
                    if is_start {
                        k.emit("interaction/requested", args).await;
                    } else {
                        k.emit("interaction/resolved", args).await;
                    }
                }
                Ok(())
            }
        })?;
        mare_shared::lua::core::ui::set_wait_hook(lua, hook)?;
        // Synchronous counter release for Lua `__close` (which cannot yield when a coroutine is
        // dropped): `UiWaits::until_idle()` sees `open == 0` immediately even if `ui/wait-end`
        // event dispatch is spawned separately.
        let close_waits = self.waits.clone();
        let close = lua.create_function(move |_, ()| {
            close_waits.open.set(close_waits.open.get().saturating_sub(1));
            if close_waits.open.get() == 0 {
                close_waits.idle.notify_waiters();
            }
            Ok(())
        })?;
        mare_shared::lua::core::ui::set_wait_close(lua, close)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ui_waits_until_idle_suspends_until_open_count_reaches_zero() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let waits = std::rc::Rc::new(UiWaits::default());
                waits.open.set(1);
                let w2 = waits.clone();
                let waiter = tokio::task::spawn_local(async move {
                    w2.until_idle().await;
                });
                tokio::task::yield_now().await;
                assert!(!waiter.is_finished());
                waits.open.set(0);
                waits.idle.notify_waiters();
                waiter.await.unwrap();
            })
            .await;
    }
}
