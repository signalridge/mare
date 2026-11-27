//! Operability: credential precedence and
//! sources, redacted approval records, session attachments, the per-source /context breakdown,
//! branch summaries through compaction, sub-agent/workflow status, tool-table changes in
//! /profile, and the runtime info's "already allowed" list.

use std::process::Stdio;

use crate::common::*;
use serde_json::json;

fn write_auth(env: &TestEnv, mut body: serde_json::Value) {
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("auth.json");
    body.as_object_mut().unwrap().insert("version".into(), json!(1));
    std::fs::write(&p, body.to_string()).unwrap();
    // owner-only, as credential files must be on Unix (Windows has no mode bits to set)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Environment variables have the lowest priority: auth.json wins over them.
#[tokio::test]
async fn auth_json_beats_the_environment_variable() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "ENV-KEY-0000");
        write_auth(&env, json!({ "openai": { "key": "FILE-KEY-1111" } }));
        let h = env.host().builtins(&["compact", "usage", "subagent", "status", "profile"]).no_session().build().await;
        global(&h, "L", h.service_table("llm").unwrap());
        let (key, src): (String, String) = eval(
            &h,
            r#"return L:api_key("openai", { "OPENAI_API_KEY" }), L:api_key_source("openai", { "OPENAI_API_KEY" })"#,
        )
        .await;
        assert_eq!(key, "FILE-KEY-1111");
        assert_eq!(src, "auth.json");
        h.shutdown().await;
    })
    .await
}

const PROVIDER_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "DEEPSEEK_API_KEY",
    "MOONSHOT_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENCODE_API_KEY",
];

fn doctor(home: &std::path::Path, envs: &[(&str, &str)]) -> String {
    let mut c = mare_cmd();
    c.arg("doctor").env("MARE_HOME", home).current_dir(home).stdin(Stdio::null());
    // the developer's own credentials never take part
    for k in PROVIDER_VARS {
        c.env_remove(k);
    }
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.run_output();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// `mare doctor` names where each provider's credential comes from, never the secret.
#[test]
fn doctor_lists_credential_sources_without_secrets() {
    let home = tempfile::tempdir().unwrap();
    let out = doctor(home.path(), &[("ANTHROPIC_API_KEY", "sk-ant-SECRET-VALUE-0123456789")]);
    assert!(out.contains("credential sources"), "{out}");
    assert!(out.contains("env ANTHROPIC_API_KEY"), "{out}");
    assert!(!out.contains("SECRET-VALUE"), "the secret leaked: {out}");
}

/// `mare doctor` reports the terminal's known capabilities on every platform.
#[test]
fn doctor_reports_terminal_capabilities_and_key_conflicts() {
    let home = tempfile::tempdir().unwrap();
    let out = doctor(
        home.path(),
        &[("TERM_PROGRAM", "Apple_Terminal"), ("TERM_PROGRAM_VERSION", "455.1"), ("TMUX", ""), ("STY", "")],
    );
    let caps = out.lines().find(|l| l.contains("terminal capabilities")).unwrap_or_else(|| panic!("{out}"));
    assert!(caps.contains("Terminal.app (before macOS Tahoe)") && caps.contains("one cell wide"), "{caps}");
    assert!(out.contains("key conflicts"), "{out}");
}

/// Disabling `keymap-default` leaves modal accept/cancel unbound, which `mare doctor` reports
/// .
#[test]
fn doctor_reports_unbound_accept_and_cancel_when_keymap_default_is_disabled() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("init.lua"), "mare.plugin('keymap-default', { enabled = false })\n").unwrap();
    let out = doctor(home.path(), &[]);
    assert!(
        out.contains("unbound keymap actions")
            && out.contains("select.accept (select)")
            && out.contains("select.back (select)")
            && out.contains("confirm.accept (confirm)")
            && out.contains("confirm.cancel (confirm)"),
        "{out}"
    );
}

#[test]
fn doctor_reports_when_no_permissions_service_or_session_sandbox_is_active() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("init.lua"), "mare.plugin('permissions', { enabled = false })\n").unwrap();
    let out = doctor(home.path(), &[]);
    assert!(out.contains("no permissions service") && out.contains("session sandbox: none"), "{out}");
    assert!(out.contains("other policy hooks may apply"), "{out}");
}

