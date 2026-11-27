//! Scrollback commits: `ui/commit-before` holds, replay after a resize.

use crate::app::*;

/// Lines waiting for `ui/commit-before` before they go into scrollback (shown in the live area
/// meanwhile, committed in order).
pub(in crate::app) struct HeldCommit {
    id: u64,
    /// `self.committed` before this batch moved finished blocks into `held`.
    committed_before: usize,
    /// `(block_idx, partial_before)` when this batch included streaming lines from `block_idx`.
    partial_before: Option<(usize, usize)>,
    pub(super) lines: Vec<Line>,
    ready: Option<Vec<Line>>,
}

impl App {
    /// Commits pass `ui/commit-before` listeners (if any) before they reach scrollback.
    pub(super) fn route_commit(
        &mut self,
        commit: Vec<Line>,
        committed_before: usize,
        partial_before: Option<(usize, usize)>,
    ) -> Vec<Line> {
        if !commit.is_empty() {
            let hooked = self.host.kernel.listener_count("ui/commit-before") > 0;
            if hooked || !self.held.is_empty() {
                self.held_seq += 1;
                let id = self.held_seq;
                let ready = if hooked { None } else { Some(commit.clone()) };
                if hooked {
                    let texts: Vec<String> = commit.iter().map(Line::plain).collect();
                    let (h, tx) = (self.host.clone(), self.app_tx.clone());
                    tokio::task::spawn_local(async move {
                        let run = async {
                            let lua = h.lua();
                            let v = mare_shared::lua::json_to_lua(lua, &serde_json::json!({ "lines": texts })).ok()?;
                            let out = h.kernel.waterfall("ui/commit-before", v, mlua::MultiValue::new()).await;
                            let j = mare_shared::lua::lua_to_json(lua, out).ok()?;
                            let lines = j.get("lines")?.as_array()?;
                            Some(lines.iter().map(|l| l.as_str().unwrap_or_default().to_string()).collect::<Vec<_>>())
                        };
                        // a stuck listener can't hold scrollback hostage: originals after 2s
                        let lines = tokio::time::timeout(Duration::from_secs(2), run).await.ok().flatten();
                        let _ = tx.send(AppMsg::CommitReady(id, lines));
                    });
                }
                self.held.push_back(HeldCommit { id, committed_before, partial_before, lines: commit, ready });
            } else {
                self.after_commit(&commit);
                return commit;
            }
        }
        let mut out = vec![];
        while self.held.front().is_some_and(|h| h.ready.is_some()) {
            out.extend(self.held.pop_front().and_then(|h| h.ready).unwrap_or_default());
        }
        if !out.is_empty() {
            self.after_commit(&out);
        }
        out
    }

    /// `ui/commit-before` answered: unchanged lines keep their styles, rewritten or added ones
    /// are untrusted text (SGR colors parsed, every other escape removed).
    pub(in crate::app) fn commit_ready(&mut self, id: u64, lines: Option<Vec<String>>) {
        let Some(h) = self.held.iter_mut().find(|h| h.id == id) else { return };
        h.ready = Some(match lines {
            None => h.lines.clone(),
            Some(texts) => texts
                .iter()
                .enumerate()
                .map(|(i, t)| match h.lines.get(i) {
                    Some(l) if l.plain() == *t => l.clone(),
                    _ => mare_shared::ansi::sgr_line(&mare_shared::ansi::sanitize(t, true), Style::default()),
                })
                .collect(),
        });
    }

    fn after_commit(&self, lines: &[Line]) {
        if self.host.kernel.listener_count("ui/commit-after") > 0 {
            let texts: Vec<String> = lines.iter().map(Line::plain).collect();
            self.emit("ui/commit-after", serde_json::json!({ "lines": texts }));
        }
    }

    /// Everything still held, in order (exit: nothing may be lost).
    pub(in crate::app) fn drain_held(&mut self) -> Vec<Line> {
        self.held.drain(..).flat_map(|h| h.ready.unwrap_or(h.lines)).collect()
    }

