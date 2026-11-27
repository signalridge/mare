use super::*;

#[tokio::test]
async fn idle_admission_reserves_before_spawn_and_never_queues_busy_work() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "accepted" }, { "text": "unexpected" }])).no_session().build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            local info = S:info()
            OWNER = { session_id = info.session_id, session_epoch = info.session_epoch }
            STARTS = {}; C.cordis:on("turn/started", function(e) STARTS[#STARTS+1] = e end)
            ENDS = {}; C.cordis:on("turn/completed", function(e) ENDS[#ENDS+1] = e end)
            FIRST = A:prompt("automatic", {when_busy="reject", expected_session=OWNER, source="test"})
            local second_ok, second_err = pcall(A.prompt, A, "must never queue", {when_busy="reject", expected_session=OWNER})
            return mare.json.encode({ first=FIRST, second_ok=second_ok, second_code=type(second_err)=="table" and second_err.code or tostring(second_err), busy=A:get().busy })
        "#,
        )
        .await;
        let result: serde_json::Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(result["first"]["status"], "started", "{result}");
        assert_eq!(result["second_ok"], false, "{result}");
        assert_eq!(result["second_code"], "busy", "{result}");
        assert_eq!(result["busy"], true);
        wait_until(10, "owned turn to finish", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ENDS == 1").await }
        })
        .await;
        let (start_turn, end_turn, text, count): (String, String, String, usize) =
            eval(&h, "return STARTS[1].turn_id, ENDS[1].turn_id, ENDS[1].text, #A:list_messages().data").await;
        assert_eq!(Some(start_turn.as_str()), result["first"]["turn_id"].as_str());
        assert_eq!(Some(end_turn.as_str()), result["first"]["turn_id"].as_str());
        assert_eq!(text, "accepted");
        assert_eq!(count, 2, "busy work must not enter history");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn idle_admission_fences_generation_and_validates_options_without_side_effects() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "unused" }])).no_session().build().await;
        expose(&h);
        let statuses: Vec<String> = eval(
            &h,
            r#"
            local info = S:info(); local owner = {session_id=info.session_id,session_epoch=info.session_epoch}
            local ok_a, err_a = pcall(A.prompt, A, "old",{when_busy="reject", expected_session={session_id=info.session_id,session_epoch="old"}})
            local ok_b = pcall(A.prompt, A, " ", {when_busy="reject", expected_session=owner})
            local ok_c = pcall(A.prompt, A, "x", {when_busy="bogus", expected_session=owner})
            local ok_d = pcall(A.prompt, A, "x", "bad")
            local ok_e = pcall(A.prompt, A, "x", {when_busy="reject", expected_session=owner, tools={42}})
            local ok_f, err_f = pcall(A.prompt, A, "x",{when_busy="reject", expected_session={session_id="wrong",session_epoch=info.session_epoch}})
            return {
                (not ok_a and type(err_a) == "table" and err_a.code) or "unexpected",
                ok_b and "accepted" or "rejected",
                ok_c and "accepted" or "rejected",
                ok_d and "accepted" or "rejected",
                ok_e and "accepted" or "rejected",
                (not ok_f and type(err_f) == "table" and err_f.code) or "unexpected",
            }
        "#,
        )
        .await;
        assert_eq!(statuses, ["stale_session", "rejected", "rejected", "rejected", "rejected", "stale_session"]);
        assert!(!eval::<bool>(&h, "return A:get().busy").await);
        assert_eq!(eval::<usize>(&h, "return #A:list_messages().data").await, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn idle_admission_owned_turn_can_be_cancelled_and_re_admitted_after_settle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([{ "text": "slow ".repeat(1000), "delay_ms": 10 }, { "text": "recovered" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            ENDS = {}; STREAM = false
            C.cordis:on("item/delta", function() STREAM = true end)
            C.cordis:on("turn/completed", function(e)
                ENDS[#ENDS + 1] = e
                if #ENDS == 1 then
                    local i = S:info()
                    local ok_ca, err_ca = pcall(A.prompt, A, 'cancelled successor', {when_busy="reject", expected_session={session_id=i.session_id, session_epoch=i.session_epoch}})
                    CANCELLED_ADMISSION = (not ok_ca and type(err_ca) == "table" and err_ca.code) or "unexpected"
                end
            end)
            local info = S:info()
            START = A:prompt("run", {when_busy="reject", expected_session={session_id=info.session_id, session_epoch=info.session_epoch}})
        "#,
        )
        .await;
        wait_until(10, "stream", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STREAM").await }
        })
        .await;
        exec(&h, r#"A:cancel({turn_id="wrong"})"#).await;
        assert!(eval::<bool>(&h, "return A:get().busy").await);
        exec(&h, r#"A:cancel({turn_id=START.turn_id})"#).await;
        wait_until(10, "cancelled run", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ENDS >= 1").await }
        })
        .await;
        assert!(eval::<bool>(&h, "return ENDS[1].turn_id == START.turn_id").await);
        assert_eq!(eval::<String>(&h, "return CANCELLED_ADMISSION").await, "agent.rejected");

        let next_status: String = eval(
            &h,
            r#"
            local i = S:info()
            NEXT = A:prompt("after cancel", {when_busy="reject", expected_session={session_id=i.session_id, session_epoch=i.session_epoch}})
            return NEXT.status
        "#,
        )
        .await;
        assert_eq!(next_status, "started");
        wait_until(10, "recovered run", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ENDS == 2").await }
        })
        .await;
        assert_eq!(eval::<String>(&h, "return ENDS[2].text").await, "recovered");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn idle_admission_rejects_identity_after_session_replacement() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "unused" }])).no_session().build().await;
        expose(&h);
        exec(&h, "local i=S:info(); OLD={session_id=i.session_id,session_epoch=i.session_epoch}; S:create()").await;
        let code: String = eval(
            &h,
            "local ok, err = pcall(A.prompt, A, 'old work',{when_busy='reject', expected_session=OLD}); return (not ok and type(err) == 'table' and err.code) or 'unexpected'",
        )
        .await;
        assert_eq!(code, "stale_session");
        assert!(!eval::<bool>(&h, "return A:get().busy").await);
        h.shutdown().await;
    })
    .await
}

