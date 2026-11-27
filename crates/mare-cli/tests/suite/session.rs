//! Session persistence: the JSONL tree (branch / fork), resume (ancestry across compactions,
//! interrupted turns, crash recovery), the persistence barrier, and sub-sessions
//! (pi agent-session-branching / tree-navigation, Codex compact_resume_fork).

use crate::common::*;
use serde_json::{Value, json};

mod branching;
mod crash;
mod damaged_fork;
mod edit_results;
mod filters;
#[cfg(debug_assertions)]
mod fork_durability;
mod lifecycle;
mod metadata_and_transitions;
mod persistence;
mod picker;
mod query;
mod restart;
mod resume;

// --- batch S / SE: session storage integrity and the session UI ----------

/// Capture `mare.ui.notify` / `set_editor` / `confirm` into globals (`N` = notices, `ED` = the
/// last editor text) and make the UI look connected.
async fn fake_ui(h: &mare_host::Host) {
    exec(
        h,
        r#"N = {}
           mare.ui.connected = function() return true end
           mare.ui.notify = function(t) N[#N + 1] = tostring(t) end
           mare.ui.set_editor = function(t, o) ED = t; ED_KEEP = o and o.keep_draft end
           mare.ui.confirm = function() return true end"#,
    )
    .await;
}

async fn notices(h: &mare_host::Host) -> String {
    eval(h, "return table.concat(N, '\\n')").await
}

/// Every row of the session index (all kinds, archived too), as the Lua side sees them.
fn index_rows() -> Vec<Value> {
    let root = mare_shared::paths::sessions_dir();
    let ix = mare_store::index::ready(&root).unwrap();
    let q = mare_store::index::Query { kinds: mare_store::index::Kinds::Any, ..Default::default() };
    let rows = ix.lock().unwrap().list(&q).unwrap().0;
    rows.iter().map(|r| r.to_json()).collect()
}

fn write_lines(path: &std::path::Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
}
