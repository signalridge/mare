//! Tool-owned records and child-session transcript persistence.

use super::*;

/// Session-owned records emitted from a tool must reach the actual JSONL journal.
#[cfg(unix)]
#[tokio::test]
async fn tool_owned_cwd_and_background_job_records_are_persisted() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir(env.cwd.join("sub")).unwrap();
        let h = env.host().builtins(&["jobs"]).faux(json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "cd sub" } }] },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "sleep 0.2", "run_in_background": true } }] },
            { "text": "done" }, { "text": "noted" }
        ])).build().await;
        expose(&h);
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "J", h.service_table("jobs").unwrap());
        h.prompt("change directory and run a background job", json!({})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        wait_until(5, "the background job to finish", || {
            let h = h.clone();
            async move {
                eval::<bool>(&h, r#"local jobs = J:list().data return #jobs > 0 and jobs[1].status ~= "running""#).await
            }
        })
        .await;
        wait_until(5, "the background job end record to be written", || {
            let path = path.clone();
            async move {
                std::fs::read_to_string(path).unwrap_or_default().lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .is_ok_and(|entry| entry["kind"] == "job" && entry["data"]["event"] == "end")
                })
            }
        })
        .await;
        let text = std::fs::read_to_string(path).unwrap();
        let entries: Vec<Value> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
        assert!(entries.iter().any(|e| e["kind"] == "working-directory/changed"), "cwd change was not saved: {text}");
        assert!(
            entries.iter().any(|e| e["kind"] == "job" && e["data"]["event"] == "start"),
            "job start was not saved: {text}"
        );
        assert!(
            entries.iter().any(|e| e["kind"] == "job" && e["data"]["event"] == "end"),
            "job end was not saved: {text}"
        );
        h.shutdown().await;
    })
    .await
}

/// The public child-session service can write its own journal from a confined tool without
/// giving that tool a general-purpose path out of the execution context.
#[tokio::test]
async fn child_session_from_tool_uses_the_host_writer_without_escaping_sandbox() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .faux(json!([
                { "tool_calls": [{ "name": "child_probe", "arguments": {} }] }, { "text": "done" }
            ]))
            .build()
            .await;
        expose(&h);
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current("read-only")
            T:register("child_probe", { description = "test", parameters = { type = "object", properties = {} },
              execute = function()
                local child = assert(S:child({ run_id = "safe", seq = 1, label = "test" }))
                CHILD_PATH = child.path
                child.append({ type = "message", message = { role = "assistant", content = "child record" } })
                child.close()
                local traversal = pcall(S.child, S, { run_id = "../escape" })
                local bad_kind = setmetatable({}, { __tostring = function()
                  pcall(mare.fs.write, CWD .. "/escaped-kind", "bad"); return "bad"
                end })
                local saved_ok = pcall(S.add_record, S, bad_kind, {})
                assert(not saved_ok, "record kind must not carry executable metamethods")
                return tostring(traversal)
              end })"#,
        )
        .await;
        h.prompt("create a child record", json!({})).await.unwrap();
        let (path, results): (String, String) =
            eval(&h, "return CHILD_PATH, mare.json.encode(A:list_messages().data)").await;
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("child record") && contents.contains("closed"), "{contents}");
        assert!(results.contains("false"), "traversal should be refused: {results}");
        assert!(!env.cwd.join("escaped-kind").exists(), "a record kind metamethod escaped the tool sandbox");
        h.shutdown().await;
    })
    .await
}

/// The transcript service must not write a child session in an explicitly ephemeral run.
#[tokio::test]
async fn an_ephemeral_session_refuses_a_child_transcript_without_blocking_tools() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "ephemeral-child.txt", "content": "ok" } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["jobs"]).faux(script).no_session().build().await;
        expose(&h);
        let (ok, err): (bool, String) = eval(
            &h,
            r#"local ok, err = pcall(S.child, S, { run_id = "ephemeral", seq = 1, label = "probe" })
               return ok, tostring(err)"#,
        )
        .await;
        assert!(!ok && err.contains("without a saved session"), "{err}");
        let child_dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd)).join("agents");
        assert!(!child_dir.exists(), "ephemeral child transcript was written");
        h.prompt("write the file", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(env.cwd.join("ephemeral-child.txt")).unwrap(), "ok");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn sub_session_is_a_normal_session_readable_while_written() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["jobs"]);
        b.opts.load_init = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "U", h.service_table("ui").unwrap());
        let (path, listed, roles, parent, mh_40, mh_10): (String, usize, String, String, i64, i64) = eval(
            &h,
            r#"
            local c = S:child({ run_id = "run1", seq = 2, label = "review api" })
            c.append({ type = "message", message = { role = "user", content = { { type = "text", text = "check it" } } } })
            c.append({ type = "message", message = { role = "assistant", content = { { type = "text", text = "ok" } } } })
            local entries = mare.store.read(c.path)       -- still open for writing
            S:load_transcript(c.path)
            local roles = {}
            for _, e in ipairs(entries) do if e.type == "message" then roles[#roles + 1] = e.message.role end end
            local got
            U:invalidate(nil, { viewport = { width = 100, height = 40 } })
            U:widget("probe", { render = function(w, max_h) got = max_h; return ui.Line({}) end })
            local h40 = got
            U:invalidate(nil, { viewport = { width = 100, height = 10 } })
            U:invalidate("widget:probe")
            local h10 = got
            return c.path, #S:children("run1"), table.concat(roles, ","), entries[1].parent_session_id, h40, h10
        "#,
        )
        .await;
        assert!(path.replace('\\', "/").contains("/agents/run1/002-review-api.jsonl"), "{path}");
        assert_eq!((listed, roles.as_str()), (1, "user,assistant"));
        let sid: String = eval(&h, "return S:info().session_id").await;
        assert_eq!(parent, sid);
        let (ratio, reserved): (f64, i64) = eval(
            &h,
            r#"local d = mare.config_describe({ "plugins.ui.widget_area_ratio", "plugins.ui.widget_reserved_rows" })
               return d["plugins.ui.widget_area_ratio"].default, d["plugins.ui.widget_reserved_rows"].default"#,
        )
        .await;
        let expected_40 = ((40.0 * ratio).floor() as i64 - reserved).max(1);
        let expected_10 = ((10.0 * ratio).floor() as i64 - reserved).max(1);
        assert_eq!(mh_40, expected_40, "widget render(width, max_height) at height=40");
        assert_eq!(mh_10, expected_10, "widget render(width, max_height) at height=10");
        assert!(mh_40 > mh_10, "larger viewport must allocate more widget height: {mh_40} vs {mh_10}");
        h.shutdown().await;
    })
    .await
}