#[test]
fn rpc_idle_admission_has_matching_identity_and_busy_results() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "slow ".repeat(1000), "delay_ms": 10 }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    assert!(init["result"]["capabilities"]["methods"].as_array().unwrap().iter().any(|m| m == "agent.prompt"));
    let info = rpc.call_raw("session.create", json!({}), |_| json!(null));
    let owner = &info["result"];
    assert!(owner.get("generation").is_none(), "{info}");
    let sid = owner["session_id"].as_str().unwrap();
    let sub = rpc.call_raw("events.subscribe", json!({ "session_id": sid, "include": ["turn/*"] }), |_| json!(null));
    let epoch = sub["result"]["session_epoch"].as_str().expect("events.subscribe returns session_epoch");
    let stale = rpc.call_raw(
        "agent.prompt",
        json!({
            "session_id": sid,
            "input": "old",
            "when_busy": "reject",
            "expected_epoch": "old"
        }),
        |_| json!(null),
    );
    assert_eq!(
        (stale["error"]["code"].as_i64(), stale["error"]["data"]["code"].as_str()),
        (Some(-32000), Some("stale_session")),
        "{stale}"
    );
    let invalid_opt = rpc.call_raw(
        "agent.prompt",
        json!({
            "session_id": sid,
            "input": "bad opt",
            "when_busy": "reject",
            "expected_epoch": epoch,
            "turn_id": "forged"
        }),
        |_| json!(null),
    );
    assert_eq!(invalid_opt["error"]["code"], -32602, "{invalid_opt}");
    let params = json!({
        "session_id": sid,
        "input": "start",
        "when_busy": "reject",
        "expected_epoch": epoch,
        "source": "rpc-test",
        "system": "be brief"
    });
    let started = rpc.call_raw("agent.prompt", params.clone(), |_| json!(null));
    assert_eq!(started["result"]["status"], "started", "{started}");
    assert!(started["result"]["turn_id"].is_string(), "{started}");
    let busy = rpc.call_raw("agent.prompt", params, |_| json!(null));
    assert_eq!(
        (busy["error"]["code"].as_i64(), busy["error"]["data"]["code"].as_str()),
        (Some(-32000), Some("busy")),
        "{busy}"
    );
    rpc.call_raw("agent.cancel", json!({ "session_id": sid }), |_| json!(null));
    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[tokio::test]