    /// Resize reflow: re-render committed history at the current width from
    /// the in-memory records, clear the terminal's scrollback and screen, and replay all of it
    /// (up to the terminal's row cap). A streaming block's committed part is recomputed from its
    /// committed *source position*, not the old row count.
    pub(in crate::app) fn rebuild_history(&mut self) {
        if self.transcript.is_open() {
            // Never emit CSI 3J or main-screen history while the alternate-screen overlay is active.
            self.transcript.frame.clear();
            self.reflow.invalidated(app_now());
            return;
        }
        // the scrollback can't be redrawn: its pictures are waited for, not reserved
        let _images = mare_tui::image::blocking();
        let theme = self.theme();
        let w = self.width.max(10);
        // Blocks still sitting in `self.held` have not cleared `ui/commit-before` yet and stay in
        // the live area until `CommitReady`; skip them here so they are neither bypassed nor
        // committed twice.
        let replay_committed =
            self.held.iter().map(|h| h.committed_before).min().unwrap_or(self.committed).min(self.committed);
        if self.committed < self.blocks.len() && replay_committed == self.committed {
            let b = &mut self.blocks[self.committed];
            if b.partial > 0
                && let Some(md) = b.md.as_mut()
            {
                let src = b.sc.drained_src.min(md.text().len());
                let n = md.rows_for(src, w.saturating_sub(2), &theme);
                md.drop_cache();
                b.sc.rewidth(n);
                b.partial = n;
            }
        }
        let cap = self.reflow_cap;
        let show_reasoning = self.show_reasoning;
        let expand_key = self.editor_action_key("toggle-expand");
        let tail_partial = if replay_committed == self.committed
            && self.committed < self.blocks.len()
            && self.blocks[self.committed].partial > 0
        {
            let idx = self.committed;
            let held_partial =
                self.held.iter().filter_map(|h| h.partial_before).filter(|(bi, _)| *bi == idx).map(|(_, p)| p).min();
            let p = held_partial.map_or(self.blocks[idx].partial, |hp| hp.min(self.blocks[idx].partial));
            if p > 0 {
                let mut r =
                    self.blocks[idx].render(w, &theme, 0, self.expand_all, show_reasoning, expand_key.as_deref());
                r.truncate(p);
                Some((idx, r))
            } else {
                None
            }
        } else {
            None
        };
        let mut total_rows = tail_partial.as_ref().map_or(0, |(idx, r)| r.len() + usize::from(*idx > 0));
        let mut chunks: Vec<(usize, Vec<Line>)> = Vec::new();
        for idx in (0..replay_committed).rev() {
            if total_rows >= cap {
                break;
            }
            // Historical image payloads are materialized only for the current visible tail.
            // Ctrl+T has its own observable viewport for older rows.
            let r = if !self.blocks[idx].has_images(self.expand_all, show_reasoning) {
                self.blocks[idx].render(w, &theme, 0, self.expand_all, show_reasoning, expand_key.as_deref())
            } else {
                let measured = {
                    let _measure = mare_tui::image::measure_only();
                    self.blocks[idx].render(w, &theme, 0, self.expand_all, show_reasoning, expand_key.as_deref())
                };
                if total_rows < self.height {
                    let _window = mare_tui::image::window(
                        measured.len().saturating_sub(self.height - total_rows),
                        measured.len(),
                    );
                    self.blocks[idx].render(w, &theme, 0, self.expand_all, show_reasoning, expand_key.as_deref())
                } else {
                    measured
                }
            };
            if let Some(md) = self.blocks[idx].md.as_mut() {
                md.drop_cache();
            }
            total_rows += r.len() + usize::from(idx > 0);
            chunks.push((idx, r));
        }
        let mut rows = Vec::with_capacity(total_rows.min(cap));
        let mut to_drop = total_rows.saturating_sub(cap);
        for (idx, r) in chunks.into_iter().rev() {
            if idx > 0 {
                if to_drop > 0 {
                    to_drop -= 1;
                } else {
                    rows.push(Line::empty());
                }
            }
            if to_drop >= r.len() {
                to_drop -= r.len();
            } else if to_drop > 0 {
                rows.extend(r.into_iter().skip(to_drop));
                to_drop = 0;
            } else {
                rows.extend(r);
            }
        }
        if let Some((idx, r)) = tail_partial {
            if idx > 0 {
                if to_drop > 0 {
                    to_drop -= 1;
                } else {
                    rows.push(Line::empty());
                }
            }
            if to_drop >= r.len() {
                // dropped
            } else if to_drop > 0 {
                rows.extend(r.into_iter().skip(to_drop));
            } else {
                rows.extend(r);
            }
        }
        let bytes = self.renderer.replay_bytes(&rows);
        self.invalidate_cpr();
        self.sync_position_known();
        mare_tui::out::write(bytes);
        self.emit("ui/reflow", serde_json::json!({ "rows": rows.len() }));
    }
}
