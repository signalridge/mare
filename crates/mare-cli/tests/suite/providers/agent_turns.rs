//! Agent turns against `ScriptedModel`: one scenario, one test per wire, with capture-time
//! request invariant checking and `finish()` verification.

use super::*;
use crate::common::Request;
use crate::common::scripted::{Part, ScriptedModel, Step, Wire, render, tool_rounds, validate_request};

const ANSWER: &str = "The note says: hello from the note.";
const PARTIAL_CUT_FRAG: &str = "PARTIAL_CUT_FRAG";
const UNREACHED_CUT_TAIL: &str = " UNREACHED_CUT_TAIL";

/// at the agent level: the model reads a file, the request carrying the tool result has
/// its answer stream cut, the retry resends the same history (the tool round exactly once,
/// nothing of the cut answer), the turn ends with the answer once in the session, and
/// `--continue` sends the whole tool round back.
async fn tool_round_with_a_cut_answer_is_retried_saved_and_resumed(wire: Wire) {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("note.txt"), "hello from the note\n").unwrap();
        let cut_parts = vec![Part::Text(PARTIAL_CUT_FRAG.into()), Part::Text(UNREACHED_CUT_TAIL.into())];
        let cut_events = render(wire, &cut_parts)
            .iter()
            .position(|ev| ev.contains(PARTIAL_CUT_FRAG))
            .expect("cut stream must emit a text delta event")
            + 1;
        let cut_prefix = render(wire, &cut_parts)[..cut_events].concat();
        assert!(cut_prefix.contains(PARTIAL_CUT_FRAG), "{wire}: cut prefix missing partial fragment");
        assert!(!cut_prefix.contains(UNREACHED_CUT_TAIL), "{wire}: cut prefix should stop before tail");

        let mock = ScriptedModel::start(
            wire,
            vec![
                Step::Reply(vec![Part::ToolCall {
                    id: "call_r1".into(),
                    name: "read".into(),
                    args: json!({ "path": "note.txt" }),
                }]),
                Step::CutAfter { parts: cut_parts, events: cut_events },
                Step::Reply(vec![Part::Text(ANSWER.into())]),
                Step::Reply(vec![Part::Text("Still hello.".into())]),
            ],
        )
        .await;

        let h = mock.host(&mut env, false, &[]).await;
        exec(
            &h,
            r#"
                RETRIES = 0
                PRE_RETRY_DELTAS = {}
                C.cordis:on("item/delta", function(e)
                    if e.kind == "text" and RETRIES == 0 then
                        table.insert(PRE_RETRY_DELTAS, e.delta)
                    end
                end)
                C.cordis:on("step/completed", function(e) if e.will_retry then RETRIES = RETRIES + 1 end end)
            "#,
        )
        .await;
        let res = h.prompt("what does note.txt say?", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{wire}: {res}");
        assert_eq!(res["text"], ANSWER, "{wire}: {res}");
        assert_eq!(eval::<i64>(&h, "return RETRIES").await, 1, "{wire}: the cut answer is retried once");
        let pre_retry_streamed: String = eval(&h, "return table.concat(PRE_RETRY_DELTAS)").await;
        assert!(
            pre_retry_streamed.contains(PARTIAL_CUT_FRAG),
            "{wire}: host/stream did not observe partial text fragment before retry: {pre_retry_streamed:?}"
        );
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;

        let sent = mock.requests();
        let turns = mock.turns();
        assert_eq!(sent.len(), 3, "{wire}: tool call, cut answer, retried answer");
        pretty_assertions::assert_eq!(sent[1].json(), sent[2].json(), "{wire}: the retry changed the request");
        let rounds = tool_rounds(wire, &sent[2].json());
        assert_eq!(rounds.len(), 1, "{wire}: {rounds:?}");
        assert_eq!((rounds[0].0.as_str(), &rounds[0].1["path"]), ("read", &json!("note.txt")), "{wire}");
        assert!(rounds[0].2.contains("hello from the note"), "{wire}: {rounds:?}");
        assert!(!sent[2].body.contains(PARTIAL_CUT_FRAG), "{wire}: partial cut fragment leaked into retry request");
        assert!(
            !turns[2].text.iter().any(|t| t.contains(PARTIAL_CUT_FRAG)),
            "{wire}: partial cut fragment leaked into retry turns(): {:?}",
            turns[2].text
        );

        let records = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        assert!(
            !records.iter().any(|r| r.to_string().contains(PARTIAL_CUT_FRAG)),
            "{wire}: partial cut fragment leaked into persisted session history"
        );
        let answers: Vec<&Value> = records
            .iter()
            .filter(|r| r["type"] == "message" && r["message"]["role"] == "assistant")
            .flat_map(|r| r["message"]["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "text")
            .collect();
        assert_eq!(answers.len(), 1, "{wire}: {answers:?}");
        assert_eq!(answers[0]["text"], ANSWER, "{wire}");

        let h = mock.host(&mut env, true, &[]).await;
        let res = h.prompt("and now?", json!({})).await.unwrap();
        assert_eq!(res["text"], "Still hello.", "{wire}: {res}");
        h.shutdown().await;

        let all_reqs = mock.requests();
        assert_eq!(all_reqs.len(), 4, "{wire}");
        let resumed = &all_reqs[3];
        let rounds = tool_rounds(wire, &resumed.json());
        assert_eq!(rounds.len(), 1, "{wire}: the resumed request lost or repeated the tool round: {rounds:?}");
        assert_eq!((rounds[0].0.as_str(), &rounds[0].1["path"]), ("read", &json!("note.txt")), "{wire}");
        assert!(rounds[0].2.contains("hello from the note"), "{wire}: {rounds:?}");
        let body = resumed.json().to_string();
        assert!(!body.contains(PARTIAL_CUT_FRAG), "{wire}: partial cut fragment leaked into resumed request");
        for part in ["what does note.txt say?", ANSWER, "and now?"] {
            assert!(body.contains(part), "{wire}: the resumed request lacks {part:?}");
        }

        mock.finish();
    })
    .await
}