async fn idle_admission_freezes_images_before_async_execution() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "accepted image" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            IMAGE_END=nil; C.cordis:on("turn/completed",function(e) IMAGE_END=e end)
            local i=S:info()
            local images={{type="image",data="original",mime="image/png"}}
            local result=A:prompt("image input",{when_busy="reject", expected_session={session_id=i.session_id,session_epoch=i.session_epoch}, images=images})
            assert(result.status == "started")
            images[1].data="mutated"
        "#,
        )
        .await;
        wait_until(10, "image run", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return IMAGE_END ~= nil").await }
        })
        .await;
        let data: String = eval(&h, "return A:list_messages().data[1].content[2].data").await;
        assert_eq!(data, "original");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn idle_admission_refuses_readonly_and_poisoned_sessions() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("locked-admission.jsonl", &[msg("m1", None, "user", "hello")]);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().faux(json!([{ "text": "unused" }])).build().await;
        expose(&h);
        global(&h, "LOCKED_PATH", path.to_string_lossy().to_string());
        let code: String = eval(
            &h,
            r#"
            mare.ui.connected=function() return true end
            mare.ui.notify=function() end
            mare.ui.set_editor=function() end
            mare.ui.confirm=function() return true end
            mare.ui.select=function() return 2 end
            S:load({ path = LOCKED_PATH })
            local i=S:info(); assert(i.readonly)
            local ok_r, err_r = pcall(A.prompt, A, "should reject",{when_busy="reject", expected_session={session_id=i.session_id,session_epoch=i.session_epoch}})
            return (not ok_r and type(err_r) == "table" and err_r.code) or "unexpected"
        "#,
        )
        .await;
        assert_eq!(code, "agent.rejected");
        assert!(!eval::<bool>(&h, "return A:get().busy").await);
        drop(lock);

        let poisoned_json: String = eval(
            &h,
            r#"
            S:create()
            local i = S:info()
            mare.task.fail_session("disk full")
            local ok, res = pcall(A.prompt, A, "after poison", {when_busy="reject", expected_session={session_id=i.session_id, session_epoch=i.session_epoch}})
            return mare.json.encode({ ok = ok, error = tostring(res) })
        "#,
        )
        .await;
        let poisoned: serde_json::Value = serde_json::from_str(&poisoned_json).unwrap();
        assert_eq!(poisoned["ok"], false, "{poisoned}");
        assert!(poisoned["error"].as_str().unwrap_or("").contains("disk full"), "{poisoned}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn idle_admission_rejects_unsupported_media_in_images_and_content() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "unused" }])).no_session().build().await;
        expose(&h);
        let reply_json: String = eval(
            &h,
            r#"
            local i = S:info()
            local owner = { session_id = i.session_id, session_epoch = i.session_epoch }
            local ok_vid_img, err_vid_img = pcall(A.prompt, A, "watch", {
                when_busy = "reject",
                expected_session = owner,
                images = { { type = "video", mime = "video/mp4", data = "AAAA" } },
            })
            local ok_vid_content, err_vid_content = pcall(A.prompt, A, "watch", {
                when_busy = "reject",
                expected_session = owner,
                content = { { type = "video", mime = "video/mp4", data = "AAAA" } },
            })
            local ok_aud_img, err_aud_img = pcall(A.prompt, A, "listen", {
                when_busy = "reject",
                expected_session = owner,
                images = { { type = "audio", mime = "audio/wav", data = "AAAA" } },
            })
            return mare.json.encode({
                ok_vid_img = ok_vid_img, err_vid_img = tostring(err_vid_img),
                ok_vid_content = ok_vid_content, err_vid_content = tostring(err_vid_content),
                ok_aud_img = ok_aud_img, err_aud_img = tostring(err_aud_img),
            })
        "#,
        )
        .await;
        let reply: serde_json::Value = serde_json::from_str(&reply_json).unwrap();
        assert_eq!(reply["ok_vid_img"], false, "{reply}");
        assert!(reply["err_vid_img"].as_str().unwrap_or("").contains("video"), "{reply}");
        assert_eq!(reply["ok_vid_content"], false, "{reply}");
        assert!(reply["err_vid_content"].as_str().unwrap_or("").contains("video"), "{reply}");
        assert_eq!(reply["ok_aud_img"], false, "{reply}");
        assert!(reply["err_aud_img"].as_str().unwrap_or("").contains("audio"), "{reply}");
        h.shutdown().await;
    })
    .await
}

