//! `status`: the unified state goes idle → working → blocked → idle; `status` shell
//! hooks are coalesced and skip unchanged states; the `exit` hook always runs and shutdown waits.
//! The hooks' external programs are the test helper's stdin sinks (`mare-test-mcp record|sink`).

use crate::common::*;
use serde_json::json;

fn init(hook: &std::path::Path, exit: &std::path::Path) -> String {
    format!(
        r#"
mare.setup {{ model = "faux/echo" }}
-- Windows' default sandbox tier has no network control, so the read-only preset of this
-- untrusted folder asks before every shell command (permissions/vote.lua `net_note`) and nobody
-- answers here: the `bash` call below runs there only without the permissions plugin
if mare.os == "windows" then mare.plugin("permissions", {{ enabled = false }}) end
mare.plugin("provider-faux", {{ script_json = mare.json.encode_strict({{
  {{ tool_calls = {{ {{ name = "bash", arguments = {{ command = "sleep 0.4" }} }} }} }},
  {{ text = "done" }},
}}) }})
mare._seen = {{}}
mare.on("status/changed", function(s) table.insert(mare._seen, s.state .. "|" .. s.message) end)
-- one line per run: $MARE_STATE <tab> state JSON (stdin)
mare.status.hook {{ on = {{ "status" }}, throttle_ms = 150,
  cmd = {{ {bin:?}, "record", {hook:?} }} }}
-- slow exit hook (300 ms, then stdin to the file): shutdown must wait for it
mare.status.hook {{ on = {{ "exit" }}, cmd = function(s)
  return {{ {bin:?}, "sink", {exit:?}, "300" }}
end }}
"#,
        bin = fake_mcp_bin(),
        hook = hook.to_string_lossy(),
        exit = exit.to_string_lossy()
    )
}

/// Lines `state<TAB>json` written by the status hook so far.
fn runs(path: &std::path::Path) -> Vec<(String, serde_json::Value)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter_map(|(e, j)| Some((e.to_string(), serde_json::from_str(j).ok()?)))
        .collect()
}

