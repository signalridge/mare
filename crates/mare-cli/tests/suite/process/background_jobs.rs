use super::{call_tool, content, job_id, notifications};
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn background_job_completion_wakes_the_idle_agent() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": {
                "description": "Print a greeting later", "command": "sleep 0.3; echo hello-from-job",
                "run_in_background": true } }] },
            { "text": "started" },
            { "text": "noted" }
        ]);
        let h = env.host().builtins(&["jobs"]).faux(script).build().await;
        expose(&h);
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res.to_string().contains("Started background job bash-"), "{res}");
        wait_until(WAIT_SECS, "the completion notification and the wake-up turn", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local seen = false
                       local m = A:list_messages().data
                       for _, x in ipairs(m) do
                         if x.notification and x.content[1].text:find("hello-from-job", 1, true) then seen = true end
                       end
                       return seen and not A:get().busy and m[#m].role == "assistant""#,
                )
                .await
            }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn foreground_timeout_moves_the_command_to_the_background() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "echo early; sleep 30; echo late", "timeout_ms": 2000 } }] },
            { "tool_calls": [{ "name": "job_list", "arguments": {} }] },
            { "text": "listed" }
        ]);
        let h = env.host().builtins(&["jobs"]).faux(script).build().await;
        expose(&h);
        let started = std::time::Instant::now();
        let res = h.prompt("slow", json!({})).await.unwrap();
        let took = started.elapsed();
        assert!(res["error"].is_null(), "{res}");
        // the 2 s limit is what ended the wait: not before it, and nowhere near the 30 s command
        assert!(
            took >= std::time::Duration::from_secs(2) && took < std::time::Duration::from_secs(15),
            "the turn took {took:?}; the command should have gone to the background at 2 s"
        );
        let t = tool_results(&h).await;
        assert!(t.contains("[still running after 2000ms; moved to background job bash-"), "{t}");
        assert!(t.contains("early"), "output so far is attached: {t}");
        assert!(t.contains("[bash] running"), "job_list shows it: {t}");
        h.shutdown().await;
    })
    .await
}

/// daily-use acceptance: exercise the real 120-second default and completion wake-up.
/// This intentionally takes over two minutes and uses only the local faux provider.
#[tokio::test]
#[ignore = "manual acceptance: runs a real command for over 120 seconds"]
async fn default_timeout_backgrounds_a_command_over_two_minutes_and_wakes_agent() {
    sandboxed_background_acceptance(None).await;
}

/// Exercise the same sandbox and handoff assertions without waiting for the default timeout.
#[tokio::test]
async fn sandboxed_background_acceptance_with_short_timeout() {
    sandboxed_background_acceptance(Some(2_000)).await;
}

