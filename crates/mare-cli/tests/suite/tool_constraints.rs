//! `llm:constrain_tools`: a plugin narrows the tool declarations of every provider attempt
//! (after request hooks, before executable binding), for as long as its registration lives.

use crate::common::*;
use serde_json::json;

fn openai_done() -> String {
    [
        json!({ "type": "response.output_item.done", "item": { "type": "message", "role": "assistant",
            "content": [{ "type": "output_text", "text": "ok" }] } }),
        json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 1 } } }),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect()
}

fn declared(body: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = body["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn a_tool_constraint_narrows_every_provider_attempt_until_removed() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let server = MockServer::start().await;
        // a retried attempt, then two answered prompts
        server.route("/openai", vec![Resp::status(500, "{}"), Resp::sse(openai_done()), Resp::sse(openai_done())]);
        let h = env
            .host()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": {
                    "provider-openai": { "base_url": format!("{}/openai", server.base) },
                    "llm": { "retries": 1, "retry_base_ms": 1 },
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let seen: String = eval(
            &h,
            r#"
            local infos = {}
            local remove = LLM:constrain_tools(function(info)
                infos[#infos + 1] = info.purpose
                return { "read", "no-such-tool" }
            end)
            A:wait(A:prompt("hi").turn_id)
            remove()
            A:wait(A:prompt("again").turn_id)
            return table.concat(infos, ",")
            "#,
        )
        .await;
        assert!(seen.starts_with("agent"), "{seen}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3, "{reqs:?}");
        // both attempts of the first prompt (the failed one and its retry) carry only the allowed tools
        assert_eq!(declared(&reqs[0].json()), ["read"]);
        assert_eq!(declared(&reqs[1].json()), ["read"]);
        // once the constraint is removed the declarations are no longer narrowed
        let after = declared(&reqs[2].json());
        assert!(after.len() > 1 && after.contains(&"bash".to_string()), "{after:?}");
        h.shutdown().await;
    })
    .await
}

/// What a request declares and what its tool calls may execute are one snapshot: a constraint
/// that changes between the attempts of one request does not narrow the retry's declarations
/// below the executable binding made for it (lazy#48 #1).
#[tokio::test]
async fn a_retried_request_declares_what_its_binding_allows() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::status(500, "{}"), Resp::sse(openai_done())]);
        let h = env
            .host()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": {
                    "provider-openai": { "base_url": format!("{}/openai", server.base) },
                    "llm": { "retries": 1, "retry_base_ms": 1 },
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let _: mlua::Value = eval(
            &h,
            r#"
            local calls = 0
            LLM:constrain_tools(function()
                calls = calls + 1
                -- the first application allows two tools; a later one would allow only one
                return calls == 1 and { "read", "bash" } or { "read" }
            end)
            A:wait(A:prompt("hi").turn_id)
            "#,
        )
        .await;
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2, "{reqs:?}");
        assert_eq!(declared(&reqs[0].json()), ["bash", "read"]);
        assert_eq!(declared(&reqs[1].json()), ["bash", "read"], "the retry declared less than its binding allows");
        h.shutdown().await;
    })
    .await
}