#[test]
fn doctor_identifies_a_replacement_permission_provider_without_claiming_its_guarantees() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    let plugin = config.join("plugins/custom-permissions");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(config.join("init.lua"), "mare.plugin('permissions', { enabled = false })\n").unwrap();
    std::fs::write(
        plugin.join("init.lua"),
        r#"return { name = 'custom-permissions', provide = { 'permissions' }, apply = function(ctx)
             local Perm = require('cordis').class('Perm', mare.Service)
             function Perm:constructor(c) Perm.super.constructor(self, c, 'permissions') end
             function Perm:preset() return 'custom' end
             for _, m in ipairs({ 'get', 'list', 'set', 'presets', 'rules', 'check', 'rule_verdict', 'describe', 'add_rules', 'request', 'grants', 'describe_request', 'resolve_request', 'commit_request', 'policy_revision', 'is_partial' }) do
               Perm[m] = function() end
             end
             ctx:plugin(Perm)
           end }"#,
    )
    .unwrap();
    let out = doctor(home.path(), &[]);
    assert!(out.contains("permissions service: custom-permissions (user)"), "{out}");
    assert!(out.contains("session sandbox: none") && out.contains("provider behavior is not verified"), "{out}");
}

/// Approval records pass the unified redactor before they reach the session file.
#[tokio::test]
async fn approval_records_are_redacted_in_the_session_file() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage", "subagent", "status", "profile"]).faux(json!([{ "text": "ok" }])).build().await;
        h.prompt("hi", json!({})).await.unwrap();
        expose(&h);
        let path: String = eval(
            &h,
            r#"S:add_record("permission/request", { command = "curl -H 'Authorization: Bearer sk-ABCDEFGHIJKLMNOPQRSTUVWX'" })
               return S:info().path"#,
        )
        .await;
        h.shutdown().await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("permission/request"), "{text}");
        assert!(!text.contains("sk-ABCDEFGHIJKLMNOP"), "secret written to the session: {text}");
    })
    .await
}

/// Pasted images live in `<session>.attachments/`, referenced from the JSONL; resume reads them
/// back and the files belong to the session (deleted with it).
#[tokio::test]
async fn pasted_images_are_stored_as_session_attachments() {
    local(async {
        let env = TestEnv::new().await;
        let data = "UE5HLUJZVEVTLTEyMzQ1Ng=="; // base64 of "PNG-BYTES-123456"
        let img = json!([{ "type": "image", "mime": "image/png", "data": data }]);
        let h = env
            .host()
            .builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([{ "text": "seen" }]))
            .build()
            .await;
        let res = h.prompt("look", json!({ "images": img })).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        expose(&h);
        let (path, files): (String, Vec<String>) =
            eval(&h, "local p = S:info().path return p, mare.store.files(p)").await;
        h.shutdown().await;

        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(!jsonl.contains(data), "image data inlined in the JSONL");
        assert!(jsonl.contains("\"attachment\""), "{jsonl}");
        let dir = format!("{}.attachments", path.trim_end_matches(".jsonl"));
        let stored: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(stored.len(), 1);
        assert_eq!(std::fs::read(&stored[0]).unwrap(), b"PNG-BYTES-123456");
        assert!(files.contains(&dir), "the attachments go with the session: {files:?}");

        let h = env
            .host()
            .builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([]))
            .resume()
            .build()
            .await;
        expose(&h);
        let back: String = eval(
            &h,
            r#"for _, m in ipairs(A:list_messages().data) do
                 for _, b in ipairs(m.content) do if b.type == "image" then return b.data end end
               end
               return "none""#,
        )
        .await;
        assert_eq!(back, data, "resume hydrates the image from its attachment");
        h.shutdown().await;
    })
    .await
}

