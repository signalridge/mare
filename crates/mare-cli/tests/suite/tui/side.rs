//! Faux TUI screen tests for `/side` and `/btw` side conversations.

use super::*;

/// Wait until the side turn settles and `needle` appears on screen.
async fn until_side_settled(t: &mut Term, what: &str, needle: &str) {
    let end = Instant::now() + WAIT;
    loop {
        t.step().await;
        let settled: bool = eval(
            &t.vt.host,
            r#"
            local c = C.cordis:get("conversations")
            local info = c and c:info()
            return info ~= nil and info.selected == true and info.busy == false
            "#,
        )
        .await;
        if settled && t.screen().contains(needle) {
            t.step().await;
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}; screen:\n{}", t.screen());
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[tokio::test]
async fn side_conversation_renders_streamed_and_nonstreamed_answers_once() {
    local(async {
        let mut t = Term::new(
            json!([
                { "text": "parent baseline" },
                { "text": "UNIQUE_SIDE_STREAM_41", "delay_ms": 5 },
                { "text": "UNIQUE_BTW_STREAM_41", "delay_ms": 5 }
            ]),
            90,
            24,
        )
        .await;
        expose(&t.vt.host);

        t.submit("seed parent turn").await;
        t.idle().await;

        // 1. Streaming `/side <question>` renders the answer once.
        t.submit("/side explain this").await;
        until_side_settled(&mut t, "streaming /side answer", "UNIQUE_SIDE_STREAM_41").await;
        let screen = t.screen();
        assert!(screen.contains("SIDE · Esc / /side back"), "side header missing:\n{screen}");
        assert_eq!(
            screen.matches("UNIQUE_SIDE_STREAM_41").count(),
            1,
            "streaming /side answer must render once:\n{screen}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("side closed after /side", |s| !s.contains("SIDE · Esc")).await;

        // 2. Streaming `/btw <question>` renders the answer once.
        t.submit("/btw quick check").await;
        until_side_settled(&mut t, "streaming /btw answer", "UNIQUE_BTW_STREAM_41").await;
        let screen = t.screen();
        assert_eq!(
            screen.matches("UNIQUE_BTW_STREAM_41").count(),
            1,
            "streaming /btw answer must render once:\n{screen}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("side closed after /btw", |s| !s.contains("SIDE · Esc")).await;

        // 3. Non-streaming provider and multi-step streaming tool turn each render once.
        exec(
            &t.vt.host,
            r#"
            T:register("side_probe_tool", {
                side_effect = "read",
                description = "read-only probe",
                parameters = { type = "object" },
                execute = function()
                    return { content = "probe-ok" }
                end,
            })
            local L = C.cordis:get("llm")
            local step = 0
            L:register_provider({
                provider = "side41",
                is_configured = function() return true end,
                models = {
                    { model_id = "nonstream", context_window = 128000, max_output = 4096 },
                    { model_id = "multistep", context_window = 128000, max_output = 4096 },
                },
                stream = function(req, emit)
                    if req.model and req.model.model_id == "nonstream" then
                        return {
                            role = "assistant",
                            stop_reason = "stop",
                            content = { { type = "text", text = "UNIQUE_NONSTREAM_SIDE_41" } },
                        }
                    end
                    step = step + 1
                    if step == 1 then
                        if emit then emit({ type = "text_delta", delta = "STEP_ONE_SIDE_41" }) end
                        return {
                            role = "assistant",
                            stop_reason = "tool_use",
                            content = {
                                { type = "text", text = "STEP_ONE_SIDE_41" },
                                { type = "tool_call", id = "c_probe_1", name = "side_probe_tool", arguments = {} },
                            },
                        }
                    end
                    if emit then emit({ type = "text_delta", delta = "STEP_TWO_SIDE_41" }) end
                    return {
                        role = "assistant",
                        stop_reason = "stop",
                        content = { { type = "text", text = "STEP_TWO_SIDE_41" } },
                    }
                end,
            })
            assert(L:set_model("side41/nonstream"))
            "#,
        )
        .await;

        t.submit("/btw nonstream check").await;
        until_side_settled(&mut t, "non-streaming /btw answer", "UNIQUE_NONSTREAM_SIDE_41").await;
        let screen = t.screen();
        assert_eq!(
            screen.matches("UNIQUE_NONSTREAM_SIDE_41").count(),
            1,
            "non-streaming side answer must render once:\n{screen}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("side closed after nonstream", |s| !s.contains("SIDE · Esc")).await;

        exec(&t.vt.host, r#"assert(C.cordis:get("llm"):set_model("side41/multistep"))"#).await;
        t.submit("/side run probe").await;
        until_side_settled(&mut t, "multi-step side answer", "STEP_TWO_SIDE_41").await;
        let screen = t.screen();
        assert_eq!(screen.matches("STEP_ONE_SIDE_41").count(), 1, "step 1 streamed text must render once:\n{screen}");
        assert_eq!(screen.matches("STEP_TWO_SIDE_41").count(), 1, "step 2 streamed text must render once:\n{screen}");
    })
    .await
}

#[tokio::test]
async fn btw_and_side_answer_on_brand_new_empty_session() {
    local(async {
        let mut t = Term::new(
            json!([
                { "text": "EMPTY_BTW_REPLY_42" },
                { "text": "EMPTY_SIDE_REPLY_42" }
            ]),
            90,
            24,
        )
        .await;

        // Brand-new session, nothing sent in main conversation yet.
        t.submit("/btw hello from empty").await;
        t.until("btw answer on empty session", |s| {
            s.contains("SIDE · Esc / /side back") && s.contains("EMPTY_BTW_REPLY_42")
        })
        .await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("side closed after btw", |s| !s.contains("SIDE · Esc")).await;

        // Second side question before any main message also succeeds.
        t.submit("/side second empty check").await;
        t.until("side answer on empty session", |s| {
            s.contains("SIDE · Esc / /side back") && s.contains("EMPTY_SIDE_REPLY_42")
        })
        .await;
    })
    .await
}
