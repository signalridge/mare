//! The agent loop against the real wires (`ScriptedModel`): what an interrupted turn leaves in
//! the history mare sends next, checked on the captured request per wire.

use crate::common::scripted::{Part, ScriptedModel, Step, Wire, tool_rounds};
use crate::common::*;
use serde_json::{Value, json};

fn three_calls() -> Vec<Part> {
    vec![
        Part::ToolCall { id: "call_1".into(), name: "slow_probe".into(), args: json!({}) },
        Part::ToolCall { id: "call_2".into(), name: "mark_probe".into(), args: json!({ "n": 2 }) },
        Part::ToolCall { id: "call_3".into(), name: "mark_probe".into(), args: json!({ "n": 3 }) },
    ]
}

async fn register_probes(h: &mare_host::Host) {
    exec(
        h,
        r#"STARTED, RAN = false, {}
           T:register("slow_probe", { description = "slow", parameters = { type = "object" },
             execute = function() STARTED = true; mare.task.sleep(60000); return "slow done" end })
           T:register("mark_probe", { description = "mark",
             parameters = { type = "object", properties = { n = { type = "number" } } },
             execute = function(args) RAN[#RAN + 1] = args.n or "no n"; return "marked" end })"#,
    )
    .await;
}

/// Positive control for the probes above: a mark_probe call that does run records its `n`.
#[tokio::test]
async fn mark_probe_records_a_call_that_runs() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Responses,
            vec![
                Step::Reply(vec![Part::ToolCall {
                    id: "call_m".into(),
                    name: "mark_probe".into(),
                    args: json!({ "n": 7 }),
                }]),
                Step::Reply(vec![Part::Text("marked it".into())]),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        register_probes(&h).await;
        let res = h.prompt("mark", json!({})).await.unwrap();
        assert_eq!(res["text"], "marked it", "{res}");
        let ran: Vec<i64> = eval(&h, "return RAN").await;
        h.shutdown().await;
        assert_eq!(ran, [7], "the probe did not record the call's argument");
        mock.finish();
    })
    .await
}

/// Agent-loop corpus: three calls in one turn, the user aborts while the first runs.
async fn abort_during_the_first_of_three_calls_answers_each_call_once(wire: Wire) {
    local(async {
        let mut env = TestEnv::new().await;
        let mock =
            ScriptedModel::start(wire, vec![Step::Reply(three_calls()), Step::Reply(vec![Part::Text("fine".into())])])
                .await;
        let h = mock.host(&mut env, false, &[]).await;
        register_probes(&h).await;
        let run = {
            let h = h.clone();
            tokio::task::spawn_local(async move { h.prompt("run three", json!({})).await.unwrap() })
        };
        wait_until(60, "the first call to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STARTED").await }
        })
        .await;
        h.abort().await.unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();
        assert_eq!(res["error"]["code"], "aborted", "{wire}: {res}");
        let next = h.prompt("and now?", json!({})).await.unwrap();
        assert_eq!(next["text"], "fine", "{wire}: {next}");
        let ran: Vec<i64> = eval(&h, "return RAN").await;
        h.shutdown().await;
        assert!(ran.is_empty(), "{wire}: calls that never started ran: {ran:?}");

        let sent = mock.requests();
        assert_eq!(sent.len(), 2, "{wire}: {sent:?}");
        let body = sent[1].json();
        let rounds = tool_rounds(wire, &body);
        let names: Vec<&str> = rounds.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, ["slow_probe", "mark_probe", "mark_probe"], "{wire}: {rounds:?}");
        assert!(rounds[0].2.contains("cancelled"), "{wire}: the running call's result: {:?}", rounds[0].2);
        for (name, _, text) in &rounds[1..] {
            assert!(text.contains("Aborted"), "{wire}: {name}'s result does not say it was aborted: {text:?}");
        }
        mock.finish();
    })
    .await
}