/// `/context` splits the context by source (compact.measure by_source).
#[tokio::test]
async fn context_breakdown_counts_each_source() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage", "subagent", "status", "profile"]).faux(json!([])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"A:set_messages({
                 { role = "user", content = { { type = "text", text = "hello there" },
                   { type = "image", mime = "image/png", data = mare.crypto.base64_encode("x") } } },
                 { role = "assistant", content = { { type = "tool_call", id = "1", name = "bash", arguments = { command = "ls" } } } },
                 { role = "tool_result", tool_call_id = "1", content = { { type = "text", text = string.rep("file ", 100) } } },
               })
               REPORT = nil
               C.cordis:on("context/report", function(b) REPORT = b end)"#,
        )
        .await;
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "CM", h.service_table("commands").unwrap());
        let v: mlua::Value = eval(&h, r#"return K:measure({ by_source = true })"#).await;
        let b = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();
        assert_eq!(b["images"], 1200, "one image = mare.text.image_tokens (pi ESTIMATED_IMAGE_CHARS / 4): {b}");
        assert!(b["tool_results"].as_i64().unwrap() > 50 && b["messages"].as_i64().unwrap() > 0, "{b}");
        assert!(b["tools"].as_i64().unwrap() > 0 && b["system"].as_i64().unwrap() > 0, "{b}");
        let sum: i64 = ["system", "instructions", "skills", "tools", "messages", "tool_results", "images"]
            .iter()
            .map(|k| b[k].as_i64().unwrap())
            .sum();
        assert_eq!(b["total"].as_i64().unwrap(), sum, "{b}");
        exec(&h, r#"CM:run("/context")"#).await;
        wait_until(WAIT_SECS, "the /context report", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return REPORT ~= nil").await }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

/// /tree branch summaries go through compaction: on overflow the oldest protocol unit is dropped
/// and said; the live context is untouched.
#[tokio::test]
async fn branch_summaries_drop_the_oldest_units_on_overflow() {
    local(async {
        let env = TestEnv::new().await;
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let h = env
            .host()
            .builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([overflow, { "text": "BRANCH-SUMMARY" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (summary, dropped, live): (String, i64, i64) = eval(
            &h,
            r#"local msgs = {
                 { role = "user", content = { { type = "text", text = "OLD question" } } },
                 { role = "assistant", content = { { type = "text", text = "old answer" } } },
                 { role = "user", content = { { type = "text", text = "NEW question" } } },
                 { role = "assistant", content = { { type = "text", text = "new answer" } } },
               }
               local res = K:run("summarize the branch", { summarize_only = true, messages = msgs })
               return res.summary, res.dropped, #A:list_messages().data"#,
        )
        .await;
        assert!(summary.contains("BRANCH-SUMMARY"), "{summary}");
        assert!(dropped >= 1, "the oldest unit was dropped");
        assert_eq!(live, 0, "the live context is untouched");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn branch_summary_rejects_a_tool_call_without_its_result() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage", "subagent", "status", "profile"]).faux(json!([{ "text": "should not be requested" }])).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        let (summary, error, kind, requests, live): (bool, String, String, i64, i64) = eval(
            &h,
            r#"local orig = L.complete
               local n = 0
               L.complete = function(...) n = n + 1 return orig(...) end
               local msgs = {
                 { role = "assistant", content = {{ type = "tool_call", id = "missing-result", name = "read", arguments = {} }} },
               }
               local ok, res = pcall(K.run, K, "summarize", { summarize_only = true, messages = msgs })
               return ok and res.summary ~= nil, ok and "" or res.message, (not ok and res.data and res.data.kind) or "", n, #A:list_messages().data"#,
        )
        .await;
        assert!(!summary && error.contains("missing-result") && kind == "invalid_history", "{error}: {kind}");
        assert_eq!(requests, 0, "invalid history must not reach the provider");
        assert_eq!(live, 0);
        h.shutdown().await;
    })
    .await
}

/// Cancelling a branch summary interrupts the provider request and releases the compact service.
#[tokio::test]
async fn branch_summary_interrupt_cancels_the_request_and_allows_another_run() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host().builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([{ "text": "a slow branch summary ".repeat(50), "delay_ms": 100 }, { "text": "NEXT-SUMMARY" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let k_emit = h.kernel.clone();
        let emit_host = h
            .lua()
            .create_async_function(move |_, (ev, val): (String, mlua::Value)| {
                let k = k_emit.clone();
                async move {
                    k.emit(&ev, mlua::MultiValue::from_vec(vec![val])).await;
                    Ok(())
                }
            })
            .unwrap();
        h.lua().globals().set("_emit_host", emit_host).unwrap();
        let (first, kind, handled, second, live): (bool, String, bool, String, i64) = eval(
            &h,
            r#"local msgs = {{ role = "user", content = {{ type = "text", text = "branch history" }} }}
               local event = {}
               local t = mare.task.spawn(function() mare.task.sleep(20) _emit_host("key/interrupt", event) end)
               local ok1, r1 = pcall(K.run, K, "summarize", { summarize_only = true, messages = msgs })
               t:wait()
               local next_res = K:run("summarize", { summarize_only = true, messages = msgs })
               return ok1 and r1.summary ~= nil, tostring(not ok1 and r1.data and r1.data.kind or ""), event.handled == true, tostring(next_res.summary), #A:list_messages().data"#,
        )
        .await;
        assert!(!first && kind == "cancelled" && handled, "{first} {kind} {handled}");
        assert_eq!(second, "NEXT-SUMMARY");
        assert_eq!(live, 0);
        h.shutdown().await;
    })
    .await
}

/// Running sub-agents and workflow runs count as working; workflows say "workflow done/total".
#[tokio::test]
async fn status_is_working_while_sub_agents_and_workflows_run() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([]))
            .no_session()
            .build()
            .await;
        expose(&h);
        let seen: Vec<String> = eval(
            &h,
            r#"local seen = {}
               C.cordis:on("status/changed", function(s) seen[#seen + 1] = s.state .. "|" .. s.message end)
               C.cordis:get("bus"):emit("subagent/started", { subagent_id = "worker-7", label = "explorer" })
               C.cordis:get("bus"):emit("subagent/completed", { subagent_id = "worker-7", status = "completed" })
               C.cordis:get("bus"):emit("workflow/started", { run_id = "r1", total = 3, name = "review" })
               C.cordis:get("bus"):emit("workflow/agent-completed", { run_id = "r1" })
               C.cordis:get("bus"):emit("workflow/completed", { run_id = "r1" })
               mare.task.sleep(50)
               return seen"#,
        )
        .await;
        let joined = seen.join("\n");
        assert!(seen.iter().any(|s| s.starts_with("working|") && s.contains("explorer")), "{joined}");
        assert!(seen.iter().any(|s| s == "working|workflow 1/3"), "{joined}");
        assert!(seen.last().is_some_and(|s| s.starts_with("idle")), "{joined}");
        h.shutdown().await;
    })
    .await
}

/// A change to the tool table between requests is recorded for /profile.
#[tokio::test]
async fn tool_table_changes_are_recorded_for_profile() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([{ "text": "a" }, { "text": "b" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(&h, r#"CHANGES = {} C.cordis:on("profile/tools-changed", function(r) CHANGES[#CHANGES + 1] = r end)"#)
            .await;
        h.prompt("one", json!({})).await.unwrap();
        exec(&h, r#"T:command({ name = "fmtcheck", description = "format check", command = { "true" } })"#).await;
        h.prompt("two", json!({})).await.unwrap();
        let added: Vec<String> = eval(&h, "return CHANGES[1] and CHANGES[1].added or {}").await;
        assert_eq!(added, ["fmtcheck"]);
        h.shutdown().await;
    })
    .await
}

/// The runtime info lists what the current sandbox already allows (open network, writable
/// directories) and tells the model not to request it. The sandbox section is the
/// permissions plugin's.
#[tokio::test]
async fn runtime_info_lists_what_is_already_allowed() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host().builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(json!([]))
            .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "X", h.service_table("context").unwrap());
        let (open, filtered, blocked): (String, String, String) = eval(
            &h,
            r#"local function sp(p) mare.proc.sandbox.set_current(p) X:invalidate() return X:system_prompt() end
               local a = sp({ level = "workspace-write", network = true })
               local b = sp({ level = "workspace-write", network = { allow = { "pypi.org" }, deny = {}, default = "deny" } })
               local c = sp({ level = "read-only", network = false })
               mare.proc.sandbox.set_current(nil)
               return a, b, c"#,
        )
        .await;
        assert!(open.contains("Already allowed without asking") && open.contains("- network access (open)"), "{open}");
        assert!(open.contains("Never request what is already allowed"), "{open}");
        assert!(filtered.contains("network access to: pypi.org"), "{filtered}");
        assert!(blocked.contains("- writing files") && blocked.contains("network access (blocked)"), "{blocked}");
        h.shutdown().await;
    })
    .await
}