async fn sandboxed_background_acceptance(timeout_ms: Option<u64>) {
    local(async {
        let env = TestEnv::new().await;
        let sleep_seconds = if timeout_ms.is_some() { 1 } else { 20 };
        let limit = timeout_ms.unwrap_or(120_000);
        let command = format!("if (printf forbidden > .mare-long-job-write-attempt) 2>/dev/null; then echo long-job-write-unexpected; else echo long-job-write-denied; fi; echo long-job-started; i=0; while [ ! -f .mare-long-job-release ]; do sleep {sleep_seconds}; i=$((i+1)); echo progress-$i; done; echo long-job-finished");
        let mut arguments = json!({ "command": command });
        if let Some(ms) = timeout_ms {
            arguments["timeout_ms"] = json!(ms);
        }
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": arguments }] },
            { "text": "waiting for the background job" },
            { "text": "background job completion received" }
        ]);
        let h = env.host().builtins(&["jobs"]).faux(script).config(json!({ "plugins": { "permissions": { "enabled": true } } })).build().await;
        expose(&h);
        global(&h, "PERMS", h.service_table("permissions").expect("UNVERIFIED: permissions service unavailable"));
        let (preset, level): (String, String) = eval(&h, r#"local p = mare.proc.sandbox.current()
            return PERMS:preset(), p and p.level or "none""#).await;
        assert_eq!((preset.as_str(), level.as_str()), ("read-only", "read-only"), "UNVERIFIED: default permission/sandbox is not active");
        let (code, enforced): (String, String) = eval(&h, r#"local r = mare.proc.run({ cmd = "sh",
            args = { "-c", "printf forbidden > .mare-long-job-write-probe" }, sandbox = { level = "read-only" } })
            return tostring(r.code), r.sandbox and table.concat(r.sandbox.enforced, ",") or "none""#).await;
        assert!(code != "0" && enforced.contains("fs_write") && !env.cwd.join(".mare-long-job-write-probe").exists(),
            "UNVERIFIED: read-only sandbox write restriction not demonstrated: code={code}, enforced={enforced}");
        let started = std::time::Instant::now();
        let res = tokio::time::timeout(std::time::Duration::from_secs(150), h.prompt("run the long job", json!({})))
            .await
            .expect("foreground handoff timed out")
            .unwrap();
        assert!(res["error"].is_null(), "{res}");
        // Faux assigns the first call this id. Its result reports the sandbox applied to the
        // long-running bash, rather than the sandbox of a separate process.
        let result: String = eval(&h, r#"for _, m in ipairs(A:list_messages().data) do
            if m.role == "tool_result" and m.tool_call_id == "call_1_1" then return mare.json.encode(m) end
        end
        return "null""#).await;
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        let enforced = result["details"]["sandbox"]["enforced"].as_array()
            .expect("UNVERIFIED: long bash call did not report sandbox enforcement");
        assert!(enforced.iter().any(|cap| cap == "fs_write"),
            "UNVERIFIED: long bash call did not enforce write restrictions: {result}");
        let results = result["content"][0]["text"].as_str().expect("long bash call returned no text");
        assert!(results.contains("long-job-write-denied") && !results.contains("long-job-write-unexpected"), "{results}");
        assert!(!env.cwd.join(".mare-long-job-write-attempt").exists(), "the long bash call wrote the protected file");
        assert!(results.contains(&format!("[still running after {limit}ms; moved to background job bash-")), "{results}");
        assert!(results.contains("long-job-started"), "{results}");
        let progress_count = if timeout_ms.is_some() { 1 } else { 5 };
        for i in 1..=progress_count {
            assert!(results.contains(&format!("progress-{i}")), "{results}");
        }
        assert!(started.elapsed() >= std::time::Duration::from_millis(limit));
        let marker = format!("[still running after {limit}ms; moved to background job ");
        let job = results.split(&marker).nth(1)
            .and_then(|s| s.split(']').next()).expect("missing background job id");
        assert!(job.starts_with("bash-"), "{job}");
        global(&h, "LONG_JOB", job.to_string());
        std::fs::write(env.cwd.join(".mare-long-job-release"), b"release").unwrap();
        wait_until(45, "the long job completion notification and wake-up turn", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local notice, answer, count = nil, nil, 0
                       local m = A:list_messages().data
                       for i, x in ipairs(m) do
                         local text = x.content and x.content[1] and x.content[1].text or ""
                         if x.notification and text:find("[background job " .. LONG_JOB .. " finished:", 1, true) and text:find("long-job-finished", 1, true)
                            and text:find("completed (exit 0)", 1, true) then notice = i count = count + 1 end
                         if x.role == "assistant" and text == "background job completion received" then answer = i end
                       end
                       return count == 1 and notice and answer and notice < answer and not A:get().busy
                          and m[#m].role == "assistant" and m[#m].content[1].text == "background job completion received""#,
                )
                .await
            }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn finished_jobs_beyond_the_cap_are_pruned_least_recently_used_first() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "jobs": { "max_jobs": 2 } } }))
            .build()
            .await;
        global(&h, "J", h.service_table("jobs").unwrap());
        expose(&h);
        let (kept, first_gone, recent_kept): (i64, bool, bool) = eval(
            &h,
            r#"local function run(i)
                 local j = assert(J:start({ kind = "bash", description = "j" .. i, cmd = "true" }))
                 j.handle:wait({ timeout_ms = 5000 })
                 mare.task.sleep(20) -- let the job's finish bookkeeping run
                 return j
               end
               local a, b = run(1), run(2)
               J:get(a.job_id) -- a used more recently than b
               local c = run(3)
               return #J:list().data, J:get(b.job_id) == nil, J:get(a.job_id) ~= nil"#,
        )
        .await;
        assert_eq!(kept, 2);
        assert!(first_gone && recent_kept, "LRU: the least recently used finished job goes first");
        h.shutdown().await;
    })
    .await
}

