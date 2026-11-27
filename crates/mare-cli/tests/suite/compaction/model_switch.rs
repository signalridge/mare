//! Model admission uses the target budget before changing the live model or its defaults.
use super::*;
use mare_host::Host;

fn switch_config(base: i64) -> serde_json::Value {
    json!({ "plugins": { "compact": {
        "auto": false, "headroom_tokens": 1000, "threshold_tokens": base + 20000,
        "keep_recent_tokens": 1, "recent_user_tokens": 0,
        "model_thresholds": { "faux/echo2": { "tokens": base + 1800 } }
    } } })
}

async fn setup(h: &Host, base: i64) {
    expose(h);
    global(h, "L", h.service_table("llm").unwrap());
    global(h, "K", h.service_table("compact").unwrap());
    global(h, "BASE", base);
    exec(
        h,
        r#"
        local target = L:resolve("faux/echo2")
        target.context_window, target.max_output = BASE + 12000, 1024
        REQUEST_MODELS, REQUEST_TOKENS, CHANGES = {}, {}, 0
        local provider = L:provider("faux")
        local stream = provider.stream
        provider.stream = function(req, sink)
            REQUEST_MODELS[#REQUEST_MODELS + 1] = req.model.ref
            local tokens = mare.text.tokens(req.system or "") + mare.text.tokens(req.tools or {})
            for _, message in ipairs(req.messages or {}) do tokens = tokens + mare.text.tokens(message) end
            REQUEST_TOKENS[#REQUEST_TOKENS + 1] = tokens
            return stream(req, sink)
        end
        C.cordis:on("model/changed", function() CHANGES = CHANGES + 1 end)
    "#,
    )
    .await;
}

#[tokio::test]
async fn smaller_model_switch_summarizes_with_old_model_before_a_real_target_request() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let script = json!([
            { "text": "original answer" },
            { "text": "SWITCH-SUMMARY: original request recorded" },
            { "text": "target reply" }
        ]);
        let cfg = switch_config(base);
        let h = env.host().builtins(&["compact", "usage"]).faux(script.clone()).config(cfg.clone()).build().await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        let selected: String = eval(&h, r#"return L:set_model("faux/echo2").ref"#).await;
        assert_eq!(selected, "faux/echo2");
        assert_eq!(eval::<i64>(&h, "return CHANGES").await, 1);
        let (models, messages, used): (String, String, i64) =
            eval(&h, "return table.concat(REQUEST_MODELS, ','), mare.json.encode(A:list_messages().data), K:measure()")
                .await;
        assert_eq!(models, "faux/echo,faux/echo", "the old model must do the summary");
        assert!(messages.contains("SWITCH-SUMMARY"), "{messages}");
        assert!(!messages.contains("ORIGINAL-DETAIL"), "{messages}");
        assert!(used < base + 1800, "target budget was not met: {used}");
        let result = h.prompt("continue", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        assert_eq!(result["text"], "target reply");
        assert_eq!(
            eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await,
            "faux/echo,faux/echo,faux/echo2"
        );
        h.shutdown().await;

        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(script)
            .config(cfg)
            .config(json!({ "model": "faux/echo2" }))
            .resume()
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo2");
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(messages.contains("SWITCH-SUMMARY") && messages.contains("target reply"), "{messages}");
        assert!(!messages.contains("ORIGINAL-DETAIL"), "{messages}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_model_switch_discards_projected_prunes_and_preserves_model_context_and_defaults() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let mut cfg = switch_config(base);
        cfg["plugins"]["compact"]["prune_protect_tokens"] = json!(0);
        cfg["plugins"]["compact"]["prune_min_saving_tokens"] = json!(1);
        cfg["plugins"]["compact"]["prune_tool_result_chars"] = json!(100);
        cfg["plugins"]["compact"]["prune_head_chars"] = json!(10);
        cfg["plugins"]["compact"]["prune_tail_chars"] = json!(10);
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "large.txt" } }] },
                { "text": "original answer" },
                { "error": "summary unavailable", "retryable": false, "kind": "invalid" },
                { "text": "old model still works" }
            ]))
            .config(cfg)
            .build()
            .await;
        std::fs::write(env.cwd.join("large.txt"), "TOOL-ORIGINAL ".repeat(1500)).unwrap();
        setup(&h, base).await;
        h.prompt(&"USER-ORIGINAL ".repeat(1500), json!({})).await.unwrap();
        let (path, before, effort, recent): (String, String, String, String) = eval(
            &h,
            r#"
            L:set_effort("high")
            return S:info().path, mare.json.encode(A:list_messages().data), L:effort(), mare.json.encode(L:recent())
        "#,
        )
        .await;
        let default_before: String = eval(&h, "return L:get_selection().default_model").await;
        let log_before = std::fs::read(&path).unwrap();
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            local ok, err = pcall(L.set_model, L, "faux/echo2")
            return ok, tostring(err)
        "#,
        )
        .await;
        assert!(!ok && err.contains("summary unavailable"), "{ok}: {err}");
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo");
        assert_eq!(eval::<String>(&h, "return L:effort()").await, effort);
        assert_eq!(eval::<String>(&h, "return L:get_selection().default_model").await, default_before);
        assert_eq!(eval::<String>(&h, "return mare.json.encode(L:recent())").await, recent);
        assert_eq!(eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await, before);
        assert_eq!(std::fs::read(&path).unwrap(), log_before, "failed admission must not persist prunes or a summary");
        assert_eq!(eval::<i64>(&h, "return CHANGES").await, 0);
        assert_eq!(
            eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await,
            "faux/echo,faux/echo,faux/echo",
            "no target request may escape failed admission"
        );
        let result = h.prompt("continue on the old model", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        assert_eq!(result["text"], "old model still works");
        assert_eq!(eval::<String>(&h, "return REQUEST_MODELS[#REQUEST_MODELS]").await, "faux/echo");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn model_switch_prunes_then_remeasures_without_a_summary_request() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let mut cfg = switch_config(base);
        cfg["plugins"]["compact"]["prune_protect_tokens"] = json!(0);
        cfg["plugins"]["compact"]["prune_min_saving_tokens"] = json!(1);
        cfg["plugins"]["compact"]["prune_tool_result_chars"] = json!(100);
        cfg["plugins"]["compact"]["prune_head_chars"] = json!(10);
        cfg["plugins"]["compact"]["prune_tail_chars"] = json!(10);
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "large.txt" } }] },
            { "text": "done reading" },
            { "text": "target reply" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script.clone()).config(cfg.clone()).build().await;
        // ~18KB over many lines: `read` cuts a single line at 2000 bytes (MK5), which would
        // leave nothing worth pruning
        let large = format!("{}\n", "PRUNABLE ".repeat(10)).repeat(200);
        std::fs::write(env.cwd.join("large.txt"), &large).unwrap();
        // ten lines of the original as they appear JSON-encoded in the transcript
        let original_run = serde_json::to_string(&large[..large.len() / 20]).unwrap().trim_matches('"').to_string();
        assert!(serde_json::to_string(&large).unwrap().contains(&original_run));
        setup(&h, base).await;
        h.prompt("read large.txt", json!({})).await.unwrap();
        exec(&h, r#"L:set_model("faux/echo2")"#).await;
        assert_eq!(
            eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await,
            "faux/echo,faux/echo",
            "model-free recovery must avoid a summary request"
        );
        let (used, before_resume): (i64, String) =
            eval(&h, "return K:measure(), mare.json.encode(A:list_messages().data)").await;
        assert!(used < base + 1800, "{used}");
        assert!(before_resume.contains("tool result middle pruned"), "{before_resume}");
        let result = h.prompt("continue", json!({})).await.unwrap();
        assert_eq!(result["text"], "target reply");
        h.shutdown().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).resume().build().await;
        expose(&h);
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(messages.contains("tool result middle pruned"), "the projection must survive resume: {messages}");
        assert!(!messages.contains(&original_run), "{messages}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn impossible_target_budget_rejects_before_any_summary_or_target_request() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "old reply" }]))
            .config(switch_config(base))
            .build()
            .await;
        setup(&h, base).await;
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            L:resolve("faux/echo2").context_window = 100
            local ok, err = pcall(L.set_model, L, "faux/echo2")
            return ok, tostring(err)
        "#,
        )
        .await;
        assert!(!ok && err.contains("leaving no message budget"), "{ok}: {err}");
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo");
        assert_eq!(eval::<i64>(&h, "return #REQUEST_MODELS").await, 0);
        assert_eq!(eval::<i64>(&h, "return CHANGES").await, 0);
        assert_eq!(h.prompt("hello", json!({})).await.unwrap()["text"], "old reply");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn summary_and_image_projections_commit_together_and_survive_resume() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let mut cfg = switch_config(base);
        cfg["plugins"]["compact"]["offload_keep_images"] = json!(0);
        cfg["plugins"]["compact"]["recent_user_tokens"] = json!(1000);
        let script = json!([{ "text": "original answer" }, { "text": "IMAGE-SWITCH-SUMMARY" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script.clone()).config(cfg.clone()).build().await;
        setup(&h, base).await;
        let image = json!({ "type": "image", "mime": "image/png", "data": "UE5HLVNXSVRDSA==" });
        h.prompt(&"IMAGE-ORIGINAL ".repeat(2000), json!({ "images": [image] })).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        exec(&h, r#"L:set_model("faux/echo2")"#).await;
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(messages.contains("IMAGE-SWITCH-SUMMARY"), "{messages}");
        assert!(!messages.contains("UE5HLVNXSVRDSA=="), "inline image survived offload: {messages}");
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(&path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let checkpoint = records.iter().find(|r| r["type"] == "compaction").expect("summary checkpoint");
        assert!(!checkpoint["projections"].as_object().unwrap().is_empty(), "{checkpoint}");
        assert!(
            !records.iter().any(|r| r["type"] == "prune" && r["method"] == "offload"),
            "offload must not persist before the summary is accepted"
        );
        h.shutdown().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(script)
            .config(cfg)
            .config(json!({ "model": "faux/echo2" }))
            .resume()
            .build()
            .await;
        expose(&h);
        let resumed: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert_eq!(resumed, messages, "summary and candidate projections must replay identically");
        h.shutdown().await;
    })
    .await
}

fn process_config(home: &std::path::Path) {
    let plugins = home.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"
        mare.setup {
            models = { ["faux/echo2"] = { context_window = 70000, max_output = 1024 } },
            plugins = { compact = { auto = false, headroom_tokens = 1000,
                threshold_tokens = 90000, keep_recent_tokens = 1, recent_user_tokens = 0,
                model_thresholds = { ["faux/echo2"] = { tokens = 50000 } } } }
        }
    "#,
    )
    .unwrap();
    std::fs::write(
        plugins.join("switch-trace.lua"),
        r#"
        return { name = "switch-trace", inject = { "llm" }, apply = function(ctx)
            local models = {}
            local provider = ctx.llm:provider("faux")
            local stream = provider.stream
            provider.stream = function(req, sink)
                models[#models + 1] = req.model.ref
                mare.fs.write(mare.cwd .. "/requests.json", mare.json.encode(models))
                return stream(req, sink)
            end
        end }
    "#,
    )
    .unwrap();
}

fn traced_models(home: &std::path::Path) -> Vec<String> {
    serde_json::from_slice(&std::fs::read(home.join("requests.json")).unwrap()).unwrap()
}

#[test]
fn rpc_model_switch_waits_for_admission_and_returns_failure_without_a_target_request() {
    use std::process::Stdio;
    for succeeds in [true, false] {
        let home = tempfile::tempdir().unwrap();
        process_config(home.path());
        let script = home.path().join("faux.json");
        let summary = if succeeds {
            json!({ "text": "RPC-SWITCH-SUMMARY" })
        } else {
            json!({ "error": "rpc summary failed", "retryable": false, "kind": "invalid" })
        };
        std::fs::write(&script, json!([{ "text": "seed answer" }, summary, { "text": "next reply" }]).to_string())
            .unwrap();
        let mut cmd = mare_cmd();
        cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
        let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
        let seed_start =
            rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "HISTORY ".repeat(35000) }), |_| {
                json!(null)
            });
        assert!(seed_start["error"].is_null(), "{seed_start}");
        let seed = rpc.call_raw(
            "agent.wait",
            json!({ "session_id": sid, "turn_id": seed_start["result"]["turn_id"] }),
            |_| json!(null),
        );
        assert!(seed["error"].is_null(), "{seed}");
        let before = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
        let options_before = rpc.call_raw("options.get", json!({ "session_id": sid }), |_| json!(null));
        let effort = if options_before["result"]["effort"] == "high" { "low" } else { "high" };
        let values = if succeeds {
            json!({ "model": "faux/echo2" })
        } else {
            json!({ "model": "faux/echo2", "effort": effort })
        };
        let switched = rpc.call_raw("options.set", json!({ "session_id": sid, "values": values }), |_| json!(null));
        assert_eq!(switched["error"].is_null(), succeeds, "{switched}");
        let current = rpc.call_raw("options.get", json!({ "session_id": sid }), |_| json!(null));
        assert_eq!(current["result"]["model"], if succeeds { "faux/echo2" } else { "faux/echo" }, "{current}");
        if !succeeds {
            assert_eq!(
                current["result"]["effort"], options_before["result"]["effort"],
                "a rejected model in the same options.set must not apply effort first"
            );
        }
        assert_eq!(traced_models(home.path()), ["faux/echo", "faux/echo"]);
        let after = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
        if succeeds {
            assert!(after.to_string().contains("RPC-SWITCH-SUMMARY"), "{after}");
            assert!(!after.to_string().contains(&"HISTORY ".repeat(100)), "{after}");
        } else {
            assert!(switched["error"]["message"].as_str().unwrap().contains("rpc summary failed"), "{switched}");
            assert_eq!(after["result"], before["result"], "rejected switch changed the context");
        }
        let next_start =
            rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "continue" }), |_| json!(null));
        assert!(next_start["error"].is_null(), "{next_start}");
        let next = rpc.call_raw(
            "agent.wait",
            json!({ "session_id": sid, "turn_id": next_start["result"]["turn_id"] }),
            |_| json!(null),
        );
        assert!(next["error"].is_null(), "{next}");
        assert_eq!(
            traced_models(home.path()),
            ["faux/echo", "faux/echo", if succeeds { "faux/echo2" } else { "faux/echo" }]
        );
        rpc.shutdown_and_wait();
    }
}