/// mare's own spill-file housekeeping never reads as a sandbox denial: an ordinary command under
/// `auto` gets no "request more access" hint (it made models ask for write access to the spill
/// dir), and the spill primitives refuse any path that isn't a spill file.
#[tokio::test]
async fn spill_housekeeping_is_not_a_sandbox_denial() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "echo hi; exit 3" } }] },
            { "text": "done" }
        ]);
        let h = env
            .host()
            .builtins(&["compact", "usage", "subagent", "status", "profile"])
            .faux(script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        h.prompt("go", json!({})).await.unwrap();
        let out = tool_results(&h).await;
        assert!(out.contains("hi"), "{out}");
        assert!(!out.contains("sandbox_permissions") && !out.contains("[sandbox:"), "bogus hint: {out}");
        let refused: bool =
            eval(&h, r#"return not pcall(mare.proc.spill_remove, "bash", mare.cwd .. "/bash-x.log")"#).await;
        assert!(refused, "spill_remove must refuse paths outside <state>/spill");
        h.shutdown().await;
    })
    .await
}

/// Spill files are named by their caller: spill_write / spill_remove take a `[a-z0-9-]+` prefix and
/// accept only `<state>/spill/<prefix>-*.log` (no name is built in: the shell tool passes "bash", a
/// plugin its own). A plugin's prefix writes, appends and removes; an invalid prefix, a file of
/// another prefix, a subdirectory, a relative path and `..` traversal are refused, nothing written.
#[tokio::test]
async fn spill_files_are_named_by_the_caller_prefix() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (text, removed, refusals, spill_dir): (String, bool, String, String) = eval(
            &h,
            r#"
            local dir = mare.paths.state .. "/spill"
            local own = dir .. "/hooks-1-probe.log"
            mare.proc.spill_write("hooks", own, "one")
            mare.proc.spill_write("hooks", own, "two", true)
            local text = mare.fs.read(own)
            mare.proc.spill_remove("hooks", own)
            local function refusal(f, ...)
                local ok, err = pcall(f, ...)
                return ok and "ACCEPTED" or tostring(err)
            end
            local w = mare.proc.spill_write
            return text, not mare.fs.exists(own), mare.json.encode({
                upper = refusal(w, "Hooks", dir .. "/Hooks-1.log", "x"),
                empty = refusal(w, "", dir .. "/-1.log", "x"),
                slash = refusal(w, "a/b", dir .. "/a/b-1.log", "x"),
                dot = refusal(w, "hooks.x", dir .. "/hooks.x-1.log", "x"),
                other_prefix = refusal(w, "hooks", dir .. "/bash-1.log", "x"),
                no_dash = refusal(w, "hooks", dir .. "/hooks1.log", "x"),
                not_log = refusal(w, "hooks", dir .. "/hooks-1.txt", "x"),
                subdir = refusal(w, "hooks", dir .. "/sub/hooks-1.log", "x"),
                relative = refusal(w, "hooks", "hooks-1.log", "x"),
                up = refusal(w, "hooks", dir .. "/../hooks-1.log", "x"),
                up_and_back = refusal(w, "hooks", dir .. "/../spill/hooks-1.log", "x"),
                remove_up = refusal(mare.proc.spill_remove, "hooks", dir .. "/../hooks-1.log"),
                remove_bad_prefix = refusal(mare.proc.spill_remove, "../hooks", dir .. "/../hooks-1.log"),
                run_up = refusal(mare.proc.run, { cmd = "true", spill = dir .. "/../hooks-1.log", spill_prefix = "hooks" }),
                start_bad_prefix = refusal(mare.proc.start, { cmd = "true", spill = dir .. "/x-1.log", spill_prefix = "X" }),
            }), dir
            "#,
        )
        .await;
        assert_eq!(text, "onetwo");
        assert!(removed, "spill_remove(\"hooks\", own) removes the file");
        let refusals: serde_json::Value = serde_json::from_str(&refusals).unwrap();
        for (case, err) in refusals.as_object().unwrap() {
            let err = err.as_str().unwrap();
            let expected = match case.as_str() {
                "upper" | "empty" | "slash" | "dot" | "remove_bad_prefix" | "start_bad_prefix" => "invalid spill prefix",
                _ => "not a spill file of `hooks`",
            };
            assert!(err.contains(expected), "{case}: {err}");
        }
        let state = std::path::Path::new(&spill_dir).parent().unwrap();
        for leftover in ["hooks-1.log", "spill/bash-1.log", "spill/hooks1.log", "spill/hooks-1.txt", "spill/sub"] {
            assert!(!state.join(leftover).exists(), "{leftover} was written");
        }
        h.shutdown().await;
    })
    .await
}

