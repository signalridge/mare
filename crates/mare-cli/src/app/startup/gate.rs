//! The startup gate: ready, blocked, recovered; the command-line prompt.

use crate::app::*;

/// The startup gate's state (written only here).
pub(in crate::app) struct StartupGate {
    /// Trust and `app/ready` haven't finished yet: no prompt is sent (
    /// `handleStartupSubmit`: the text stays in the input box).
    pub(in crate::app) starting: bool,
    /// the command-line prompt (`mare "…"`), sent once startup is ready; put back into the
    /// input box when startup is blocked
    pub(in crate::app) prompt: Option<String>,
    pub(in crate::app) error: Option<String>,
    /// the releases of the listeners waiting for what lifts a startup block
    /// ([`watch_startup_recovery`])
    pub(in crate::app) watch: Vec<mlua::Function>,
}

impl StartupGate {
    /// Startup in progress: nothing failed, no prompt held, nothing watched.
    pub(in crate::app) fn new() -> Self {
        Self { starting: true, prompt: None, error: None, watch: vec![] }
    }
}

/// Listen for what lifts a startup block (see [`StartupFailure`]): sends
/// [`AppMsg::StartupRecovered`]. Returns the listeners' releases (called once recovered).
fn watch_startup_recovery(
    host: &Arc<Host>,
    tx: mpsc::UnboundedSender<AppMsg>,
    kind: StartupFailure,
) -> mlua::Result<Vec<mlua::Function>> {
    // listeners of the host's fiber
    let (ctx, owner) = (host.kernel.host_ctx()?, host.kernel.host_owner()?);
    let events: &[&str] = match kind {
        StartupFailure::Resume => &["session/new", "session/loaded"],
        StartupFailure::Trust => &["reload/completed"],
    };
    let mut ids = vec![];
    for &event in events {
        let tx = tx.clone();
        let reload = event == "reload/completed";
        let f = host.lua().create_function(move |_, args: mlua::MultiValue| {
            // a reload with notices (a package, plugin or hook failed) leaves the block in place
            let clean = !reload
                || match args.into_iter().next() {
                    Some(mlua::Value::Table(t)) => {
                        t.get::<Option<mlua::Table>>("notices").ok().flatten().is_none_or(|n| n.raw_len() == 0)
                    }
                    _ => true,
                };
            if clean {
                let _ = tx.send(AppMsg::StartupRecovered);
            }
            Ok(())
        })?;
        // it only posts to the app's loop, which handles it after this dispatch: no order needed
        ids.push(host.kernel.host_on(&ctx, owner, event, f, mare_kernel::HostOn::default())?);
    }
    Ok(ids)
}

impl App {
    /// While startup is in progress or blocked, Enter sends nothing and keeps nothing for later
    /// like every refused prompt,
    /// "not sent: <reason>" in the transcript (a card while startup still sets the transcript
    /// up) and the text back in the input box ([`crate::not_sent`]). A blocked startup never
    /// traps the input: slash commands (/new, /resume, /reload, /quit, …) still run. True when
    /// `text` was refused.
    pub(in crate::app) fn refused_during_startup(&mut self, text: &str) -> bool {
        let reason = match &self.startup.error {
            Some(_) if text.trim_start().starts_with('/') => return false,
            Some(error) => format!("startup is blocked: {error} — /new, /resume or /reload can fix it"),
            None if self.startup.starting => "startup is still in progress — press Enter once it's ready".into(),
            None => return false,
        };
        crate::not_sent::restore(&mut self.ed(), text);
        self.sync_editor_state();
        if self.startup.starting {
            // the transcript is still being set up (a resumed session replaces it): a card
            self.push_notice((format!("not sent: {reason}"), "warning".into(), Instant::now()));
        } else {
            let node = crate::not_sent::notice(&self.ui.theme.read().unwrap(), &reason);
            self.handle_cmd(UiCmd::Print { node });
        }
        true
    }

    /// The command-line prompt: sent once startup is ready, in the input box when it's blocked.
    pub(in crate::app) fn startup_prompt(&mut self, prompt: String) {
        if self.startup.starting || self.startup.error.is_some() {
            self.startup.prompt = Some(prompt);
        } else {
            self.dispatch(prompt, Vec::new(), Vec::new());
        }
    }

    /// Startup is ready: prompts may be sent (the command-line prompt goes now).
    pub(in crate::app) fn startup_ready(&mut self) {
        self.startup.starting = false;
        if let Some(prompt) = self.startup.prompt.take() {
            self.dispatch(prompt, Vec::new(), Vec::new());
        }
    }

    /// Startup failed: prompts are refused (slash commands still run) until what failed is
    /// fixed; the command-line prompt goes into the input box instead of being sent.
    pub(in crate::app) fn startup_failed(&mut self, error: String, kind: StartupFailure) {
        self.startup.starting = false;
        self.startup.error = Some(error.clone());
        if let Some(prompt) = self.startup.prompt.take()
            && self.ed().text().trim().is_empty()
        {
            self.ed().set_text(&prompt);
            self.sync_editor_state();
        }
        self.push_notice((startup_blocked_notice(&error), "error".into(), Instant::now()));
        if self.startup.watch.is_empty() {
            match watch_startup_recovery(&self.host, self.app_tx.clone(), kind) {
                Ok(ids) => self.startup.watch = ids,
                Err(e) => tracing::warn!("startup recovery: {e}"),
            }
        }
    }

    /// The startup block lifted: prompts may be sent again; nothing is sent on its own.
    pub(in crate::app) fn startup_recovered(&mut self) {
        for release in self.startup.watch.drain(..) {
            if let Err(e) = release.call::<()>(()) {
                tracing::warn!("startup recovery: {e}");
            }
        }
        // the startup task stopped at the failure, before ui/ready (only it reports one): that is
        // sent now, once, so what waits for it (onboarding, the model catalogue refresh) still runs
        if self.startup.error.take().is_some() {
            self.push_notice(("Startup unblocked".into(), "info".into(), Instant::now()));
            let h = self.host.clone();
            tokio::task::spawn_local(async move { finish_startup(&h).await });
        }
    }
}

/// The end of startup once trust and session restoration are settled: `ui/ready`. The startup
/// task sends it, or lifting a startup block does when a failure stopped the task before it.
pub(in crate::app) async fn finish_startup(h: &mare_host::Host) {
    mare_shared::profile::mark("ui/ready");
    h.emit_json("ui/ready", serde_json::json!({})).await;
}

/// The notice while startup is blocked: what failed and what lifts it (nothing is held).
fn startup_blocked_notice(error: &str) -> String {
    format!("Startup blocked: {error} — /new, /resume or /reload can fix it; prompts are not sent until then")
}
