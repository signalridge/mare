//! Session writes through the sensitive-value projection: detached copies, main's
//! write-time U+FFFD replacement, and nested tool results carrying raw bytes.

use super::*;

fn lossy_notices(rx: &mut tokio::sync::mpsc::UnboundedReceiver<mare_shared::ui::UiCmd>) -> Vec<(String, String)> {
    let mut out = vec![];
    while let Ok(cmd) = rx.try_recv() {
        if let mare_shared::ui::UiCmd::Notify { text, level, .. } = cmd
            && text.contains("non-UTF-8")
        {
            out.push((text, level));
        }
    }
    out
}

async fn tui_host(
    env: &TestEnv,
    script: Option<serde_json::Value>,
) -> (std::sync::Arc<mare_host::Host>, tokio::sync::mpsc::UnboundedReceiver<mare_shared::ui::UiCmd>) {
    let mut opts = match script {
        Some(script) => env.host().faux(script).opts,
        None => env.host().opts,
    };
    opts.mode = mare_plugin::Mode::Tui;
    let ui = mare_shared::ui::UiShared::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    *ui.tx.write().unwrap() = Some(tx);
    let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
    expose(&h);
    while rx.try_recv().is_ok() {}
    (h, rx)
}

// Reference: Codex rollout/src/recorder_tests.rs:645 (record, then re-read and compare the
// stored items) and Kimi Code agent-core-v2/test/persistence/backends/node-fs/
// appendLogStore.test.ts:75 (in-memory view equals what was appended). Neither aliases the
// caller's object; here the caller keeps mutating its table after a synchronous record.
#[tokio::test]
async fn recorded_table_is_detached_from_the_callers_later_mutation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (memory, path): (String, String) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local data = { nested = { value = "before" }, list = { "a" } }
               assert(sess:add_record("test/detached", data, { sync = true }))
               data.nested.value = "after"
               data.list[1] = "changed"
               local rows = sess:list_records("test/detached").data
               return mare.json.encode(rows[1]), S:info().path"#,
        )
        .await;
        h.shutdown().await;
        let memory: serde_json::Value = serde_json::from_str(&memory).unwrap();
        let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let disk = entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "test/detached")
            .expect("missing test/detached");
        let expected = json!({ "nested": { "value": "before" }, "list": ["a"] });
        assert_eq!(disk["data"], expected);
        assert_eq!(memory, expected, "session records must not alias the caller's table");
    })
    .await
}

// Reference: reference implementation (native record
// round trip) and Codex protocol/src/exec_output_tests.rs:64 (lossy U+FFFD fallback) for the disk
// side; the timing (memory keeps bytes, replacement + warning only when the file is
// actually written) is main's SESSION-WRITE behavior, which no reference project has.
#[tokio::test]
async fn buffered_non_utf8_value_keeps_bytes_in_memory_until_the_file_is_written() {
    local(async {
        let env = TestEnv::new().await;
        let (h, mut rx) = tui_host(&env, None).await;
        let (buffered_ok, raw_hex, path_before): (bool, String, Option<String>) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local ok = sess:add_record("test/buffered", { raw = "ab\228\184" })
               local raw = sess:list_records("test/buffered").data[1].raw
               return type(ok) == "table", (raw:gsub(".", function(c) return ("%02x"):format(c:byte()) end)), S:info().path"#,
        )
        .await;
        assert!(buffered_ok);
        assert_eq!(raw_hex, "6162e4b8", "the in-memory record keeps the original bytes");
        assert!(
            path_before.as_deref().is_none_or(|p| !std::path::Path::new(p).exists()),
            "a plain record must stay buffered: {path_before:?}"
        );
        assert!(lossy_notices(&mut rx).is_empty(), "no warning before anything is written");

        let (raw_hex_after, path): (String, String) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               assert(sess:add_record("test/materialize", { x = 1 }, { sync = true }))
               local raw = sess:list_records("test/buffered").data[1].raw
               return (raw:gsub(".", function(c) return ("%02x"):format(c:byte()) end)), S:info().path"#,
        )
        .await;
        assert_eq!(raw_hex_after, "6162e4b8", "writing the file does not rewrite the in-memory record");
        let notices = lossy_notices(&mut rx);
        assert_eq!(notices.len(), 1, "materializing the file warns once: {notices:?}");
        assert_eq!(notices[0].1, "warning");
        h.shutdown().await;
        let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let rec = entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "test/buffered")
            .expect("missing test/buffered");
        assert_eq!(rec["data"]["raw"], "ab\u{FFFD}");
    })
    .await
}

// Reference: Codex core/tests/suite/code_mode.rs:3987 (a script calls nested tools and each
// result comes back to it) and protocol/src/exec_output_tests.rs:64 (invalid bytes still yield a
// visible result instead of failing). Here the child tool itself returns raw bytes (not a later
// hook), and the script then makes a second nested call: both outcomes must be recorded.
#[tokio::test]
async fn nested_tool_returning_non_utf8_bytes_is_recorded_before_the_next_nested_call() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "nested_parent", "arguments": {} }] },
            { "text": "done" }
        ]);
        let (h, _rx) = tui_host(&env, Some(script)).await;
        eval::<()>(
            &h,
            r#"local tools = C.cordis:get("tools")
               local function plain(name, execute)
                   tools:register(name, {
                       description = name,
                       parameters = { type = "object", properties = {} },
                       side_effect = "read",
                       exposure = "nested",
                       execute = execute,
                   })
               end
               plain("raw_child", function() return { content = "prefix\228\184suffix" } end)
               plain("after_child", function() return { content = "second ran" } end)
               tools:register("nested_parent", {
                   description = "calls two nested tools",
                   parameters = { type = "object", properties = {} },
                   side_effect = "read",
                   execute = function(_, tctx)
                       local decls = tools:definitions(tctx.model, { "raw_child", "after_child" }, { step_id = tctx.step_id })
                       local invoke = tctx.bind_tools(decls)
                       local first = invoke("raw_child", {})
                       local second = invoke("after_child", {})
                       return { content = ("first=%s second=%s"):format(tostring(first.error_code), tostring(second.content)) }
                   end,
               })"#,
        )
        .await;
        let res = h.prompt("run nested", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "unexpected error: {res}");
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let nested: Vec<_> =
            entries.iter().filter(|e| e["type"] == "custom" && e["kind"] == "tool/nested-result").collect();
        let first = nested
            .iter()
            .find(|e| e["data"]["name"] == "raw_child")
            .unwrap_or_else(|| panic!("the raw-bytes nested call must leave a tool/nested-result: {entries:?}"));
        // main's controlled path: the program value is rejected as an error result, recorded.
        assert_eq!(first["data"]["result"]["is_error"], true, "{first}");
        assert!(
            first["data"]["result"]["content"].as_str().is_some_and(|c| c.contains("must be UTF-8")),
            "{first}"
        );
        assert!(
            nested.iter().any(|e| e["data"]["name"] == "after_child"),
            "the second nested call is recorded too: {entries:?}"
        );
    })
    .await
}