/// The example must follow Herdr's CLI order, including release-agent's required first pane id.
#[tokio::test]
async fn herdr_example_reports_and_releases_the_correct_pane() {
    // Lua's C runtime on Windows can retain the environment from process startup, so setting
    // these with std::env::set_var after startup is not enough for os.getenv. Start the test
    // process with the same inherited variables a real Herdr-launched mare receives.
    if std::env::var_os("MARE_TEST_HERDR_EXAMPLE_CHILD").is_none() {
        // the example's status hooks really run `herdr-owner` (at startup too): a stand-in that
        // exits 0, first on the child's PATH, so no hook fails for want of a Herdr install
        let bin = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        std::fs::write(bin.path().join("herdr-owner.cmd"), "@exit /b 0\r\n").unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let fake = bin.path().join("herdr-owner");
            std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths(
            std::iter::once(bin.path().to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "suite::status::herdr_example_reports_and_releases_the_correct_pane", "--nocapture"])
            .env("MARE_TEST_HERDR_EXAMPLE_CHILD", "1")
            .env("PATH", path)
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", "w9:p3")
            .env("HERDR_BIN_PATH", "herdr-owner")
            .run_output();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "child failed: {}\n{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/herdr/init.lua")),
        )
        .unwrap();
        let h = env.host().user_config(false).build().await;
        let (report, session, release): (Vec<String>, Vec<String>, Vec<String>) = eval(
            &h,
            r#"local s = { state = "idle", message = "", seq = 10,
                    session = { session_id = "session-1", path = "/session.jsonl", resume = { "mare", "-r", "session-1" } } }
               local hooks = mare.config.status_hooks
               return hooks[1].cmd(s), hooks[2].cmd(s), hooks[3].cmd(s)"#,
        )
        .await;
        for (argv, command) in
            [(&report, "report-agent"), (&session, "report-agent-session"), (&release, "release-agent")]
        {
            assert_eq!(&argv[..4], ["herdr-owner", "pane", command, "w9:p3"], "{argv:?}");
        }
        assert!(report.windows(4).any(|w| w == ["--", "mare", "-r", "session-1"]), "{report:?}");
        assert!(session.windows(4).any(|w| w == ["--", "mare", "-r", "session-1"]), "{session:?}");
        assert!(!release.iter().any(|arg| arg == "--"), "{release:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn state_transitions_drive_coalesced_hooks_and_exit_always_fires() {
    local(async {
        let env = TestEnv::new().await;
        let (hook_out, exit_out) = (env.home.path().join("hook.log"), env.home.path().join("exit.json"));
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), init(&hook_out, &exit_out)).unwrap();
        let h = env.host().user_config(false).build().await;

        // working (a tool runs) → idle, then blocked while a (replaced) prompt waits → idle
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        exec(
            &h,
            r#"mare.ui.input = function() mare.task.sleep(400) return "x" end
                    mare.ui.input({ title = "Password" })"#,
        )
        .await;
        // the coalesced hook catches up with the final idle state (no fixed sleeps)
        wait_until(WAIT_SECS, "status hook to record the final idle state", || {
            let p = hook_out.clone();
            async move {
                let r = runs(&p);
                r.iter().any(|(s, _)| s == "blocked") && r.last().is_some_and(|(s, _)| s == "idle")
            }
        })
        .await;

        let seen: Vec<String> = eval(&h, "return mare._seen").await;
        let mut states: Vec<&str> = seen.iter().map(|s| s.split('|').next().unwrap()).collect();
        states.dedup();
        let pos = |from: usize, s: &str| states[from..].iter().position(|x| *x == s).map(|i| i + from);
        let w = pos(0, "working").expect("no working state");
        let b = pos(pos(w, "idle").expect("no idle after working"), "blocked").expect("no blocked state");
        pos(b, "idle").expect("no idle after blocked");
        assert!(seen.iter().any(|s| s.starts_with("working|bash: sleep 0.4")), "{seen:?}");
        assert!(seen.iter().any(|s| s == "blocked|waiting: Password"), "{seen:?}");

        // coalescing, deterministically: a burst of changes inside one throttle window (150ms)
        // runs the hook at most for the first change and the final state (a timing-only check
        // of "fewer runs than changes" depends on machine load)
        let before = runs(&hook_out).len();
        expose(&h);
        let burst: i64 = eval(
            &h,
            r#"local n0 = #mare._seen
               for i = 1, 10 do
                 C.cordis:get("bus"):emit("ui/wait-started", { kind = "input", title = "burst " .. i })
                 C.cordis:get("bus"):emit("ui/wait-completed", {})
               end
               return #mare._seen - n0"#,
        )
        .await;
        assert!(burst >= 10, "the burst produced {burst} status changes");
        // the burst ends where it started (idle): after the throttle window nothing new may reach
        // the hook — not ten runs, and not a repeat of the idle it already has
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let after = runs(&hook_out).len();
        assert_eq!(
            after,
            before,
            "hooks are coalesced and skip unchanged states: {} runs for {burst} changes",
            after - before
        );
        let runs = runs(&hook_out);
        for (env_state, v) in &runs {
            assert_eq!(env_state, v["state"].as_str().unwrap(), "MARE_STATE matches stdin");
        }
        for pair in runs.windows(2) {
            let key = |v: &serde_json::Value| (v["state"].clone(), v["message"].clone());
            assert_ne!(key(&pair[0].1), key(&pair[1].1), "an unchanged state ran twice");
            assert!(pair[1].1["seq"].as_u64() > pair[0].1["seq"].as_u64());
        }
        let last = runs.last().unwrap().1.clone();
        assert_eq!(last["session"]["resume"][1], "-r");
        assert_eq!(last["cwd"], env.cwd.to_string_lossy().as_ref());

        h.shutdown().await;
        let exit: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&exit_out).unwrap()).unwrap();
        assert_eq!(exit["state"], "idle", "shutdown waited for the slow exit hook");
        assert!(exit["seq"].as_u64() > last["seq"].as_u64());
    })
    .await
}

#[tokio::test]
async fn waiting_for_an_approval_is_blocked_with_what_is_awaited() {
    // an approval dialog (ui/wait-start kind = "approval") makes the session blocked
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["status", "notify"]).no_session().build().await;
        let (kinds, seen): (Vec<String>, Vec<String>) = eval(
            &h,
            r#"local kinds, seen = {}, {}
               mare.on("ui/wait-started", function(w) table.insert(kinds, w.kind) end)
               mare.on("status/changed", function(s) table.insert(seen, s.state .. "|" .. (s.message or "")) end)
               mare.ui.select = function() mare.task.sleep(200) return 1 end
               mare.ui.select({ wait_kind = "approval", title = "Allow bash: rm -rf build?", items = { "Allow once", "Deny" } })
               mare.task.sleep(100)
               return kinds, seen"#,
        )
        .await;
        assert_eq!(kinds, ["approval"]);
        let b = seen.iter().position(|s| s.starts_with("blocked|") && s.contains("Allow bash")).unwrap_or_else(|| panic!("{seen:?}"));
        assert!(seen[b..].iter().any(|s| s.starts_with("idle")), "back to idle after the answer: {seen:?}");
        h.shutdown().await;
    })
    .await
}