/// A corrupt auth.json is never overwritten: writes are refused, a copy goes to auth.json.bak,
/// and `mare doctor` says the file is corrupt instead of "no credentials".
#[tokio::test]
async fn corrupt_auth_json_is_kept_backed_up_and_reported() {
    local(async {
        let env = TestEnv::new().await;
        let auth = env.config_dir().join("auth.json");
        write_auth(&env, json!({}));
        let h = env.host().builtins(&["compact", "usage", "subagent", "status", "profile"]).no_session().build().await;
        std::fs::write(&auth, "{\"anthropic\": {\"key\": \"KEEP-ME\"").unwrap(); // truncated
        global(&h, "L", h.service_table("llm").unwrap());
        let (ok, err): (bool, String) =
            eval(&h, r#"local ok, e = pcall(L.auth_set, L, "openai", { key = "new" }) return ok, tostring(e)"#).await;
        h.shutdown().await;
        assert!(!ok && err.contains("corrupt"), "{err}");
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), "{\"anthropic\": {\"key\": \"KEEP-ME\"");
        let bak = std::fs::read_to_string(env.config_dir().join("auth.json.bak")).unwrap();
        assert!(bak.contains("KEEP-ME"), "{bak}");
        let out = doctor(env.home.path(), &[]);
        let line =
            out.lines().find(|l| l.contains("auth.json") && l.contains("corrupt")).unwrap_or_else(|| panic!("{out}"));
        assert!(line.contains('✗'), "{line}");
    })
    .await
}

