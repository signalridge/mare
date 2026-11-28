//! Plain line-by-line frontend for `TERM=dumb`: no cursor movement, no
//! escape sequences, no raw mode. The terminal's own line editing reads the input; finished
//! output is printed as plain text lines, a streaming answer one finished Markdown block at a
//! time (the same "only completed blocks are final" rule the TUI uses for scrollback commits).
//! Selections and confirmations become numbered questions answered by the next input line, so
//! approvals still work.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::{mpsc, oneshot};

use mare_host::{Host, HostOptions};
use mare_shared::style::{Line, Theme};
use mare_shared::ui::{Dismissed, Node, SelectReply, UiCmd, UiShared};
use mare_tui::markdown::StreamingMarkdown;
use mare_tui::render::{STATIC_FRAME, render_node};

/// `TERM=dumb`: the terminal can't move the cursor.
pub fn is_dumb(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var("TERM").is_some_and(|t| t == "dumb")
}

/// Run the interactive session on stdin / stdout.
pub async fn run(opts: HostOptions, initial: Option<String>, strict_config: bool) -> Result<i32> {
    let ui = UiShared::new();
    let (tx, rx) = mpsc::unbounded_channel();
    *ui.tx.write().unwrap() = Some(tx);
    let width = mare_tui::tty::size()
        .ok()
        .map(|(w, _)| w)
        .filter(|w| *w > 0)
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()))
        .unwrap_or(80);
    ui.width.store(width, std::sync::atomic::Ordering::Relaxed);
    ui.height.store(24, std::sync::atomic::Ordering::Relaxed);
    // no images and no hyperlinks: nothing but text reaches the terminal
    mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
    let host = Host::with_ui(opts, ui.clone()).await?;
    host.restore_startup_gc();
    if strict_config && let Some(msg) = crate::report_config_warnings(&host, true).err().or_else(|| host.config_error())
    {
        crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::HostShutdown);
        host.shutdown().await;
        crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::RuntimeDrain);
        eprintln!("mare: {msg}");
        return Ok(2);
    }
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut out = std::io::stdout();
    let code = drive(host.clone(), ui, rx, input, &mut out, initial, true).await;
    crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::HostShutdown);
    host.shutdown().await;
    crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::RuntimeDrain);
    Ok(code)
}

struct PlainBlock {
    kind: String,
    node: Option<Node>,
    md: Option<StreamingMarkdown>,
    /// Markdown rows already printed (only finished blocks are printed while streaming)
    printed: usize,
}

/// A question waiting for the next input line.
enum Pending {
    Select {
        modal_id: u64,
        choices: Vec<usize>,
        item_ids: Vec<Option<String>>,
        default: usize,
        checked: Vec<usize>,
        multi: bool,
        tab: usize,
        confirm: bool,
        reply: oneshot::Sender<Result<SelectReply, Dismissed>>,
    },
    Input {
        modal_id: u64,
        default: String,
        reply: oneshot::Sender<Result<String, Dismissed>>,
    },
}

struct Plain<'a, W: Write> {
    out: &'a mut W,
    ui: Arc<UiShared>,
    blocks: HashMap<u64, PlainBlock>,
    pending: Option<Pending>,
    working: Option<String>,
    quit: bool,
}

