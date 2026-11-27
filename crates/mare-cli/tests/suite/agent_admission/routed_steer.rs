use super::*;

/// A steer that the active frontend routes elsewhere (e.g. a focused side conversation) is
/// deduplicated by `request_id` like any other steer: a resend after the reply replays it, a
/// concurrent resend while routing is still in flight waits for that outcome, and a different
/// input under the same id is refused. The frontend sees the steer once.
#[tokio::test]
async fn routed_steer_request_id_replays_instead_of_routing_twice() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "unused" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            ROUTED = {}
            GATE_TX, GATE_RX = mare.task.channel()
            C.cordis:on("agent/frontend-route", function(req)
                if type(req) ~= "table" or req.op ~= "steer" then return end
                ROUTED[#ROUTED + 1] = req.text
                if #ROUTED == 1 then GATE_RX:recv() end -- hold routing so a resend overlaps it
                return { handled = true, result = { input_id = "side-" .. #ROUTED } }
            end, { prepend = true })
            mare.task.spawn(function() FIRST = A:steer("to the side", { request_id = "steer-1" }) end)
        "#,
        )
        .await;
        wait_until(WAIT_SECS, "the first steer to reach the frontend", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ROUTED == 1").await }
        })
        .await;
        exec(&h, r#"mare.task.spawn(function() OVERLAP = A:steer("to the side", { request_id = "steer-1" }) end)"#)
            .await;
        exec(&h, "GATE_TX:send(true)").await;
        wait_until(WAIT_SECS, "both steers to return", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return FIRST ~= nil and OVERLAP ~= nil").await }
        })
        .await;
        let result: String = eval(
            &h,
            r#"
            local again = A:steer("to the side", { request_id = "steer-1" })
            local ok, err = pcall(A.steer, A, "something else", { request_id = "steer-1" })
            return mare.json.encode({
                routed = #ROUTED, first = FIRST, overlap = OVERLAP, again = again,
                conflict = (not ok) and type(err) == "table" and err.code or tostring(err),
                queued = #A:list_queue().data,
            })
        "#,
        )
        .await;
        let r: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(r["routed"], 1, "the frontend must see the steer once: {r}");
        assert_eq!(r["first"]["input_id"], "side-1", "{r}");
        assert_ne!(r["first"]["duplicate"], true, "{r}");
        assert_eq!(r["overlap"]["input_id"], "side-1", "{r}");
        assert_eq!(r["overlap"]["duplicate"], true, "{r}");
        assert_eq!(r["again"]["input_id"], "side-1", "{r}");
        assert_eq!(r["again"]["duplicate"], true, "{r}");
        assert_eq!(r["conflict"], "request_id_conflict", "{r}");
        assert_eq!(r["queued"], 0, "a routed steer never enters this session's queue: {r}");
        h.shutdown().await;
    })
    .await
}
