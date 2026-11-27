//! A peer's input keeps its peer classification when a trusted plugin registers the value
//! "peer" for projection: the host attaches its
//! control fields after projecting the content, so file-ref still sees a peer and gives the
//! peer's `@path` only a mention.
//!
//! Reference: Codex core/src/session/turn_input_tests.rs:1108 (an input keeps its source) and
//! reference implementation (origin
//! kept); neither registers a value colliding with the origin label.
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn a_peers_input_stays_a_peers_when_peer_is_a_registered_value() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("notes.txt"), "notes-secret-body").unwrap();
        let mut b = env
            .host()
            .builtins(&["file-ref"])
            .faux(json!([{ "text": "ok" }]))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            KEEP_PEER = mare.projection.register("peer")
            SEEN = nil
            -- runs after file-ref: what the transform produced for the agent
            C.cordis:on("input/transform", function(msg, ev)
                SEEN = { source = msg.source, content = mare.json.encode(msg.content) }
                return ev.next()
            end)"#,
        )
        .await;
        let res = h.prompt("inspect @notes.txt", json!({ "source": "peer" })).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let seen: String = eval(&h, "return mare.json.encode(SEEN)").await;
        let seen: serde_json::Value = serde_json::from_str(&seen).unwrap();
        assert_eq!(seen["source"], "peer", "the classification is not rewritten: {seen}");
        let content = seen["content"].as_str().unwrap();
        assert!(!content.contains("notes-secret-body"), "a peer's @path was read: {content}");
        assert!(content.contains("\"mention\""), "a peer's @path is a mention: {content}");
        h.shutdown().await;
    })
    .await
}