/// Codex `codex-rs/login/src/auth/storage_error_tests.rs:87`: report credential format errors
/// without exposing the contents. reference implementation keeps invalid bytes.
#[test]
fn doctor_reports_invalid_auth_versions_without_exposing_credentials() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("auth.json");
    for raw in [
        r#""DO-NOT-LOG""#,
        r#"{"openai":{"key":"DO-NOT-LOG"}}"#,
        r#"{"version":"1","openai":{"key":"DO-NOT-LOG"}}"#,
        r#"{"version":2,"openai":{"key":"DO-NOT-LOG"}}"#,
    ] {
        mare_shared::paths::write_atomic(&path, raw.as_bytes(), mare_shared::paths::WriteMode::Private).unwrap();
        let out = doctor(home.path(), &[]);
        let line = out.lines().find(|line| line.starts_with("✗ auth.json ")).unwrap_or("");
        assert!(line.contains("corrupt"), "doctor must report {raw:?}: {out}");
        assert!(!out.contains("DO-NOT-LOG"), "doctor leaked stored credentials");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }
}

/// Codex `codex-rs/login/src/auth/storage_error_tests.rs:6`: report real read failures.
#[test]
fn doctor_reports_non_utf8_auth_as_unreadable() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("auth.json");
    let raw = b"\xff\xfe";
    mare_shared::paths::write_atomic(&path, raw, mare_shared::paths::WriteMode::Private).unwrap();
    let out = doctor(home.path(), &[]);
    let line = out.lines().find(|line| line.starts_with("✗ auth.json ")).unwrap_or("");
    assert!(line.contains("cannot be read"), "doctor must report the read error: {out}");
    assert_eq!(std::fs::read(path).unwrap(), raw);
}

/// Codex `codex-rs/login/src/auth/storage_tests.rs:44`: exercise real file permissions.
#[cfg(unix)]
#[test]
fn doctor_reports_unreadable_auth_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("auth.json");
    let raw = br#"{"version":1,"openai":{"key":"DO-NOT-LOG"}}"#;
    mare_shared::paths::write_atomic(&path, raw, mare_shared::paths::WriteMode::Private).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
    let out = doctor(home.path(), &[]);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let line = out.lines().find(|line| line.starts_with("✗ auth.json ")).unwrap_or("");
    assert!(line.contains("cannot be read"), "doctor must report the read error: {out}");
    assert!(!out.contains("DO-NOT-LOG"));
    assert_eq!(std::fs::read(path).unwrap(), raw);
}