impl<W: Write> Plain<'_, W> {
    fn width(&self) -> usize {
        self.ui.width.load(std::sync::atomic::Ordering::Relaxed).max(20) as usize
    }

    fn theme(&self) -> Theme {
        self.ui.theme.read().unwrap().clone()
    }

    fn print_lines(&mut self, lines: &[Line]) {
        for l in lines {
            let _ = writeln!(self.out, "{}", l.plain().trim_end());
        }
        let _ = self.out.flush();
    }

    fn say(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    fn prompt(&mut self) {
        if self.pending.is_none() && self.working.is_none() && !self.quit {
            let _ = write!(self.out, "> ");
            let _ = self.out.flush();
        }
    }

    fn render_block(&self, b: &mut PlainBlock) -> Vec<Line> {
        let (w, theme) = (self.width(), self.theme());
        match (&b.node, b.md.as_mut()) {
            (Some(n), _) => render_node(n, w, &theme, STATIC_FRAME),
            (None, Some(md)) => md.render(w, &theme),
            _ => vec![],
        }
    }

    /// Print what became final: a streaming answer's finished blocks.
    fn flush_stable(&mut self, id: u64) {
        let Some(mut b) = self.blocks.remove(&id) else { return };
        if b.node.is_none()
            && let Some(md) = b.md.as_mut()
        {
            let lines = md.render(self.width(), &self.theme());
            let stable = md.stable_lines().min(lines.len());
            if stable > b.printed {
                let rows = lines[b.printed..stable].to_vec();
                self.print_lines(&rows);
                b.printed = stable;
            }
        }
        self.blocks.insert(id, b);
    }

    fn finish(&mut self, id: u64) {
        let Some(mut b) = self.blocks.remove(&id) else { return };
        // the user's own line is already on screen (the terminal echoed it)
        if b.kind == "user" {
            return;
        }
        let lines = self.render_block(&mut b);
        let rows: Vec<Line> = lines.into_iter().skip(b.printed).collect();
        if !rows.is_empty() {
            self.print_lines(&rows);
        }
    }

    fn handle(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::BlockPush { id, kind, patch } => {
                let mut b = PlainBlock { kind, node: None, md: None, printed: 0 };
                apply(&mut b, patch);
                self.blocks.insert(id, b);
                self.flush_stable(id);
            }
            UiCmd::BlockUpdate { id, patch } => {
                if let Some(b) = self.blocks.get_mut(&id) {
                    apply(b, patch);
                }
                self.flush_stable(id);
            }
            UiCmd::BlockFinish { id } => self.finish(id),
            UiCmd::BlockRemove { id } => {
                self.blocks.remove(&id);
            }
            UiCmd::Print { node } => {
                let lines = render_node(&node, self.width(), &self.theme(), STATIC_FRAME);
                self.print_lines(&lines);
            }
            UiCmd::Overlay { title, node, .. } | UiCmd::TranscriptView { title, node, .. } => {
                self.say(&format!("── {title} ──"));
                let lines = render_node(&node, self.width(), &self.theme(), STATIC_FRAME);
                self.print_lines(&lines);
            }
            UiCmd::Notify { text, level, .. } => {
                let tag = match level.as_str() {
                    "error" => "error",
                    "warning" | "warn" => "warning",
                    _ => "info",
                };
                self.say(&format!("[{tag}] {text}"));
            }
            UiCmd::Working { label, .. } => {
                let was_idle = self.working.is_none();
                self.working = label;
                if was_idle && self.working.is_some() {
                    self.say(&format!("(working… {} interrupts)", mare_shared::bindings::key_label("ctrl+c")));
                }
                if !was_idle && self.working.is_none() {
                    self.prompt();
                }
            }
            UiCmd::Select { title, items, selected, multi, kind, body, reply, tabs, tab, modal_id, .. } => {
                if self.pending.is_some() {
                    // Plain mode has one response slot. Don't drop an older question by
                    // replacing it with a later dialog that arrived before EOF was observed.
                    let _ = reply.send(Err(Dismissed::Cancelled));
                    return;
                }
                // a tabbed select asks about its first (or initial) tab's items
                let items = if items.is_empty() {
                    tabs.into_iter().nth(tab).map(|t| t.items).unwrap_or_default()
                } else {
                    items
                };
                let item_ids: Vec<Option<String>> = items.iter().map(|it| it.item_id.clone()).collect();
                let confirm = kind == "confirm";
                if !title.is_empty() {
                    self.say(&title);
                }
                for b in &body {
                    self.say(&format!("  {b}"));
                }
                if confirm {
                    let yes = items.first().map(|i| i.label.clone()).unwrap_or_else(|| "Yes".into());
                    let no = items.get(1).map(|i| i.label.clone()).unwrap_or_else(|| "No".into());
                    let _ = write!(self.out, "{yes} / {no} [y/n]> ");
                } else {
                    let choices: Vec<_> =
                        items.iter().enumerate().filter_map(|(i, item)| item.is_selectable().then_some(i)).collect();
                    let default = choices.iter().position(|&i| i == selected).unwrap_or(0);
                    let checked: Vec<_> = choices.iter().copied().filter(|&i| items[i].checked).collect();
                    for (i, it) in items.iter().enumerate() {
                        if it.header {
                            self.say(&format!("  {}", it.label));
                            continue;
                        }
                        let detail = it.detail.as_deref().map(|d| format!("  {d}")).unwrap_or_default();
                        if it.disabled {
                            self.say(&format!("    {}{detail}", it.label));
                            continue;
                        }
                        let n = choices.iter().position(|&choice| choice == i).unwrap_or_default();
                        let mark = if multi {
                            if it.checked { "[x]" } else { "[ ]" }
                        } else if i == selected {
                            "*"
                        } else {
                            " "
                        };
                        self.say(&format!("{mark}{:>3}. {}{detail}", n + 1, it.label));
                        if let Some(node) = &it.preview {
                            for line in render_node(node, self.width().saturating_sub(6), &self.theme(), STATIC_FRAME) {
                                self.say(&format!("      {}", line.plain()));
                            }
                        }
                    }
                    if choices.is_empty() {
                        self.say("(no selectable items)");
                        let _ = reply.send(Err(Dismissed::Cancelled));
                        return;
                    }
                    if multi {
                        let _ = write!(
                            self.out,
                            "choose numbers 1-{} separated by commas (Enter: keep selection, q: cancel)> ",
                            choices.len()
                        );
                    } else {
                        let _ = write!(self.out, "choose 1-{} (Enter: {}, q: cancel)> ", choices.len(), default + 1);
                    }
                    let _ = self.out.flush();
                    self.pending = Some(Pending::Select {
                        modal_id,
                        choices,
                        item_ids,
                        default,
                        checked,
                        multi,
                        tab,
                        confirm,
                        reply,
                    });
                    return;
                }
                let _ = self.out.flush();
                self.pending = Some(Pending::Select {
                    modal_id,
                    choices: vec![],
                    item_ids,
                    default: selected,
                    checked: vec![],
                    multi: false,
                    tab,
                    confirm,
                    reply,
                });
            }
            UiCmd::Input { title, default, secret, reply, modal_id, .. } => {
                if self.pending.is_some() {
                    let _ = reply.send(Err(Dismissed::Cancelled));
                    return;
                }
                if secret {
                    self.say("(this terminal can't hide what you type)");
                }
                if default.is_empty() {
                    let _ = write!(self.out, "{title}> ");
                } else {
                    let _ = write!(self.out, "{title} [default: {default}]> ");
                }
                let _ = self.out.flush();
                self.pending = Some(Pending::Input { modal_id, default, reply });
            }
            UiCmd::CancelModal { modal_id } if modal_id != 0 => {
                let matches = matches!(
                    &self.pending,
                    Some(Pending::Select { modal_id: id, .. } | Pending::Input { modal_id: id, .. }) if *id == modal_id
                );
                if matches {
                    self.pending = None;
                    self.say("(cancelled)");
                }
            }
            UiCmd::Exit | UiCmd::ExitForce => self.quit = true,
            // Select updates/previews, editor chrome, widgets, status bar, flags, keymaps,
            // themes, OS notifications, titles, exit guard and layout apply to the TUI only.
            UiCmd::SelectUpdate { .. }
            | UiCmd::SelectPreview { .. }
            | UiCmd::CancelModal { .. }
            | UiCmd::TranscriptClose { .. }
            | UiCmd::SetEditor { .. }
            | UiCmd::EditorConfig { .. }
            | UiCmd::EditorHint { .. }
            | UiCmd::Flag { .. }
            | UiCmd::SetBindings { .. }
            | UiCmd::SetBindingsBatch { .. }
            | UiCmd::SetContext { .. }
            | UiCmd::SetContextActive { .. }
            | UiCmd::EditorOp(_)
            | UiCmd::SyncEditorChanged
            | UiCmd::CompletionSourceRemoved { .. }
            | UiCmd::ThemeChanged
            | UiCmd::Redraw
            | UiCmd::Widget { .. }
            | UiCmd::Status { .. }
            | UiCmd::NotifyOs { .. }
            | UiCmd::Title { .. }
            | UiCmd::Clear
            | UiCmd::ExitGuard { .. }
            | UiCmd::Layout(_)
            | UiCmd::ColorMode(_) => {}
        }
    }

    /// Nobody can answer once input has closed, including questions opened later.
    fn cancel_pending(&mut self) {
        self.ui.responder.store(false, std::sync::atomic::Ordering::Relaxed);
        match self.pending.take() {
            Some(Pending::Input { reply, .. }) => {
                let _ = reply.send(Err(Dismissed::Cancelled));
            }
            Some(Pending::Select { reply, .. }) => {
                let _ = reply.send(Err(Dismissed::Cancelled));
            }
            None => {}
        }
    }

    /// An input line answers the pending question; true when it did.
    fn answer(&mut self, line: &str) -> bool {
        let Some(p) = self.pending.take() else { return false };
        let t = line.trim();
        match p {
            Pending::Input { default, reply, .. } => {
                let out = if line.is_empty() && !default.is_empty() { default } else { line.to_string() };
                let _ = reply.send(Ok(out));
            }
            Pending::Select { confirm: true, reply, default, item_ids, .. } => {
                let pick = match t.to_ascii_lowercase().as_str() {
                    "y" | "yes" => Some(0),
                    "n" | "no" => Some(1),
                    "" => Some(default),
                    _ => None,
                };
                let _ = reply.send(
                    pick.map(|index| SelectReply {
                        index,
                        item_id: item_ids.get(index).cloned().flatten(),
                        ..Default::default()
                    })
                    .ok_or(Dismissed::Cancelled),
                );
            }
            Pending::Select { choices, item_ids, default, checked, multi: true, tab, reply, .. } => {
                let selected = if matches!(t, "q" | "Q") {
                    None
                } else if t.is_empty() {
                    Some(checked)
                } else {
                    t.split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|part| !part.is_empty())
                        .map(|part| {
                            part.parse::<usize>()
                                .ok()
                                .and_then(|i| i.checked_sub(1))
                                .and_then(|i| choices.get(i).copied())
                        })
                        .collect::<Option<Vec<_>>>()
                };
                let answer = selected.map(|mut selected_indices| {
                    selected_indices.sort_unstable();
                    selected_indices.dedup();
                    let index =
                        selected_indices.first().copied().or_else(|| choices.get(default).copied()).unwrap_or(0);
                    SelectReply {
                        index,
                        selected_indices,
                        tab,
                        item_id: item_ids.get(index).cloned().flatten(),
                        ..Default::default()
                    }
                });
                if answer.is_none() && !matches!(t, "q" | "Q") {
                    let _ = writeln!(self.out, "(not a choice: cancelled)");
                }
                let _ = reply.send(answer.ok_or(Dismissed::Cancelled));
            }
            Pending::Select { choices, item_ids, default, tab, reply, .. } => {
                let pick = if t.is_empty() {
                    choices.get(default).copied()
                } else {
                    t.parse::<usize>().ok().and_then(|i| i.checked_sub(1)).and_then(|i| choices.get(i).copied())
                };
                if pick.is_none() && !matches!(t, "q" | "Q") {
                    let _ = writeln!(self.out, "(not a choice: cancelled)");
                }
                let _ = reply.send(
                    pick.map(|index| SelectReply {
                        index,
                        tab,
                        item_id: item_ids.get(index).cloned().flatten(),
                        ..Default::default()
                    })
                    .ok_or(Dismissed::Cancelled),
                );
            }
        }
        true
    }
}

