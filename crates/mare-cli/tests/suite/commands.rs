//! Commands: argument trimming, command availability, /effort as an alias,
//! /effort's supported levels and per-model memory, usage across sessions, /context's
//! summary / free-space / over-window rows, /review's read-only session, the per-turn change
//! diffs. The TUI side (overlays, pickers) is in `suite::tui`.

use serde_json::{Value, json};

use crate::common::*;

mod usage;

/// Capture notices and overlays (rendered to text) and make the UI look connected.
async fn fake_ui(h: &mare_host::Host) {
    exec(
        h,
        r#"N = {}; OV = {}
           mare.ui.connected = function() return true end
           mare.ui.notify = function(t) N[#N + 1] = tostring(t) end
           mare.ui.overlay = function(title, node)
             OV[#OV + 1] = title .. "\n" .. mare.ui.render_text(node, 120)
           end"#,
    )
    .await;
}

async fn notices(h: &mare_host::Host) -> String {
    eval(h, "return table.concat(N, '\\n')").await
}

async fn last_overlay(h: &mare_host::Host) -> String {
    eval(h, "return OV[#OV] or ''").await
}

fn assistant(id: &str, parent: &str, text: &str, input: u64, output: u64) -> Value {
    json!({ "type": "message", "entry_id": id, "parent_entry_id": parent,
            "message": { "role": "assistant", "provider": "faux", "model": "echo",
                         "content": [{ "type": "text", "text": text }],
                         "usage": { "input": input, "output": output, "cache_read": 0, "cache_write": 0 } } })
}

#[tokio::test]
async fn arguments_are_trimmed_and_idle_only_commands_wait_for_the_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([{ "text": "slow answer ".repeat(100), "delay_ms": 40 }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"GOT = {}
               CM:register("probe", { availability = "idle", run = function(a) GOT[#GOT + 1] = "[" .. a .. "]" end })"#,
        )
        .await;
        exec(&h, r#"CM:run("/probe   two words  \n")"#).await;
        let got: String = eval(&h, "return table.concat(GOT, ',')").await;
        assert_eq!(got, "[two words]", "surrounding blanks are gone, inner ones stay");
        // a bad availability is refused at registration
        let err: String = eval(
            &h,
            r#"local ok, e = pcall(CM.register, CM, "bad", { availability = "x", run = print }) return tostring(e)"#,
        )
        .await;
        assert!(err.contains("availability"), "{err}");
        // while a turn runs: refused with a notice; afterwards it runs
        exec(&h, r#"mare.task.spawn(function() A:prompt("hi") end)"#).await;
        wait_until(WAIT_SECS, "the turn to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return A:get().busy").await }
        })
        .await;
        exec(&h, r#"local ok, e = pcall(CM.run, CM, "/probe now") assert(not ok and e.code == "commands.command_unavailable", tostring(e))"#).await;
        assert!(notices(&h).await.contains("isn't available while a turn is running"));
        let n: usize = eval(&h, "return #GOT").await;
        assert_eq!(n, 1, "not run during the turn");
        // /effort is the listed command and has no alias target.
        let listed: bool =
            eval(&h, r#"for _, d in ipairs(CM:list().data) do if d.name == "effort" then return true end end return false"#)
                .await;
        assert!(listed, "/effort must be listed as the primary command");
        let (is_primary, old_absent, show_reasoning_present, show_thinking_absent): (bool, bool, bool, bool) = eval(
            &h,
            r#"return CM:get("effort").alias_of == nil, CM:get("thinking") == nil, CM:get("show-reasoning") ~= nil, CM:get("show-thinking") == nil"#,
        )
        .await;
        assert!(is_primary);
        assert!(old_absent);
        assert!(show_reasoning_present);
        assert!(show_thinking_absent);
        h.shutdown().await;
    })
    .await
}

/// Availability beyond "idle": the session commands need the session store — refused under
/// --no-session (and nothing touches an existing session file: not /resume, not -c, not a
/// direct load), in a child agent and during a persistence fault; "main" refuses a command in
/// a child agent; completion dims what can't run.
#[tokio::test]
async fn session_commands_need_the_session_store_and_no_session_never_writes_a_session_file() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("old.jsonl", &[msg("u1", None, "user", "q1"), msg("a1", Some("u1"), "assistant", "r1")]);
        let before = std::fs::read(&path).unwrap();
        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([])).no_session().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        for cmd in ["/resume s1", "/sessions", "/continue", "/tree", "/undo", "/fork", "/clone", "/label x", "/retry"] {
            exec(&h, &format!("N = {{}} local ok = pcall(CM.run, CM, {cmd:?}) assert(not ok)")).await;
            let name = cmd.trim_start_matches('/').split(' ').next().unwrap();
            assert_eq!(
                notices(&h).await,
                format!("/{name} isn't available without a saved session (--no-session)"),
                "{cmd}"
            );
        }
        // the session service refuses a load on its own (pickers, plugins calling it directly)
        let err: String =
            eval(
                &h,
                &format!("local ok, e = pcall(S.load, S, {{ path = {:?} }}) return tostring(e)", path.to_str().unwrap()),
            )
            .await;
        assert!(err.contains("--no-session"), "{err}");
        // completion keeps the command, dimmed
        let raw: String = eval(
            &h,
            r#"return mare.json.encode(mare.ui.completion.collect({ token = "/resu", start = 0, line_index = 0, force = true }))"#,
        )
        .await;
        let menu: Value = serde_json::from_str(&raw).unwrap();
        let item = menu.as_array().and_then(|a| a.first().cloned()).expect("a menu item");
        assert_eq!(item["style"], "MareDim");
        assert!(item["detail"].as_str().unwrap().ends_with("(unavailable here)"), "{item}");
        // a child agent: the store belongs to the parent; "main" refuses too
        exec(
            &h,
            r#"GOT = 0
               CM:register("mainonly", { availability = "main", run = function() GOT = GOT + 1 end })
               CM:register("both", { availability = { "idle", "main" }, run = function() GOT = GOT + 1 end })
               N = {} CM:run("/mainonly") CM:run("/both")
               local mode = mare.mode
               mare.mode = "child"
               assert(not pcall(CM.run, CM, "/mainonly"))
               assert(not pcall(CM.run, CM, "/both"))
               assert(not pcall(CM.run, CM, "/resume"))
               mare.mode = mode"#,
        )
        .await;
        assert_eq!(eval::<i64>(&h, "return GOT").await, 2, "ran outside the child only");
        let n = notices(&h).await;
        assert!(n.contains("/mainonly isn't available in a child agent") && n.contains("/resume isn't available in a child agent"), "{n}");
        // a bad list entry is refused at registration
        let err: String =
            eval(&h, r#"local ok, e = pcall(CM.register, CM, "bad", { availability = { "idle", "x" }, run = print }) return tostring(e)"#)
                .await;
        assert!(err.contains("availability"), "{err}");
        h.shutdown().await;
        assert_eq!(std::fs::read(&path).unwrap(), before, "the session file is byte-identical");
        assert!(!std::path::Path::new(&format!("{}.lock", path.display())).exists());

        // -c under --no-session: an error, the file untouched
        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([])).args(json!({ "no_session": true, "continue": true })).build().await;
        expose(&h);
        let err: String = eval(&h, "return S:info().resume_error or ''").await;
        assert!(err.contains("--no-session"), "{err}");
        h.shutdown().await;
        assert_eq!(std::fs::read(&path).unwrap(), before, "-c --no-session leaves the file alone");

        // a persistence fault: the session commands wait until the writes recover
        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"A.persist_error = function() return "disk full" end N = {} assert(not pcall(CM.run, CM, "/label x"))"#).await;
        assert_eq!(notices(&h).await, "/label isn't available while session writes are failing");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn thinking_adjusts_unsupported_levels_out_loud_and_remembers_one_per_model() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .faux(json!([]))
            .build()
            .await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        // faux/echo supports low|medium|high: xhigh becomes high, and says so
        exec(&h, r#"CM:run("/effort  XHIGH ")"#).await;
        let eff: String = eval(&h, "return L:raw_effort()").await;
        assert_eq!(eff, "high");
        let n = notices(&h).await;
        assert!(n.contains("doesn't support effort xhigh") && n.contains("using high"), "{n}");
        exec(&h, r#"assert(not pcall(CM.run, CM, "/effort banana"))"#).await;
        assert!(notices(&h).await.contains("unknown effort \"banana\""));
        // remembered for faux/echo; another model without reasoning; back on echo it returns
        exec(&h, r#"CM:run("/effort low")"#).await;
        let saved: String = eval(&h, r#"return mare.state.read().model_params["faux/echo"].effort"#).await;
        assert_eq!(saved, "low");
        exec(&h, r#"L:set_effort("medium", { save = false }) L:set_model("faux/echo2") L:set_model("faux/echo")"#)
            .await;
        let eff: String = eval(&h, "return L:raw_effort()").await;
        assert_eq!(eff, "medium", "the session selection stays with faux/echo");
        // a model without reasoning effort: /effort says so instead of setting anything
        exec(&h, r#"L:set_model("faux/echo2") local ok = pcall(CM.run, CM, "/effort high") assert(not ok)"#).await;
        assert!(notices(&h).await.contains("has no reasoning effort"));
        // --session doesn't touch state.json
        exec(&h, r#"L:set_model("faux/echo") CM:run("/effort --session high")"#).await;
        let (eff, saved): (String, String) =
            eval(&h, r#"return L:raw_effort(), mare.state.read().model_params["faux/echo"].effort"#).await;
        assert_eq!((eff.as_str(), saved.as_str()), ("high", "low"));
        h.shutdown().await;
    })
    .await
}

/// Model parameters in the commands: /model's preview of the current model
/// lists value ← source, wire fields and options; /effort on a model with other parameters names
/// them; the choice lands in state.json `model_params`; /profile lists clamps.
#[tokio::test]
async fn model_preview_effort_and_profile_show_the_parameters() {
    local(async {
        let env = TestEnv::new().await;
        let models = json!({ "faux/echo2": { "params": { "thinking": { "type": "bool", "default": true } } } });
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref", "profile"])
            .faux(json!([]))
            .config(json!({ "models": models }))
            .build()
            .await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"mare.ui.select = function(o) SEL = o return nil end CM:run("/model")"#).await;
        let preview: String = eval(
            &h,
            r#"for _, it in ipairs(SEL.items) do
                 if it.label:find("✓ faux/echo", 1, true) == 1 then return mare.ui.render_text(it.preview, 120) end
               end
               return ''"#,
        )
        .await;
        assert!(preview.contains("parameters"), "{preview}");
        assert!(
            preview.lines().any(|l| l.trim_start().starts_with("effort ") && l.contains("← builtin")),
            "the effort row names its source layer: {preview}"
        );
        assert!(preview.contains("options low medium high"), "{preview}");
        exec(&h, r#"CM:run("/effort high")"#).await;
        let saved: String = eval(&h, r#"return mare.state.read().model_params["faux/echo"].effort"#).await;
        assert_eq!(saved, "high");
        exec(&h, r#"L:set_model("faux/echo2") assert(not pcall(CM.run, CM, "/effort high")) CM:run("/effort")"#).await;
        let n = notices(&h).await;
        assert_eq!(n.matches("faux/echo2 has no effort parameter; its parameters: thinking").count(), 2, "{n}");
        // a session effort the model doesn't offer (e.g. restored): clamped per request, and /profile says so
        exec(
            &h,
            r#"L:set_model("faux/echo") L:restore_selection({ effort = "xhigh" })
               L:complete({ messages = { { role = "user", content = { { type = "text", text = "hi" } } } } })
               CM:run("/profile")"#,
        )
        .await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("model parameter adjustments: 1"), "{ov}");
        assert!(ov.contains("faux/echo effort xhigh→high (clamp)"), "{ov}");
        h.shutdown().await;
    })
    .await
}

/// Usage belongs to the session: in-process /resume recounts from the file (nothing carried
/// over), `-c` starts with the resumed session's totals, a new session starts at 0; requests
/// outside the conversation (llm/usage) count; a subscription shows as not billed.
#[tokio::test]
async fn usage_is_recounted_per_session_and_subscriptions_are_not_billed() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "old.jsonl",
            &[
                msg("u1", None, "user", "q1"),
                assistant("a1", "u1", "r1", 1000, 200),
                msg("u2", Some("a1"), "user", "q2"),
                assistant("a2", "u2", "r2", 3000, 300),
            ],
        );
        // -c: the totals of the resumed session, not 0
        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([])).resume().build().await;
        global(&h, "U", h.service_table("usage").unwrap());
        let (input, output, req): (u64, u64, u64) =
            eval(&h, "local t = U:totals() return t.input, t.output, t.requests").await;
        assert_eq!((input, output, req), (4000, 500, 2), "-c recounts the session");
        h.shutdown().await;

        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([{ "text": "fresh" }])).build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        h.prompt("hello", json!({})).await.unwrap();
        let before: u64 = eval(&h, "return U:totals().requests").await;
        assert_eq!(before, 1);
        // in-process resume: the other session's numbers, without this one's
        global(&h, "P", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = P })").await;
        let (input, req): (u64, u64) = eval(&h, "local t = U:totals() return t.input, t.requests").await;
        assert_eq!((input, req), (4000, 2), "nothing carried over from the session before");
        // a compaction summary request reported by llm counts (outside the conversation)
        exec(&h, r#"C.cordis:get("bus"):emit("llm/usage", { usage = { input = 50, output = 5 }, model = "faux/echo", purpose = "compact" })"#)
            .await;
        let (input, side): (u64, u64) = eval(&h, "local t = U:totals() return t.input, t.side_requests").await;
        assert_eq!((input, side), (4050, 1));
        // a new session starts at 0
        exec(&h, "S:create()").await;
        let req: u64 = eval(&h, "return U:totals().requests").await;
        assert_eq!(req, 0);
        // subscription-only: not "$0.0000"
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("item/completed", { item_id = "it-sub", status = "completed", item = { id = "it-sub", type = "assistant_message", message = { role = "assistant", provider = "openai", model = "x",
                 subscription = true, usage = { input = 10, output = 1 } } } })"#,
        )
        .await;
        let text: String = eval(&h, "return U:cost_text()").await;
        assert_eq!(text, "subscription, not billed");
        // quota windows from the rate-limit headers
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("llm/rate-limits", { provider = "openai", limits = {
                 primary = { used_percent = 42, window_minutes = 300 },
                 secondary = { used_percent = 95, window_minutes = 10080 } } })"#,
        )
        .await;
        let q: String = eval(
            &h,
            r#"local out = {} for _, l in ipairs(U:quota_lines()) do out[#out + 1] = l.text .. (l.warning and " !" or "") end
               return table.concat(out, "\n")"#,
        )
        .await;
        assert!(q.contains("openai 5h: 42% used, 58% left"), "{q}");
        assert!(q.contains("openai weekly: 95% used, 5% left") && q.contains(" !"), "{q}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn context_shows_the_summary_free_space_and_warns_over_the_window() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .faux(json!([]))
            .no_session()
            .build()
            .await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"A:set_messages({
                 { role = "user", compaction = true, summary = "s",
                   content = { { type = "text", text = string.rep("summary ", 500) } } },
                 { role = "user", content = { { type = "text", text = "question" } } } })"#,
        )
        .await;
        let (summary, free, total): (i64, i64, i64) =
            eval(&h, r#"local b = CM:run("/context").result return b.summary, b.free, b.total"#).await;
        assert!(summary >= 900, "the summary has its own row: {summary}");
        assert_eq!(free, 100_000 - total, "faux/echo's window is 100k");
        let ov = last_overlay(&h).await;
        for row in ["Compaction summary", "Free space", "Total", "/context all"] {
            assert!(ov.contains(row), "{row}: {ov}");
        }
        // /context all: per tool definition and instruction file
        exec(&h, r#"CM:run("/context all")"#).await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("Tool definitions") && ov.contains("Largest messages") && ov.contains("read"), "{ov}");
        // over the window: a warning row
        exec(
            &h,
            r#"A:set_messages({ { role = "user", content = { { type = "text", text = string.rep("x", 480000) } } } })"#,
        )
        .await;
        let over: i64 = eval(&h, r#"return CM:run("/context").result.over"#).await;
        assert!(over > 0);
        assert!(last_overlay(&h).await.contains("Over the context window"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn status_lists_tokens_context_permissions_and_quota() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"]).faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        h.prompt("hi", json!({})).await.unwrap();
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("llm/rate-limits", { provider = "openai", limits = { primary = { used_percent = 10, window_minutes = 300 } } })
               CM:run("/status")"#,
        )
        .await;
        let ov = last_overlay(&h).await;
        for want in ["tokens", "cost", "context", "permissions", "rate limits", "openai 5h"] {
            assert!(ov.contains(want), "{want}: {ov}");
        }
        assert!(ov.contains("off — no sandbox"), "the test host runs without the permissions plugin: {ov}");
        h.shutdown().await;
    })
    .await
}

/// `/status` reads quota lines through the usage service's guarded proxy: with no quota yet
/// (a new session) the empty result is still a valid array, and with quota the lines show.
#[tokio::test]
async fn status_works_before_and_after_a_provider_reports_quota() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["status", "usage"]).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        let empty: String = eval(&h, r#"return mare.json.encode(C.cordis:get("usage"):quota_lines())"#).await;
        assert_eq!(empty, "[]");
        exec(&h, r#"CM:run("/status")"#).await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("tokens") && !ov.contains("rate limits"), "{ov}");
        assert!(!notices(&h).await.contains("invalid_result"));
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("llm/rate-limits", { provider = "openai", limits = { primary = { used_percent = 10, window_minutes = 300 } } })
               CM:run("/status")"#,
        )
        .await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("rate limits") && ov.contains("openai 5h"), "{ov}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn status_includes_mcp_section_when_servers_configured() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "mcp": {
                    "servers": {
                        "alpha": {
                            "command": "true",
                            "tier": "deferred"
                        }
                    }
                }
            }
        });
        let h = env.host().builtins(&["mcp"]).config(cfg).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/status")"#).await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("MCP"), "expected MCP section in /status output:\n{ov}");
        assert!(ov.contains("alpha"), "expected configured server in /status MCP section:\n{ov}");
        assert!(ov.contains("deferred, idle, 0 tools"), "expected status detail in /status:\n{ov}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn debug_shows_the_newest_crash_report_from_the_crashes_dir() {
    local(async {
        let env = TestEnv::new().await;
        let crashes = env.home.path().join("state").join("crashes");
        let old = crashes.join("1700000000000");
        let newest = crashes.join("1800000000000");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&newest).unwrap();
        std::fs::create_dir_all(crashes.join("not-a-timestamp")).unwrap();
        std::fs::write(crashes.join("1900000000001"), "not a directory").unwrap();
        std::fs::write(old.join("report.txt"), "old report\n").unwrap();
        let body = (1..=40).map(|n| format!("report line {n}\n")).collect::<String>();
        std::fs::write(newest.join("report.txt"), format!("mare 0.0.0 crash\nos: linux\nnewest\n{body}")).unwrap();
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        // Render wide enough to inspect the complete paths independently of the temp root's length.
        let width = (newest.to_string_lossy().len() + 80).max(120);
        exec(&h, &format!("mare.ui.overlay = function(title, node) OV[#OV + 1] = title .. '\\n' .. mare.ui.render_text(node, {width}) end")).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/debug")"#).await;
        let ov = last_overlay(&h).await;
        let newest_report = format!("{}/1800000000000/report.txt", crashes.display());
        let old_report = format!("{}/1700000000000/report.txt", crashes.display());
        assert!(ov.contains("crash reports: 2"), "expected two crash reports in overlay: {ov}");
        assert!(ov.find(&newest_report).unwrap() < ov.find(&old_report).unwrap(), "{ov}");
        let date: String = eval(&h, r#"return os.date("%Y-%m-%d %H:%M:%S", 1800000000)"#).await;
        assert!(ov.contains(&format!("{date}  {newest_report}")), "{ov}");
        assert!(ov.contains("mare 0.0.0 crash") && ov.contains("os: linux") && ov.contains("report line 37"), "{ov}");
        assert!(!ov.contains("old report") && !ov.contains("report line 38"), "{ov}");

        // Numeric order, the five-entry limit, and an incomplete newest crash directory.
        for name in ["9", "10", "11", "12", "1900000000000"] {
            std::fs::create_dir_all(crashes.join(name)).unwrap();
        }
        exec(&h, r#"CM:run("/debug")"#).await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("crash reports: 7"), "{ov}");
        assert!(
            ov.contains(&format!("{}/1900000000000/report.txt (no report.txt)", crashes.display())),
            "{ov}"
        );
        let twelve = format!("{}/12/report.txt", crashes.display());
        let eleven = format!("{}/11/report.txt", crashes.display());
        assert!(ov.find(&twelve).unwrap() < ov.find(&eleven).unwrap(), "{ov}");
        assert!(!ov.contains(&format!("{}/10/report.txt", crashes.display())), "{ov}");
        assert!(!ov.contains(&format!("{}/9/report.txt", crashes.display())), "{ov}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn debug_tails_the_last_hundred_log_lines() {
    local(async {
        let env = TestEnv::new().await;
        let log = env.home.path().join("state").join("log").join("mare.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, (1..=150).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"local read = mare.fs.read
               READ_LOG_CALLS = 0
               mare.fs.read = function(path, ...)
                 if path == mare.paths.state .. "/log/mare.log"
                   or (mare.paths.log and path == mare.paths.log .. "/mare.log") then
                   READ_LOG_CALLS = READ_LOG_CALLS + 1
                 end
                 return read(path, ...)
               end
               CM:run("/debug")"#,
        )
        .await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains(&format!("log: {}/mare.log", log.parent().unwrap().display())), "{ov}");
        assert!(ov.contains("line 150") && ov.contains("line 51") && !ov.contains("line 50"), "{ov}");
        let whole_reads: i64 = eval(&h, "return READ_LOG_CALLS").await;
        assert_eq!(whole_reads, 0, "/debug must not read the whole log");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn debug_without_log_or_crashes_says_so() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/debug")"#).await;
        let ov = last_overlay(&h).await;
        assert!(
            ov.contains(&format!("no log yet ({}/mare.log)", env.home.path().join("state").join("log").display())),
            "expected missing-log message in overlay: {ov}"
        );
        assert!(
            ov.contains(&format!("no crash reports ({})", env.home.path().join("state").join("crashes").display())),
            "{ov}"
        );
        let log = env.home.path().join("state").join("log").join("mare.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, [0xff]).unwrap();
        exec(&h, r#"CM:run("/debug")"#).await;
        let ov = last_overlay(&h).await;
        assert!(ov.contains("no log yet"), "{ov}");
        h.shutdown().await;
    })
    .await
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = fixture_git(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// /review runs a read-only one-shot agent: the uncommitted diff it gets includes untracked
/// files; `/review <sha>` reviews that commit; the findings never enter the conversation.
#[tokio::test]
async fn review_runs_a_read_only_session_over_uncommitted_changes_or_a_commit() {
    local(async {
        let env = TestEnv::new().await;
        git(&env.cwd, &["init", "-q"]);
        std::fs::write(env.cwd.join("tracked.txt"), "one\n").unwrap();
        git(&env.cwd, &["add", "."]);
        git(&env.cwd, &["commit", "-qm", "first commit"]);
        std::fs::write(env.cwd.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(env.cwd.join("brand-new.txt"), "untracked content\n").unwrap();
        // an empty script: the faux model echoes the prompt it got, so the reply shows the diff
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .faux(json!([]))
            .build()
            .await;
        expose(&h);
        global(&h, "R", h.service_table("review").unwrap());
        let text: String = eval(&h, r#"return R:run({ kind = "uncommitted" })"#).await;
        assert!(text.contains("staged, unstaged, and untracked"), "{text}");
        assert!(
            text.contains("+two") && text.contains("brand-new.txt") && text.contains("+untracked content"),
            "{text}"
        );
        let msgs: usize = eval(&h, "return #A:list_messages().data").await;
        assert_eq!(msgs, 0, "the review isn't added to the conversation");
        let head = fixture_git(&env.cwd).args(["rev-parse", "HEAD"]).output().unwrap();
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        global(&h, "SHA", sha[..10].to_string());
        let text: String = eval(&h, r#"return R:run({ kind = "commit", sha = SHA })"#).await;
        assert!(text.contains(&sha) && text.contains("first commit") && text.contains("+one"), "{text}");
        // nothing to review / a bad commit: an error, no session
        let err: String = eval(&h, r#"local _, e = R:run({ kind = "commit", sha = "nope" }) return e"#).await;
        assert!(err.contains("no such commit"), "{err}");
        h.shutdown().await;
    })
    .await
}

/// changes:diffs() gives each turn its own change (before → the next turn's backup), not the
/// file's state now.
#[tokio::test]
async fn per_turn_diffs_show_each_turns_own_change() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "v1\n").unwrap();
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "CH", h.service_table("changes").unwrap());
        let call = |id: &str, name: &str, args: Value| {
            mare_shared::lua::json_to_lua(h.lua(), &json!({ "id": id, "name": name, "arguments": args })).unwrap()
        };
        exec(&h, r#"mare.snapshot.begin_turn("t1")"#).await;
        global(&h, "R", call("r", "read", json!({ "path": "a.txt" })));
        exec(&h, "T:execute(R, {})").await;
        global(&h, "W", call("w1", "write", json!({ "path": "a.txt", "content": "v2\n" })));
        exec(&h, "T:execute(W, {})").await;
        exec(&h, r#"mare.snapshot.begin_turn("t2")"#).await;
        global(&h, "W", call("w2", "write", json!({ "path": "a.txt", "content": "v3\n" })));
        exec(&h, "T:execute(W, {})").await;
        let (o1, n1, o2, n2): (String, String, String, String) = eval(
            &h,
            r#"local d = CH:diffs()
               return d[1].files[1].old, d[1].files[1].new, d[2].files[1].old, d[2].files[1].new"#,
        )
        .await;
        assert_eq!((o1.as_str(), n1.as_str()), ("v1\n", "v2\n"), "turn 1: its own change");
        assert_eq!((o2.as_str(), n2.as_str()), ("v2\n", "v3\n"), "turn 2");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn memory_add_appends_to_the_project_agents_md() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        std::fs::write(env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(env.cwd.join("AGENTS.md"), "# Rules\n- old").unwrap();
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .faux(json!([]))
            .no_session()
            .build()
            .await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/memory add   use tabs \n")"#).await;
        wait_until(WAIT_SECS, "the note to be written", || async {
            std::fs::read_to_string(env.cwd.join("AGENTS.md")).unwrap().contains("- use tabs")
        })
        .await;
        // the notice follows the write: under load it can land a moment later
        wait_until(WAIT_SECS, "the next-session notice", || async { notices(&h).await.contains("next session") }).await;
        assert_eq!(std::fs::read_to_string(env.cwd.join("AGENTS.md")).unwrap(), "# Rules\n- old\n- use tabs\n");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cfg_root_config_command_has_project_tab_and_ignored_markers() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub");
        let dir = sub.join("dir");
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        let run_config = |cwd: &std::path::Path| {
            let mut b = env.host().faux(json!([])).no_session();
            b.opts.cwd = cwd.to_path_buf();
            async move {
                let h = b.build().await;
                expose(&h);
                fake_ui(&h).await;
                global(&h, "CM", h.service_table("commands").unwrap());
                exec(
                    &h,
                    r#"SEL = nil
                       mare.ui.select = function(o) SEL = o return nil end
                       CM:run("/config")"#,
                )
                .await;
                wait_until(WAIT_SECS, "/config select opened", || {
                    let h = h.clone();
                    async move { eval::<bool>(&h, "return SEL ~= nil").await }
                })
                .await;
                let tabs_json: String = eval(&h, "return mare.json.encode(SEL.tabs)").await;
                h.shutdown().await;
                let tabs: Value = serde_json::from_str(&tabs_json).unwrap();
                tabs
            }
        };

        // (a) Fixture with repo/.mare only -> resource root = repo, detail names started in dir, no ignored item.
        let tabs_a = run_config(&dir).await;
        let proj_a = tabs_a
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "Project")
            .expect("Project tab missing in case (a)");
        let items_a = proj_a["items"].as_array().unwrap();
        let root_a = items_a.iter().find(|it| it["label"] == "resource root").expect("resource root item missing");
        let detail_a = root_a["detail"].as_str().unwrap();
        assert!(
            detail_a.contains(&repo.display().to_string()) && detail_a.contains("started in"),
            "unexpected detail_a: {detail_a}"
        );
        assert!(items_a.iter().all(|it| it["label"] != "ignored"), "no ignored expected: {items_a:?}");

        // (b) Add repo/sub/.mcp.json -> resource root = repo/sub, ignored item names repo.
        std::fs::write(sub.join(".mcp.json"), "{}").unwrap();
        let tabs_b = run_config(&dir).await;
        let proj_b = tabs_b
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "Project")
            .expect("Project tab missing in case (b)");
        let items_b = proj_b["items"].as_array().unwrap();
        let root_b = items_b.iter().find(|it| it["label"] == "resource root").expect("resource root item missing");
        assert!(root_b["detail"].as_str().unwrap().contains(&sub.display().to_string()), "{root_b}");
        let ign_b = items_b.iter().find(|it| it["label"] == "ignored").expect("ignored item missing");
        assert!(ign_b["detail"].as_str().unwrap().contains(&repo.display().to_string()), "{ign_b}");
    })
    .await
}

/// commands domain: a command receives what
/// was submitted with its line — the arguments as the first text block, then the images — as
/// `run(args, input)`; an alias hands it on. Reference: the plugin repository's `/goal`
/// (`plugins/goal/lua/goal/input.lua`) and `/plan` (`plugins/plan-mode/init.lua:214`) read
/// `input.content` this way. reference implementation, Codex and Kimi Code have no test of attachments on a slash
/// command; the case follows the split's acceptance item.
#[tokio::test]
async fn a_command_receives_the_submitted_content() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            "CM:register('probe', { run = function(args, input) mare.g.probe = mare.json.encode({ args, input }) end })
             CM:register('pr', { alias_of = 'probe' })",
        )
        .await;
        let image = json!({ "type": "image", "data": "aGk=", "mime": "image/png" });
        for line in ["/probe look here", "/pr look here"] {
            h.submit_with(line, vec![image.clone()]).await;
            let got: String = eval(&h, "return mare.g.probe").await;
            let got: Value = serde_json::from_str(&got).unwrap();
            assert_eq!(got[0], "look here", "{line}");
            assert_eq!(got[1]["content"], json!([{ "type": "text", "text": "look here" }, image]), "{line}");
        }
        h.shutdown().await;
    })
    .await
}

/// commands domain: a command's `complete(prefix)` answers Tab on
/// its argument — `/model gpt` offers the matching models (the definition existed, nothing called
/// it). A failing `complete` offers nothing instead of breaking completion. Reference: Codex
/// `codex-rs/tui/src/bottom_pane/command_popup.rs:375` (completing command names) and
/// `chat_composer/slash_input.rs:736` (an existing argument tail is kept) — neither completes
/// arguments; Kimi Code and reference implementation no argument-completion test; the case follows the split.
#[tokio::test]
async fn a_command_completes_its_own_arguments() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            "CM:register('probe', { args = '[name]', run = function() end,
               complete = function(prefix) return { { label = prefix .. '-one', insert = prefix .. '-one' } } end })
             CM:register('broken', { run = function() end, complete = function() error('boom') end })",
        )
        .await;
        let menu = h
            .bail_json("input/complete", json!({ "token": "ab", "start": 7, "line_index": 0, "text": "/probe ab" }))
            .await;
        assert_eq!(menu.unwrap_or_default()[0]["label"], "ab-one");
        let broken = h
            .bail_json("input/complete", json!({ "token": "x", "start": 8, "line_index": 0, "text": "/broken x" }))
            .await;
        assert!(broken.is_none_or(|m| m.as_array().is_none_or(|a| a.is_empty())), "nothing offered");
        h.shutdown().await;
    })
    .await
}