/// reference implementation: unsupported versions
/// abort loading without changing the stored bytes.
#[test]
fn startup_rejects_unsupported_state_version() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("state");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    let raw = r#"{"version":2,"trust_default":"trust"}"#;
    std::fs::write(&path, raw).unwrap();
    let out = mare_cmd()
        .args(["--clean", "-p", "hello"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("state.json") && err.contains("unsupported version"), "{err}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
}

/// auth.json read-modify-writes hold a cross-process lock: two processes adding keys at the same
/// time keep every key.
#[test]
fn concurrent_auth_writes_from_two_processes_keep_both() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("authwriter.lua"),
        r#"return { name = "authwriter", inject = { "llm" }, apply = function(ctx)
             local who = mare.env("WRITER")
             if not who then return end
             for i = 1, 20 do ctx.llm:auth_set(who .. "-" .. i, { key = "k" }) end
           end }"#,
    )
    .unwrap();
    let spawn = |who: &str| {
        let mut c = mare_cmd();
        c.arg("doctor").env("MARE_HOME", home.path()).env("WRITER", who).current_dir(home.path());
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        ChildGroup::spawn(&mut c).unwrap()
    };
    let (a, b) = (spawn("a"), spawn("b"));
    let _ = wait_output_timeout(a, DEFAULT_CMD_TIMEOUT);
    let _ = wait_output_timeout(b, DEFAULT_CMD_TIMEOUT);
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("config/auth.json")).unwrap()).unwrap();
    let keys = auth.as_object().unwrap();
    for who in ["a", "b"] {
        for i in 1..=20 {
            assert!(keys.contains_key(&format!("{who}-{i}")), "lost {who}-{i}: {} keys", keys.len());
        }
    }
}

/// `mare doctor` checks only configured providers: a DeepSeek-only setup gets no probes of
/// OpenAI / Anthropic / ChatGPT and its credential source is named.
#[test]
fn doctor_checks_only_configured_providers() {
    let home = tempfile::tempdir().unwrap();
    let out = doctor(home.path(), &[("DEEPSEEK_API_KEY", "sk-deepseek-0123456789abcdefghij")]);
    let creds = out.lines().find(|l| l.contains("credential sources")).unwrap_or_else(|| panic!("{out}"));
    assert!(creds.contains("deepseek: env DEEPSEEK_API_KEY") && creds.contains('✓'), "{creds}");
    for other in ["chatgpt.com", "api.openai.com", "api.anthropic.com", "reach openai", "reach anthropic"] {
        assert!(!out.contains(other), "{other} probed without being configured: {out}");
    }
}

/// `install.ps1` must treat User PATH entries as an array even when only one entry exists
/// (avoiding PowerShell scalar string concatenation), stay synced with `docs-site/public/install.ps1`,
/// and pass its PowerShell self-test whenever `pwsh`/`powershell` is available.
#[test]
fn install_ps1_path_join_uses_array_and_matches_docs_copy() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root_ps1 = std::fs::read_to_string(repo_root.join("install.ps1")).unwrap();
    let docs_ps1 = std::fs::read_to_string(repo_root.join("docs-site/public/install.ps1")).unwrap();
    assert_eq!(root_ps1, docs_ps1, "docs-site/public/install.ps1 must stay identical to install.ps1");
    assert!(
        root_ps1.contains("function Add-MarePathEntry"),
        "install.ps1 must expose Add-MarePathEntry helper for PATH updates and self-testing"
    );
    assert!(
        root_ps1.contains("@(") && !root_ps1.contains("($PathEntries + $InstallDir) -join"),
        "install.ps1 must coerce PATH entries with @(...) instead of concatenating a scalar string"
    );
    let test_script = repo_root.join("scripts/test_install_ps1.ps1");
    assert!(test_script.is_file(), "missing {}", test_script.display());
    for shell in ["pwsh", "powershell"] {
        let mut cmd = std::process::Command::new(shell);
        cmd.args(["-NoProfile", "-NonInteractive"]);
        if cfg!(windows) {
            cmd.args(["-ExecutionPolicy", "Bypass"]);
        }
        cmd.args(["-File", test_script.to_str().unwrap()]);
        if let Ok(out) = cmd.output() {
            assert!(
                out.status.success(),
                "{shell} scripts/test_install_ps1.ps1 failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
    }
}