/// a job whose leader exited but left a background child in its group: the group stays
/// registered, so mare still ends it on exit (pi bash.ts keeps the detached group)
#[cfg(unix)]
#[tokio::test]
async fn an_exited_jobs_leftover_children_end_with_mare() {
    let env = TestEnv::new().await;
    let pidfile = env.home.path().join("orphan.pid");
    let script = env.home.path().join("faux.json");
    let cmd = format!("sleep 120 > /dev/null 2>&1 & echo $! > {}", pidfile.display());
    // the next turn waits for the job to have run: commands start in a login shell, and a slow
    // profile (CI runners) would otherwise let mare exit before the job got to `echo`
    let wait = format!("while [ ! -s {0} ]; do sleep 0.05; done", pidfile.display());
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": cmd, "run_in_background": true } }] },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": wait } }] },
            { "text": "done" }
        ])
        .to_string(),
    )
    .unwrap();
    let st = mare_cmd()
        .args(["--yolo", "--model", "faux/echo", "-p", "go"])
        .env("MARE_HOME", env.home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(&env.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    let pid: i32 = std::fs::read_to_string(&pidfile).expect("the command ran").trim().parse().unwrap();
    let t0 = std::time::Instant::now();
    while unsafe { libc::kill(pid, 0) } == 0 {
        if t0.elapsed() > std::time::Duration::from_secs(WAIT_SECS) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("the leftover child {pid} outlived mare");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Resuming a session whose background jobs were still running when mare exited: they are
/// marked interrupted and the model is told, once, with its next request.
#[tokio::test]
async fn a_resume_tells_the_model_which_background_jobs_were_lost() {
    local(async {
        let env = TestEnv::new().await;
        let job = |event: &str, id: &str| {
            json!({ "type": "custom", "kind": "job", "data": { "event": event, "job_id": id, "kind": "bash", "description": format!("cmd {id}") } })
        };
        env.write_session(
            "2026-01-01T00-00-00_test.jsonl",
            &[
                msg("u1", None, "user", "start the servers"),
                msg("a1", Some("u1"), "assistant", "started"),
                job("start", "bash-lost1"),
                job("start", "bash-done1"),
                job("end", "bash-done1"),
            ],
        );
        let h = env.host().builtins(&["jobs"]).faux(json!([{ "text": "ok" }, { "text": "ok" }])).resume().build().await;
        expose(&h);
        h.prompt("are they up?", json!({})).await.unwrap();
        let ctx_text: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(ctx_text.contains("were stopped when mare exited"), "{ctx_text}");
        assert!(ctx_text.contains("bash-lost1: cmd bash-lost1"), "{ctx_text}");
        assert!(!ctx_text.contains("bash-done1:"), "a finished job is not reported: {ctx_text}");
        h.prompt("and now?", json!({})).await.unwrap();
        let again: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert_eq!(again.matches("were stopped when mare exited").count(), 1, "told once");
        h.shutdown().await;
    })
    .await
}

/// A command the persistent shell moved to the background is tracked by itself: the job ends
/// when the command does (its end marker), not when the shell would — so it stops counting
/// against max_running — and the completion notice carries its output, never the marker.
#[tokio::test]
async fn a_backgrounded_persistent_command_ends_with_the_command() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .faux(json!([{ "text": "noted" }]))
            .config(json!({ "plugins": { "tools": { "stall_ms": 400 } } }))
            .build()
            .await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());
        let r = call_tool(&h, "bash", json!({ "command": "echo early; sleep 1; echo late-line; false" })).await;
        let t = content(&r);
        assert!(t.contains("no output for") && t.contains("background job"), "{t}");
        assert!(t.contains("early"), "{t}");
        global(&h, "ID", job_id(&t));
        wait_until(10, "the job to end with its command", || {
            let h = h.clone();
            async move { eval::<String>(&h, "return J:get(ID).status").await != "running" }
        })
        .await;
        let (status, running): (String, i64) = eval(
            &h,
            r#"local n = 0
               for _, j in ipairs(J:list().data) do if j.status == "running" then n = n + 1 end end
               return J:get(ID).status, n"#,
        )
        .await;
        assert_eq!(status, "failed", "the command's own exit code (false → 1)");
        assert_eq!(running, 0, "nothing keeps running once the command is done");
        wait_until(10, "the completion notice", || {
            let h = h.clone();
            async move { notifications(&h).await.contains("late-line") }
        })
        .await;
        let n = notifications(&h).await;
        assert!(n.contains("failed (exit 1)"), "{n}");
        assert!(!n.contains("__MARE_BASH"), "no shell markers leak: {n}");
        // the next call gets a fresh, working shell
        let r = call_tool(&h, "bash", json!({ "command": "echo fresh" })).await;
        assert!(content(&r).contains("fresh"), "{r}");
        h.shutdown().await;
    })
    .await
}