/// commands domain: `running` lets a command run
/// only during a turn, `tui` only in the terminal UI — refused with the reason otherwise — and
/// /compact declares `idle` instead of refusing a busy agent in its own body. Reference: Codex
/// `codex-rs/tui/src/slash_command.rs:180-259` (per-command availability during a task) and its
/// test `codex-rs/tui/src/bottom_pane/command_popup.rs:337` (an unavailable command is shown only
/// when searched); reference implementation Kimi Code have no availability-matrix test.
#[tokio::test]
async fn running_and_tui_availability_are_declared_not_hand_written() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "slow answer ".repeat(100), "delay_ms": 40 }]))
            .build()
            .await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"GOT = {}
               CM:register("during", { availability = "running", run = function() GOT[#GOT + 1] = "during" end })
               CM:register("termonly", { availability = "tui", run = function() GOT[#GOT + 1] = "tui" end })"#,
        )
        .await;
        let idle: String =
            eval(&h, r#"local ok, r = pcall(CM.run, CM, "/during") assert(not ok) return tostring(r.message)"#).await;
        assert!(idle.contains("isn't available when no turn is running"), "{idle}");
        let headless: String =
            eval(&h, r#"local ok, r = pcall(CM.run, CM, "/termonly") assert(not ok) return tostring(r.message)"#).await;
        assert!(headless.contains("isn't available outside the terminal UI"), "{headless}");
        exec(&h, r#"mare.task.spawn(function() A:prompt("hi") end)"#).await;
        wait_until(WAIT_SECS, "the turn to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return A:get().busy").await }
        })
        .await;
        exec(&h, r#"CM:run("/during")"#).await;
        let busy_compact: String =
            eval(&h, r#"local ok, r = pcall(CM.run, CM, "/compact") assert(not ok) return tostring(r.message)"#).await;
        assert!(busy_compact.contains("/compact isn't available while a turn is running"), "{busy_compact}");
        assert_eq!(eval::<String>(&h, "return table.concat(GOT, ',')").await, "during");
        h.shutdown().await;
    })
    .await
}

/// commands contract: `commands:run` waits out an in-flight `/reload` (`reload_barrier`) and
/// fails closed when the VM's durable session barrier is poisoned (`exec_gate`).
#[tokio::test]
async fn commands_run_enforces_exec_gate_and_reload_barrier() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        let gate_dir = env.cwd.join("reload-gate");
        std::fs::create_dir_all(&gate_dir).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/reload_gate.lua"),
            format!(
                r#"return {{ name = "reload-gate", apply = function(ctx)
  ctx:effect(function() return function()
    local gate = {:?}
    local f = assert(io.open(gate .. "/started", "w")); f:close()
    for _ = 1, 3000 do
      if mare.fs.exists(gate .. "/release") then return end
      mare.task.sleep(10)
    end
    error("reload gate timed out")
  end end)
end }}"#,
                gate_dir
            ),
        )
        .unwrap();
        let h = env.host().faux(json!([])).no_session().user_plugins().build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"RAN = 0; CM:register("probe", { run = function() RAN = RAN + 1 end })"#).await;

        let reloading = {
            let h = h.clone();
            tokio::task::spawn_local(async move { h.kernel.reload(Some("reload-gate")).await.unwrap() })
        };
        wait_until(WAIT_SECS, "reload cleanup to enter", || async { gate_dir.join("started").exists() }).await;

        let running = {
            let h = h.clone();
            tokio::task::spawn_local(async move {
                exec(&h, r#"CM:run("/probe")"#).await;
            })
        };
        tokio::task::yield_now().await;
        assert_eq!(eval::<i64>(&h, "return RAN").await, 0, "commands.run must wait behind reload_barrier");

        std::fs::write(gate_dir.join("release"), "ok").unwrap();
        reloading.await.unwrap();
        running.await.unwrap();
        assert_eq!(eval::<i64>(&h, "return RAN").await, 1);

        mare_binding::exec_gate::poison(h.lua(), "test-poison");
        let err: String =
            eval(&h, r#"local ok, e = pcall(CM.run, CM, "/probe") assert(not ok) return tostring(e)"#).await;
        assert!(err.contains("session transaction failed: test-poison"), "{err}");
        assert_eq!(eval::<i64>(&h, "return RAN").await, 1, "poisoned VM refuses commands.run");
        h.shutdown().await;
    })
    .await
}