#[test]
fn headless_resume_model_override_uses_the_same_admission_gate() {
    use std::{io::Write, process::Stdio};
    for succeeds in [true, false] {
        let home = tempfile::tempdir().unwrap();
        process_config(home.path());
        let script = home.path().join("faux.json");
        std::fs::write(&script, json!([{ "text": "seed answer" }]).to_string()).unwrap();
        let mut seed_cmd = mare_cmd();
        seed_cmd
            .args(["--json", "--yolo", "--model", "faux/echo"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut seed_process = ChildGroup::spawn(&mut seed_cmd).unwrap();
        seed_process.0.stdin.take().unwrap().write_all("HISTORY ".repeat(35000).as_bytes()).unwrap();
        let seed = wait_output_timeout(seed_process, DEFAULT_CMD_TIMEOUT);
        assert!(seed.status.success(), "{}", String::from_utf8_lossy(&seed.stderr));
        let summary = if succeeds {
            json!({ "text": "HEADLESS-SWITCH-SUMMARY" })
        } else {
            json!({ "error": "headless summary failed", "retryable": false, "kind": "invalid" })
        };
        std::fs::write(&script, json!([summary, { "text": "target reply" }]).to_string()).unwrap();
        let resumed = mare_cmd()
            .args(["--json", "--yolo", "-c", "--model", "faux/echo2", "continue"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert_eq!(
            resumed.status.success(),
            succeeds,
            "stdout={} stderr={}",
            String::from_utf8_lossy(&resumed.stdout),
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert_eq!(
            traced_models(home.path()),
            if succeeds { vec!["faux/echo", "faux/echo2"] } else { vec!["faux/echo"] }
        );
        if succeeds {
            let records: Vec<serde_json::Value> = String::from_utf8(resumed.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records.last().unwrap()["type"], "settled");
            assert_eq!(records.last().unwrap()["data"]["text"], "target reply");
        } else {
            let output =
                format!("{}{}", String::from_utf8_lossy(&resumed.stdout), String::from_utf8_lossy(&resumed.stderr));
            assert!(output.contains("headless summary failed"), "{output}");
        }
    }
}

#[tokio::test]
async fn child_fork_to_a_smaller_model_passes_admission_before_its_first_request() {
    local(async {
        for succeeds in [true, false] {
            let env = TestEnv::new().await;
            let base = super::stages::overhead(&env).await;
            let summary = if succeeds {
                json!({ "text": "CHILD-SWITCH-SUMMARY" })
            } else {
                json!({ "error": "child summary failed", "retryable": false, "kind": "invalid" })
            };
            let plugins = env.config_dir().join("plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            std::fs::write(plugins.join("child-switch-trace.lua"), r#"
                return { name = "child-switch-trace", inject = { "llm", "bus" },
                    contracts = { probe = { owner = "child-switch-trace", events = {
                        ["provider-request"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
                    } } }, apply = function(ctx)
                    local provider = ctx.llm:provider("faux")
                    local stream = provider.stream
                    provider.stream = function(req, sink)
                        ctx.bus:emit("probe/provider-request", { model = req.model.ref, messages = req.messages })
                        return stream(req, sink)
                    end
                end }
            "#).unwrap();
            let cfg = switch_config(base);
            let h = env
                .host().builtins(&["compact", "usage"])
                .faux(json!([summary, { "text": "child target reply" }]))
                .config(cfg)
                .no_session()
                .build()
                .await;
            setup(&h, base).await;
            // The child inherits its own smaller model metadata through config.
            global(&h, "TARGET_WINDOW", base + 12000);
            global(&h, "CWD", env.cwd.to_string_lossy().to_string());
            let (ok, text, requests, target_request): (bool, String, String, String) = eval(
                &h,
                r#"
                local history = {
                    { role = "user", content = { { type = "text", text = string.rep("CHILD-ORIGINAL ", 2000) } } },
                    { role = "assistant", content = { { type = "text", text = "old answer" } } },
                }
                local a = mare.subagent.start({ model = "faux/echo2", cwd = CWD, no_session = true,
                    messages = history, prompt = "continue", config = {
                        models = { ["faux/echo2"] = { context_window = TARGET_WINDOW, max_output = 1024 } }
                    } })
                local requests, target_request = {}, ""
                while true do
                    local event = a:next_event()
                    if not event then break end
                    if event.name == "probe/provider-request" then
                        requests[#requests + 1] = event.payload.model
                        if event.payload.model == "faux/echo2" then target_request = mare.json.encode(event.payload.messages) end
                    end
                    -- shutdown is the completed request boundary; VM references can outlive wait().
                    if event.name == "app/shutdown" then break end
                end
                local ok, result = pcall(a.wait, a)
                return ok and not result.error, ok and mare.json.encode(result) or tostring(result), table.concat(requests, ","), target_request
            "#,
            )
            .await;
            assert_eq!(ok, succeeds, "{text}");
            assert_eq!(requests, if succeeds { "faux/echo,faux/echo2" } else { "faux/echo" },
                "failed child admission must never send a target request");
            if succeeds {
                assert!(text.contains("child target reply"), "{text}");
                assert!(target_request.contains("CHILD-SWITCH-SUMMARY"), "{target_request}");
                assert!(!target_request.contains(&"CHILD-ORIGINAL ".repeat(100)), "{target_request}");
            } else {
                assert!(text.contains("child summary failed"), "{text}");
            }
            assert_eq!(
                eval::<String>(&h, "return L:current().ref").await,
                "faux/echo",
                "child admission must not change its parent"
            );
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn resume_explicit_model_record_wins_over_conflicting_assistant_metadata() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead_for(env.host().all_builtins()).await;
        let mut assistant = msg("a1", Some("u1"), "assistant", "answer from a different routed model");
        assistant["message"]["provider"] = json!("faux");
        assistant["message"]["model"] = json!("echo2");
        let path = env.write_session(
            "model-source-conflict.jsonl",
            &[
                msg("u1", None, "user", &"SOURCE-DETAIL ".repeat(2000)),
                assistant,
                json!({ "type": "model", "model": "faux/echo", "at_entry_id": "a1" }),
            ],
        );
        let h = env
            .host()
            .user_config(true)
            .faux(json!([{ "text": "AUTHORITATIVE-SOURCE-SUMMARY" }, { "text": "target reply" }]))
            .config(switch_config(base))
            .config(json!({ "model": "faux/echo2", "plugins": { "permissions": { "enabled": false } } }))
            .build()
            .await;
        setup(&h, base).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = TARGET })").await;
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo2");
        assert_eq!(
            eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await,
            "faux/echo",
            "the explicit source record must choose the old summarization model"
        );
        let context: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(context.contains("AUTHORITATIVE-SOURCE-SUMMARY"), "{context}");
        let result = h.prompt("continue", json!({})).await.unwrap();
        assert_eq!(result["text"], "target reply");
        assert_eq!(eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await, "faux/echo,faux/echo2");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_without_a_source_admits_only_history_already_below_the_target_budget() {
    local(async {
        for fits in [true, false] {
            let env = TestEnv::new().await;
            let base = super::stages::overhead_for(env.host().all_builtins()).await;
            let text = if fits { "small history".to_string() } else { "UNSOURCED-DETAIL ".repeat(2000) };
            let path = env.write_session("no-model-source.jsonl", &[msg("u1", None, "user", &text)]);
            let h = env
                .host()
                .user_config(true)
                .faux(json!([{ "text": "target reply" }]))
                .config(switch_config(base))
                .config(json!({ "model": "faux/echo2", "plugins": { "permissions": { "enabled": false } } }))
                .build()
                .await;
            setup(&h, base).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let (ok, error): (bool, String) =
                eval(&h, "local ok, err = pcall(S.load, S, { path = TARGET }); return ok, tostring(err)").await;
            assert_eq!(ok, fits, "{error}");
            assert_eq!(
                eval::<i64>(&h, "return #REQUEST_MODELS").await,
                0,
                "no source must never cause a guessed summary request"
            );
            if fits {
                assert_eq!(h.prompt("continue", json!({})).await.unwrap()["text"], "target reply");
                assert_eq!(eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await, "faux/echo2");
            } else {
                assert!(error.contains("no source model is recorded"), "{error}");
                let context: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
                assert!(context.contains(&"UNSOURCED-DETAIL ".repeat(100)), "{context}");
                assert!(h.prompt("must not send", json!({})).await.is_err());
                assert_eq!(eval::<i64>(&h, "return #REQUEST_MODELS").await, 0);
            }
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn ui_resume_command_to_sourceless_session_with_cli_model_admits_via_llm_spi() {
    local(async {
        for fits in [true, false] {
            let env = TestEnv::new().await;
            let base = super::stages::overhead_for(env.host().all_builtins()).await;
            let text = if fits { "small history".to_string() } else { "UNSOURCED-DETAIL ".repeat(2000) };
            let path = env.write_session("no-model-source.jsonl", &[msg("u1", None, "user", &text)]);
            let h = env
                .host()
                .user_config(true)
                .faux(json!([{ "text": "target reply" }]))
                .config(switch_config(base))
                .config(json!({ "model": "faux/echo2", "plugins": { "permissions": { "enabled": false } } }))
                .build()
                .await;
            setup(&h, base).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            exec(
                &h,
                r#"
                N = {}
                RESUMED = false
                mare.ui.connected = function() return true end
                mare.ui.notify = function(msg) N[#N + 1] = tostring(msg) end
                C.cordis:on("session/changed", function(ev)
                    if ev and ev.path == TARGET then RESUMED = true end
                end)
            "#,
            )
            .await;

            let reply = h.submit_command(&format!("/resume {}", path.display())).await;
            assert!(reply.get("message").and_then(|m| m.as_str()).is_none_or(|m| !m.contains("unhandled")), "{reply}");
            wait_until(WAIT_SECS, "UI /resume outcome", || {
                let h = h.clone();
                async move {
                    if fits {
                        eval::<bool>(&h, "return RESUMED and not S:switching()").await
                    } else {
                        eval::<bool>(
                            &h,
                            r#"return table.concat(N, "\n"):find("no source model is recorded", 1, true) ~= nil"#,
                        )
                        .await
                    }
                }
            })
            .await;

            let notices: String = eval(&h, r#"return table.concat(N, "\n")"#).await;
            assert!(
                !notices.contains("cannot dispatch llm/model-changing"),
                "unexpected event dispatch failure: {notices}"
            );
            if fits {
                assert!(!notices.contains("completion failed"), "unexpected completion failure on /resume: {notices}");
                assert!(eval::<bool>(&h, "return RESUMED").await, "session should switch to TARGET");
                assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo2");
                assert_eq!(h.prompt("continue", json!({})).await.unwrap()["text"], "target reply");
                assert_eq!(eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await, "faux/echo2");
            } else {
                assert!(notices.contains("no source model is recorded"), "expected budget refusal notice: {notices}");
                assert!(h.prompt("must not send", json!({})).await.is_err());
                assert_eq!(eval::<i64>(&h, "return #REQUEST_MODELS").await, 0);
            }
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn cancellation_after_summary_persistence_finishes_the_admitted_model_switch() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "original answer" }, { "text": "DURABLY-COMMITTED-SUMMARY" }, { "text": "target reply" }
            ]))
            .config(switch_config(base))
            .build()
            .await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        exec(
            &h,
            r#"
            local append = S.append_compaction
            S.append_compaction = function(...)
                local result = table.pack(append(...))
                if result[1] then K:cancel() end
                return table.unpack(result, 1, result.n)
            end
            L:set_model("faux/echo2")
            S.append_compaction = append
        "#,
        )
        .await;
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo2");
        let context: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(context.contains("DURABLY-COMMITTED-SUMMARY"), "{context}");
        assert!(!context.contains("ORIGINAL-DETAIL"), "{context}");
        assert_eq!(eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await, "faux/echo,faux/echo");
        assert!(eval::<i64>(&h, "return K:measure()").await < base + 1800);
        assert_eq!(h.prompt("continue", json!({})).await.unwrap()["text"], "target reply");
        let sent_tokens: i64 = eval(&h, "return REQUEST_TOKENS[#REQUEST_TOKENS]").await;
        assert!(sent_tokens < base + 1800, "the actual target request exceeded its admitted budget: {sent_tokens}");
        assert_eq!(
            eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await,
            "faux/echo,faux/echo,faux/echo2"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_summary_persistence_preserves_the_model_defaults_and_live_context() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "original answer" }, { "text": "UNCOMMITTED-SUMMARY" }, { "text": "old model reply" }
            ]))
            .config(switch_config(base))
            .build()
            .await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        let (state, context, path): (String, String, String) = eval(
            &h,
            "return mare.json.encode(L:get_selection()), mare.json.encode(A:list_messages().data), S:info().path",
        )
        .await;
        let (accepted, error): (bool, String) = eval(
            &h,
            r#"
            local append = S.append_compaction
            S.append_compaction = function() return nil, "injected durable write failure" end
            local accepted, error = pcall(L.set_model, L, "faux/echo2")
            S.append_compaction = append
            return accepted, tostring(error)
        "#,
        )
        .await;
        assert!(!accepted && error.contains("injected durable write failure"), "{accepted}: {error}");
        assert_eq!(eval::<String>(&h, "return mare.json.encode(L:get_selection())").await, state);
        assert_eq!(eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await, context);
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(!records.iter().any(|record| record["type"] == "compaction"));
        assert_eq!(eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await, "faux/echo,faux/echo");
        assert_eq!(h.prompt("continue", json!({})).await.unwrap()["text"], "old model reply");
        assert_eq!(
            eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await,
            "faux/echo,faux/echo,faux/echo"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn later_model_admission_failure_discards_prepared_summary_and_resumes_original_history() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let cfg = switch_config(base);
        let script = json!([{ "text": "original answer" }, { "text": "REJECTED-SUMMARY" }, { "text": "continued" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script.clone()).config(cfg.clone()).build().await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        exec(
            &h,
            r#"
                ORIGINAL = mare.json.encode(A:list_messages().data)
                local listener = C.cordis:on("llm/model-changing", function()
                    error("late admission veto")
                end)
                ACCEPTED, FAILURE = pcall(L.set_model, L, "faux/echo2")
                listener()
            "#,
        )
        .await;
        let (accepted, error, model, same): (bool, String, String, bool) = eval(
            &h,
            r#"
                return ACCEPTED, tostring(FAILURE), L:current().ref,
                    ORIGINAL == mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert!(!accepted);
        assert!(error.contains("late admission veto"), "{error}");
        assert_eq!(model, "faux/echo");
        assert!(same, "admission failure changed live history");
        assert_eq!(eval::<i64>(&h, "return CHANGES").await, 0);
        let result = h.prompt("continue original context", json!({})).await.unwrap();
        assert_eq!(result["text"], "continued");
        h.shutdown().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).resume().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo");
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(messages.contains("ORIGINAL-DETAIL"), "{messages}");
        assert!(!messages.contains("REJECTED-SUMMARY"), "{messages}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn committed_context_publication_failure_blocks_work_until_resume() {
    local(async {
        for fault in ["agent_throw", "agent_yield", "usage_throw"] {
            let env = TestEnv::new().await;
            let base = super::stages::overhead(&env).await;
            let cfg = switch_config(base);
            let h = env
                .host().builtins(&["compact", "usage"])
                .faux(json!([
                    {"text":"original answer"}, {"text":"COMMITTED-PUBLICATION-SUMMARY"}, {"text":"MUST-NOT-SEND"}
                ]))
                .config(cfg.clone())
                .build()
                .await;
            setup(&h, base).await;
            global(&h, "U", h.service_table("usage").unwrap());
            global(&h, "T", h.service_table("tools").unwrap());
            global(&h, "FAULT", fault);
            h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
            let path: String = eval(&h, "return S:info().path").await;
            let (accepted, error): (bool, String) = eval(
                &h,
                r#"
                EFFECTS = 0
                T:register("publication-effect", {description="must stay blocked", parameters={type="object"},
                    side_effect="external", execute=function() EFFECTS=EFFECTS+1; return {content="executed"} end})
                -- the fault goes on the published service table its consumers call through: with one
                -- service slot another provider cannot take `usage` / `agent` from the running
                -- built-in (already_provided), and the root ctx's provide never reaches the registry
                if FAULT == "usage_throw" then
                    U.set_context = function() error("injected usage publication failure") end
                else
                    A.set_messages = function()
                        if FAULT == "agent_yield" then mare.task.sleep(1000) end
                        error("injected agent publication failure")
                    end
                end
                local accepted, error = pcall(L.set_model, L, "faux/echo2")
                return accepted, tostring(error)
            "#,
            )
            .await;
            assert!(!accepted, "{fault}: a failed publication cannot succeed");
            assert!(error.contains("committed") && !error.contains("discarded"), "{fault}: {error}");
            assert!(
                mare_shared::session_barrier::barrier(h.lua()).reason().is_some(),
                "{fault}: no publication barrier"
            );
            let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(
                records.iter().any(|r| r["type"] == "compaction"
                    && r["summary"].as_str().unwrap_or("").contains("COMMITTED-PUBLICATION-SUMMARY")),
                "{fault}: {records:?}"
            );
            assert!(records.iter().any(|r| r["type"] == "model" && r["model"] == "faux/echo2"), "{fault}: {records:?}");
            assert!(h.prompt("MUST-NOT-SEND", json!({})).await.is_err(), "{fault}: prompt crossed the barrier");
            let (model_call, model_change): (bool, bool) = eval(&h, r#"
                local complete = pcall(L.complete, L, {messages={{role="user",content={{type="text",text="MUST-NOT-SEND"}}}}})
                local change = pcall(L.set_model, L, "faux/echo")
                return complete, change
            "#).await;
            assert!(!model_call && !model_change, "{fault}: direct model operation crossed the publication barrier");

            exec(
                &h,
                r#"pcall(T.execute, T, {id="blocked-publication", name="publication-effect", arguments={}}, {
                approve=function() return {decision="allow"} end})"#,
            )
            .await;
            assert_eq!(eval::<i64>(&h, "return EFFECTS").await, 0, "{fault}: tool crossed the barrier");
            assert_eq!(eval::<String>(&h, "return table.concat(REQUEST_MODELS, ',')").await, "faux/echo,faux/echo");
            h.shutdown().await;

            let h = env.host().builtins(&["compact", "usage"]).faux(json!([{"text":"RECOVERED-REPLY"}])).config(cfg).resume().build().await;
            expose(&h);
            global(&h, "L", h.service_table("llm").unwrap());
            assert!(mare_shared::session_barrier::barrier(h.lua()).reason().is_none());
            assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo2");
            let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
            assert!(
                messages.contains("COMMITTED-PUBLICATION-SUMMARY") && !messages.contains("ORIGINAL-DETAIL"),
                "{fault}: {messages}"
            );
            assert_eq!(h.prompt("continue recovered context", json!({})).await.unwrap()["text"], "RECOVERED-REPLY");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn compaction_observers_can_yield_after_context_and_model_are_published() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                {"text":"original answer"}, {"text":"OBSERVED-PUBLISHED-SUMMARY"}, {"text":"target reply"}
            ]))
            .config(switch_config(base))
            .build()
            .await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        exec(
            &h,
            r#"
            OBSERVED_MESSAGES, OBSERVED_USAGE = false, false
            C.cordis:on("agent/messages-replaced", function(event)
                assert(L:current().ref == "faux/echo2", "observer ran before model publication")
                assert(mare.json.encode(event.messages):find("OBSERVED%-PUBLISHED%-SUMMARY"))
                OBSERVED_MESSAGES = true
                mare.task.sleep(1)
                error("message observer failure must not veto the durable commit")
            end)
            C.cordis:on("usage/changed", function()
                if L:current().ref == "faux/echo2" then
                    assert(mare.json.encode(A:list_messages().data):find("OBSERVED%-PUBLISHED%-SUMMARY"))
                    OBSERVED_USAGE = true
                    mare.task.sleep(1)
                    error("usage observer failure must not veto the durable commit")
                end
            end)
            L:set_model("faux/echo2")
        "#,
        )
        .await;
        assert_eq!(eval::<(bool, bool)>(&h, "return OBSERVED_MESSAGES, OBSERVED_USAGE").await, (true, true));
        assert!(mare_shared::session_barrier::barrier(h.lua()).reason().is_none());
        assert_eq!(h.prompt("continue on target", json!({})).await.unwrap()["text"], "target reply");
        h.shutdown().await;
    })
    .await
}

/// A compaction stage that raises during a model switch is an unexpected failure: the RPC caller
/// gets `internal` with nothing of the exception (it goes to the log), and nothing changes. A
/// reported failure is the expected refusal llm.model_unavailable (the test above).
#[test]
fn rpc_model_switch_hides_a_compaction_stage_exception() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    process_config(home.path());
    std::fs::write(
        home.path().join("config/plugins/crash-stage.lua"),
        r#"return { name = "crash-stage", inject = { "compact" }, apply = function(ctx)
             for _, name in ipairs({ "prune", "offload", "remote", "local" }) do
               ctx.compact:register_stage(name, function() error("stage crashed s3cret") end)
             end
           end }"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "seed answer" }, { "text": "unused" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let seed_start =
        rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "HISTORY ".repeat(35000) }), |_| json!(null));
    assert!(seed_start["error"].is_null(), "{seed_start}");
    let seed =
        rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": seed_start["result"]["turn_id"] }), |_| {
            json!(null)
        });
    assert!(seed["error"].is_null(), "{seed}");
    let switched =
        rpc.call_raw("options.set", json!({ "session_id": sid, "values": { "model": "faux/echo2" } }), |_| json!(null));
    assert_eq!(switched["error"]["data"]["code"], "internal", "{switched}");
    assert!(!switched.to_string().contains("s3cret"), "{switched}");
    let current = rpc.call_raw("options.get", json!({ "session_id": sid }), |_| json!(null));
    assert_eq!(current["result"]["model"], "faux/echo", "{current}");
    rpc.shutdown_and_wait();
}

#[tokio::test]
async fn restore_selection_skips_budget_check_and_keeps_poison_barrier() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let script = json!([{ "text": "original answer" }, { "text": "unused summary" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(switch_config(base)).build().await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        let (default_before, current_after, default_after, req_models, poison_ok, poison_err): (
            String,
            String,
            String,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local snap_before = L:get_selection()
            L:restore_selection({ model = "faux/echo2" })
            local after_restore = L:current().ref
            local snap_after = L:get_selection()
            local off = C.cordis:on("llm/model-changed", function()
                error("restore poison boom")
            end)
            local ok, err = pcall(L.restore_selection, L, { model = "faux/echo" })
            off()
            return tostring(snap_before.default_model),
                after_restore,
                tostring(snap_after.default_model),
                table.concat(REQUEST_MODELS, ","),
                ok,
                tostring(err)
            "#,
        )
        .await;
        assert_eq!(current_after, "faux/echo2");
        assert_eq!(default_after, default_before, "restore_selection must never persist defaults");
        assert_eq!(req_models, "faux/echo", "restore_selection must skip compaction summary request");
        assert!(!poison_ok && poison_err.contains("restore poison boom"), "poison barrier failed: {poison_err}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn set_model_with_unknown_source_is_refused_over_budget() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let script = json!([
            { "text": "original answer" },
            { "error": "summary unavailable", "retryable": false, "kind": "invalid" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(switch_config(base)).build().await;
        setup(&h, base).await;
        h.prompt(&"ORIGINAL-DETAIL ".repeat(2000), json!({})).await.unwrap();
        let (ok, err, cur_after): (bool, String, String) = eval(
            &h,
            r#"
            local ok, err = pcall(L.set_model, L, "faux/echo2", { target = "session", source_unknown = true })
            return ok, tostring(err), L:current().ref
            "#,
        )
        .await;
        assert!(!ok, "over-budget set_model with source_unknown=true must run budget check and refuse: {err}");
        assert!(
            !err.contains("invalid_arguments") && !err.contains("source_unknown"),
            "set_model must accept Lua-only source_unknown instead of invalid_arguments: {err}"
        );
        assert_eq!(cur_after, "faux/echo");
        h.shutdown().await;
    })
    .await;
}
