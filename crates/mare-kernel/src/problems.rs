//! The problems a kernel keeps for `/plugins`, `/status` and `mare doctor` beyond plugin start
//! failures: runtime errors of plugin handlers (listeners, tasks, timers, dispose hooks), each
//! one logged and notified (`kernel/error`: the frontend shows it under the turn's prompt, or as
//! a notice outside a turn), one problem
//! line per handler with its latest error, and the clean-up a successful full reload does.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::Kernel;
use crate::plugins::prims::current_owner;

/// Non-fatal declaration warning attached to a plugin, keyed by `(plugin, reporter, key)`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeclarationProblem {
    plugin: String,
    reporter: String,
    key: String,
    message: String,
    reporter_owner: Option<u64>,
    target_owner: Option<u64>,
}

struct DeclarationProblemsStore(Arc<Mutex<Vec<DeclarationProblem>>>);

impl Kernel {
    fn declaration_store(&self) -> Arc<Mutex<Vec<DeclarationProblem>>> {
        if let Some(store) = self.lua().app_data_ref::<DeclarationProblemsStore>() {
            return Arc::clone(&store.0);
        }
        let store = Arc::new(Mutex::new(Vec::new()));
        let _ = self.lua().set_app_data(DeclarationProblemsStore(Arc::clone(&store)));
        store
    }

    fn prune_against_live_owners(&self, list: &mut Vec<DeclarationProblem>) {
        let (live, live_by_plugin): (HashSet<u64>, std::collections::HashMap<String, u64>) = {
            let st = self.st();
            let mut live = HashSet::new();
            let mut by_plugin = std::collections::HashMap::new();
            for (id, c) in &st.ctxs {
                if !c.disposed {
                    live.insert(*id);
                    by_plugin.entry(c.plugin.clone()).and_modify(|cur: &mut u64| *cur = (*cur).max(*id)).or_insert(*id);
                }
            }
            (live, by_plugin)
        };
        list.retain_mut(|p| {
            if let Some(rid) = p.reporter_owner
                && !live.contains(&rid)
            {
                return false;
            }
            match p.target_owner {
                Some(tid) => live.contains(&tid),
                None => {
                    if let Some(&tid) = live_by_plugin.get(&p.plugin) {
                        p.target_owner = Some(tid);
                    }
                    true
                }
            }
        });
    }

    /// Bind any unbound `target_owner` entries to live plugin owners once startup finishes.
    pub fn bind_declaration_problem_targets(&self) {
        let store = self.declaration_store();
        let mut list = store.lock().unwrap();
        self.prune_against_live_owners(&mut list);
    }

    /// Record or replace a declaration warning for `plugin` under `(plugin, reporter, key)`.
    pub fn report_declaration_problem(&self, plugin: &str, key: &str, message: &str) {
        let reporter_owner = current_owner();
        let (reporter, target_owner) = {
            let st = self.st();
            let reporter = reporter_owner
                .and_then(|id| st.ctxs.get(&id).map(|c| c.plugin.clone()))
                .unwrap_or_else(|| "host".to_string());
            let target_owner =
                st.ctxs.iter().filter(|(_, c)| !c.disposed && c.plugin == plugin).map(|(id, _)| *id).max();
            (reporter, target_owner)
        };
        let store = self.declaration_store();
        let mut list = store.lock().unwrap();
        self.prune_against_live_owners(&mut list);
        if let Some(existing) = list.iter_mut().find(|p| p.plugin == plugin && p.reporter == reporter && p.key == key) {
            existing.message = message.to_string();
            existing.reporter_owner = reporter_owner;
            existing.target_owner = target_owner;
        } else {
            list.push(DeclarationProblem {
                plugin: plugin.to_string(),
                reporter,
                key: key.to_string(),
                message: message.to_string(),
                reporter_owner,
                target_owner,
            });
        }
    }