/// Review regressions:
/// 1. TUI `/commands` `browse()` uses `run_local` so `/lua` (`local_only`) works and unavailable
///    commands only emit a warning without raising a second red error bar;
/// 2. Double-`Esc` (`/undo` in `--no-session`), `/skills` picking a failing skill, and `<M-p>`
///    on `/model` failure never escape `key/interrupt` / `ctx:spawn` into kernel `problems`.
#[tokio::test]
async fn tui_browse_double_esc_skills_and_model_key_isolate_command_errors() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("skills/badskill")).unwrap();
        std::fs::write(
            env.config_dir().join("skills/badskill/SKILL.md"),
            "---\nname: badskill\ndescription: broken skill\n---\nbody\n",
        )
        .unwrap();
        let mut b = env.host().faux(json!([])).no_session();
        b.opts.mode = mare_shared::ui::Mode::Tui;
        let h = b.build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"mare.ui.connected = function() return true end"#).await;
        h.kernel.reload(Some("ui")).await.unwrap();
        exec(
            &h,
            r#"
            N = {}
            mare.ui.notify = function(t, lvl) N[#N + 1] = (lvl or "info") .. ":" .. tostring(t) end
            mare.ui.overlay = function() end
            local picked
            mare.ui.select = function(opts)
              for i, item in ipairs(opts.items or {}) do
                if item.label == picked then return i end
              end
              return nil
            end
            CM:register("lua", { local_only = true, run = function() LUA_BROWSED = true end })
            CM:register("skill:badskill", { run = function() error("skill-boom") end })
            _G.set_pick = function(l) picked = l end
            "#,
        )
        .await;

        // 1. Double-Esc -> `/undo` fails in `--no-session` cleanly with warning, zero problems.
        h.kernel.emit_json("key/interrupt", json!({ "editor_empty": true })).await;
        h.kernel.emit_json("key/interrupt", json!({ "editor_empty": true })).await;
        assert!(h.kernel.problems().is_empty(), "double-Esc leaked into problems: {:?}", h.kernel.problems());
        assert!(notices(&h).await.contains("without a saved session"), "{}", notices(&h).await);

        // 2. `/commands` browse picks `/lua` (succeeds) and `/resume` (warns, no red error bar).
        exec(
            &h,
            r#"
            N = {}
            set_pick("/lua")
            C.cordis:get("commands"):run("/commands")
            set_pick("/resume")
            C.cordis:get("commands"):run("/commands")
            "#,
        )
        .await;
        assert!(eval::<bool>(&h, "return LUA_BROWSED == true").await);
        let n = notices(&h).await;
        assert!(!n.contains("error:"), "browse emitted unexpected error notice: {n}");
        assert!(h.kernel.problems().is_empty(), "browse leaked into problems: {:?}", h.kernel.problems());

        // 3. `/skills` selecting a failing skill does not pollute problems.
        exec(
            &h,
            r#"
            set_pick("/skill:badskill")
            C.cordis:get("commands"):run("/skills")
            "#,
        )
        .await;
        assert!(h.kernel.problems().is_empty(), "/skills leaked into problems: {:?}", h.kernel.problems());
        h.shutdown().await;
    })
    .await
}