/// Reference: manual compaction must be rejected
/// while an agent turn is busy, and agent turns (prompt / try_prompt / continue) must be
/// rejected/blocked while compaction is running.
#[tokio::test]
async fn compaction_and_agent_turn_are_mutually_exclusive() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "keep_recent_tokens": 1, "stages": ["local"] } } });
        let h = env
            .host().builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "slow turn ".repeat(1000), "delay_ms": 15 },
                { "text": "unused summary" }
            ]))
            .config(cfg.clone())
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());

        // (a) Compaction rejected while an agent turn is busy.
        exec(
            &h,
            r#"
            local msgs = {}
            for i = 1, 4 do
                msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = ("OLD-%d "):format(i) .. string.rep("words ", 200) } } }
                msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "ack" } } }
            end
            A:set_messages(msgs)
            STREAM = false
            ENDS = {}
            C.cordis:on("item/delta", function() STREAM = true end)
            C.cordis:on("turn/completed", function(e) ENDS[#ENDS + 1] = e end)
            local info = S:info()
            OWNER = { session_id = info.session_id, session_epoch = info.session_epoch }
            START = A:prompt("busy turn", {when_busy="reject", expected_session=OWNER})
            assert(START.status == "started")
        "#,
        )
        .await;
        wait_until(10, "agent stream", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STREAM").await }
        })
        .await;
        let (compact_ok, compact_err): (bool, String) = eval(
            &h,
            r#"
            local ok, res = pcall(K.run, K)
            return ok and res.status == "compacted", tostring(res)
        "#,
        )
        .await;
        assert!(!compact_ok, "compaction must be rejected while agent turn is busy, got ok=true");
        assert!(!compact_err.is_empty(), "expected rejection reason, got: {compact_err}");
        exec(&h, "A:cancel()").await;
        wait_until(10, "aborted turn end", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ENDS >= 1").await }
        })
        .await;
        h.shutdown().await;
        drop(env);

        // (b) Agent turn rejected/blocked while compaction is running.
        let env2 = TestEnv::new().await;
        let h2 = env2
            .host().builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "slow summary ".repeat(150), "delay_ms": 15 },
                { "text": "must not run during compact" }
            ]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h2);
        global(&h2, "K", h2.service_table("compact").unwrap());
        exec(
            &h2,
            r#"
            local msgs = {}
            for i = 1, 4 do
                msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = ("OLD-%d "):format(i) .. string.rep("words ", 200) } } }
                msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "ack" } } }
            end
            A:set_messages(msgs)
            COMPACT_STARTED = false
            COMPACT_ENDS = {}
            C.cordis:on("compact/started", function() COMPACT_STARTED = true end)
            C.cordis:on("compact/completed", function(e) COMPACT_ENDS[#COMPACT_ENDS + 1] = e end)
            mare.task.spawn(function() K:run() end)
        "#,
        )
        .await;
        wait_until(10, "compaction started", || {
            let h2 = h2.clone();
            async move { eval::<bool>(&h2, "return COMPACT_STARTED and K:busy()").await }
        })
        .await;
        let blocked_json: String = eval(
            &h2,
            r#"
            local info = S:info()
            local owner = { session_id = info.session_id, session_epoch = info.session_epoch }
            local ok_try, err_try = pcall(A.prompt, A, "during compact", {when_busy="reject", expected_session=owner})
            local ok_prompt, err_prompt = pcall(A.prompt, A, "during compact")
            local ok_cont, err_cont = pcall(A.continue, A)
            return mare.json.encode({
                ok_try = ok_try,
                try_code = type(err_try) == "table" and err_try.code or tostring(err_try),
                ok_prompt = ok_prompt,
                err_prompt = tostring(err_prompt),
                ok_cont = ok_cont,
                cont_code = type(err_cont) == "table" and err_cont.code or tostring(err_cont),
                agent_busy = A:get().busy,
            })
        "#,
        )
        .await;
        let blocked: serde_json::Value = serde_json::from_str(&blocked_json).unwrap();
        assert_eq!(blocked["ok_try"], false, "{blocked}");
        assert_eq!(blocked["try_code"], "busy", "when_busy=reject must return busy during compaction: {blocked}");
        assert_eq!(blocked["ok_prompt"], false, "prompt must be rejected during compaction: {blocked}");
        assert!(blocked["err_prompt"].as_str().unwrap_or("").contains("compaction"), "{blocked}");
        assert_eq!(blocked["ok_cont"], false, "{blocked}");
        assert_eq!(
            blocked["cont_code"], "busy",
            "continue must report busy during compaction: {blocked}"
        );
        assert_eq!(blocked["agent_busy"], false, "{blocked}");

        wait_until(10, "compaction finished", || {
            let h2 = h2.clone();
            async move { eval::<bool>(&h2, "return #COMPACT_ENDS == 1").await }
        })
        .await;
        h2.shutdown().await;
    })
    .await
}