/// bash_job (workdir, escalation, fresh shells) moves a silent command to the background like
/// the persistent shell, and Ctrl+B (`background` action) moves a running command there at once.
#[tokio::test]
async fn silent_or_ctrl_b_commands_move_to_the_background() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "tools": { "stall_ms": 400 } } }))
            .build()
            .await;
        expose(&h);
        let r = call_tool(&h, "bash", json!({ "command": "sleep 5", "workdir": "." })).await;
        let t = content(&r);
        assert!(t.contains("no output for") && t.contains("still running as background job bash-"), "{t}");
        call_tool(&h, "job_kill", json!({ "job_id": job_id(&t) })).await;
        // Ctrl+B, on both paths (persistent shell, temporary shell)
        for args in [
            json!({ "command": "echo hi; sleep 0.2; echo more; sleep 5" }),
            json!({ "command": "echo hi; sleep 0.2; echo more; sleep 5", "workdir": "." }),
        ] {
            global(
                &h,
                "CALL",
                mare_shared::lua::json_to_lua(h.lua(), &json!({ "id": "t1", "name": "bash", "arguments": args }))
                    .unwrap(),
            );
            let v: mlua::Value = eval(
                &h,
                r#"mare.task.spawn(function() mare.task.sleep(300); mare.keymap.invoke("background") end)
                   return T:execute(CALL, {})"#,
            )
            .await;
            let t = content(&mare_shared::lua::lua_to_json(h.lua(), v).unwrap());
            assert!(t.contains("by the user"), "{t}");
            call_tool(&h, "job_kill", json!({ "job_id": job_id(&t) })).await;
        }
        h.shutdown().await;
    })
    .await
}