/// An empty response is retried by mare (transient in `llm/stream.lua`) and the second step answers.
async fn an_injected_empty_response_is_retried(wire: Wire) {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(wire, vec![Step::Empty, Step::Reply(vec![Part::Text("hello".into())])]).await;
        let h = mock.host(&mut env, false, &[]).await;
        let res = h.prompt("hi", json!({})).await.unwrap();
        assert_eq!(res["text"], "hello", "{wire}: {res}");
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 2, "{wire}");
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

per_wire!(
    tool_round_with_a_cut_answer_is_retried_saved_and_resumed:
    responses => super::Wire::Responses,
    anthropic => super::Wire::Anthropic,
    compat => super::Wire::Compat,
    gemini => super::Wire::Gemini
);
per_wire!(
    an_injected_empty_response_is_retried:
    responses => super::Wire::Responses,
    anthropic => super::Wire::Anthropic
);

// ---- helper guarantees ----------------------------------------------------------------------

#[test]
fn validate_request_checks_order_counts_and_exceptions() {
    let fails = |wire: Wire, body: Value, pending: &[String]| {
        let req =
            Request { method: "POST".into(), path: wire.path().to_string(), body: body.to_string(), headers: vec![] };
        !validate_request(wire, &req, pending, &Default::default()).is_empty()
    };
    let call = |id: &str| json!({ "type": "function_call", "call_id": id, "name": "read", "arguments": "{}" });
    let out = |id: &str| json!({ "type": "function_call_output", "call_id": id, "output": "x" });
    let asst = json!({ "type": "message", "role": "assistant", "content": [] });
    assert!(!fails(Wire::Responses, json!({ "input": [call("c1"), out("c1")] }), &[]));
    assert!(
        !fails(Wire::Responses, json!({ "input": [call("c1"), call("c2"), out("c2"), out("c1")] }), &[]),
        "parallel"
    );
    assert!(fails(Wire::Responses, json!({ "input": [out("c9")] }), &[]), "orphan");
    assert!(fails(Wire::Responses, json!({ "input": [call("c1"), out("c1"), out("c1")] }), &[]), "duplicate");
    assert!(fails(Wire::Responses, json!({ "input": [out("c1"), call("c1")] }), &[]), "result before call");
    assert!(fails(Wire::Responses, json!({ "input": [call("c1")] }), &[]), "unanswered");
    assert!(!fails(Wire::Responses, json!({ "input": [call("c1")] }), &["c1".into()]), "left open on purpose");
    assert!(
        fails(Wire::Responses, json!({ "input": [call("c1"), out("c1"), out("c1")] }), &["c1".into()]),
        "pending != duplicate"
    );
    assert!(fails(Wire::Responses, json!({ "input": [out("c1")] }), &["c1".into()]), "pending != orphan");
    assert!(fails(Wire::Responses, json!({ "input": [call("c1"), asst, out("c1")] }), &[]), "turn over an open call");
    assert!(fails(Wire::Responses, json!({ "input": [call(""), out("")] }), &[]), "empty id");
    assert!(
        fails(Wire::Responses, json!({ "input": [call("c1"), call("c1"), out("c1"), out("c1")] }), &[]),
        "id twice"
    );
    assert!(
        fails(Wire::Responses, json!({ "input": [call("c1"), out("c1"), call("c1"), out("c1")] }), &[]),
        "id reused"
    );
    assert!(fails(Wire::Responses, json!({ "input": [call("c1"), call("c1")] }), &["c1".into()]), "pending id twice");

    let use_ = |id: &str| json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": id, "name": "read", "input": {} }] });
    let res =
        |id: &str| json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": "x" }] });
    assert!(!fails(Wire::Anthropic, json!({ "max_tokens": 100, "messages": [use_("t1"), res("t1")] }), &[]));
    assert!(
        fails(Wire::Anthropic, json!({ "max_tokens": 100, "messages": [res("t1"), use_("t1")] }), &[]),
        "anthropic: early result"
    );
    assert!(
        fails(
            Wire::Anthropic,
            json!({ "max_tokens": 100, "messages": [use_("t1"), res("t1"), use_("t1"), res("t1")] }),
            &[]
        ),
        "anthropic: id reused"
    );

    let fc = |name: &str| json!({ "role": "model", "parts": [{ "functionCall": { "name": name, "args": {} } }] });
    let fr =
        |name: &str| json!({ "role": "user", "parts": [{ "functionResponse": { "name": name, "response": {} } }] });
    assert!(
        !fails(Wire::Gemini, json!({ "contents": [fc("read"), fr("read"), fc("read"), fr("read")] }), &[]),
        "same name twice"
    );
    assert!(fails(Wire::Gemini, json!({ "contents": [fc("read"), fr("read"), fr("read")] }), &[]), "gemini duplicate");
    assert!(fails(Wire::Gemini, json!({ "contents": [fc(""), fr("")] }), &[]), "gemini empty name");

    let fc_id =
        |id: Value| json!({ "role": "model", "parts": [{ "functionCall": { "id": id, "name": "read", "args": {} } }] });
    let fr_id = |id: Value| json!({ "role": "user", "parts": [{ "functionResponse": { "id": id, "name": "read", "response": {} } }] });
    assert!(!fails(Wire::Gemini, json!({ "contents": [fc_id(json!("g1")), fr_id(json!("g1"))] }), &[]), "gemini id");
    assert!(fails(Wire::Gemini, json!({ "contents": [fc_id(json!("")), fr_id(json!(""))] }), &[]), "gemini empty id");
    assert!(
        fails(Wire::Gemini, json!({ "contents": [fc_id(json!(7)), fr_id(json!(7))] }), &[]),
        "gemini non-string id"
    );
    assert!(
        fails(
            Wire::Gemini,
            json!({ "contents": [fc_id(json!("g1")), fr_id(json!("g1")), fc_id(json!("g1")), fr_id(json!("g1"))] }),
            &[]
        ),
        "gemini id reused"
    );
}

