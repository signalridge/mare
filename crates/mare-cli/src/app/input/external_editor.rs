//! Ctrl+G: edit the input in an external editor asynchronously without blocking the runtime.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use crate::app::*;

/// Graceful wait after sending SIGTERM to an external editor during shutdown before escalating to SIGKILL.
#[cfg(unix)]
const ABORT_SIGTERM_GRACE: Duration = Duration::from_millis(500);
/// Bounded wait to reap the editor child after SIGKILL.
const ABORT_SIGKILL_GRACE: Duration = Duration::from_millis(200);

pub(in crate::app) struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// An in-flight external editor subprocess (`Ctrl+G`). While active, tty input and frame drawing
/// pause while the event loop continues serving Lua tasks, host streams, and `UiCmd`/`AppMsg`.
pub(in crate::app) struct ExternalEditorSession {
    exe: String,
    file: TempFileGuard,
    saved_tokens: Vec<(u64, String, String)>,
    pub(in crate::app) pending_editor_cmds: Vec<UiCmd>,
    pub(in crate::app) child: tokio::process::Child,
}

impl App {
    /// Ctrl+G: hand off the terminal and spawn `$VISUAL`/`$EDITOR` asynchronously.
    /// Must be called from the main event loop after pausing `tty::Events` so `mare-input` cannot
    /// race the external editor on stdin.
    pub(in crate::app) fn start_external_editor(&mut self) {
        if self.external_editor.is_some() {
            return;
        }
        let editor = crate::editor_cmd::program(&self.host);
        let (exe, args) = match crate::editor_cmd::split_program(&editor) {
            Ok(parts) => parts,
            Err(msg) => {
                self.push_notice((msg, "error".into(), app_now()));
                return;
            }
        };
        let dir = mare_shared::paths::state_dir().join("editor");
        if let Err(e) = mare_shared::private::create_dir_all(&dir) {
            self.push_notice((format!("Could not prepare private dir for editor: {e}"), "error".into(), app_now()));
            return;
        }
        let path = dir.join(format!("input-{}.md", uuid::Uuid::new_v4()));
        let mut file = match mare_shared::private::create_new(&path) {
            Ok(f) => f,
            Err(e) => {
                self.push_notice((format!("Could not create temp file for editor: {e}"), "error".into(), app_now()));
                return;
            }
        };
        let guard = TempFileGuard(path.clone());
        let saved_tokens = self.ed().snapshot_tokens();
        let raw = self.ed().raw_text();
        if let Err(e) = std::io::Write::write_all(&mut file, raw.as_bytes()) {
            self.push_notice((format!("Could not write temp file for editor: {e}"), "error".into(), app_now()));
            return;
        }
        drop(file);
        if !self.transcript.is_open() {
            self.renderer.erase();
        }
        // Focus reporting turns off during handoff (`?1004l`): treat focus as unknown until a new report arrives.
        self.ui.focus_known.store(false, std::sync::atomic::Ordering::Relaxed);
        // Every mode off and our kitty entry popped while the editor owns the terminal; an exit
        // signal meanwhile writes no resets.
        hand_off_terminal();
        let spawned = tokio::process::Command::new(&exe)
            .args(&args)
            .arg(&path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();
        match spawned {
            Ok(child) => {
                self.editor.set_external_editor_active(true);
                self.external_editor = Some(ExternalEditorSession {
                    exe,
                    file: guard,
                    saved_tokens,
                    pending_editor_cmds: Vec::new(),
                    child,
                });
            }
            Err(e) => {
                take_back_terminal();
                self.reacquire_terminal(app_now());
                self.push_notice((format!("Could not start editor `{exe}`: {e}"), "error".into(), app_now()));
            }
        }
    }

    /// Reclaim the terminal and apply the edited input after the external editor exits.
    pub(in crate::app) fn finish_external_editor(
        &mut self,
        session: ExternalEditorSession,
        status: std::io::Result<ExitStatus>,
    ) {
        self.editor.set_external_editor_active(false);
        let ExternalEditorSession { exe, file, saved_tokens, pending_editor_cmds, child: _ } = session;
        take_back_terminal();
        self.reacquire_terminal(app_now());
        match status {
            Ok(s) if s.success() => {
                if let Ok(text) = std::fs::read_to_string(&file.0) {
                    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                    let trimmed = normalized.trim_end_matches('\n');
                    self.ed().set_text(trimmed);
                    // Match each saved placeholder anywhere in the edited text (claiming each
                    // occurrence at most once) and reuse its original element ID so pre-edit
                    // undo snapshots and the restored buffer share attachment identity.
                    let mut claimed: Vec<std::ops::Range<usize>> = Vec::new();
                    for (old_id, token, content) in saved_tokens {
                        if token.is_empty() {
                            continue;
                        }
                        let mut search_from = 0usize;
                        while let Some(rel) = trimmed[search_from..].find(&token) {
                            let start = search_from + rel;
                            let end = start + token.len();
                            if claimed.iter().any(|r| r.start < end && start < r.end) {
                                search_from =
                                    trimmed[start..].chars().next().map_or(trimmed.len(), |c| start + c.len_utf8());
                                continue;
                            }
                            claimed.push(start..end);
                            let _ = self.ed().register_element_with_id_in_text(
                                old_id,
                                trimmed,
                                start..end,
                                &token,
                                &content,
                            );
                            break;
                        }
                    }
                    // Retain attachments reachable from the current buffer or undo/redo/kill-ring
                    // history so undoing an external-editor edit preserves image payloads.
                    let live_ids = self.ed().referenced_element_ids();
                    self.attachments.retain(|(id, _, _)| live_ids.contains(id));
                    self.reset_after_editor_replace();
                }
            }
            Ok(s) => {
                self.push_notice((format!("Editor `{exe}` exited with {s}"), "warning".into(), app_now()));
            }
            Err(e) => {
                self.push_notice((format!("Could not wait on editor `{exe}`: {e}"), "error".into(), app_now()));
            }
        }
        drop(file);
        for cmd in pending_editor_cmds {
            self.handle_editor_cmd(cmd);
        }
    }

    /// Non-blocking completion check for testkit's frame-stepping loop.
    pub(in crate::app) fn poll_external_editor_nonblocking(&mut self) -> bool {
        let Some(mut session) = self.external_editor.take() else {
            return false;
        };
        match session.child.try_wait() {
            Ok(Some(status)) => {
                self.finish_external_editor(session, Ok(status));
                true
            }
            Ok(None) => {
                self.external_editor = Some(session);
                false
            }
            Err(err) => {
                self.finish_external_editor(session, Err(err));
                true
            }
        }
    }

    /// Forward an interrupt to an active cooked-mode external editor (e.g. `code --wait` receiving
    /// Ctrl+C / SIGINT) without exiting mare; the normal exit arm reclaims the terminal once it exits.
    pub(in crate::app) fn cancel_external_editor(&mut self) {
        let Some(session) = self.external_editor.as_mut() else {
            return;
        };
        #[cfg(unix)]
        if let Some(pid) = session.child.id() {
            // SAFETY: pid belongs to our live child process.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = session.child.start_kill();
        }
    }

    /// Terminate an active external editor session during shutdown, wait briefly for graceful
    /// terminal cleanup (`SIGTERM` -> `SIGKILL`), reclaim terminal teardown ownership, and clean up
    /// its temp file.
    pub(in crate::app) async fn abort_external_editor(&mut self) {
        self.editor.set_external_editor_active(false);
        let Some(mut session) = self.external_editor.take() else {
            return;
        };
        #[cfg(unix)]
        let clean_exit = {
            if let Some(pid) = session.child.id() {
                // SAFETY: pid belongs to our live child process.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
            match tokio::time::timeout(ABORT_SIGTERM_GRACE, session.child.wait()).await {
                Ok(Ok(status)) => status.success(),
                _ => {
                    let _ = session.child.start_kill();
                    let _ = tokio::time::timeout(ABORT_SIGKILL_GRACE, session.child.wait()).await;
                    false
                }
            }
        };
        #[cfg(not(unix))]
        let clean_exit = {
            let _ = session.child.start_kill();
            let _ = tokio::time::timeout(ABORT_SIGKILL_GRACE, session.child.wait()).await;
            false
        };
        drop(session);
        reclaim_terminal_for_exit(!clean_exit);
    }
}