/// The wake-up limit counts wake-ups since the user's last prompt: a prompt from a peer session
/// (`source = "peer"`) is not the user and doesn't reset it.
#[tokio::test]
async fn peer_prompts_do_not_reset_the_wakeup_limit() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host().builtins(&["jobs"])
            .no_session()
            .faux(json!([{ "text": "woke" }, { "text": "woke again" }]))
            .config(json!({ "plugins": { "jobs": { "max_wakeups": 1 } } }))
            .build()
            .await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());
        let count = r#"local n = 0
                       for _, m in ipairs(A:list_messages().data) do if m.role == "assistant" then n = n + 1 end end
                       return n"#;
        exec(&h, r#"assert(J:start({ kind = "bash", description = "one", cmd = "true", background = true }))"#).await;
        wait_until(WAIT_SECS, "the first wake-up turn", || {
            let h = h.clone();
            async move { eval::<i64>(&h, count).await == 1 && !eval::<bool>(&h, "return A:get().busy").await }
        })
        .await;
        // what a delivered peer message looks like to listeners
        exec(&h, r#"C.cordis:get("bus"):emit("turn/started", { turn_id = "t-peer", trigger = "peer", text = "from a peer", source = "peer" })"#).await;
        exec(&h, r#"assert(J:start({ kind = "bash", description = "two", cmd = "true", background = true }))"#).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(eval::<i64>(&h, count).await, 1, "the limit still holds: no second wake-up");
        h.shutdown().await;
    })
    .await
}

/// The cap on running background jobs (`max_running`) refuses new background jobs, but never the
/// persistent shell's handover on a timeout: the shell keeps running as a job
/// beyond the cap — it is neither stopped nor left untracked — and the model is told the cap is
/// exceeded.
#[tokio::test]
async fn a_timed_out_shell_keeps_running_as_a_job_beyond_the_cap() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().faux(json!([{ "text": "noted" }])).build().await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());
        let max: i64 = eval(
            &h,
            r#"for i = 1, 8 do
                 assert(J:start({ kind = "bash", description = "filler" .. i, cmd = "sleep", args = { "60" } }))
               end
               local _, err = J:start({ kind = "bash", description = "ninth", cmd = "true" })
               return err and 8 or 0"#,
        )
        .await;
        assert_eq!(max, 8, "the cap is full: a ninth job is refused");
        let r = call_tool(&h, "bash", json!({ "command": "echo early; sleep 30", "timeout_ms": 1000 })).await;
        let t = content(&r);
        assert!(t.contains("[still running after 1000ms; moved to background job bash-"), "{t}");
        assert!(t.contains("exceeds the limit of 8 running background jobs"), "the model is told: {t}");
        global(&h, "ID", job_id(&t));
        let (status, running): (String, i64) = eval(
            &h,
            r#"local n = 0
               for _, j in ipairs(J:list().data) do if j.status == "running" then n = n + 1 end end
               return J:get(ID).status, n"#,
        )
        .await;
        assert_eq!(status, "running", "tracked and still running, not stopped");
        assert_eq!(running, 9);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelling_bash_job_tool_call_cancels_the_job() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).no_session().build().await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());
        let raw: String = eval(
            &h,
            r#"local tok = mare.task.token()
               STARTED_ID = nil
               C.cordis:on("jobs/changed", function(ev)
                   if not STARTED_ID and ev.job and ev.job.status == "running" then
                       STARTED_ID = ev.job.job_id
                       tok:cancel()
                   end
               end)
               local r = T:execute({
                   id = "cancel-bash-job",
                   name = "bash",
                   arguments = { command = "echo started; sleep 60", workdir = "." },
               }, { cancel = tok })
               return mare.json.encode({ is_error = r.is_error == true, content = tostring(r.content or ""), id = STARTED_ID })"#,
        )
        .await;
        let out: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["is_error"], true, "{out}");
        let body = out["content"].as_str().unwrap_or("");
        assert!(body.contains("[command aborted]"), "expected clean bash_run cancel result, got: {out}");
        let jid = out["id"].as_str().expect("running job id");
        wait_until(WAIT_SECS, "cancelled bash job to stop running", || {
            let h = h.clone();
            let jid = jid.to_owned();
            async move { eval::<bool>(&h, &format!(r#"return J:get({jid:?}).status ~= "running""#)).await }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_default_timeout_is_120s_and_job_wait_does_not_auto_background() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["jobs"]).no_session();
        b.opts.load_init = true;
        let h = b.build().await;
        expose(&h);
        let default_ms: i64 = eval(
            &h,
            r#"return mare.config_describe({ "plugins.tools.bash_timeout_ms" })["plugins.tools.bash_timeout_ms"].default"#,
        )
        .await;
        assert_eq!(default_ms, 120_000, "default bash foreground budget must be 120s");
        h.shutdown().await;
        drop(env);

        // Even with a short bash foreground timeout, `job_output { wait = true }` waits directly
        // and never auto-backgrounds itself into another background job.
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "tools": { "bash_timeout_ms": 400 } } }))
            .build()
            .await;
        expose(&h);
        let started =
            call_tool(&h, "bash", json!({ "command": "sleep 1.0; echo waited-ok", "run_in_background": true })).await;
        let jid = job_id(&content(&started));
        let waited = call_tool(&h, "job_output", json!({ "job_id": jid, "wait": true, "timeout_ms": 5000 })).await;
        let body = content(&waited);
        assert!(
            body.contains("waited-ok") && !body.contains("moved to background job"),
            "job_output wait must finish directly without auto-backgrounding: {body}"
        );
        h.shutdown().await;
    })
    .await
}

