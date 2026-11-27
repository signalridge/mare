//! Processes and background jobs: bounded waits, complete spill files, whole
//! process trees cleaned up, the job ring buffer, background notifications and the
//! foreground-timeout → background hand-off.
#![cfg(unix)]

mod background_jobs;
mod job_io;
mod proc_and_pty;

use crate::common::*;
use serde_json::json;

pub(super) fn alive(pid: &str) -> bool {
    pid.trim().parse::<u32>().is_ok_and(pid_alive)
}

/// Start `snippet` (Lua) where `TREE(name)` is a shell command that leaves a grandchild running and
/// records its pid; assert every named grandchild dies.
pub(super) async fn assert_trees_die(snippet: &str, names: &[&str]) {
    let env = TestEnv::new().await;
    let h = env.host().builtins(&["jobs"]).no_session().build().await;
    let dir = env.cwd.to_string_lossy().to_string();
    global(&h, "DIR", dir);
    exec(
        &h,
        r#"function TREE(name) return "sleep 120 & echo $! > " .. DIR .. "/" .. name .. ".pid; wait" end
                -- block until TREE(name) has recorded its grandchild (a slow runner may take long to start the shell)
                function PID_UP(name)
                  local file, deadline = DIR .. "/" .. name .. ".pid", mare.task.clock() + 60000
                  while (mare.fs.read(file) or "") == "" do
                    assert(mare.task.clock() < deadline, name .. ": the grandchild never started")
                    mare.task.sleep(10)
                  end
                end"#,
    )
    .await;
    exec(&h, snippet).await;
    for name in names {
        let pidfile = env.cwd.join(format!("{name}.pid"));
        wait_until(WAIT_SECS, &format!("{name}: grandchild pid"), || {
            let p = pidfile.clone();
            async move { std::fs::read_to_string(&p).is_ok_and(|s| !s.trim().is_empty()) }
        })
        .await;
        let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
        wait_until(WAIT_SECS, &format!("{name}: grandchild {pid} to exit"), || {
            let pid = pid.clone();
            async move { !alive(&pid) }
        })
        .await;
    }
    h.shutdown().await;
}

/// One tool call through the tools service (no agent turn) → its result as JSON.
pub(super) async fn call_tool(h: &mare_host::Host, name: &str, args: serde_json::Value) -> serde_json::Value {
    let call = json!({ "id": "t1", "name": name, "arguments": args });
    global(h, "CALL", mare_shared::lua::json_to_lua(h.lua(), &call).unwrap());
    let v: mlua::Value = eval(h, "return T:execute(CALL, {})").await;
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

pub(super) fn content(r: &serde_json::Value) -> String {
    match &r["content"] {
        serde_json::Value::String(s) => s.clone(),
        c => c.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
    }
}

/// The job id a "moved to background job <id>" / "Started background job <id>" text names.
pub(super) fn job_id(text: &str) -> String {
    let at = text.find("bash-").unwrap_or_else(|| panic!("no job id in {text}"));
    text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect()
}

/// All notification texts in the agent's context, joined.
pub(super) async fn notifications(h: &mare_host::Host) -> String {
    eval(
        h,
        r#"local out = {}
           for _, m in ipairs(A:list_messages().data) do
             if m.notification then out[#out + 1] = m.content[1].text end
           end
           return table.concat(out, "\n---\n")"#,
    )
    .await
}