    /// Clear a declaration warning previously reported by the current caller under `(plugin, key)`.
    pub fn clear_declaration_problem(&self, plugin: &str, key: &str) {
        let reporter = {
            let st = self.st();
            current_owner()
                .and_then(|id| st.ctxs.get(&id).map(|c| c.plugin.clone()))
                .unwrap_or_else(|| "host".to_string())
        };
        let store = self.declaration_store();
        let mut list = store.lock().unwrap();
        self.prune_against_live_owners(&mut list);
        list.retain(|p| !(p.plugin == plugin && p.reporter == reporter && p.key == key));
    }

    /// All active declaration problems: `(plugin, reporter, key, kind, message)`.
    pub fn declaration_problems(&self) -> Vec<(String, String, String, &'static str, String)> {
        let store = self.declaration_store();
        let mut list = store.lock().unwrap();
        self.prune_against_live_owners(&mut list);
        list.iter()
            .map(|p| (p.plugin.clone(), p.reporter.clone(), p.key.clone(), "declaration", p.message.clone()))
            .collect()
    }

    /// Active declaration problems for `plugin`: `(plugin, reporter, key, kind, message)`.
    pub fn declaration_problems_for(&self, plugin: &str) -> Vec<(String, String, String, &'static str, String)> {
        self.declaration_problems().into_iter().filter(|p| p.0 == plugin).collect()
    }
}

impl Kernel {
    pub fn problems(&self) -> Vec<String> {
        self.st().problems.clone()
    }

    /// Record a startup problem from outside the plugin system (e.g. a package that couldn't be
    /// installed); shown with the plugin problems.
    pub fn add_problem(&self, msg: String) {
        self.st().problems.push(msg);
    }

    /// A failure outside an event callback (a background task, a timer, a dispose hook): logged
    /// and shown in the UI through `kernel/error`.
    pub fn notify_error(&self, plugin: &str, what: &str, err: &str) {
        self.handler_failed(plugin, what, None, err);
    }

    /// Drop the problems `keep` rejects (a successful full reload clears what startup reported).
    pub fn retain_problems(&self, keep: impl Fn(&str) -> bool) {
        self.st().problems.retain(|p| keep(p));
    }

    /// Forget the handler errors and their problem lines (a successful full reload).
    pub fn clear_handler_errors(&self) {
        let mut st = self.st();
        let heads: Vec<String> = st.handler_errors.iter().map(|(p, w)| handler_error_head(p, w)).collect();
        st.problems.retain(|p| !heads.iter().any(|h| p.starts_with(h.as_str())));
        st.handler_errors.clear();
    }

    /// Record a runtime error of `plugin`'s `what` (a listener, task, timer, dispose hook): its
    /// problem line names it with the latest error. The plugin stays loaded.
    fn record_handler_error(&self, plugin: &str, what: &str, err: &str) {
        let mut st = self.st();
        st.handler_errors.insert((plugin.to_string(), what.to_string()));
        let head = handler_error_head(plugin, what);
        let line = format!("{head}: {err}");
        match st.problems.iter_mut().find(|p| p.starts_with(head.as_str())) {
            Some(p) => *p = line,
            None => st.problems.push(line),
        }
    }