/// Agent-loop corpus: two steers arrive while a tool runs.
async fn two_steers_during_a_tool_follow_its_result_in_order(wire: Wire) {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            wire,
            vec![
                Step::Reply(vec![Part::ToolCall { id: "call_g".into(), name: "gate_probe".into(), args: json!({}) }]),
                Step::Reply(vec![Part::Text("steered".into())]),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        exec(
            &h,
            r#"STARTED, RELEASE = false, false
               T:register("gate_probe", { description = "gate", parameters = { type = "object" },
                 execute = function()
                   STARTED = true
                   while not RELEASE do mare.task.sleep(5) end
                   return "GATE-RESULT"
                 end })"#,
        )
        .await;
        let run = {
            let h = h.clone();
            tokio::task::spawn_local(async move { h.prompt("start", json!({})).await.unwrap() })
        };
        wait_until(60, "the tool to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STARTED").await }
        })
        .await;
        h.steer("FIRST-STEER").await.unwrap();
        h.steer("SECOND-STEER").await.unwrap();
        exec(&h, "RELEASE = true").await;
        let res = tokio::time::timeout(std::time::Duration::from_secs(30), run).await.unwrap().unwrap();
        assert_eq!(res["text"], "steered", "{wire}: {res}");
        h.shutdown().await;

        let sent = mock.requests();
        assert_eq!(sent.len(), 2, "{wire}: {sent:?}");
        let text = sent[1].json().to_string();
        let at = |needle: &str| {
            let hits: Vec<usize> = text.match_indices(needle).map(|(i, _)| i).collect();
            assert_eq!(hits.len(), 1, "{wire}: {needle} must be in the request exactly once: {text}");
            hits[0]
        };
        let (result, first, second) = (at("GATE-RESULT"), at("FIRST-STEER"), at("SECOND-STEER"));
        assert!(
            result < first && first < second,
            "{wire}: want tool result < first steer < second steer, got {result} / {first} / {second}: {text}"
        );
        mock.finish();
    })
    .await
}

/// Agent-loop corpus: the user aborts while a tool runs, then follows up and resumes.
async fn abort_during_a_tool_then_follow_up_carries_its_output_and_one_marker(wire: Wire) {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            wire,
            vec![
                Step::Reply(vec![Part::ToolCall { id: "call_s".into(), name: "slow_probe".into(), args: json!({}) }]),
                Step::Reply(vec![Part::Text("after".into())]),
                Step::Reply(vec![Part::Text("third answer".into())]),
            ],
        )
        .await;
        const MARKER: &str = "<turn_aborted>";
        let check = |body: &Value, what: &str| {
            let rounds = tool_rounds(wire, body);
            assert_eq!(rounds.len(), 1, "{wire} {what}: {rounds:?}");
            assert_eq!(rounds[0].0, "slow_probe", "{wire} {what}");
            assert!(rounds[0].2.contains("cancelled"), "{wire} {what}: the output must say so: {:?}", rounds[0].2);
            let text = body.to_string();
            assert_eq!(text.matches(MARKER).count(), 1, "{wire} {what}: the marker exactly once: {text}");
            assert_eq!(text.matches("and now?").count(), 1, "{wire} {what}: {text}");
            let (result, marker, follow) =
                (text.find(&rounds[0].2).unwrap(), text.find(MARKER).unwrap(), text.find("and now?").unwrap());
            assert!(result < marker && marker < follow, "{wire} {what}: output < marker < follow-up: {text}");
            rounds
        };

        let h = mock.host(&mut env, false, &[]).await;
        register_probes(&h).await;
        let run = {
            let h = h.clone();
            tokio::task::spawn_local(async move { h.prompt("go", json!({})).await.unwrap() })
        };
        wait_until(60, "the tool to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STARTED").await }
        })
        .await;
        h.abort().await.unwrap();
        h.abort().await.unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();
        assert_eq!(res["error"]["code"], "aborted", "{wire}: {res}");
        let next = h.prompt("and now?", json!({})).await.unwrap();
        assert_eq!(next["text"], "after", "{wire}: {next}");
        h.shutdown().await;

        let sent = mock.requests();
        assert_eq!(sent.len(), 2, "{wire}: {sent:?}");
        let before = check(&sent[1].json(), "follow-up");

        let h = mock.host(&mut env, true, &[]).await;
        register_probes(&h).await;
        let third = h.prompt("third", json!({})).await.unwrap();
        assert_eq!(third["text"], "third answer", "{wire}: {third}");
        h.shutdown().await;

        let all_reqs = mock.requests();
        assert_eq!(all_reqs.len(), 3, "{wire}: {all_reqs:?}");
        let after = check(&all_reqs[2].json(), "after --continue");
        assert_eq!(before, after, "{wire}: the resumed history changed the round");
        assert!(all_reqs[2].json().to_string().contains("after"), "{wire}: the follow-up answer is in the history");
        mock.finish();
    })
    .await
}

