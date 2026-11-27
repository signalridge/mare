//! `copy` plugin: what /copy copies (Markdown source, the Nth-last response, a code
//! block from the picker, a file with Ctrl+W, "always full" with Ctrl+A), table alignment, the copy/completed
//! event and copy/request from other views. The clipboard primitive is stubbed here; its path
//! choice is unit-tested in mare-binding and its OSC 52 write path in tests/pty_terminal.py.

use crate::common::*;

/// A host with two assistant responses and a recording clipboard stub (`COPIED`, `DONE`).
async fn host_with_responses(env: &TestEnv) -> std::sync::Arc<mare_host::Host> {
    let h = env.host().builtins(&["copy"]).no_session().build().await;
    expose(&h);
    global(&h, "K", h.service_table("copy").unwrap());
    exec(
        &h,
        r#"COPIED, DONE = {}, {}
           mare.clipboard.copy = function(text)
             COPIED[#COPIED + 1] = text
             return { ok = true, method = "native", verified = true, bytes = #text }
           end
           C.cordis:on("copy/completed", function(ev) DONE[#DONE + 1] = ev end)
           local function a(text) return { role = "assistant", content = { { type = "text", text = text } } } end
           local function u(text) return { role = "user", content = { { type = "text", text = text } } } end
           A:set_messages({
             u("one"), a("first answer"),
             u("two"), a("Here:\n\n```lua\nprint(1)\n```\n\nand\n\n~~~sh\necho hi\n~~~\n\n| a | bb |\n|---|--:|\n| ccc | d |"),
           })"#,
    )
    .await;
    h
}

#[tokio::test]
async fn copy_n_copies_the_markdown_source_of_the_nth_last_response() {
    local(async {
        let env = TestEnv::new().await;
        let h = host_with_responses(&env).await;
        // headless: no picker, the full response (Markdown source, not rendered text)
        let (copied, method): (String, String) = eval(&h, r#"K:last(2); return COPIED[1], DONE[1].method"#).await;
        assert_eq!(copied, "first answer");
        assert_eq!(method, "native");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_picker_offers_each_code_block_and_writes_files_with_ctrl_w() {
    local(async {
        let env = TestEnv::new().await;
        let h = host_with_responses(&env).await;
        exec(
            &h,
            r#"mare.ui.connected = function() return true end
               PICKS = {}
               NEXT = { { 1, 1, nil }, { 2, 1, "ctrl+w" } }
               mare.ui.select = function(t)
                 PICKS[#PICKS + 1] = t
                 return table.unpack(table.remove(NEXT, 1))
               end"#,
        )
        .await;
        let (items, keys, first): (i64, String, String) = eval(
            &h,
            r#"K:last(1)
               local t = PICKS[1]
               local ks = {}
               for _, k in ipairs(t.keys) do ks[#ks + 1] = k.key end
               return #t.items, table.concat(ks, ","), COPIED[1]"#,
        )
        .await;
        // two code blocks + "Full response"; Ctrl+W and Ctrl+A are offered
        assert_eq!(items, 3);
        assert_eq!(keys, "ctrl+w,ctrl+a");
        assert_eq!(first, "print(1)");
        // Ctrl+W on the second block: a file in the working directory, named by the language
        let path: String = eval(&h, r#"K:last(1); return DONE[#DONE].target"#).await;
        assert!(path.ends_with(".sh"), "{path}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo hi");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn always_full_skips_the_picker_and_aligns_tables() {
    local(async {
        let env = TestEnv::new().await;
        let h = host_with_responses(&env).await;
        exec(
            &h,
            r#"mare.ui.connected = function() return true end
               PICKS = 0
               mare.ui.select = function(t) PICKS = PICKS + 1; return 1, 1, "ctrl+a" end"#,
        )
        .await;
        // Ctrl+A remembers "always the full response" and copies it now
        let (picks, full): (i64, String) = eval(&h, r#"K:last(1); return PICKS, COPIED[1]"#).await;
        assert_eq!(picks, 1);
        assert!(full.contains("| a   |  bb |\n| --- | --: |\n| ccc |   d |"), "{full}");
        // next time no picker at all
        let picks: i64 = eval(&h, r#"K:last(1); return PICKS"#).await;
        assert_eq!(picks, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn other_views_ask_for_a_copy_with_copy_request() {
    local(async {
        let env = TestEnv::new().await;
        let h = host_with_responses(&env).await;
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("copy/request", { text = "a single message", kind = "user", from = "tree" })"#,
        )
        .await;
        wait_until(WAIT_SECS, "the copy/request to be copied", || {
            let h = h.clone();
            async move { eval::<bool>(&h, r#"return COPIED[1] == "a single message""#).await }
        })
        .await;
        let target: String = eval(&h, r#"return DONE[1].target"#).await;
        assert_eq!(target, "the message");
        h.shutdown().await;
    })
    .await
}