    /// Record a handler error, log it and tell the frontends, every time (`kernel/error
    /// {message, plugin, event, what, error}`; `error` is the first line without mlua's prefix
    /// and the traceback). `event` is the failing listener's event, if it was one; an error of a
    /// `kernel/error` listener itself is only recorded and logged (no loop).
    pub(crate) fn handler_failed(&self, plugin: &str, what: &str, event: Option<&str>, err: &str) {
        self.record_handler_error(plugin, what, err);
        let msg = format!("[{plugin}] {what} failed: {err}");
        tracing::warn!("{msg}");
        // a callback aborted by the watchdog marks its plugin as errored (shown by /status,
        // doctor and the UI's kernel/error notice)
        if err.contains("watchdog:") {
            let place = match event {
                Some(ev) => format!("in `{ev}`"),
                None => format!("in a {what}"),
            };
            let line = format!("plugin {plugin} errored: aborted by the watchdog {place}");
            let mut st = self.st();
            if !st.problems.contains(&line) {
                st.problems.push(line);
            }
        }
        if event == Some("kernel/error") {
            return;
        }
        let Ok(t) = self.lua().create_table() else { return };
        let _ = t.set(mare_shared::MESSAGE_KEY, msg);
        let _ = t.set("plugin", plugin);
        let _ = t.set("event", event.unwrap_or(what));
        let _ = t.set(
            "what",
            match event {
                Some(ev) => format!("hook `{ev}`"),
                None => what.to_string(),
            },
        );
        let _ = t.set("error", first_line(err));
        let k = self.clone();
        // fire and forget: no recursion into the emit that failed
        mare_shared::txn_scope::spawn_local(async move {
            k.emit("kernel/error", mlua::MultiValue::from_vec(vec![mlua::Value::Table(t)])).await;
        });
    }
}

/// The problem-line prefix of a handler error.
fn handler_error_head(plugin: &str, what: &str) -> String {
    format!("plugin {plugin}: {what} failed")
}

/// An error's first line for the screen: no traceback, no mlua "runtime error: " prefix.
fn first_line(err: &str) -> String {
    let mut line = err.lines().next().unwrap_or("").trim();
    while let Some(rest) = line.strip_prefix("runtime error: ") {
        line = rest;
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use crate::Kernel;
    use crate::plugins::prims::in_owner;

    #[test]
    fn first_line_drops_prefixes_and_traceback() {
        let e = "runtime error: init.lua:3: boom\nstack traceback:\n\t[C]: in ?";
        assert_eq!(super::first_line(e), "init.lua:3: boom");
    }

    #[tokio::test]
    async fn declaration_problems_are_keyed_and_pruned_by_owner_lifecycle() {
        let k = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
        let target = k.open_owner("bad-plugin", "user").unwrap();
        let rep_a = k.open_owner("llm", "builtin").unwrap();
        let rep_b = k.open_owner("other", "user").unwrap();

        in_owner(Some(rep_a), async {
            k.report_declaration_problem("bad-plugin", "k1", "first msg");
            k.report_declaration_problem("bad-plugin", "k2", "second msg");
            k.report_declaration_problem("bad-plugin", "k1", "updated msg");
        })
        .await;
        in_owner(Some(rep_b), async {
            k.report_declaration_problem("bad-plugin", "k1", "from other reporter");
        })
        .await;

        let all = k.declaration_problems_for("bad-plugin");
        assert_eq!(all.len(), 3, "expected 2 from llm + 1 from other: {all:?}");
        assert!(all.contains(&("bad-plugin".into(), "llm".into(), "k1".into(), "declaration", "updated msg".into())));
        assert!(all.contains(&("bad-plugin".into(), "llm".into(), "k2".into(), "declaration", "second msg".into())));
        assert!(all.contains(&(
            "bad-plugin".into(),
            "other".into(),
            "k1".into(),
            "declaration",
            "from other reporter".into()
        )));

        // Clearing by rep_b only removes rep_b's (bad-plugin, k1)
        in_owner(Some(rep_b), async {
            k.clear_declaration_problem("bad-plugin", "k1");
        })
        .await;
        let after_clear = k.declaration_problems_for("bad-plugin");
        assert_eq!(after_clear.len(), 2, "{after_clear:?}");

        // Closing reporter removes its remaining entries
        k.owner_close(rep_a);
        assert!(k.declaration_problems_for("bad-plugin").is_empty());

        // New problem on target is pruned when target unloads/reloads
        in_owner(Some(rep_b), async {
            k.report_declaration_problem("bad-plugin", "k3", "will vanish on target close");
        })
        .await;
        assert_eq!(k.declaration_problems_for("bad-plugin").len(), 1);
        k.owner_close(target);
        assert!(k.declaration_problems_for("bad-plugin").is_empty());
    }
}