/// Reference: public `agent:abort()` must also cancel
/// an in-progress compaction.
#[tokio::test]
async fn agent_abort_cancels_in_progress_compaction() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "keep_recent_tokens": 1, "stages": ["local"] } } });
        let h = env
            .host().builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "slow summary ".repeat(1000), "delay_ms": 20 }
            ]))
            .config(cfg)
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"
            local msgs = {}
            for i = 1, 4 do
                msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = ("OLD-%d "):format(i) .. string.rep("words ", 200) } } }
                msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "ack" } } }
            end
            A:set_messages(msgs)
            COMPACT_STARTED = false
            COMPACT_ENDS = {}
            C.cordis:on("compact/started", function() COMPACT_STARTED = true end)
            C.cordis:on("compact/completed", function(e) COMPACT_ENDS[#COMPACT_ENDS + 1] = e end)
            mare.task.spawn(function()
                local ok, res = pcall(K.run, K)
                COMPACT_OK = ok and res.status == "compacted"
                COMPACT_ERR = not ok and tostring(res) or nil
                COMPACT_KIND = (not ok and type(res) == "table" and res.data) and res.data.kind or nil
            end)
        "#,
        )
        .await;
        wait_until(10, "compaction started", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return COMPACT_STARTED and K:busy()").await }
        })
        .await;
        exec(&h, "A:cancel()").await;
        wait_until(10, "compaction cancelled", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #COMPACT_ENDS == 1 and not K:busy()").await }
        })
        .await;
        let (ok, kind, cancelled): (bool, Option<String>, bool) = eval(
            &h,
            "return COMPACT_OK == true, COMPACT_KIND, COMPACT_ENDS[1].cancelled == true",
        )
        .await;
        assert!(!ok, "cancelled compaction must not succeed");
        assert_eq!(kind.as_deref(), Some("cancelled"));
        assert!(cancelled, "compact/completed must report cancelled=true");
        h.shutdown().await;
    })
    .await
}