/// Alias argument completion (`/effort <Tab>`) and unavailable completion hints (`(after this
/// turn)` / `(unavailable here)` with `MareDim`); plus in-process `/q` and `/status` exemption
/// when `exec_gate` is poisoned; plus `/doctor` warning redaction & quiet non-UI RPC mode.
#[tokio::test]
async fn alias_completion_hints_status_exemption_and_doctor_redaction() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "A", h.service_table("agent").unwrap());

        // Alias argument completion: `/effort ` delegates to `thinking`'s complete(prefix).
        exec(
            &h,
            r#"
            CM:register("probe-target", {
              args = "[x]",
              complete = function(p) return { { label = p .. "-via-alias", insert = p .. "-via-alias" } } end,
              run = function() end,
            })
            CM:register("pa", { alias_of = "probe-target" })
            "#,
        )
        .await;
        let alias_comp = h
            .bail_json("input/complete", json!({ "token": "hi", "start": 4, "line_index": 0, "text": "/pa hi" }))
            .await
            .expect("alias completion");
        assert_eq!(alias_comp[0]["label"], "hi-via-alias");

        // Unavailable completion detail for `/resume` under `--no-session`.
        let cmd_menu_raw: String = eval(
            &h,
            r#"return mare.json.encode(mare.ui.completion.collect({ token = "/res", start = 0, line_index = 0, force = true }))"#,
        )
        .await;
        let cmd_menu: Value = serde_json::from_str(&cmd_menu_raw).unwrap();
        let resume = cmd_menu.as_array().unwrap().iter().find(|i| i["label"] == "/resume").expect("/resume item");
        assert_eq!(resume["style"], "MareDim", "{resume}");
        assert!(resume["detail"].as_str().unwrap_or("").contains("(unavailable here)"), "{resume}");

        // `/compact` while busy shows `(after this turn)`.
        exec(&h, r#"A.activity = function() return { version = 1, busy = true, queued = 0, turn_cancelled = false } end"#).await;
        let busy_menu_raw: String = eval(
            &h,
            r#"return mare.json.encode(mare.ui.completion.collect({ token = "/com", start = 0, line_index = 0, force = true }))"#,
        )
        .await;
        let busy_menu: Value = serde_json::from_str(&busy_menu_raw).unwrap();
        let compact = busy_menu.as_array().unwrap().iter().find(|i| i["label"] == "/compact").expect("/compact item");
        assert_eq!(compact["style"], "MareDim", "{compact}");
        assert!(compact["detail"].as_str().unwrap_or("").contains("(after this turn)"), "{compact}");

        // /doctor redacts config_warnings and suppresses stdout print in non-print/non-tui mode.
        let doctor_ok: bool = eval(
            &h,
            r#"
            mare.config_warnings = function() return { "bad api_key = sk-super-secret-value-123456" } end
            local checks = require("commands.doctor")(C.cordis)
            for _, c in ipairs(checks) do
              if c.name == "config" then
                assert(not c.detail:find("sk-super-secret-value-123456", 1, true), "unredacted config warning: " .. c.detail)
                return true
              end
            end
            return false
            "#,
        )
        .await;
        assert!(doctor_ok);

        // In-process Lua `/status` and `/q` remain exempt under exec_gate::poison.
        mare_binding::exec_gate::poison(h.lua(), "test-poison");
        exec(
            &h,
            r#"
            mare.ui.connected = function() return false end
            CM:run("/status")
            mare.ui.exit = function() QUIT_CALLED = true end
            CM:run("/q")
            assert(QUIT_CALLED == true, "/q must run under exec_gate")
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn review_enforces_read_only_and_blocks_file_writes() {
    local(async {
        let env = TestEnv::new().await;
        git(&env.cwd, &["init", "-q"]);
        std::fs::write(env.cwd.join("tracked.txt"), "v1\n").unwrap();
        git(&env.cwd, &["add", "."]);
        git(&env.cwd, &["commit", "-qm", "init"]);
        std::fs::write(env.cwd.join("tracked.txt"), "v2\n").unwrap();

        // Script the reviewer subagent to attempt both `write` (not in read-only tools) and
        // a mutating `bash` command (denied under read-only preset/sandbox).
        let script = json!([
            { "tool_calls": [
                { "name": "write", "arguments": { "path": "review-write.txt", "content": "nope\n" } },
                { "name": "bash", "arguments": { "command": "echo nope > review-bash.txt" } }
            ] },
            { "text": "review complete" }
        ]);
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .faux(script)
            .build()
            .await;
        expose(&h);
        global(&h, "R", h.service_table("review").unwrap());
        let text: String = eval(&h, r#"return R:run({ kind = "uncommitted" })"#).await;
        assert!(text.contains("review complete"), "{text}");
        assert!(!env.cwd.join("review-write.txt").exists(), "/review must not allow write tool");
        assert!(!env.cwd.join("review-bash.txt").exists(), "/review must not allow mutating bash");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn clear_starts_fresh_session_and_session_id_reports_current_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["changes", "review", "status", "usage", "compact", "copy", "file-ref"])
            .faux(json!([{ "text": "hello back" }]))
            .build()
            .await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        h.prompt("hello", json!({})).await.unwrap();
        let before_id: String = eval(&h, "return S:info().session_id").await;
        let before_msgs: usize = eval(&h, "return #A:list_messages().data").await;
        assert!(before_msgs >= 2, "expected messages before /clear");

        let (noted, after_id, after_msgs): (String, String, usize) = eval(
            &h,
            r#"
            local notes = {}
            mare.ui.notify = function(msg) notes[#notes + 1] = tostring(msg) end
            CM:run("/session id")
            CM:run("/clear")
            return table.concat(notes, "\n"), S:info().session_id, #A:list_messages().data
            "#,
        )
        .await;
        assert!(noted.contains(&before_id), "/session id must notify current id {before_id}: {noted}");
        assert_ne!(after_id, before_id, "/clear must switch to a fresh session id");
        assert_eq!(after_msgs, 0, "/clear must clear conversation history");
        h.shutdown().await;
    })
    .await
}

/// `/init` is registered with `availability =
/// "idle"` and submits the AGENTS.md initialization prompt via `input/submit`.
#[tokio::test]
async fn init_command_submits_agents_md_prompt_and_is_idle_only() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&[]).no_session().build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        let (avail, submitted): (String, String) = eval(
            &h,
            r#"local def = assert(CM:get("init"))
               local got = ""
               C.cordis:on("input/submit", function(ev) got = tostring(ev.text or "") end)
               CM:run("/init")
               local deadline = mare.task.clock() + 2000
               while got == "" and mare.task.clock() < deadline do mare.task.sleep(10) end
               return tostring(def.availability), got"#,
        )
        .await;
        assert_eq!(avail, "idle");
        assert!(
            submitted.contains("AGENTS.md") && submitted.contains("build, lint and test commands"),
            "unexpected /init prompt: {submitted}"
        );
        h.shutdown().await;
    })
    .await
}

