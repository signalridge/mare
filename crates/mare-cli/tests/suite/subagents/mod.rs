//! Sub-agents (`mare.subagent.start`) and permissions: a child inherits the
//! parent's preset as its ceiling and may only be stricter; its approval requests bubble up to the
//! parent's approval path, labelled with their origin, and are honoured; headless with nobody to
//! answer denies.

use crate::common::*;
use serde_json::{Value, json};

pub(super) const SUBAGENT_BUILTINS: &[&str] = &["subagent", "agents"];

/// the child writes `file`, then finishes
fn write_script(file: &str) -> Value {
    json!([
        { "tool_calls": [{ "name": "write", "arguments": { "path": file, "content": "hi" } }] },
        { "text": "done" }
    ])
}

/// A parent host (permissions on, trusted project) at `preset`; the child shares the faux script.
async fn parent(env: &TestEnv, preset: &str, script: Value) -> std::sync::Arc<mare_host::Host> {
    parent_with(env, preset, script, json!([])).await
}

/// `parent` with these built-ins turned off (their name slots closed).
async fn parent_with(env: &TestEnv, preset: &str, script: Value, disabled: Value) -> std::sync::Arc<mare_host::Host> {
    let mut plugins = json!({ "permissions": { "enabled": true, "preset": preset } });
    for name in disabled.as_array().into_iter().flatten().filter_map(Value::as_str) {
        plugins[name] = json!({ "enabled": false });
    }
    let mut b = env
        .host()
        .builtins(SUBAGENT_BUILTINS)
        .faux(script)
        .config(json!({ "plugins": plugins }))
        .args(json!({ "no_session": true }));
    b.opts.project_trusted = true;
    let h = b.build().await;
    expose(&h);
    global(&h, "CWD", env.cwd.to_string_lossy().to_string());
    global(&h, "P", h.service_table("permissions").unwrap());
    // SPAWN(opts) runs a child to the end in this workspace (no session of its own)
    exec(
        &h,
        r#"function SPAWN(o)
               o.cwd, o.no_session = CWD, true
               local a = mare.subagent.start(o)
               return pcall(a.wait, a)
           end
           -- the parent's approval listener: records each request and answers with ANSWER
           ASKED = {}
           ANSWER = "allow"
           function LISTEN()
               local seen = {}
               C.cordis:get("policy"):voter("tool/approval", function(call, info)
                   local key = call.id or #ASKED + 1
                   if not seen[key] then
                       seen[key] = true
                       ASKED[#ASKED + 1] = { tool = call.name, origin = call.origin, info_origin = info.origin }
                   end
                   return { decision = ANSWER, grant = GRANT }
               end)
           end"#,
    )
    .await;
    h
}

/// the child runs `touch <file>` asking for `all`, then finishes
fn touch_script(file: &str) -> Value {
    json!([
        { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("touch {file}"),
            "sandbox_permissions": "all", "justification": "make the file" } }] },
        { "text": "done" }
    ])
}

/// the shared rules file of `env`, as JSON
fn rules_file(env: &TestEnv) -> Value {
    let file = env.home.path().join("state/permissions.json");
    std::fs::read_to_string(&file).map(|s| serde_json::from_str(&s).unwrap()).unwrap_or(Value::Null)
}

mod contract;
mod identity;
mod lifecycle;
mod permissions;
mod rules_and_dialogs;
mod transcripts;
