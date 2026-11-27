//! A tool result's `terminate`: when every result of a tool batch carries it, the run takes the
//! stopping path instead of another model request (pi agent-loop.ts:281); `agent/turn-stopping`
//! still decides. Reference: pi packages/agent/test/agent-loop.test.ts:1855 (all terminate:
//! stops) and :2022 (only some terminate: continues).

use crate::common::*;
use serde_json::json;

fn script(calls: serde_json::Value) -> serde_json::Value {
    json!([{ "tool_calls": calls }, { "text": "follow-up request" }, { "text": "another" }])
}

const TOOLS: &str = r#"
    for _, name in ipairs({ "finish", "plain" }) do
        C.cordis:get("tools"):register(name, { description = name, parameters = { type = "object", properties = {} },
            side_effect = "read", idempotent = true,
            execute = function() return { content = name, terminate = name == "finish" } end })
    end
    TURNS = 0
    C.cordis:on("step/started", function() TURNS = TURNS + 1 end)
"#;

async fn turns(calls: serde_json::Value, extra: &str) -> i64 {
    let env = TestEnv::new().await;
    let h = env.host().faux(script(calls)).no_session().build().await;
    expose(&h);
    let turns: i64 = eval(&h, &format!("{TOOLS}\n{extra}\nA:wait(A:prompt('go').turn_id)\nreturn TURNS")).await;
    h.shutdown().await;
    turns
}

#[tokio::test]
async fn a_batch_whose_results_all_terminate_ends_without_another_request() {
    local(async {
        let both = json!([{ "id": "c1", "name": "finish", "arguments": {} }, { "id": "c2", "name": "finish", "arguments": {} }]);
        assert_eq!(turns(both, "").await, 1, "every result terminated: no follow-up request");
        // only some terminate: the model sees the results and is asked again
        let mixed = json!([{ "id": "c1", "name": "finish", "arguments": {} }, { "id": "c2", "name": "plain", "arguments": {} }]);
        assert_eq!(turns(mixed, "").await, 2);
        // the stopping path is the usual one: agent/turn-stopping may keep the run going
        let one = json!([{ "id": "c1", "name": "finish", "arguments": {} }]);
        let keep = "local once = false C.cordis:on('agent/turn-stopping', function() if not once then once = true return 'continue' end end)";
        assert_eq!(turns(one, keep).await, 2);
    })
    .await
}