/// `/profile` warns (without failing startup) when a third-party plugin yields during `apply`.
#[tokio::test]
async fn profile_warns_when_third_party_plugin_apply_yields() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(plugins_dir.join("slow-user")).unwrap();
        std::fs::create_dir_all(plugins_dir.join("fast-user")).unwrap();
        std::fs::write(
            plugins_dir.join("slow-user/init.lua"),
            r#"return {
                name = "slow-user",
                apply = function()
                    mare.task.sleep(1)
                end,
            }"#,
        )
        .unwrap();
        std::fs::write(
            plugins_dir.join("fast-user/init.lua"),
            r#"return {
                name = "fast-user",
                apply = function() end,
            }"#,
        )
        .unwrap();

        let h = env.host().builtins(&["profile"]).user_plugins().no_session().build().await;
        let status = h.kernel.plugin_status();
        assert!(
            status.iter().any(|(name, origin, active)| name == "slow-user" && origin == "user" && *active),
            "yielding third-party plugin must still activate cleanly: {status:?}"
        );
        assert!(
            h.kernel.problems().iter().all(|p| !p.contains("slow-user")),
            "yielding third-party plugin must not record a startup error: {:?}",
            h.kernel.problems()
        );

        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/profile")"#).await;
        let ov = last_overlay(&h).await;
        assert!(
            ov.contains("warning:") && ov.contains("yielded during apply") && ov.contains("slow-user"),
            "expected /profile warning for slow-user yielding in apply:\n{ov}"
        );
        let warning_section = ov.split("yielded during apply").nth(1).unwrap_or("");
        assert!(
            !warning_section.contains("fast-user"),
            "synchronous third-party plugin must not appear in yield warning:\n{ov}"
        );
        h.shutdown().await;
    })
    .await
}