#[test]
fn tool_rounds_needs_the_call_and_its_result() {
    let full = json!({ "input": [
        { "type": "message", "role": "user", "content": "q" },
        { "type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"path\":\"note.txt\"}" },
        { "type": "function_call_output", "call_id": "c1", "output": "hello from the note" },
    ] });
    let rounds = tool_rounds(Wire::Responses, &full);
    assert_eq!(rounds, [("read".to_string(), json!({ "path": "note.txt" }), "hello from the note".to_string())]);
    let dropped = json!({ "input": [{ "type": "message", "role": "user", "content": "q hello from the note" }] });
    assert!(tool_rounds(Wire::Responses, &dropped).is_empty());
}

#[tokio::test]
async fn a_provider_left_at_its_real_address_is_refused_before_dialing() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "test-anthropic");
        let mock = ScriptedModel::start(Wire::Anthropic, vec![]).await;
        let mut cfg = mock.host_config();
        cfg["plugins"]["provider-anthropic"] = json!({});
        let h = env.host().config(cfg).build().await;
        let res = h.prompt("hi", json!({})).await.unwrap();
        h.shutdown().await;
        let err = res["error"]["message"].as_str().unwrap_or_default();
        assert!(err.contains("offline test guard"), "expected the offline guard to refuse api.anthropic.com: {res}");
        mock.finish();
    })
    .await
}

const LIVE_CHILD: &str = "MARE_TEST_SCRIPTED_LIVE_CHILD";

#[test]
fn scripted_model_refuses_the_live_override() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "suite::providers::agent_turns::scripted_start_under_the_live_override", "--ignored"])
        .env(LIVE_CHILD, "1")
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .run_output();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "ScriptedModel started under MARE_TEST_ALLOW_LIVE=1:\n{text}");
    assert!(text.contains("never run with MARE_TEST_ALLOW_LIVE=1"), "failed for another reason:\n{text}");
}

#[test]
#[ignore = "child of scripted_model_refuses_the_live_override"]
fn scripted_start_under_the_live_override() {
    assert_eq!(std::env::var(LIVE_CHILD).as_deref(), Ok("1"));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _mock = ScriptedModel::start(Wire::Compat, vec![]).await;
    });
}
