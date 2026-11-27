//! Damaged read-only views cannot be silently converted into a fork.
use super::*;

#[tokio::test]
async fn damaged_readonly_fork_refuses_without_losing_the_in_memory_conversation() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "damaged-view.jsonl",
            &[msg("m1", None, "user", "INTACT"), msg("m2", Some("m1"), "assistant", "AFTER-BAD")],
        );
        let mut body = std::fs::read_to_string(&path).unwrap();
        let offset = body.lines().take(2).map(|line| line.len() + 1).sum();
        body.insert_str(offset, "invalid json\n");
        std::fs::write(&path, &body).unwrap();
        let h = env.host().faux(json!([{ "text": "READONLY-REPLY" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, r#"mare.ui.select = function(o) assert(#o.items == 1 and o.items[1].label == "View read-only"); return 1 end; S:load({ path = TARGET })"#).await;
        assert_eq!(h.prompt("READONLY-QUESTION", json!({})).await.unwrap()["text"], "READONLY-REPLY");
        let (before_id, before_messages): (String, String) =
            eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
        let (ok, err, after_id, after_messages): (bool, String, String, String) = eval(
            &h,
            r#"
            local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
            return ok, tostring(err), S:info().session_id, mare.json.encode(A:list_messages().data)
        "#,
        )
        .await;
        assert!(!ok && err.contains("damaged read-only") && err.contains("back up or repair"), "{err}");
        assert_eq!((after_id, after_messages), (before_id, before_messages));
        assert!(std::fs::read_to_string(&path).unwrap() == body);
        h.shutdown().await;
        assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    })
    .await
}
