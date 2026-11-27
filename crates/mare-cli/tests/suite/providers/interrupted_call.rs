//! An abort while a tool call's arguments are half-streamed (agent-loop corpus):
//! the history mare sends next has no trace of the unfinished call and keeps the completed text
//! before it.

use super::*;
use crate::common::scripted::{Part, ScriptedModel, Step, Wire};

#[tokio::test]
async fn abort_mid_tool_arguments_leaves_no_dangling_call_and_keeps_the_text_before_it() {
    let mut problems = Vec::new();
    for wire in [Wire::Anthropic, Wire::Responses] {
        local(async {
            let mut env = TestEnv::new().await;
            let half_id = if wire == Wire::Anthropic { "toolu_half" } else { "call_half" };
            let mock = ScriptedModel::start(
                wire,
                vec![
                    Step::StallAfter {
                        parts: vec![
                            Part::Text("LEAD-TEXT".into()),
                            Part::PartialToolCall { id: half_id.into(), name: "read".into(), raw_args: "{\"pa".into() },
                        ],
                        events: 99,
                        ms: 600_000,
                    },
                    Step::Reply(vec![Part::Text("ok".into())]),
                ],
            )
            .await;
            let h = mock.host(&mut env, false, &[]).await;
            global(&h, "AGENT", h.service_table("agent").unwrap());
            let run = {
                let h = h.clone();
                tokio::task::spawn_local(async move { h.prompt("go", json!({})).await.unwrap() })
            };
            let consumed = r#"for _, b in ipairs(AGENT:activity(nil).streaming or {}) do
                                  if b.type == "tool_call" and b.raw_arguments == '{"pa' then return true end
                              end
                              return false"#;
            wait_until(30, "the parser to consume the argument fragment", || {
                let h = h.clone();
                async move { eval::<bool>(&h, consumed).await }
            })
            .await;
            h.abort().await.unwrap();
            let res = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();
            assert_eq!(res["error"]["code"], "aborted", "{wire}: {res}");
            let next = h.prompt("again", json!({})).await.unwrap();
            h.shutdown().await;
            let reqs = mock.requests();
            if reqs.len() != 2 || next["text"] != "ok" {
                problems.push(format!("{wire}: {} requests, follow-up {next}", reqs.len()));
            } else {
                let req = reqs[1].json().to_string();
                if req.contains(half_id) {
                    problems.push(format!("{wire}: the half call {half_id} is still in the next request"));
                }
                if !req.contains("LEAD-TEXT") {
                    problems.push(format!("{wire}: the completed text before the half call was dropped"));
                }
            }
            mock.finish();
        })
        .await;
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