/// Two waits that overlap (two plugins asking at once) are two waits: the one that ends first
/// closes itself, not whichever opened last (ui/wait-end carries the start's id).
#[tokio::test]
async fn overlapping_waits_end_their_own_entry() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["status", "notify"]).no_session().build().await;
        let (during, after, ids, focus): (String, String, i64, bool) = eval(
            &h,
            r#"local last = ""
               local ids = {}
               mare.on("ui/wait-started", function(w) ids[w.wait_id] = true end)
               mare.on("status/changed", function(s) last = s.state .. "|" .. (s.message or "") end)
               mare.ui.select = function(t) mare.task.sleep(t.ms) return 1 end
               mare.task.spawn(function() mare.ui.select({ title = "first", ms = 200 }) end)
               mare.task.sleep(50)
               mare.task.spawn(function() mare.ui.select({ title = "second", ms = 500 }) end)
               mare.task.sleep(250) -- "first" has ended, "second" still waits
               local during = last
               mare.task.sleep(450)
               local n = 0
               for _ in pairs(ids) do n = n + 1 end
               -- a terminal that never reported focus: unknown (nil), never "focused"
               return during, last, n, mare.ui.focused() == nil"#,
        )
        .await;
        assert_eq!(during, "blocked|waiting: second");
        assert!(after.starts_with("idle"), "{after}");
        assert_eq!(ids, 2, "each overlapping wait is reported");
        assert!(focus, "focus is unknown until the terminal reports it");
        h.shutdown().await;
    })
    .await
}

/// notify.cmd (Codex config `notify`): an external program gets the event as JSON on stdin when
/// a turn finishes, in any mode.
#[tokio::test]
async fn notify_cmd_receives_the_event_as_json_on_stdin() {
    local(async {
        let env = TestEnv::new().await;
        let out = env.cwd.join("notify.json");
        let cmd = json!([fake_mcp_bin(), "sink", out.to_string_lossy()]);
        let h = env
            .host()
            .builtins(&["status", "notify"])
            .faux(json!([{ "text": "all done" }]))
            .config(json!({ "plugins": { "notify": { "cmd": cmd } } }))
            .build()
            .await;
        h.prompt("go", json!({ "turn_id": "turn-7" })).await.unwrap();
        let o = out.clone();
        wait_until(10, "the notify program to write its stdin", move || {
            let o = o.clone();
            async move { std::fs::read_to_string(&o).is_ok_and(|s| s.ends_with('}')) }
        })
        .await;
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["type"], "agent-turn-complete", "{v}");
        assert_eq!(v["turn-id"], "turn-7", "{v}");
        assert_eq!(v["last-assistant-message"], "all done", "{v}");
        assert!(v["cwd"].is_string() && v["session-id"].is_string(), "{v}");
        h.shutdown().await;
    })
    .await
}

#[test]
fn rpc_frontends_receive_status_changes_in_order() {
    // every connection gets status/changed; seq strictly increases
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "hi", "delay_ms": 50 }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .current_dir(home.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(std::process::Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init =
        rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "client": { "name": "test" } }), |_| json!(null));
    assert!(init["error"].is_null(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let id = rpc.request("agent.prompt", json!({ "session_id": sid, "input": "go" }));
    let (mut states, mut seqs, mut done) = (vec![], vec![], false);
    while !(done && states.last().is_some_and(|s: &String| s == "idle")) {
        let m = rpc.read();
        assert_ne!(
            m["method"], "status/changed",
            "standalone status/changed notification must not be emitted outside event envelopes: {m}"
        );
        if m["method"] == "event" && m["params"]["type"] == "status/changed" {
            let data = &m["params"]["data"];
            states.push(data["state"].as_str().unwrap_or("").to_string());
            seqs.push(data["seq"].as_u64().unwrap_or(0));
        } else if m["id"] == id {
            assert!(m["error"].is_null(), "{m}");
            done = true;
        }
        assert!(states.len() < 100, "no final idle: {states:?}");
    }
    rpc.kill_and_wait();
    let w = states.iter().position(|s| s == "working").unwrap_or_else(|| panic!("{states:?}"));
    assert!(states[w..].iter().any(|s| s == "idle"), "{states:?}");
    assert!(seqs.windows(2).all(|p| p[1] > p[0]), "seq strictly increases: {seqs:?}");
}

#[tokio::test]
async fn status_hook_rejects_legacy_changed_alias() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
mare._hook_changed_str = { pcall(mare.status.hook, { on = "changed", cmd = { "true" } }) }
mare._hook_changed_list = { pcall(mare.status.hook, { on = { "changed" }, cmd = { "true" } }) }
"#,
        )
        .unwrap();
        let h = env.host().user_config(false).builtins(&["status"]).no_session().build().await;
        let (ok_str, err_str, ok_list, err_list): (bool, String, bool, String) = eval(
            &h,
            r#"return mare._hook_changed_str[1], tostring(mare._hook_changed_str[2]),
                      mare._hook_changed_list[1], tostring(mare._hook_changed_list[2])"#,
        )
        .await;
        assert!(!ok_str && err_str.contains("status"), "on='changed' must error with hint: {err_str}");
        assert!(!ok_list && err_list.contains("status"), "on={{'changed'}} must error with hint: {err_list}");
        h.shutdown().await;
    })
    .await
}