fn apply(b: &mut PlainBlock, p: mare_shared::ui::BlockPatch) {
    if let Some(k) = p.kind {
        b.kind = k;
    }
    if let Some(n) = p.node {
        b.node = Some(n);
    }
    if let Some(m) = p.markdown {
        let mut sm = StreamingMarkdown::new();
        sm.set_text(&m);
        b.md = Some(sm);
        b.printed = 0;
    }
    if let Some(a) = p.append {
        b.md.get_or_insert_with(StreamingMarkdown::new).push(&a);
    }
}

/// The plain session loop over any line input and output (tests drive it in-process). Input EOF
/// ends the session once the running turn is over.
pub async fn drive<R: AsyncBufRead + Unpin, W: Write>(
    host: Arc<Host>,
    ui: Arc<UiShared>,
    mut rx: mpsc::UnboundedReceiver<UiCmd>,
    input: R,
    out: &mut W,
    initial: Option<String>,
    interactive: bool,
) -> i32 {
    let mut p = Plain { out, ui, blocks: HashMap::new(), pending: None, working: None, quit: false };
    host.restore_startup_gc();
    p.say("mare (plain mode for TERM=dumb) — type a message, Enter sends; /exit quits");
    for (p2, level) in host.startup_notices() {
        p.say(&format!("[{level}] {p2}"));
    }
    {
        let h = host.clone();
        tokio::task::spawn_local(async move {
            h.emit_json("app/ready", serde_json::json!({})).await;
            h.emit_json("ui/ready", serde_json::json!({})).await;
        });
    }
    if let Some(text) = initial.filter(|t| !t.trim().is_empty()) {
        let h = host.clone();
        tokio::task::spawn_local(async move { h.submit(&text).await });
    }
    let mut lines = input.lines();
    let mut eof = false;
    let mut prompted = false;
    let mut exit_code = 0;
    // after input ends: leave once nothing has happened for a moment (a submitted line may start a
    // turn a little later, or be a command that never does)
    let mut quiet_since = std::time::Instant::now();
    let quiet = std::time::Duration::from_millis(300);
    // Always listen to SIGINT: `install_forceable_shutdown` installs a native SIGINT handler
    // process-wide, so non-interactive plain must also catch SIGINT to shut down gracefully rather
    // than hanging 5s until the forced-exit watchdog triggers `_exit(130)`.
    let mut sigint = crate::signals::Interrupts::listen(true);
    let terminate = crate::signals::terminate();
    tokio::pin!(terminate);
    loop {
        if p.quit
            || (eof && p.working.is_none() && p.pending.is_none() && rx.is_empty() && quiet_since.elapsed() > quiet)
        {
            break;
        }
        if !prompted {
            // give plugins (welcome line, a submitted prompt) a moment before the first prompt
            tokio::task::yield_now().await;
            while let Ok(c) = rx.try_recv() {
                p.handle(c);
            }
            p.prompt();
            prompted = true;
        }
        tokio::select! {
            biased;
            Some(cmd) = rx.recv() => {
                quiet_since = std::time::Instant::now();
                p.handle(cmd);
                if eof { p.cancel_pending(); }
                while let Ok(c) = rx.try_recv() {
                    p.handle(c);
                    if eof { p.cancel_pending(); }
                }
            }
            line = lines.next_line(), if !eof => match line {
                Ok(Some(l)) => {
                    quiet_since = std::time::Instant::now();
                    if !p.answer(&l) && !l.trim().is_empty() {
                        let h = host.clone();
                        tokio::task::spawn_local(async move { h.submit(&l).await });
                    } else if p.pending.is_none() {
                        p.prompt();
                    }
                }
                _ => {
                    eof = true;
                    // nobody can answer a question any more
                    p.cancel_pending();
                }
            },
            _ = sigint.recv() => {
                if !interactive {
                    crate::signals::begin_forceable_shutdown(130);
                    exit_code = 130;
                    break;
                }
                if p.working.is_some() || p.pending.is_some() {
                    // Disarm the 5s process-exit watchdog immediately: this SIGINT cancels only
                    // the active turn/prompt, even if tool teardown takes >5s. A second Ctrl+C
                    // while idle (or during shutdown) still exits the process.
                    crate::signals::complete_turn_interrupt();
                    p.cancel_pending();
                    let _ = host.abort().await;
                } else {
                    crate::signals::begin_forceable_shutdown(0);
                    break;
                }
            }
            _ = &mut terminate => {
                crate::signals::begin_forceable_shutdown(0);
                break;
            }
            // an idle tick so EOF with an idle host ends the loop
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)), if eof => {}
        }
    }
    let _ = host.abort().await;
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use mare_shared::ui::{Mode, SelectItem};
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    #[tokio::test]
    async fn plain_multi_select_returns_all_chosen_indices() {
        let ui = UiShared::new();
        let mut out = Vec::new();
        let mut p = Plain { out: &mut out, ui, blocks: HashMap::new(), pending: None, working: None, quit: false };
        let (tx, rx) = oneshot::channel();
        p.handle(UiCmd::Select {
            title: "Choose".into(),
            title_right: vec![],
            split: Default::default(),
            items: vec![
                SelectItem { label: "one".into(), ..Default::default() },
                SelectItem { label: "heading".into(), header: true, ..Default::default() },
                SelectItem { label: "two".into(), ..Default::default() },
            ],
            selected: 0,
            multi: true,
            accept_label: None,
            allow_search: true,
            filter: String::new(),
            tabs: vec![],
            tab: 0,
            kind: "list".into(),
            keys: vec![],
            body: vec![],
            vim: false,
            highlight: None,
            reply: tx,
            token: 0,
            modal_id: 0,
            approval: false,
        });
        assert!(p.answer("1,2"));
        let answer = rx.await.unwrap().unwrap();
        assert_eq!(answer.selected_indices, [0, 2]);
    }

    #[tokio::test]
    async fn plain_select_preserves_original_item_id_when_select_update_is_ignored() {
        let ui = UiShared::new();
        let mut out = Vec::new();
        let mut p = Plain { out: &mut out, ui, blocks: HashMap::new(), pending: None, working: None, quit: false };
        let (tx, rx) = oneshot::channel();
        p.handle(UiCmd::Select {
            title: "Select model".into(),
            title_right: vec![],
            split: Default::default(),
            items: vec![
                SelectItem {
                    label: "deepseek/deepseek-chat".into(),
                    item_id: Some("deepseek/deepseek-chat".into()),
                    ..Default::default()
                },
                SelectItem {
                    label: "openai/gpt-6.1-terra".into(),
                    item_id: Some("openai/gpt-6.1-terra".into()),
                    ..Default::default()
                },
            ],
            selected: 0,
            multi: false,
            accept_label: None,
            allow_search: true,
            filter: String::new(),
            tabs: vec![],
            tab: 0,
            kind: "list".into(),
            keys: vec![],
            body: vec![],
            vim: false,
            highlight: None,
            reply: tx,
            token: 1,
            modal_id: 1,
            approval: false,
        });
        // Plain mode ignores SelectUpdate; typing "1" must still carry the original item's item_id.
        p.handle(UiCmd::SelectUpdate {
            token: 1,
            items: vec![SelectItem {
                label: "deepseek/deepseek-000-new".into(),
                item_id: Some("deepseek/deepseek-000-new".into()),
                ..Default::default()
            }],
            tabs: vec![],
        });
        assert!(p.answer("1"));
        let answer = rx.await.unwrap().unwrap();
        assert_eq!(answer.index, 0);
        assert_eq!(answer.item_id.as_deref(), Some("deepseek/deepseek-chat"));
    }

    struct EofInput(Option<oneshot::Sender<()>>);

    impl tokio::io::AsyncRead for EofInput {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncBufRead for EofInput {
        fn poll_fill_buf(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
            if let Some(tx) = self.0.take() {
                tx.send(()).unwrap();
            }
            Poll::Ready(Ok(&[]))
        }

        fn consume(self: Pin<&mut Self>, _: usize) {}
    }

    #[tokio::test(flavor = "current_thread")]
    async fn eof_cancels_pending_and_queued_questions_without_losing_replies() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let ui = UiShared::new();
                let (tx, rx) = mpsc::unbounded_channel();
                *ui.tx.write().unwrap() = Some(tx.clone());
                let opts = HostOptions {
                    cwd: dir.path().to_path_buf(),
                    mode: Mode::Tui,
                    config: serde_json::json!({}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({}),
                    no_user_plugins: true,
                };
                let host = Host::with_ui(opts, ui.clone()).await.unwrap();
                let (input_tx, input_rx) = oneshot::channel();
                tx.send(UiCmd::Input {
                    title: "input".into(),
                    placeholder: "".into(),
                    default: "".into(),
                    secret: false,
                    reply: input_tx,
                    modal_id: 0,
                })
                .unwrap();
                let (select_tx, select_rx) = oneshot::channel();
                let delayed_tx = tx.clone();
                let (eof_tx, eof_rx) = oneshot::channel();
                // EOF notification happens while drive polls next_line. On this LocalSet,
                // the sender task cannot run until drive finishes its synchronous EOF
                // branch (including cancel_pending) and yields at the next select.
                let late_question = tokio::task::spawn_local(async move {
                    eof_rx.await.unwrap();
                    let (late_tx, late_rx) = oneshot::channel();
                    delayed_tx
                        .send(UiCmd::Input {
                            title: "late input".into(),
                            placeholder: "".into(),
                            default: "".into(),
                            secret: false,
                            reply: late_tx,
                            modal_id: 0,
                        })
                        .unwrap();
                    assert_eq!(
                        tokio::time::timeout(std::time::Duration::from_secs(2), late_rx)
                            .await
                            .expect("a question arriving after EOF must cancel immediately")
                            .unwrap(),
                        Err(Dismissed::Cancelled),
                        "a question arriving after EOF must cancel"
                    );
                });
                tx.send(UiCmd::Select {
                    title: "select".into(),
                    title_right: vec![],
                    split: Default::default(),
                    items: vec![],
                    selected: 0,
                    multi: false,
                    accept_label: None,
                    allow_search: true,
                    filter: String::new(),
                    tabs: vec![],
                    tab: 0,
                    kind: "list".into(),
                    keys: vec![],
                    body: vec![],
                    vim: false,
                    highlight: None,
                    reply: select_tx,
                    token: 0,
                    modal_id: 0,
                    approval: false,
                })
                .unwrap();
                let mut out = Vec::new();
                let finished = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    drive(host.clone(), ui, rx, EofInput(Some(eof_tx)), &mut out, None, false),
                )
                .await;
                assert_eq!(input_rx.await.unwrap(), Err(Dismissed::Cancelled), "EOF is not the input string q");
                finished.expect("plain frontend stalled after EOF");
                assert!(
                    matches!(select_rx.await.unwrap(), Err(Dismissed::Cancelled)),
                    "a queued question must not replace the pending one"
                );
                late_question.await.expect("late question task failed");
                host.shutdown().await;
            })
            .await;
    }

    #[test]
    fn plain_cancel_modal_clears_pending_and_input_default_works() {
        let ui = UiShared::new();
        let mut out = Vec::new();
        let mut p = Plain { out: &mut out, ui, blocks: HashMap::new(), pending: None, working: None, quit: false };

        // 1. Input with modal_id=42 is cancelled by CancelModal { modal_id: 42 }, freeing pending slot
        let (tx1, _rx1) = oneshot::channel();
        p.handle(UiCmd::Input {
            title: "First".into(),
            placeholder: "".into(),
            default: "".into(),
            secret: false,
            reply: tx1,
            modal_id: 42,
        });
        assert!(p.pending.is_some());
        p.handle(UiCmd::CancelModal { modal_id: 99 }); // non-matching id leaves it open
        assert!(p.pending.is_some());
        p.handle(UiCmd::CancelModal { modal_id: 42 });
        assert!(p.pending.is_none());

        // 2. Subsequent Input with default returns default on empty line
        let (tx2, mut rx2) = oneshot::channel();
        p.handle(UiCmd::Input {
            title: "Second".into(),
            placeholder: "".into(),
            default: "prefilled".into(),
            secret: false,
            reply: tx2,
            modal_id: 43,
        });
        assert!(p.answer(""));
        assert_eq!(rx2.try_recv().unwrap(), Ok("prefilled".to_string()));
        assert!(String::from_utf8_lossy(&out).contains("[default: prefilled]"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plain_drive_restores_startup_gc_threshold() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let ui = UiShared::new();
                let (tx, rx) = mpsc::unbounded_channel();
                *ui.tx.write().unwrap() = Some(tx);
                let opts = HostOptions {
                    cwd: dir.path().to_path_buf(),
                    mode: Mode::Tui,
                    config: serde_json::json!({}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({}),
                    no_user_plugins: true,
                };
                let host = Host::with_ui(opts, ui.clone()).await.unwrap();
                let gc_param = |h: &Host| -> i32 {
                    h.lua().load(r#"return collectgarbage("param", "minormajor")"#).eval().unwrap()
                };
                assert_eq!(gc_param(&host), 1000, "Mode::Tui startup must begin with raised GC threshold");
                let mut out = Vec::new();
                let _ = drive(host.clone(), ui, rx, tokio::io::empty(), &mut out, None, false).await;
                assert_eq!(gc_param(&host), 68, "plain mode must restore default generational GC threshold");
                host.shutdown().await;
            })
            .await;
    }
}
