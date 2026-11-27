//! Edit results in the session: a diff, not the file's old and new text (details
//! `version = 2`); results written before keep their texts and still load, render and rewind.

use super::*;

/// 40 lines; `changed` rewrites one of them
fn text(changed: Option<usize>) -> String {
    (1..=40).map(|i| if Some(i) == changed { format!("line {i} changed\n") } else { format!("line {i}\n") }).collect()
}

/// the edit tool's own view of a result `{ details = D }` (live and in the replayed transcript)
const VIEW: &str = r#"return mare.ui.render_text(T:get("edit").render({ path = "a.txt" }, { details = D }), 80)"#;

fn lua_value(h: &mare_host::Host, v: &Value) -> mlua::Value {
    mare_shared::lua::json_to_lua(h.lua(), v).unwrap()
}

#[tokio::test]
async fn an_edit_stores_a_diff_that_views_and_rewinds_like_the_texts() {
    local(async {
        let env = TestEnv::new().await;
        let (old, new) = (text(None), text(Some(20)));
        let file = env.cwd.join("a.txt");
        std::fs::write(&file, &old).unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "tool_calls": [{ "name": "edit", "arguments":
                { "path": "a.txt", "old_string": "line 20\n", "new_string": "line 20 changed\n" } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["changes"]).faux(script).build().await;
        expose(&h);
        h.prompt("edit it", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), new);

        // the record: the diff and the line count, not the texts; the model's text as before
        let path: String = eval(&h, "return S:info().path").await;
        let body = std::fs::read_to_string(&path).unwrap();
        let rec: Value = body
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|e| e["message"]["tool_name"] == "edit")
            .expect("the edit result is in the session");
        let m = &rec["message"];
        assert_eq!(m["content"][0]["text"], "Edited a.txt (+1 -1)");
        let d = &m["details"];
        assert_eq!((d["version"].as_u64(), d["lines"].as_u64()), (Some(2), Some(40)), "{d}");
        assert!(d.get("old").is_none() && d.get("new").is_none(), "{d}");
        assert!(d["diff"].as_str().unwrap().contains("-line 20\n+line 20 changed\n"), "{d}");

        // the view is the one a full-text result of the same change gives
        global(&h, "D", lua_value(&h, d));
        let stored: String = eval(&h, VIEW).await;
        global(&h, "D", lua_value(&h, &json!({ "path": file, "old": old, "new": new })));
        let texts: String = eval(&h, VIEW).await;
        assert_eq!(stored, texts);
        assert!(stored.contains("+1 −1") && stored.contains("20 + line 20 changed"), "{stored}");

        // Ctrl+R (the change journal) and /rewind read the snapshots, not the result: unchanged
        global(&h, "CH", h.service_table("changes").unwrap());
        let (before, after, turn): (String, String, String) =
            eval(&h, "local t = CH:diffs()[1]; return t.files[1].old, t.files[1].new, t.turn").await;
        assert_eq!((before.as_str(), after.as_str()), (old.as_str(), new.as_str()));
        global(&h, "TURN", turn);
        let restored: usize = eval(&h, "return #CH:rewind(TURN, {}).restored").await;
        assert_eq!(restored, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), old);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_older_session_with_full_texts_still_loads_views_and_undoes() {
    local(async {
        let env = TestEnv::new().await;
        let (old, new) = (text(None), text(Some(20)));
        let file = env.cwd.join("a.txt");
        std::fs::write(&file, &new).unwrap();
        let call = json!({ "type": "tool_call", "id": "c1", "name": "edit",
            "arguments": { "path": "a.txt", "old_string": "line 20\n", "new_string": "line 20 changed\n" } });
        let entries = [
            msg("m1", None, "user", "edit it"),
            json!({ "type": "message", "entry_id": "m2", "parent_entry_id": "m1",
                    "message": { "role": "assistant", "content": [call] } }),
            // a result as written before version 2: the whole file before and after
            json!({ "type": "message", "entry_id": "m3", "parent_entry_id": "m2", "message": { "role": "tool_result",
                    "tool_call_id": "c1", "tool_name": "edit", "is_error": false,
                    "content": [{ "type": "text", "text": "Edited a.txt (+1 -1)" }],
                    "details": { "path": file, "old": old, "new": new } } }),
            msg("m4", Some("m3"), "assistant", "done"),
        ];
        let path = env.write_session("2026-01-01T00-00-00_old-edit.jsonl", &entries);
        let written = std::fs::read_to_string(&path).unwrap();
        let h = env.host().builtins(&["changes"]).resume().build().await;
        expose(&h);

        // loads: the result comes back with its texts, the file is not rewritten
        let (o, n): (String, String) = eval(
            &h,
            r#"for _, m in ipairs(A:list_messages().data) do
                 if m.role == "tool_result" then D = m.details return D.old, D.new end
               end"#,
        )
        .await;
        assert_eq!((o.as_str(), n.as_str()), (old.as_str(), new.as_str()));
        assert!(std::fs::read_to_string(&path).unwrap().starts_with(&written), "the old records stay as written");

        // views exactly like a version-2 result of the same change
        let texts: String = eval(&h, VIEW).await;
        let stored: String = eval(
            &h,
            r#"local diff, _, _, lines = mare.text.diff(D.old, D.new, { path = "a.txt" })
               D = { version = 2, path = D.path, diff = diff, lines = lines }
               return mare.ui.render_text(T:get("edit").render({ path = "a.txt" }, { details = D }), 80)"#,
        )
        .await;
        assert_eq!(texts, stored);
        assert!(texts.contains("20 + line 20 changed"), "{texts}");

        // /undo rewinds the conversation over it
        exec(&h, "mare.ui.set_editor = function(t) mare.g.editor = t end").await;
        global(&h, "CMD", h.service_table("commands").unwrap());
        exec(&h, r#"CMD:run("/undo 1")"#).await;
        wait_until(WAIT_SECS, "/undo to put the prompt back", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return mare.g.editor").await.as_deref() == Some("edit it") }
        })
        .await;
        h.shutdown().await;
    })
    .await
}
