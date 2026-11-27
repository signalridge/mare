use mare_shared::ui::{Mode, Node, UiCmd, UiShared};
use serde_json::json;

use crate::common::*;

fn node_images(
    node: &Node,
    out_resources: &mut Vec<mare_shared::image_resource::Reference>,
    out_bytes: &mut Vec<Vec<u8>>,
) {
    match node {
        Node::Image { resource, bytes, fallback, .. } => {
            if let Some(r) = resource {
                out_resources.push(r.clone());
            }
            if let Some(b) = bytes {
                out_bytes.push(b.to_vec());
            }
            if let Some(fb) = fallback {
                node_images(fb, out_resources, out_bytes);
            }
        }
        Node::PresentedMarkdown { blocks, .. } => {
            for child in blocks.values() {
                node_images(child, out_resources, out_bytes);
            }
        }
        Node::Lines(items) | Node::Row(items) => {
            for item in items {
                node_images(item, out_resources, out_bytes);
            }
        }
        Node::Indent { child, .. } | Node::Clip { child, .. } | Node::Block { child, .. } => {
            node_images(child, out_resources, out_bytes);
        }
        Node::Split { left, right, .. } => {
            node_images(left, out_resources, out_bytes);
            node_images(right, out_resources, out_bytes);
        }
        Node::Frames { frames, still, .. } => {
            for f in frames {
                node_images(f, out_resources, out_bytes);
            }
            node_images(still, out_resources, out_bytes);
        }
        _ => {}
    }
}

#[tokio::test]
async fn tui_transcript_keeps_artifact_image_and_bytes_across_cached_updates() {
    local(async {
        let env = TestEnv::new().await;
        let ui = UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        *ui.caps.write().unwrap() = json!({ "images": "kitty" });
        let body = format!("lead\n\n```artimg\nbox\n```\n\n```rawimg\nbox\n```\n\n{}DONE", "tail ".repeat(80));
        let mut opts = env.host().faux(json!([{ "text": body, "delay_ms": 2 }])).opts;
        opts.mode = Mode::Tui;
        let h = mare_host::Host::with_ui(opts, ui.clone()).await.unwrap();
        assert!(h.kernel.problems().is_empty(), "plugin problems: {:?}", h.kernel.problems());
        expose(&h);
        exec(
            &h,
            r##"
            local png = mare.crypto.base64_decode("iVBORw0KGgoAAAANSUhEUgAAACAAAAAQCAYAAAB3AH1ZAAAAJElEQVR4nGOQ23Lp/0BihlEHjDpg1AGjDhh1wKgDRh0w0A4AALXQRkwBCanJAAAAAElFTkSuQmCC")
            TEST_CLOSE_RES = mare.ui.image_resolver("b12a-art", function() return png end)
            TEST_UI = C.cordis:get("ui")
            TEST_UI:markdown_presenter("artimg", {
                requires_artifact_images = true,
                render = function()
                    return mare.ui.image({ resource = mare.ui.image_resource("b12a-art", "art-k1", 32, 16) })
                end,
            })
            TEST_UI:markdown_presenter("rawimg", {
                render = function()
                    return mare.ui.image({ bytes = png })
                end,
            })
            DONE_TX, DONE_RX = mare.task.channel()
            C.cordis:on("turn/completed", function() DONE_TX:send(true) end)
            "##,
        )
        .await;

        while rx.try_recv().is_ok() {}
        exec(
            &h,
            r#"
            assert(C.cordis:get("agent"):prompt("go"))
            assert(DONE_RX:recv())
            "#,
        )
        .await;

        let mut last_assistant_node: Option<Node> = None;
        let mut warnings = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                UiCmd::BlockPush { kind, patch, .. } if kind == "assistant" => {
                    if let Some(n) = patch.node {
                        last_assistant_node = Some(n);
                    }
                }
                UiCmd::BlockUpdate { patch, .. } => {
                    if let Some(n) = patch.node {
                        last_assistant_node = Some(n);
                    }
                }
                UiCmd::Notify { text, .. } => warnings.push(text),
                _ => {}
            }
        }
        assert!(
            !warnings.iter().any(|w| w.contains("unavailable") || w.contains("resolver_id")),
            "unexpected presentation error warning: {warnings:?}"
        );
        let final_node = last_assistant_node.expect("expected final assistant block node");
        let mut resources = Vec::new();
        let mut bytes = Vec::new();
        node_images(&final_node, &mut resources, &mut bytes);
        assert!(
            resources.iter().any(|r| r.key.as_ref() == "art-k1" && r.resolver.id() != 0),
            "final TUI node lost artifact_image resource: {final_node:?}"
        );
        assert!(!bytes.is_empty(), "final TUI node lost inline bytes image after cached update: {final_node:?}");
        let node_debug = format!("{final_node:?}");
        assert!(node_debug.contains("DONE"), "final TUI node missed trailing DONE update (likely threw in update): {node_debug}");

        exec(&h, "if TEST_CLOSE_RES then TEST_CLOSE_RES() end").await;
        h.shutdown().await;
    })
    .await;
}