/// The `jobs` widget renders running background jobs above the
/// editor, `/jobs` notifies when none exist, views live output via `ui.transcript`, and stops a
/// running job with `"stopped by the user"`.
#[tokio::test]
async fn jobs_widget_and_slash_jobs_ui_show_running_jobs_output_and_stop() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs", "ui", "commands"]).no_session().build().await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());
        global(&h, "CM", h.service_table("commands").unwrap());

        // 1. `/jobs` with no jobs notifies "No background jobs".
        let empty_note: String = eval(
            &h,
            r#"local note = ""
               mare.ui.notify = function(msg) note = tostring(msg) end
               CM:run("/jobs")
               return note"#,
        )
        .await;
        assert_eq!(empty_note, "No background jobs");

        // 2. Start a background job; verify the above-editor `jobs` widget renders it, `/jobs`
        //    opens the transcript view with its output, and `/jobs` -> Stop cancels it.
        let (widget_json, transcript_title, transcript_body, kill_reason): (String, String, String, String) = eval(
            &h,
            r#"local ui_svc = C.cordis:get("ui")
               local wjson = ""
               mare.ui.connected = function() return true end
               local orig_set_widget = mare.ui.set_widget
               mare.ui.set_widget = function(id, slot, node, order)
                 if id == "jobs" and slot == "above-editor" and node ~= nil then
                   wjson = mare.json.encode(node)
                 end
                 if orig_set_widget then pcall(orig_set_widget, id, slot, node, order) end
               end
               local res = T:execute({
                 id = "bg-ui",
                 name = "bash",
                 arguments = { command = "echo JOB_OUTPUT_LINE; sleep 30", run_in_background = true, description = "sleeper" },
               }, {})
               local jid = assert(res.details and res.details.job_id)
               ui_svc:invalidate("jobs")
               -- Wait briefly for JOB_OUTPUT_LINE to flush into the job buffer
               local deadline = mare.task.clock() + 3000
               while mare.task.clock() < deadline do
                 local out = T:execute({ id = "o1", name = "job_output", arguments = { job_id = jid, tail = 10 } }, {})
                 local txt = type(out.content) == "table" and out.content[1] and out.content[1].text or tostring(out.content)
                 if type(txt) == "string" and txt:find("JOB_OUTPUT_LINE", 1, true) then
                   break
                 end
                 mare.task.sleep(25)
               end
               if wjson == "" then ui_svc:invalidate("jobs") mare.task.sleep(25) end

               -- `/jobs` -> View output
               local t_title, t_body = "", ""
               mare.ui.transcript = function(title, node)
                 t_title = tostring(title)
                 t_body = mare.json.encode(node)
               end
               mare.ui.transcript_title = function() return nil end
               local pick_view = { 1, 1 }
               local idx = 0
               mare.ui.select = function() idx = idx + 1; return pick_view[idx] end
               CM:run("/jobs")

               -- `/jobs` -> Stop
               local pick_stop = { 1, 2 }
               idx = 0
               mare.ui.select = function() idx = idx + 1; return pick_stop[idx] end
               CM:run("/jobs")

               local deadline2 = mare.task.clock() + 3000
               while J:get(jid).status == "running" and mare.task.clock() < deadline2 do
                 mare.task.sleep(25)
               end
               return wjson, t_title, t_body, tostring(J:get(jid).kill_reason or "")"#,
        )
        .await;
        assert!(
            widget_json.contains("1 background job:") && widget_json.contains("sleeper") && widget_json.contains("/jobs"),
            "jobs widget must show count, description, and /jobs hint: {widget_json}"
        );
        assert!(transcript_title.starts_with("Job bash-"), "{transcript_title}");
        assert!(transcript_body.contains("JOB_OUTPUT_LINE"), "transcript view must show job output: {transcript_body}");
        assert_eq!(kill_reason, "stopped by the user");
        h.shutdown().await;
    })
    .await
}