macro_rules! per_wire {
    ($scenario:ident: $($fn_name:ident => $wire:expr),+) => {
        mod $scenario {
            $(
                #[tokio::test]
                async fn $fn_name() {
                    super::$scenario($wire).await
                }
            )+
        }
    };
}

/// Agent-loop corpus, its last clause: the stream is cut after a complete tool call but
/// before the response ends. The retried answer is the turn's answer and the tool call of the cut
/// attempt never runs (it was never committed). reference implementation;
/// ("失败那次的工具调用不执行").
async fn a_tool_call_in_a_cut_attempt_is_not_run_and_the_retry_answers(wire: Wire) {
    local(async {
        let mut env = TestEnv::new().await;
        // events before the response's terminal ones: the call is fully streamed, the end is not
        let before_end = match wire {
            Wire::Responses => 5, // created, item added, arguments delta/done, item done
            Wire::Anthropic => 4, // message_start, block start/delta/stop
            Wire::Compat => 2,    // role chunk, the tool_calls chunk
            Wire::Gemini => 1,    // the functionCall chunk
        };
        let call = Part::ToolCall { id: "call_cut".into(), name: "mark_probe".into(), args: json!({ "n": 5 }) };
        let mock = ScriptedModel::start(
            wire,
            vec![
                Step::CutAfter { parts: vec![call], events: before_end },
                Step::Reply(vec![Part::Text("answered after the retry".into())]),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        register_probes(&h).await;
        exec(&h, r#"RETRIES = 0; C.cordis:on("step/completed", function(e) if e.will_retry then RETRIES = RETRIES + 1 end end)"#).await;
        let res = h.prompt("go", json!({})).await.unwrap();
        let ran: Vec<i64> = eval(&h, "return RAN").await;
        let retries: i64 = eval(&h, "return RETRIES").await;
        h.shutdown().await;
        assert!(ran.is_empty(), "{wire}: the cut attempt's tool call ran: {ran:?}");
        assert_eq!(retries, 1, "{wire}: the cut stream is retried once: {res}");
        assert_eq!(res["text"], "answered after the retry", "{wire}: {res}");
        let sent = mock.requests();
        assert!(
            tool_rounds(wire, &sent[1].json()).is_empty(),
            "{wire}: the retry carries a round of the cut attempt: {}",
            sent[1].body
        );
        mock.finish();
    })
    .await
}

per_wire!(
    abort_during_the_first_of_three_calls_answers_each_call_once:
    responses => super::Wire::Responses,
    anthropic => super::Wire::Anthropic
);
per_wire!(
    two_steers_during_a_tool_follow_its_result_in_order:
    responses => super::Wire::Responses,
    anthropic => super::Wire::Anthropic
);
per_wire!(
    abort_during_a_tool_then_follow_up_carries_its_output_and_one_marker:
    responses => super::Wire::Responses,
    anthropic => super::Wire::Anthropic
);
per_wire!(
    a_tool_call_in_a_cut_attempt_is_not_run_and_the_retry_answers:
    responses => super::Wire::Responses,
    anthropic => super::Wire::Anthropic,
    compat => super::Wire::Compat,
    gemini => super::Wire::Gemini
);

#[tokio::test]
async fn wire_prompt_rejects_every_lua_only_field() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().faux(json!([{ "text": "ok" }])).build().await;
        let caller = h.kernel.host_owner().unwrap();

        let prompt_lua_fields = [
            ("turn_id", json!("forged-turn")),
            ("messages", json!([])),
            ("history_model", json!("faux/echo")),
            ("history_effort", json!("high")),
            ("images", json!([])),
            ("image_markers", json!([])),
            ("queued", json!(true)),
            ("queue_only", json!(true)),
            ("custom_message", json!({})),
            ("transition_id", json!("tr-1")),
            ("input_context", json!({ "k": 1 })),
            ("peer_message", json!({})),
            ("content", json!([])),
            ("internal_notification", json!(true)),
            ("_internal_notification", json!(true)),
        ];
        for (field, val) in prompt_lua_fields {
            let mut args = json!({ "input": "hello" });
            args[field] = val;
            let err = h
                .kernel
                .call_service_export(caller, None, "agent", "prompt", args, Default::default())
                .await
                .expect_err(&format!("agent.prompt must reject wire field `{field}`"));
            assert_eq!(err.code(), "invalid_arguments", "agent.prompt.{field}: {err}");
        }

        for (field, val) in [
            ("images", json!([])),
            ("image_markers", json!([])),
            ("custom_message", json!({})),
            ("input_context", json!({ "k": 1 })),
            ("peer_message", json!({})),
            ("content", json!([])),
        ] {
            let mut args = json!({ "input": "steer" });
            args[field] = val;
            let err = h
                .kernel
                .call_service_export(caller, None, "agent", "steer", args, Default::default())
                .await
                .expect_err(&format!("agent.steer must reject wire field `{field}`"));
            assert_eq!(err.code(), "invalid_arguments", "agent.steer.{field}: {err}");
        }

        for (field, val) in [
            ("turn_id", json!("forged-turn")),
            ("messages", json!([])),
            ("history_model", json!("faux/echo")),
            ("history_effort", json!("high")),
            ("queued", json!(true)),
            ("transition_id", json!("tr-1")),
        ] {
            let mut args = json!({});
            args[field] = val;
            let err = h
                .kernel
                .call_service_export(caller, None, "agent", "continue", args, Default::default())
                .await
                .expect_err(&format!("agent.continue must reject wire field `{field}`"));
            assert_eq!(err.code(), "invalid_arguments", "agent.continue.{field}: {err}");
        }

        for (field, val) in [("images", json!([])), ("image_markers", json!([]))] {
            let mut args = json!({ "input_id": "i-1", "input": "edited" });
            args[field] = val;
            let err = h
                .kernel
                .call_service_export(caller, None, "agent", "update_queued", args, Default::default())
                .await
                .expect_err(&format!("agent.update_queued must reject wire field `{field}`"));
            assert_eq!(err.code(), "invalid_arguments", "agent.update_queued.{field}: {err}");
        }

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn agent_prompt_and_steer_lua_fields_peer_message_boolean_and_no_internal_notification() {
    let lua = mlua::Lua::new();
    let contracts: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let agent: mlua::Table = contracts.get("agent").unwrap();
    let methods: mlua::Table = agent.get("methods").unwrap();
    for m in ["prompt", "steer"] {
        let spec: mlua::Table = methods.get(m).unwrap();
        let lua_fields: mlua::Table = spec.get("lua").unwrap();
        assert_eq!(
            lua_fields.get::<Option<String>>("peer_message").unwrap().as_deref(),
            Some("boolean"),
            "agent.{m}.lua.peer_message must be boolean"
        );
        assert!(
            lua_fields.get::<Option<String>>("internal_notification").unwrap().is_none(),
            "agent.{m}.lua must not declare dead internal_notification field"
        );
    }

    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ok" }])).no_session().build().await;
        expose(&h);
        let (peer_ok, notif_err_code): (bool, String) = eval(
            &h,
            r#"
            local ok_peer, res_peer = pcall(function()
                return A:prompt("peer hello", { source = "peer", peer_message = true, queue_only = true })
            end)
            local ok_notif, err_notif = pcall(function()
                return A:prompt("spoof", { internal_notification = true })
            end)
            return ok_peer and type(res_peer) == "table" and res_peer.status == "queued",
                ok_notif and "unexpected_ok" or tostring(type(err_notif) == "table" and err_notif.code or err_notif)
            "#,
        )
        .await;
        assert!(peer_ok, "Lua agent.prompt with peer_message = true must succeed");
        assert_eq!(notif_err_code, "invalid_arguments", "dead internal_notification field must be rejected");
        h.shutdown().await;
    })
    .await;
}