/// Waiting for an approval triggers `notify` with `input-required` and the approval prompt.
#[tokio::test]
async fn notify_triggers_input_required_when_waiting_for_approval() {
    local(async {
        let env = TestEnv::new().await;
        let out = env.cwd.join("approval-notify.json");
        let cmd = json!([fake_mcp_bin(), "sink", out.to_string_lossy()]);
        let h = env
            .host()
            .builtins(&["status", "notify"])
            .config(json!({ "plugins": { "notify": { "cmd": cmd, "on_input": true } } }))
            .no_session()
            .build()
            .await;
        exec(
            &h,
            r#"
            mare.ui.select = function() mare.task.sleep(100) return 1 end
            mare.ui.select({ wait_kind = "approval", title = "Approve write to notes.txt?", items = { "Allow once", "Deny" } })
            "#,
        )
        .await;
        let o = out.clone();
        wait_until(10, "approval notify sink", move || {
            let o = o.clone();
            async move { std::fs::read_to_string(&o).is_ok_and(|s| s.ends_with('}')) }
        })
        .await;
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["type"], "input-required", "{v}");
        assert!(v["message"].as_str().unwrap_or("").contains("Approve write to notes.txt?"), "{v}");
        h.shutdown().await;
    })
    .await
}

/// A failing status hook command logs without breaking subsequent status transitions.
#[tokio::test]
async fn failing_status_hook_does_not_break_status_updates() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let bad_cmd = if cfg!(windows) { r#"{ "cmd", "/c", "exit", "1" }"# } else { r#"{ "sh", "-c", "exit 1" }"# };
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("mare.status.hook({{ on = \"status\", cmd = {bad_cmd} }})\n"),
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .builtins(&["status"])
            .faux(json!([{ "text": "still ok" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        let res = h.prompt("ping", json!({})).await.unwrap();
        assert_eq!(res["text"], "still ok", "{res}");
        let st: String = eval(&h, "return C.cordis:get('status'):get().state").await;
        assert_eq!(st, "idle");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn session_status_hooks_are_throttled_and_coalesced() {
    local(async {
        let env = TestEnv::new().await;
        let out = env.cwd.join("session-hook.log");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"mare.plugin("status", {{ throttle_ms = 600 }})
mare.status.hook({{
    on = {{ "session" }},
    throttle_ms = 600,
    cmd = function()
        return {{ "sh", "-c", 'printf "%s\n" "$MARE_SESSION_ID" >> ' .. {out:?} }}
    end,
}})
"#
            ),
        )
        .unwrap();
        let h = env.host().user_config(true).builtins(&["status"]).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local bus = C.cordis:get("bus")
            for i = 1, 8 do
                bus:emit("session/changed", { session_id = "sess-" .. i, path = "/tmp/s-" .. i })
            end
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "session status hook coalesced to final sess-8", || {
            let p = out.clone();
            async move {
                std::fs::read_to_string(&p).ok().and_then(|s| s.lines().last().map(|l| l == "sess-8")).unwrap_or(false)
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await; // sleep-ok: negative-assertion quiet window confirming no extra coalesced hook runs
        let lines: Vec<String> =
            std::fs::read_to_string(&out).unwrap_or_default().lines().map(str::to_string).collect();
        assert!(
            lines.len() <= 2,
            "session status hooks must coalesce bursts (expected <=2 runs for 8 rapid changes, got {}): {lines:?}",
            lines.len()
        );
        assert_eq!(lines.last().map(String::as_str), Some("sess-8"), "{lines:?}");

        // Repeating the same session snapshot must not re-fire the hook
        let before_dup = lines.len();
        exec(&h, r#"C.cordis:get("bus"):emit("session/changed", { session_id = "sess-8", path = "/tmp/s-8" })"#).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await; // sleep-ok: negative-assertion quiet window confirming duplicate snapshot does not re-fire hook
        let after_dup = std::fs::read_to_string(&out).unwrap_or_default().lines().count();
        assert_eq!(after_dup, before_dup, "unchanged session snapshot must not re-run hook");
        h.shutdown().await;
    })
    .await
}