/// Cancelling a background job must emit a terminal `jobs/changed` completion event (`cancelled`),
/// wake concurrent `job_output { wait = true }` waiters, record the end entry, and notify the agent
/// when `silent = false` — for both `mare.proc.start` handles and custom `jobs:track` handles that
/// only implement the documented `:kill` method (without `:stop`).
#[tokio::test]
async fn cancelling_background_job_emits_completion_event_wakes_waiters_and_supports_kill_only_handles() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["jobs"]).faux(json!([{ "text": "noted" }, { "text": "noted2" }])).build().await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());

        // 1. Real background process: cancel wakes `job_output { wait = true }`, emits `jobs/changed`
        //    with `status = "cancelled"`, records `end`, and notifies the agent when `silent = false`.
        let (wait_out, terminal_status, end_recorded): (String, String, bool) = eval(
            &h,
            r#"local seen_terminal = nil
               local done_tx, done_rx = mare.task.channel()
               local r = T:execute({
                   id = "bg-cancel-1",
                   name = "bash",
                   arguments = { command = "sleep 30", description = "real-bg", run_in_background = true },
               }, { on_state = function() return true end })
               local jid = assert(r.details and r.details.job_id, mare.json.encode(r))
               local off = C.cordis:on("jobs/changed", function(ev)
                   if ev and ev.job and ev.job.job_id == jid and ev.job.status ~= "running" then
                       seen_terminal = ev.job.status
                       done_tx:send(true)
                   end
               end)
               local ready_tx, ready_rx = mare.task.channel()
               local waiter = mare.task.spawn(function()
                   ready_tx:send(true)
                   local out = T:execute({
                       id = "wait-cancel-1",
                       name = "job_output",
                       arguments = { job_id = jid, wait = true, timeout_ms = 10000 },
                   }, { on_state = function() return true end })
                   return type(out.content) == "table" and out.content[1].text or tostring(out.content)
               end)
               ready_rx:recv()
               mare.task.yield()
               J:cancel(jid, "stopped by test", { silent = false })
               local wait_text = waiter:wait()
               done_rx:recv()
               off()
               local recs = require("mare_lib").all_records(S, "job")
               local has_end = false
               for _, rec in ipairs(recs or {}) do
                   if rec.event == "end" and rec.job_id == jid and rec.status == "cancelled" then
                       has_end = true
                   end
               end
               return wait_text, tostring(seen_terminal), has_end"#,
        )
        .await;
        assert!(wait_out.contains("cancelled"), "waiter must wake and see cancelled status: {wait_out}");
        assert_eq!(terminal_status, "cancelled");
        assert!(end_recorded, "session must record end event with status = cancelled");

        // 2. Custom handle implementing only `:kill` (no `:stop`, matching plugins/jobs.lua:6-9):
        //    `jobs:cancel` must still invoke `:kill`, unblock `:wait`, and emit `jobs/changed`
        //    with `status = "cancelled"`.
        let (killed_sig, custom_status): (String, String) = eval(
            &h,
            r#"local done_tx, done_rx = mare.task.channel()
               local exit_tx, exit_rx = mare.task.channel()
               local exited, got_sig = false, ""
               local hdl = {
                   status = function()
                       return { state = exited and "exited" or "running", code = nil, signal = exited and 15 or nil, unread = 0 }
                   end,
                   read = function() return { text = "", dropped = 0, done = exited } end,
                   read_at = function() return { text = "", next = 0, dropped = 0 } end,
                   tail = function() return "" end,
                   wait = function(self)
                       if not exited then exit_rx:recv() end
                       return self:status()
                   end,
                   kill = function(_, sig)
                       got_sig = tostring(sig or "TERM")
                       exited = true
                       exit_tx:send(true)
                   end,
               }
               local job = J:track({ kind = "custom", description = "kill-only-handle", handle = hdl, background = true })
               local terminal = nil
               local off = C.cordis:on("jobs/changed", function(ev)
                   if ev and ev.job and ev.job.job_id == job.job_id and ev.job.status ~= "running" then
                       terminal = ev.job.status
                       done_tx:send(true)
                   end
               end)
               J:cancel(job.job_id, "cancelled custom", { silent = false })
               done_rx:recv()
               off()
               return got_sig, tostring(terminal)"#,
        )
        .await;
        assert_eq!(killed_sig, "TERM");
        assert_eq!(custom_status, "cancelled");
        h.shutdown().await;
    })
    .await
}

