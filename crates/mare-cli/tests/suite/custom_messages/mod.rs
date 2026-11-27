//! tests: custom message types (`validate` + `render`), delivery modes, display-only
//! (`session:record` / `entry_renderer`) vs entering model context (`message:append`),
//! Markdown transforms across surfaces, and RPC schema/events parity.

use std::process::Stdio;
use std::sync::Arc;

use mare_host::{Host, HostOptions};
use mare_shared::ui::{Mode, Node, UiCmd, UiShared};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::common::*;

mod delivery_and_display;
mod markdown_and_durability;
mod rpc_callbacks;

fn ui_host_opts(env: &TestEnv, script: Value) -> HostOptions {
    let mut b = env.host().builtins(&["message"]).faux(script);
    b.opts.mode = Mode::Tui;
    b.opts
}

async fn eval_json(h: &Host, expr: &str) -> Value {
    let raw: String = eval(h, &format!("return mare.json.encode((function()\n{expr}\nend)())")).await;
    serde_json::from_str(&raw).unwrap()
}

async fn tui_host(env: &TestEnv, script: Value) -> (Arc<Host>, mpsc::UnboundedReceiver<UiCmd>, Arc<UiShared>) {
    let ui = UiShared::new();
    let (tx, rx) = mpsc::unbounded_channel();
    *ui.tx.write().unwrap() = Some(tx);
    let h = Host::with_ui(ui_host_opts(env, script), ui.clone()).await.unwrap();
    assert!(h.kernel.problems().is_empty(), "plugin problems: {:?}", h.kernel.problems());
    (h, rx, ui)
}

fn drain_cmds(rx: &mut mpsc::UnboundedReceiver<UiCmd>) -> Vec<UiCmd> {
    let mut out = vec![];
    while let Ok(cmd) = rx.try_recv() {
        out.push(cmd);
    }
    out
}

fn render_node_plain(node: &Node, width: usize) -> String {
    let theme = mare_shared::style::Theme::default();
    mare_tui::render::render_node(node, width, &theme, 0)
        .into_iter()
        .map(|l| l.plain().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_pushed_blocks(cmds: &[UiCmd]) -> Vec<(u64, String, String, Option<String>)> {
    let mut blocks: Vec<(u64, String, String, Option<String>)> = vec![];
    for cmd in cmds {
        match cmd {
            UiCmd::BlockPush { id, kind, patch } => {
                let txt = patch
                    .node
                    .as_ref()
                    .map(|n| render_node_plain(n, 80))
                    .or_else(|| patch.markdown.clone())
                    .unwrap_or_default();
                let exp = patch.expanded.as_ref().map(|n| render_node_plain(n, 80));
                blocks.push((*id, kind.clone(), txt, exp));
            }
            UiCmd::BlockUpdate { id, patch } => {
                if let Some(entry) = blocks.iter_mut().find(|(bid, _, _, _)| bid == id) {
                    if let Some(n) = &patch.node {
                        entry.2 = render_node_plain(n, 80);
                    } else if let Some(m) = &patch.markdown {
                        entry.2 = m.clone();
                    }
                    if let Some(en) = &patch.expanded {
                        entry.3 = Some(render_node_plain(en, 80));
                    }
                }
            }
            UiCmd::Clear => blocks.clear(),
            _ => {}
        }
    }
    blocks
}

fn collect_pushed_text(cmds: &[UiCmd]) -> Vec<(u64, String, String)> {
    collect_pushed_blocks(cmds).into_iter().map(|(id, kind, txt, _)| (id, kind, txt)).collect()
}

fn rpc_call_collecting(rpc: &mut RpcClient, method: &str, params: Value, events: &mut Vec<Value>) -> Value {
    let id = rpc.request(method, params);
    loop {
        let m = rpc.read();
        if m.get("method").is_some() {
            if m["method"] == "event" {
                events.push(m["params"].clone());
            }
            continue;
        }
        if m["id"] == id {
            return m;
        }
    }
}
