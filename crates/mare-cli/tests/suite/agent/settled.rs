//! `agent/settled`: published once per run after every `turn/completed` listener has returned, with
//! `busy` telling whether another run (or an outer end dispatch) is still in progress.
//! Reference: pi `packages/agent/test/agent.test.ts:400` ("waitForIdle should wait for async
//! subscribers") — idle is reported only after asynchronous end subscribers finish.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn settled_follows_yielding_end_listeners_once_per_run() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "one" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"ORDER = {}
            C.cordis:on("turn/completed", function(ev)
              mare.task.yield()
              ORDER[#ORDER + 1] = "end-listener:" .. tostring(ev.turn_id)
            end)
            C.cordis:on("agent/settled", function(ev)
              ORDER[#ORDER + 1] = "settled:" .. tostring(ev.turn_id) .. ":" .. tostring(ev.busy)
            end)"#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let turn: String = res["turn_id"].as_str().expect("turn_id").to_string();
        let order: Vec<String> = eval(&h, "return ORDER").await;
        assert_eq!(order, vec![format!("end-listener:{turn}"), format!("settled:{turn}:false")]);
        let mut names = Vec::new();
        while let Ok(Some(rec)) = cursor.try_recv() {
            if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec {
                names.push(ev.name);
            }
        }
        assert!(names.iter().any(|n| n == "turn/completed"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "agent/settled"),
            "agent/settled is expose = \"process\" and must not leave the process via tap cursors: {names:?}"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn settled_reports_busy_while_an_outer_end_dispatch_is_still_running() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "one" }, { "text": "two" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"SETTLED = {}
            NESTED = false
            C.cordis:on("turn/completed", function()
              if not NESTED then
                NESTED = true
                -- an end listener that runs a whole follow-up prompt before returning
                local ag = C.cordis:get("agent")
                local s = ag:prompt("again")
                local r = s.turn_id and ag:wait(s.turn_id) or s
                assert(not r.error, tostring(r.error))
              end
            end)
            C.cordis:on("agent/settled", function(ev)
              SETTLED[#SETTLED + 1] = ev.busy
            end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        // inner run settles while the outer turn/completed dispatch is still in progress: busy;
        // the outer run settles last, idle.
        let settled: Vec<bool> = eval(&h, "return SETTLED").await;
        assert_eq!(settled, vec![true, false]);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn deferred_transition_waits_while_busy_at_end_and_runs_once_idle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "name": "defer_handoff", "arguments": {} }] },
                { "text": "one" },
                { "text": "two" }
            ]))
            .build()
            .await;
        expose(&h);
        let finished = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = finished.clone();
        global(
            &h,
            "SIGNAL_FINISHED",
            h.lua()
                .create_function(move |_, ()| {
                    signal.notify_one();
                    Ok(())
                })
                .unwrap(),
        );
        exec(
            &h,
            r#"FINISHED = nil
            BUSY_AT_END = nil
            FOLLOWED = false
            C.cordis:on("session/transition-completed", function(ev) FINISHED = ev; SIGNAL_FINISHED() end)
            C.cordis:get("tools"):register("defer_handoff", {
              description = "request a deferred clean-context handoff",
              side_effect = "read",
              parameters = { type = "object" },
              execute = function()
                local info = C.cordis:get("session"):info()
                SOURCE_ID = info.session_id
                local res = C.cordis:get("session"):transition({
                  mode = "new",
                  records = { { kind = "handoff/context", data = { plan = "p" }, scope = "session" } },
                }, { session_id = info.session_id, session_epoch = info.session_epoch, defer = true })
                assert(res.status == "deferred", tostring(res.status))
                return { content = "deferred" }
              end,
            })
            -- On the first run's end, start a follow-up run and hold it inside its turn/started, so
            -- the agent is busy when the transition's own turn/completed listener (lowest priority)
            -- looks; the test releases it after the first run has returned.
            local started_tx, started_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            RELEASE = release_tx
            C.cordis:on("turn/started", function(ev)
              if ev.text == "follow" then
                started_tx:send(true)
                release_rx:recv()
              end
            end)
            C.cordis:on("turn/completed", function()
              if not FOLLOWED then
                FOLLOWED = true
                local ag = C.cordis:get("agent")
                mare.task.spawn(function()
                  local s = ag:prompt("follow")
                  FOLLOW_RES = s.turn_id and ag:wait(s.turn_id) or s
                end)
                started_rx:recv()
                BUSY_AT_END = ag:get().busy
              end
            end)"#,
        )
        .await;
        let _ = h.prompt("start", json!({})).await;
        assert!(eval::<bool>(&h, "return BUSY_AT_END == true and FINISHED == nil").await);
        exec(&h, "RELEASE:send(true)").await;
        tokio::time::timeout(std::time::Duration::from_secs(10), finished.notified())
            .await
            .unwrap_or_else(|_| panic!("deferred transition never ran after the agent became idle"));
        let ok: bool = eval(
            &h,
            r#"return BUSY_AT_END == true and FINISHED ~= nil and FINISHED.mode == "new"
                and C.cordis:get("session"):info().session_id ~= SOURCE_ID"#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}