/// A per-call elevated `bash` call (`sandbox_permissions = "all"`) must not spawn a persistent
/// writable `stdin`/`tty` background job that turns a one-shot approval into a permanent
/// unconfined input channel.
#[tokio::test]
async fn elevated_bash_with_stdin_or_tty_refuses_persistent_job_creation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
            .project_trusted(true)
            .build()
            .await;
        expose(&h);
        global(&h, "J", h.service_table("jobs").unwrap());

        let (stdin_err, tty_err, jobs_count): (String, String, usize) = eval(
            &h,
            r#"
            local approve = function() return { decision = "allow" } end
            local r1 = T:execute({
                id = "elev-stdin-1",
                name = "bash",
                arguments = {
                    command = "sh",
                    stdin = true,
                    run_in_background = true,
                    sandbox_permissions = "all",
                    justification = "test single-call elevation",
                },
            }, { approve = approve, on_state = function() return true end })
            assert(r1.is_error == true, "elevated stdin bash must fail: " .. mare.json.encode(r1))
            local r2 = T:execute({
                id = "elev-tty-1",
                name = "bash",
                arguments = {
                    command = "sh",
                    tty = true,
                    run_in_background = true,
                    sandbox_permissions = "all",
                    justification = "test single-call elevation",
                },
            }, { approve = approve, on_state = function() return true end })
            assert(r2.is_error == true, "elevated tty bash must fail: " .. mare.json.encode(r2))
            local list = J:list()
            local count = type(list) == "table" and type(list.data) == "table" and #list.data or 0
            local c1 = type(r1.content) == "table" and r1.content[1].text or tostring(r1.content)
            local c2 = type(r2.content) == "table" and r2.content[1].text or tostring(r2.content)
            return c1, c2, count
            "#,
        )
        .await;
        assert!(stdin_err.contains("PTY_POLICY"), "expected PTY_POLICY refusal for stdin: {stdin_err}");
        assert!(tty_err.contains("PTY_POLICY"), "expected PTY_POLICY refusal for tty: {tty_err}");
        assert_eq!(jobs_count, 0, "no background job may be created for refused elevated stdin/tty calls");
        h.shutdown().await;
    })
    .await
}