/// RPC `compact`, `prompt`, `prompt.try`, and `abort` enforce mutual exclusion and
/// compaction cancellation.
#[test]
fn rpc_compact_and_prompt_are_mutually_exclusive_and_abort_cancels_compact() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        r#"
        mare.setup {
            plugins = {
                compact = { auto = false, keep_recent_tokens = 1, recent_user_tokens = 0, stages = { "local" } }
            }
        }
        "#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            { "text": "first ".repeat(400) },
            { "text": "second ".repeat(400) },
            { "text": "third ".repeat(400) },
            { "text": "slow turn ".repeat(5000), "delay_ms": 20 },
            { "text": "slow summary ".repeat(5000), "delay_ms": 20 }
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    // writes through RpcClient; reads through a bounded reader thread, and the child is killed if
    // the test ends early (panics included)
    let mut lines = TimedLines::spawn(child.0.stdout.take().unwrap());
    let mut rpc = RpcClient::new(std::io::BufReader::new(std::io::empty()), child.0.stdin.take().unwrap());
    // Every message is kept until asked for: a reply or event that arrives while another reply is
    // awaited is never dropped (b11 ADMIT #2: `call`/`wait` discarded them and the test could hang).
    let mut inbox = Inbox::default();
    let id = rpc.request("initialize", json!({"protocol_version": "3.0"}));
    inbox.reply(&mut lines, id);
    let id = rpc.request("session.create", json!({}));
    let info = inbox.reply(&mut lines, id);
    let owner = info["result"].clone();
    let sid = owner["session_id"].as_str().unwrap().to_string();
    let sub_id = rpc.request(
        "events.subscribe",
        json!({ "session_id": sid, "include": ["item/delta", "turn/completed", "compact/started", "compact/completed"] }),
    );
    let sub_reply = inbox.reply(&mut lines, sub_id);
    let epoch = sub_reply["result"]["session_epoch"].as_str().unwrap().to_string();

    // Build enough history for compaction.
    for text in ["msg 1", "msg 2", "msg 3"] {
        let id = rpc.request("agent.prompt", json!({ "session_id": sid, "input": text }));
        let st = inbox.reply(&mut lines, id);
        let wid = rpc.request("agent.wait", json!({ "session_id": sid, "turn_id": st["result"]["turn_id"] }));
        inbox.reply(&mut lines, wid);
    }
    inbox.clear_events();

    // (a) A slow turn (long enough that it cannot end on its own here), then compact -> rejected.
    let id = rpc.request("agent.prompt", json!({ "session_id": sid, "input": "slow" }));
    let started = inbox.reply(&mut lines, id);
    assert_eq!(started["result"]["status"], "started", "{started}");
    inbox.event(&mut lines, "item/delta");
    let id = rpc.request("compact.run", json!({ "session_id": owner["session_id"] }));
    let compact_while_busy = inbox.reply(&mut lines, id);
    assert_eq!(
        compact_while_busy["error"]["data"]["code"], "compact.not_compacted",
        "RPC compact.run must be rejected while agent turn is busy: {compact_while_busy}"
    );
    let id = rpc.request("agent.cancel", json!({ "session_id": sid }));
    inbox.reply(&mut lines, id);
    inbox.event(&mut lines, "turn/completed");

    // (b) & (c) A slow compaction: prompt is rejected, when_busy=reject reports busy, agent.cancel cancels it.
    let compact_id = rpc.request("compact.run", json!({ "session_id": owner["session_id"] }));
    inbox.event(&mut lines, "compact/started");
    assert!(inbox.take_reply(compact_id).is_none(), "compact finished before its abort");
    let id = rpc.request("agent.prompt", json!({ "session_id": sid, "input": "during compact" }));
    let prompt_during_compact = inbox.reply(&mut lines, id);
    assert!(
        prompt_during_compact.get("error").is_some(),
        "RPC prompt must be rejected while compaction is running: {prompt_during_compact}"
    );
    assert_eq!(
        (prompt_during_compact["error"]["code"].as_i64(), prompt_during_compact["error"]["data"]["code"].as_str()),
        (Some(-32000), Some("busy")),
        "{prompt_during_compact}"
    );
    let id = rpc.request(
        "agent.prompt",
        json!({
            "session_id": sid,
            "input": "during compact",
            "when_busy": "reject",
            "expected_epoch": epoch
        }),
    );
    let try_during_compact = inbox.reply(&mut lines, id);
    assert_eq!(
        (try_during_compact["error"]["code"].as_i64(), try_during_compact["error"]["data"]["code"].as_str()),
        (Some(-32000), Some("busy")),
        "RPC agent.prompt when_busy=reject must report busy while compaction is running: {try_during_compact}"
    );
    let abort_id = rpc.request("agent.cancel", json!({ "session_id": sid }));
    // either reply may come first; both are kept
    let abort_res = inbox.reply(&mut lines, abort_id);
    assert!(abort_res.get("error").is_none(), "{abort_res}");
    let compact_res = inbox.reply(&mut lines, compact_id);
    assert_eq!(
        compact_res["error"]["data"]["code"], "compact.not_compacted",
        "aborted compaction must fail with compact.not_compacted: {compact_res}"
    );

    let id = rpc.request("shutdown", json!({}));
    inbox.reply(&mut lines, id);
    let status = child.wait_timeout(std::time::Duration::from_secs(30));
    assert!(status.success(), "child failed: {status}");
}
