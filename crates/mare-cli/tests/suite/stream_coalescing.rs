//! Actual agent sink ordering with a controlled clock and custom provider.

use crate::common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn stream_coalescing_keeps_event_order_and_event_driven_timing() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"
            CLOCK, EVENTS = 100, {}
            mare.task.clock = function() return CLOCK end
            C.cordis:on("item/delta", function(e)
                local raw = e.kind == "event" and e.event or nil
                EVENTS[#EVENTS + 1] = {
                    kind = e.kind,
                    etype = raw and raw.type or nil,
                    id = e.tool_call_id or (raw and raw.tool_call_id) or nil,
                    delta = raw and raw.delta or (e.kind ~= "event" and e.delta or nil),
                }
            end)
            L:register_provider({ provider = "p1-order", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                stream = function(req, sink)
                    CLOCK=100; sink({type="text_delta", delta="a"})
                    CLOCK=101; sink({type="text_delta", delta="b"})
                    CLOCK=102; sink({type="text_delta", delta="c"})
                    CLOCK=103; sink({type="tool_call_start", tool_call_id="1", name="example"})
                    CLOCK=104; sink({type="tool_call_delta", tool_call_id="1", delta="x"})
                    CLOCK=105; sink({type="tool_call_delta", tool_call_id="2", delta="y"})
                    CLOCK=106; sink({type="other"})
                    CLOCK=107; sink({type="text_delta", delta="z"})
                    return { role="assistant", stop_reason="stop", content={{type="text", text="done"}} }
                end })
            L:set_model("p1-order/m", {save=false})
        "#,
        )
        .await;
        let response = h.prompt("check", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let events: Value = serde_json::from_str(&eval::<String>(&h, "return mare.json.encode(EVENTS)").await).unwrap();
        assert_eq!(
            events,
            json!([
                {"kind":"text", "delta":"a"},
                {"kind":"text", "delta":"bc"},
                {"kind":"event", "etype":"tool_call_start", "id":"1"},
                {"kind":"tool_arguments", "id":"1", "delta":"x"},
                {"kind":"tool_arguments", "id":"2", "delta":"y"},
                {"kind":"event", "etype":"other"},
                {"kind":"text", "delta":"z"}
            ])
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn stream_coalescing_preserves_reentrant_listener_and_string_ids() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"
            CLOCK, EVENTS, REENTERED = 100, {}, false
            mare.task.clock = function() return CLOCK end
            C.cordis:on("item/delta", function(e)
                local raw = e.kind == "event" and e.event or nil
                local id = e.tool_call_id or (raw and raw.tool_call_id) or nil
                EVENTS[#EVENTS + 1] = {kind=e.kind, etype=raw and raw.type or nil, id=type(id)=="table" and "table" or id, delta=e.delta}
                if e.kind == "text" and e.delta == "b" and not REENTERED then
                    REENTERED = true
                    P1_SINK({type="reasoning_delta", tool_call_id="nested", delta="N"})
                end
            end)
            L:register_provider({ provider = "p1-reenter", is_configured = function() return true end,
                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                stream = function(req, sink)
                    P1_SINK = sink
                    CLOCK=100; sink({type="text_delta", delta="a"})
                    CLOCK=101; sink({type="text_delta", delta="b"})
                    CLOCK=102; sink({type="tool_call_delta", tool_call_id="outer", delta="x"})
                    CLOCK=103; sink({type="other"})
                    return { role="assistant", stop_reason="stop", content={{type="text", text="done"}} }
                end })
            L:set_model("p1-reenter/m", {save=false})
        "#,
        )
        .await;
        let response = h.prompt("check", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let events: Value = serde_json::from_str(&eval::<String>(&h, "return mare.json.encode(EVENTS)").await).unwrap();
        assert_eq!(
            events,
            json!([
                {"kind":"text", "delta":"a"},
                {"kind":"text", "delta":"b"},
                {"kind":"reasoning", "delta":"Nx"},
                {"kind":"event", "etype":"other", "delta":""}
            ])
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn stream_listener_mutation_is_read_after_key_change_flush() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"
            CLOCK, EVENTS = 0, {}
            mare.task.clock = function() return CLOCK end
            -- Every provider event and listener mutation stays within the stream contract.
            local cases = {
              {id="new", value="changed"},
              {value="changed"},
              {id="new", value="changed", nested=true},
              {value="changed", nested=true},
            }
            local active, target, feed
            C.cordis:on("item/delta", function(e)
              local raw = e.kind == "event" and e.event or nil
              EVENTS[#EVENTS + 1] = {kind=e.kind, etype=raw and raw.type or nil, id=e.tool_call_id, delta=e.delta}
              if e.kind == "text" and e.delta == "b" then
                target.tool_call_id, target.delta = active.id, active.value
                if active.nested then feed({type="reasoning_delta", tool_call_id="nested", delta="N"}) end
              end
            end)
            L:register_provider({ provider="p1-mutate", is_configured=function() return true end,
              models={{model_id="m", context_window=100000, max_output=4096}},
              stream=function(req, sink)
                feed = sink
                for i, case in ipairs(cases) do
                  active = case
                  CLOCK=i*100; sink({type="text_delta", delta="a"})
                  CLOCK=i*100+1; sink({type="text_delta", delta="b"})
                  target={type="tool_call_delta", tool_call_id="old", delta="original"}
                  CLOCK=i*100+2; sink(target)
                  CLOCK=i*100+3; sink({type="other"})
                end
                return {role="assistant", stop_reason="stop", content={{type="text", text="done"}}}
              end })
            L:set_model("p1-mutate/m", {save=false})
        "#,
        )
        .await;
        let response = h.prompt("check", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let events: Value = serde_json::from_str(&eval::<String>(&h, "return mare.json.encode(EVENTS)").await).unwrap();
        let mut expected = Vec::new();
        for (kind, id, delta) in [
            ("tool_arguments", Some("new"), "changed"),
            ("tool_arguments", None, "changed"),
            ("reasoning", None, "Nchanged"),
            ("reasoning", None, "Nchanged"),
        ] {
            expected.extend([
                json!({"kind":"text", "delta":"a"}),
                json!({"kind":"text", "delta":"b"}),
                {
                    let mut event = json!({"kind":kind, "delta":delta});
                    if let Some(id) = id {
                        event["id"] = json!(id);
                    }
                    event
                },
                json!({"kind":"event", "etype":"other", "delta":""}),
            ]);
        }
        assert_eq!(events, json!(expected));
        h.shutdown().await;
    })
    .await
}
