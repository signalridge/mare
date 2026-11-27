//! Regression tests for Lua session-level state isolation and lifecycle invariants.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn session_scoped_permission_rules_clear_on_session_transitions_and_preset_change() {
    local(async {
        let env = TestEnv::new().await;
        let saved_other = env.write_session(
            "other-ro.jsonl",
            &[
                json!({
                    "type": "message",
                    "entry_id": "m1",
                    "parent_entry_id": null,
                    "message": { "role": "user", "content": "hi" }
                }),
                json!({
                    "type": "custom",
                    "kind": "permission/preset",
                    "provenance": "session-host",
                    "data": { "preset": "read-only", "source": "user" }
                }),
            ],
        );

        let h = env
            .host()
            .builtins(&["permissions"])
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "read-only" }
                }
            }))
            .faux(json!([
                { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "one" } }] },
                { "text": "wrote-1" },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "two" } }] },
                { "text": "wrote-2" },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "c.txt", "content": "three" } }] },
                { "text": "wrote-3" },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "d.txt", "content": "four" } }] },
                { "text": "wrote-4" },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "e.txt", "content": "five" } }] },
                { "text": "wrote-5" }
            ]))
            .build_exposed()
            .await;

        global(&h, "OTHER_SESSION", saved_other.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            P = C.cordis:get("permissions")
            APPROVAL_ASKS = 0
            APPROVAL_MODE = "allow_amend"
            C.cordis:get("policy"):voter("tool/approval-host", function(_, _, verdict)
                APPROVAL_ASKS = APPROVAL_ASKS + 1
                if APPROVAL_MODE == "allow_amend" then
                    assert(verdict and verdict.amendment and verdict.amendment.expires == "session", "expected session-scoped amendment")
                    return {
                        decision = "allow",
                        grant = "allow_amend",
                        amendment = verdict.amendment,
                        source = "user",
                        human_approval = mare.crypto.random_hex(32),
                    }
                else
                    return {
                        decision = "deny",
                        source = "user",
                        human_approval = mare.crypto.random_hex(32),
                    }
                end
            end)
            "#,
        )
        .await;

        // 1. Grant session-scoped write rule via allow_amend.
        let r1 = h.prompt("write a", json!({})).await.unwrap();
        assert_eq!(r1["text"], "wrote-1", "{r1}");
        assert_eq!(eval::<usize>(&h, "return APPROVAL_ASKS").await, 1);

        // Within the same session, write is auto-allowed by P.session_rules.
        let r2 = h.prompt("write b", json!({})).await.unwrap();
        assert_eq!(r2["text"], "wrote-2", "{r2}");
        assert_eq!(eval::<usize>(&h, "return APPROVAL_ASKS").await, 1, "same-session rule should apply");

        // 2. After /new, the session rule MUST be cleared and write MUST ask again.
        exec(&h, r#"APPROVAL_MODE = "deny"; S:create()"#).await;
        let r3 = h.prompt("write c", json!({})).await.unwrap();
        assert_eq!(r3["text"], "wrote-3", "{r3}");
        assert_eq!(
            eval::<usize>(&h, "return APPROVAL_ASKS").await,
            2,
            "session-scoped permission rule must be cleared on /new"
        );
        assert!(!env.cwd.join("c.txt").exists(), "denied write after /new must not create file");

        // 3. Re-grant in Session 2, then /permission read-only (set preset) MUST clear it.
        exec(&h, r#"APPROVAL_MODE = "allow_amend""#).await;
        let _ = h.prompt("write d", json!({})).await.unwrap();
        assert_eq!(eval::<usize>(&h, "return APPROVAL_ASKS").await, 3);
        exec(&h, r#"APPROVAL_MODE = "deny"; assert(P:set("read-only"))"#).await;
        let _ = h.prompt("write e", json!({})).await.unwrap();
        assert_eq!(
            eval::<usize>(&h, "return APPROVAL_ASKS").await,
            4,
            "session-scoped permission rule must be cleared when preset is set"
        );
        assert!(!env.cwd.join("e.txt").exists(), "denied write after preset change must not create file");

        // 4. Re-grant, then /resume OTHER_SESSION MUST clear it.
        let h2 = env
            .host()
            .builtins(&["permissions"])
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "read-only" }
                }
            }))
            .faux(json!([
                { "tool_calls": [{ "name": "write", "arguments": { "path": "f.txt", "content": "six" } }] },
                { "text": "wrote-6" },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "g.txt", "content": "seven" } }] },
                { "text": "wrote-7" }
            ]))
            .build_exposed()
            .await;
        global(&h2, "OTHER_SESSION", saved_other.to_string_lossy().to_string());
        exec(
            &h2,
            r#"
            APPROVAL_ASKS = 0
            APPROVAL_MODE = "allow_amend"
            C.cordis:get("policy"):voter("tool/approval-host", function(_, _, verdict)
                APPROVAL_ASKS = APPROVAL_ASKS + 1
                if APPROVAL_MODE == "allow_amend" then
                    return {
                        decision = "allow",
                        grant = "allow_amend",
                        amendment = verdict and verdict.amendment or nil,
                        source = "user",
                        human_approval = mare.crypto.random_hex(32),
                    }
                else
                    return {
                        decision = "deny",
                        source = "user",
                        human_approval = mare.crypto.random_hex(32),
                    }
                end
            end)
            "#,
        )
        .await;
        let _ = h2.prompt("write f", json!({})).await.unwrap();
        assert_eq!(eval::<usize>(&h2, "return APPROVAL_ASKS").await, 1);
        exec(&h2, r#"APPROVAL_MODE = "deny"; S:load({ path = OTHER_SESSION })"#).await;
        let _ = h2.prompt("write g", json!({})).await.unwrap();
        assert_eq!(
            eval::<usize>(&h2, "return APPROVAL_ASKS").await,
            2,
            "session-scoped permission rule must be cleared on /resume"
        );
        assert!(!env.cwd.join("g.txt").exists(), "denied write after /resume must not create file");

        h2.shutdown().await;
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn rewind_after_compaction_and_before_item_id_semantics_and_snapshots_isolation() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("doc.txt"), "v1\n").unwrap();

        // Part A: session.rewind({ before_item_id = X }) moves leaf BEFORE X (to X.parent_entry_id).
        let h = env
            .host()
            .builtins(&["changes", "compact"])
            .faux(json!([
                { "text": "reply-1" },
                { "text": "reply-2" }
            ]))
            .build_exposed()
            .await;
        assert_eq!(h.prompt("prompt-1", json!({})).await.unwrap()["text"], "reply-1");
        assert_eq!(h.prompt("prompt-2", json!({})).await.unwrap()["text"], "reply-2");

        let (leaf_before_second, expected_first_assistant, msg_count_after_second, leaf_before_first): (
            String,
            String,
            usize,
            String,
        ) = eval(
            &h,
            r#"
            local sid = S:info().session_id
            local msgs = {}
            for _, e in ipairs(S:entries()) do
                if e.type == "message" and e.entry_id then
                    msgs[#msgs + 1] = e
                end
            end
            assert(#msgs >= 4, "expected 4 message entries")
            local u1, a1, u2 = msgs[1].entry_id, msgs[2].entry_id, msgs[3].entry_id
            local rew1 = S:rewind({ session_id = sid, before_item_id = u2 })
            local count1 = #A:list_messages().data
            local rew2 = S:rewind({ session_id = sid, before_item_id = u1 })
            return tostring(rew1.leaf_entry_id or ""), a1, count1, tostring(rew2.leaf_entry_id or "")
            "#,
        )
        .await;
        assert_eq!(
            leaf_before_second, expected_first_assistant,
            "session.rewind(before_item_id = u2) must land on u2's parent (a1), not u2 itself"
        );
        assert_eq!(msg_count_after_second, 2, "rewinding before u2 must keep only turn 1 (u1, a1)");
        assert_eq!(leaf_before_first, "", "rewinding before u1 must land on nil leaf");
        h.shutdown().await;

        // Part B: /rewind ("Restore files and the conversation") right after compaction must succeed
        // without tearing files from conversation, landing back on the compaction entry.
        let h2 = env
            .host()
            .builtins(&["changes", "compact", "commands"])
            .config(json!({
                "plugins": {
                    "compact": { "keep_recent_tokens": 1 }
                }
            }))
            .faux(json!([
                { "text": "turn-1-ok ".repeat(200) },
                { "text": "ok" },
                { "text": "short summary" },
                { "text": "short summary" },
                { "tool_calls": [{ "name": "read", "arguments": { "path": "doc.txt" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "doc.txt", "content": "v2\n" } }] },
                { "text": "turn-2-edited" }
            ]))
            .build_exposed()
            .await;
        let _ = h2.prompt(&"turn 1a ".repeat(200), json!({})).await.unwrap();
        let _ = h2.prompt("ok", json!({})).await.unwrap();
        let compaction_leaf: String = eval(
            &h2,
            r#"
            local K = C.cordis:get("compact")
            local res = K:run()
            assert(res and res.status == "compacted", mare.json.encode(res))
            return S:leaf()
            "#,
        )
        .await;
        // Consume any unused extra summary entry only if needed — check that leaf is a compaction entry.
        let leaf_type: String = eval(
            &h2,
            r#"
            local leaf = S:leaf()
            for _, e in ipairs(S:entries()) do
                if e.entry_id == leaf then return tostring(e.type) end
            end
            return ""
            "#,
        )
        .await;
        assert_eq!(leaf_type, "compaction", "expected compaction entry as S:leaf()");
        assert!(!compaction_leaf.is_empty());

        // Turn 2 starts right after the compaction entry and edits doc.txt.
        assert_eq!(h2.prompt("edit doc after compact", json!({})).await.unwrap()["text"], "turn-2-edited");
        assert_eq!(std::fs::read_to_string(env.cwd.join("doc.txt")).unwrap(), "v2\n");

        // Verify session.file_changes() sees doc.txt before /new, and clears after /new (Part C).
        let changes_before_new: usize = eval(&h2, "return #S:file_changes()").await;
        assert_eq!(changes_before_new, 1);

        // Run /rewind selecting the latest turn (item 1 in reverse list) and option 2 ("Restore files and the conversation").
        exec(
            &h2,
            r#"
            REWIND_NOTICES = {}
            local step = 0
            mare.ui.connected = function() return true end
            mare.ui.notify = function(msg) REWIND_NOTICES[#REWIND_NOTICES + 1] = tostring(msg) end
            mare.ui.set_editor = function() end
            mare.ui.select = function()
                step = step + 1
                if step == 1 then return 1 end
                return 2
            end
            C.cordis:get("commands"):run("/rewind")
            "#,
        )
        .await;

        wait_until(WAIT_SECS, "/rewind after compaction to restore file and leaf", || {
            let h2 = h2.clone();
            let env_doc = env.cwd.join("doc.txt");
            async move {
                std::fs::read_to_string(&env_doc).ok().as_deref() == Some("v1\n")
                    && eval::<bool>(&h2, "return #REWIND_NOTICES > 0").await
            }
        })
        .await;

        let (leaf_after_rewind, rewind_notices, file_changes_after_new): (String, String, usize) = eval(
            &h2,
            r#"
            local leaf = tostring(S:leaf() or "")
            local notes = table.concat(REWIND_NOTICES, "\n")
            S:create()
            return leaf, notes, #S:file_changes()
            "#,
        )
        .await;
        assert_eq!(
            leaf_after_rewind, compaction_leaf,
            "/rewind after compaction must rewind conversation leaf to compaction entry, notices: {rewind_notices}"
        );
        assert!(
            !rewind_notices.contains("conversation not rewound"),
            "must not tear files from conversation: {rewind_notices}"
        );
        assert_eq!(file_changes_after_new, 0, "S:file_changes() snapshots must clear on /new");
        h2.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn background_jobs_output_channel_and_persist_failed_gate_isolation() {
    local(async {
        let env = TestEnv::new().await;
        let started = env.cwd.join("bg_started.marker");
        let release = env.cwd.join("bg_release.gate");
        let marker = env.cwd.join("bg_done.marker");
        let started_sh = started.to_string_lossy().replace('\\', "/");
        let release_sh = release.to_string_lossy().replace('\\', "/");
        let marker_sh = marker.to_string_lossy().replace('\\', "/");
        // Gate completion on `release` rather than a short sleep so heavy load during
        // the second model turn cannot let the job exit before `S:create()` stops it.
        let cmd = format!(
            "touch '{started_sh}'; while [ ! -f '{release_sh}' ]; do sleep 0.05; done; echo leak-from-session-1; touch '{marker_sh}'"
        );
        let h = env
            .host()
            .builtins(&["jobs"])
            .faux(json!([
                {
                    "tool_calls": [{
                        "name": "bash",
                        "arguments": {
                            "description": "s1 background job",
                            "command": cmd,
                            "run_in_background": true
                        }
                    }]
                },
                { "text": "started-s1" },
                { "text": "unexpected-wake-s2" }
            ]))
            .build_exposed()
            .await;

        let res = h.prompt("start bg", json!({})).await.unwrap();
        assert_eq!(res["text"], "started-s1", "{res}");
        wait_until(WAIT_SECS, "Session 1 background job started", || async { started.exists() }).await;
        assert!(!marker.exists(), "marker must not exist before session switch");

        global(&h, "J", h.service_table("jobs").unwrap());
        let listed_in_s2: usize = eval(
            &h,
            r#"
            S:create()
            return #J:list().data
            "#,
        )
        .await;
        assert_eq!(listed_in_s2, 0, "Session 2 must not list Session 1's background jobs");

        // Unblock the gate AFTER switching sessions: if Session 1's background job had survived
        // `S:create()`, it would now wake up immediately, echo, and touch `marker`.
        std::fs::write(&release, "go").unwrap();

        // Session 1's background job must be stopped on session switch (never creating the marker),
        // and even after a 400ms wait Session 2 must receive no leaked notification or job record.
        let (s2_has_leak_msg, s2_has_s1_record): (bool, bool) = eval(
            &h,
            r#"
            local h_proc = mare.proc.start({ cmd = "sh", args = { "-c", "sleep 0.4; echo ok-s2" } })
            h_proc:wait({})
            local msgs = A:list_messages().data
            local leaked = false
            for _, m in ipairs(msgs) do
                local t = m.content and m.content[1] and m.content[1].text or ""
                if t:find("leak-from-session-1", 1, true) or t:find("s1 background job", 1, true) then
                    leaked = true
                end
            end
            local recs = require("mare_lib").all_records(S, "job") or {}
            return leaked, #recs > 0
            "#,
        )
        .await;
        assert!(!marker.exists(), "Session 1 background job should have been stopped on session switch");
        assert!(!s2_has_leak_msg, "Session 1 job completion must not notify or wake Session 2");
        assert!(!s2_has_s1_record, "Session 1 job end record must not be written into Session 2");
        h.shutdown().await;

        // Part B: output_channel checks session_id and session_epoch (not nil session.id/generation).
        // Part C: loop_tools closes output_gate on status == "persist_failed" (not "persist-failed").
        let h2 = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "name": "probe_out", "arguments": {} }] },
                { "text": "after-probe" },
                { "tool_calls": [{ "name": "late_sender", "arguments": {} }] },
                { "text": "after-late" }
            ]))
            .build_exposed()
            .await;
        exec(
            &h2,
            r#"
            PROBE_RES = {}
            HELD_CH = nil
            FAIL_COMPLETE = false
            T:register("probe_out", {
                description = "probe output channel session checks",
                side_effect = "read",
                parameters = { type = "object", properties = {} },
                execute = function()
                    local ch1 = assert(T:output_channel())
                    local orig_info = S.info
                    S.info = function(self)
                        local info = orig_info(self)
                        info.session_epoch = tostring(info.session_epoch or "") .. "-mutated"
                        return info
                    end
                    local r1 = T:send_output(ch1, { { type = "text", text = "hi" } })
                    S.info = orig_info

                    local ch2 = assert(T:output_channel())
                    S.info = function(self)
                        local info = orig_info(self)
                        info.session_id = tostring(info.session_id or "") .. "-other"
                        return info
                    end
                    local r2 = T:send_output(ch2, { { type = "text", text = "hi2" } })
                    S.info = orig_info
                    PROBE_RES = { r1 = r1.error or "", r2 = r2.error or "" }
                    return { content = "ok" }
                end,
            })
            T:register("late_sender", {
                description = "captures output channel",
                side_effect = "write",
                parameters = { type = "object", properties = {} },
                execute = function()
                    HELD_CH = assert(T:output_channel())
                    return { content = "ok" }
                end,
            })
            local orig_record = S.record_tool_state
            S.record_tool_state = function(self, opts)
                if FAIL_COMPLETE and type(opts) == "table" and opts.state == "completed" then
                    error("simulated disk full")
                end
                return orig_record(self, opts)
            end
            "#,
        )
        .await;

        let _ = h2.prompt("probe", json!({})).await.unwrap();
        let (err1, err2): (String, String) = eval(&h2, "return PROBE_RES.r1, PROBE_RES.r2").await;
        assert_eq!(err1, "session_changed", "output_channel must detect session_epoch change");
        assert_eq!(err2, "session_changed", "output_channel must detect session_id change");

        exec(&h2, "FAIL_COMPLETE = true").await;
        let _ = h2.prompt("late", json!({})).await.unwrap();
        let (late_status, late_error): (String, String) = eval(
            &h2,
            r#"
            assert(HELD_CH ~= nil, "expected output channel")
            local r = T:send_output(HELD_CH, { { type = "text", text = "late" } })
            return tostring(r.status), tostring(r.error or "")
            "#,
        )
        .await;
        assert_eq!(late_status, "rejected");
        assert_eq!(
            late_error, "initial_result_persist_failed",
            "persist_failed must close output_gate with initial_result_persist_failed"
        );
        h2.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn remote_compaction_reads_provider_id_and_model_id_on_model_switch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact"])
            .faux(json!([{ "text": "should-not-run-local-summary" }]))
            .build_exposed()
            .await;

        // Place a remote compaction message written with the real `provider_id`/`model_id` schema
        // matching the target model (`faux/echo`). With the old `remote.provider`/`remote.model` read,
        // `llm/before-model-change` treats it as incompatible and rejects when `source_unknown = true`.
        let (same_ok, same_err, diff_ok, diff_err, legacy_ok, legacy_err): (bool, String, bool, String, bool, String) =
            eval(
                &h,
                r#"
            local L = C.cordis:get("llm")
            local cur = L:current()
            A:set_messages({
                {
                    role = "user",
                    compaction = true,
                    content = { { type = "text", text = "portable summary" } },
                    remote = {
                        provider_id = cur.provider,
                        model_id = cur.id,
                        items = { { type = "compaction", encrypted_content = "opaque" } },
                    },
                },
            })
            local same_refused
            local same_ok, same_e = pcall(function()
                C.cordis:emit("llm/model-changing", {
                    model = cur,
                    previous = nil,
                    source_unknown = true,
                    refuse = function(msg)
                        same_refused = msg
                        error(msg, 0)
                    end,
                })
            end)

            -- Verify a mismatched model_id IS still treated as incompatible.
            A:set_messages({
                {
                    role = "user",
                    compaction = true,
                    content = { { type = "text", text = "portable summary" } },
                    remote = {
                        provider_id = cur.provider,
                        model_id = "different-model",
                        items = { { type = "compaction", encrypted_content = "opaque" } },
                    },
                },
            })
            local diff_refused
            local diff_ok, diff_e = pcall(function()
                C.cordis:emit("llm/model-changing", {
                    model = cur,
                    previous = nil,
                    source_unknown = true,
                    refuse = function(msg)
                        diff_refused = msg
                        error(msg, 0)
                    end,
                })
            end)

            -- Legacy `provider`/`model` fields must NOT be accepted as fallback.
            A:set_messages({
                {
                    role = "user",
                    compaction = true,
                    content = { { type = "text", text = "portable summary" } },
                    remote = {
                        provider = cur.provider,
                        model = cur.id,
                        items = { { type = "compaction", encrypted_content = "opaque" } },
                    },
                },
            })
            local legacy_refused
            local legacy_ok, legacy_e = pcall(function()
                C.cordis:emit("llm/model-changing", {
                    model = cur,
                    previous = nil,
                    source_unknown = true,
                    refuse = function(msg)
                        legacy_refused = msg
                        error(msg, 0)
                    end,
                })
            end)
            return same_ok, tostring(same_refused or same_e or ""),
                diff_ok, tostring(diff_refused or diff_e or ""),
                legacy_ok, tostring(legacy_refused or legacy_e or "")
            "#,
            )
            .await;
        assert!(same_ok, "matching remote.provider_id/model_id must be recognized as compatible (err: {same_err})");
        assert!(
            !diff_ok && diff_err.contains("Cannot switch"),
            "mismatched remote.model_id must still be treated as incompatible (ok={diff_ok}, err={diff_err})"
        );
        assert!(
            !legacy_ok && legacy_err.contains("Cannot switch"),
            "legacy remote.provider/model fields must not be accepted as fallback (ok={legacy_ok}, err={legacy_err})"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn new_session_resets_persistent_bash_and_shell_mode_uses_tools_cwd_and_isolates_sessions() {
    local(async {
        let env = TestEnv::new().await;
        let sub = env.cwd.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let leak_marker = env.cwd.join("shell_mode_done.marker");
        let slow_shell_cmd = format!("!sleep 0.25; echo shell-mode-leak; touch {}", leak_marker.to_string_lossy());
        let pwd_cmd = if cfg!(windows) { "pwd -W" } else { "pwd -P" };

        let h = env
            .host()
            .builtins(&["ui", "shell-mode", "jobs"])
            .config(json!({
                "plugins": {
                    "tools": {
                        "shell_session": "persistent",
                        "shell_buffer_bytes": 256
                    }
                }
            }))
            .faux(json!([
                {
                    "tool_calls": [{
                        "name": "bash",
                        "arguments": { "command": format!("cd sub && export MARE_TEST_LEAK=leaked && {pwd_cmd}") }
                    }]
                },
                { "text": "moved-to-sub" },
                {
                    "tool_calls": [{
                        "name": "bash",
                        "arguments": { "command": format!("printf 'PWD=%s\\nLEAK=%s\\n' \"$({pwd_cmd})\" \"${{MARE_TEST_LEAK:-}}\"") }
                    }]
                },
                { "text": "checked-after-new" },
                {
                    "tool_calls": [{
                        "name": "bash",
                        "arguments": { "command": "head -c 1024 /dev/zero | tr '\\0' 'A'" }
                    }]
                },
                { "text": "overflowed" }
            ]))
            .build_exposed()
            .await;

        // 1. cd sub && export MARE_TEST_LEAK=leaked in Session 1.
        assert_eq!(h.prompt("cd sub", json!({})).await.unwrap()["text"], "moved-to-sub");
        let cwd_before: String = eval(&h, "return T:cwd()").await;
        let canonical_root = dunce::canonicalize(&env.cwd).unwrap().to_string_lossy().to_string();
        let canonical_sub = dunce::canonicalize(&sub).unwrap().to_string_lossy().to_string();
        assert_eq!(cwd_before, canonical_sub);

        // 2. shell-mode `!pwd` while agent is busy: must run in tools:cwd() (`sub`) and not clear U.working(nil).
        global(&h, "SHELL_PWD_CMD", format!("!{pwd_cmd}"));
        let (shell_mode_pwd, cleared_while_busy): (String, bool) = eval(
            &h,
            r#"
            local UI = C.cordis:get("ui")
            local steered = nil
            local cleared_busy = false
            local orig_activity = A.activity
            local orig_steer = A.steer
            A.activity = function() return { busy = true } end
            A.steer = function(_, text) steered = text end
            mare.ui.connected = function() return true end
            mare.ui.working = function(msg)
                if msg == nil then cleared_busy = true end
            end
            mare.ui.push = function() return 1 end
            mare.ui.finish = function() end
            C.cordis:emit("input/submit", { text = SHELL_PWD_CMD, handled = false })
            local deadline = mare.task.clock() + 5000
            while steered == nil and mare.task.clock() < deadline do
                mare.task.sleep(10)
            end
            A.activity = orig_activity
            A.steer = orig_steer
            return tostring(steered or ""), cleared_busy
            "#,
        )
        .await;
        // On Windows Git Bash `pwd -W` emits forward slashes (`C:/...`); normalize separators for comparison.
        let expected_sub = if cfg!(windows) { canonical_sub.replace('\\', "/") } else { canonical_sub.clone() };
        let actual_shell_pwd = if cfg!(windows) { shell_mode_pwd.replace('\\', "/") } else { shell_mode_pwd.clone() };
        assert!(
            actual_shell_pwd.contains(&expected_sub),
            "shell-mode must run in tools:cwd() ({canonical_sub}), got: {shell_mode_pwd}"
        );
        assert!(!cleared_while_busy, "shell-mode must not clear U.working(nil) while agent is busy");

        // 3. Start slow `!sleep ...` in Session 1, immediately call S:create().
        global(&h, "SLOW_SHELL_CMD", slow_shell_cmd);
        exec(
            &h,
            r#"
            C.cordis:emit("input/submit", { text = SLOW_SHELL_CMD, handled = false })
            S:create()
            "#,
        )
        .await;

        // 4. Verify tools:cwd() reset immediately on S:new(), and next bash has root pwd & empty env.
        let cwd_after_new: String = eval(&h, "return T:cwd()").await;
        assert_eq!(cwd_after_new, canonical_root, "/new must reset tools:cwd() to session root");

        assert_eq!(h.prompt("check env", json!({})).await.unwrap()["text"], "checked-after-new");
        let after_new_result = tool_results(&h).await;
        let pwd_line = after_new_result.lines().find_map(|l| l.strip_prefix("PWD=")).unwrap_or("");
        let leak_line = after_new_result.lines().find_map(|l| l.strip_prefix("LEAK=")).unwrap_or("MISSING");
        let actual_root = dunce::canonicalize(pwd_line)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| pwd_line.to_string());
        assert_eq!(actual_root, canonical_root, "/new must reset persistent shell pwd: {after_new_result}");
        assert_eq!(leak_line, "", "/new must reset persistent shell env: {after_new_result}");

        // 5. Verify persistent bash output overflow preserves spill file even when #out < MAX_BYTES.
        assert_eq!(h.prompt("overflow", json!({})).await.unwrap()["text"], "overflowed");
        let (overflow_has_note, spill_exists): (bool, bool) = eval(
            &h,
            r#"
            local msgs = A:list_messages().data
            local last_tr = ""
            for i = #msgs, 1, -1 do
                if msgs[i].role == "tool_result" then
                    last_tr = msgs[i].content[1].text or ""
                    break
                end
            end
            local path = last_tr:match("%[full output[^:]*:%s*([^%]]+)%]")
            local ok = false
            if path then
                local st = mare.fs.stat(path)
                ok = st ~= nil and st.kind == "file" and (st.size or 0) >= 1024
            end
            return last_tr:find("only in the file", 1, true) ~= nil, ok
            "#,
        )
        .await;
        assert!(
            overflow_has_note && spill_exists,
            "persistent bash overflow must report dropped bytes and preserve full spill file"
        );

        // 6. Wait for slow shell-mode command from Session 1 to finish (or verify it was cancelled) and check Session 2 is clean.
        wait_until(WAIT_SECS, "Session 1 shell-mode command to finish or be cancelled", || {
            let h = h.clone();
            let marker = leak_marker.clone();
            async move { marker.exists() || eval::<bool>(&h, "return true").await }
        })
        .await;
        let s2_leaked: bool = eval(
            &h,
            r#"
            local h_proc = mare.proc.start({ cmd = "sh", args = { "-c", "sleep 0.35" } })
            h_proc:wait({})
            for _, m in ipairs(A:list_messages().data) do
                local t = m.content and m.content[1] and m.content[1].text or ""
                if t:find("shell-mode-leak", 1, true) then
                    return true
                end
            end
            return false
            "#,
        )
        .await;
        assert!(!s2_leaked, "shell-mode command started in Session 1 must not append into Session 2");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn parent_sandbox_tightening_propagates_to_running_subagent_ceiling() {
    local(async {
        let env = TestEnv::new().await;
        let first_file = env.cwd.join("child-first.txt");
        let second_file = env.cwd.join("child-second.txt");
        let gate_file = env.cwd.join("parent-tightened.gate");

        // Install a tiny child-side plugin that waits on `parent-tightened.gate` after the first
        // write succeeds, then attempts a second write in the same running child VM.
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("s3-probe.lua"),
            format!(
                r#"
return {{
    name = "s3-probe",
    inject = {{ "tools" }},
    apply = function(ctx)
        if mare.mode ~= "child" then return end
        ctx.tools:register("wait_for_gate", {{
            description = "Wait until parent signals sandbox tightening",
            side_effect = "read",
            parameters = {{ type = "object", properties = {{}} }},
            execute = function()
                local gate = {gate:?}
                local deadline = mare.task.clock() + 10000
                while not mare.fs.exists(gate) and mare.task.clock() < deadline do
                    mare.task.sleep(20)
                end
                local cur = mare.proc.sandbox.current()
                return {{ content = "level=" .. tostring(cur and cur.level or "none") }}
            end,
        }})
    end,
}}
"#,
                gate = gate_file.to_string_lossy().to_string()
            ),
        )
        .unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "child-first.txt", "content": "ok-1" } }] },
            { "tool_calls": [{ "name": "wait_for_gate", "arguments": {} }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "child-second.txt", "content": "should-fail" } }] },
            { "text": "child-finished" }
        ]);

        let h = env
            .host()
            .builtins(&["permissions", "subagent"])
            .user_plugins()
            .project_trusted(true)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" }
                }
            }))
            .faux(script)
            .build_exposed()
            .await;

        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            P = C.cordis:get("permissions")
            CHILD = mare.subagent.start({
                cwd = CWD,
                prompt = "run probe",
                no_session = true,
            })
            "#,
        )
        .await;

        // Wait until child has written child-first.txt under `auto`.
        wait_until(WAIT_SECS, "child first write under auto", || {
            let p = first_file.clone();
            async move { p.exists() }
        })
        .await;

        // Tighten parent preset from `auto` to `read-only`, then release child's gate.
        exec(&h, r#"assert(P:set("read-only"))"#).await;
        std::fs::write(&gate_file, "go").unwrap();

        let (child_ok, child_msgs): (bool, String) = eval(
            &h,
            r#"
            local ok, res = pcall(CHILD.wait, CHILD)
            return ok, mare.json.encode(res)
            "#,
        )
        .await;
        assert!(child_ok, "child wait failed: {child_msgs}");
        assert!(
            child_msgs.contains("level=read-only"),
            "running child VM must observe tightened read-only sandbox immediately: {child_msgs}"
        );
        assert!(
            !second_file.exists(),
            "running child VM must not be allowed to write after parent tightened to read-only: {child_msgs}"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn session_switch_drains_terminal_and_proc_before_publish_without_poison() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["permissions", "terminal", "proc", "jobs"])
            .project_trusted(true)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "read-only" }
                }
            }))
            .faux(json!([{ "text": "after-switch-ok" }]))
            .build_exposed()
            .await;

        global(&h, "TERM", h.service_table("terminal").unwrap());
        global(&h, "PROC", h.service_table("proc").unwrap());
        global(&h, "P", h.service_table("permissions").unwrap());

        let (term_count_after, proc_unknown_after, switch_ok, switch_err): (usize, bool, bool, String) = eval(
            &h,
            r#"
            -- Move off the initial preset (`read-only` -> `auto`), then open both a persistent
            -- terminal PTY and a proc child process.
            assert(P:set("auto"))
            local t = TERM:create({ cmd = "sh", args = { "-c", "sleep 30" } })
            assert(t and t.terminal_id, "expected terminal session")
            local p = PROC:spawn({ process_id = "proc-drain-test", command = { "sh", "-c", "sleep 30" } })
            assert(p and p.process_id == "proc-drain-test", "expected proc process")

            -- Switching via S:create() must drain terminal + proc BEFORE publish/sandbox reset,
            -- avoiding PTY_POLICY_BUSY and VM poison.
            local ok, err = pcall(S.create, S)
            local list = TERM:list()
            local count = #(list and list.data or {})
            local pok, perr = pcall(PROC.kill, PROC, { process_id = p.process_id })
            local proc_gone = (not pok) and tostring(perr):find("unknown_process", 1, true) ~= nil
            return count, proc_gone, ok, tostring(err or "")
            "#,
        )
        .await;

        assert!(switch_ok, "S:create() with open terminal + proc must not fail or poison: {switch_err}");
        assert_eq!(term_count_after, 0, "old session terminal must be drained before publish");
        assert!(proc_unknown_after, "old session proc child must be closed before publish");

        // Verify the host is NOT poisoned and still handles prompts cleanly.
        let reply = h.prompt("still alive?", json!({})).await.unwrap();
        assert_eq!(reply["text"], "after-switch-ok", "{reply}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn revoking_workspace_trust_cancels_running_subagents() {
    local(async {
        let env = TestEnv::new().await;
        let first_file = env.cwd.join("trusted-first.txt");
        let second_file = env.cwd.join("trusted-second.txt");
        let gate_file = env.cwd.join("trust-revoked.gate");

        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("m3-probe.lua"),
            format!(
                r#"
return {{
    name = "m3-probe",
    inject = {{ "tools" }},
    apply = function(ctx)
        if mare.mode ~= "child" then return end
        ctx.tools:register("wait_for_trust_gate", {{
            description = "Wait until gate appears",
            side_effect = "read",
            parameters = {{ type = "object", properties = {{}} }},
            execute = function()
                local gate = {gate:?}
                local deadline = mare.task.clock() + 10000
                while not mare.fs.exists(gate) and mare.task.clock() < deadline do
                    mare.task.sleep(20)
                end
                return {{ content = "gate-released" }}
            end,
        }})
    end,
}}
"#,
                gate = gate_file.to_string_lossy().to_string()
            ),
        )
        .unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "trusted-first.txt", "content": "ok" } }] },
            { "tool_calls": [{ "name": "wait_for_trust_gate", "arguments": {} }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "trusted-second.txt", "content": "bad" } }] },
            { "text": "should-not-finish" }
        ]);

        let h = env
            .host()
            .builtins(&["permissions", "subagent"])
            .user_plugins()
            .project_trusted(true)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" }
                }
            }))
            .faux(script)
            .build_exposed()
            .await;

        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            CHILD = mare.subagent.start({
                cwd = CWD,
                prompt = "run trust probe",
                no_session = true,
            })
            "#,
        )
        .await;

        wait_until(WAIT_SECS, "trusted child first write", || {
            let p = first_file.clone();
            async move { p.exists() }
        })
        .await;

        // Revoke trust on the parent host while the child is still running.
        h.set_workspace_trust(false, false).await.unwrap();
        std::fs::write(&gate_file, "go").unwrap();

        let (child_ok, child_err): (bool, String) = eval(
            &h,
            r#"
            local ok, res = pcall(CHILD.wait, CHILD)
            return ok, tostring(res)
            "#,
        )
        .await;
        assert!(
            !child_ok && child_err.contains("cancelled"),
            "revoking workspace trust must cancel running subagents (ok={child_ok}, err={child_err})"
        );
        assert!(!second_file.exists(), "cancelled subagent must not execute writes after trust revocation");
        h.shutdown().await;
    })
    .await;
}

/// External sandbox ceiling tightening via `PolicyHandle::tighten_ceiling` (as performed by a parent
/// host on a running child VM) bumps `mare.proc.sandbox.generation()` without emitting `sandbox/changed`.
/// The child VM's Lua `permissions` plugin must detect the generation change at vote/query entry,
/// clear `session_cache`/grants, demote `P.preset`, and stop auto-allowing unconfined tools.
#[tokio::test]
async fn external_policy_handle_tightening_syncs_lua_permissions_preset_and_blocks_unconfined_tools() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["permissions"])
            .project_trusted(true)
            .no_session()
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "full-access" }
                }
            }))
            .faux(json!([]))
            .build_exposed()
            .await;

        let (before_preset, before_vote): (String, String) = eval(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            local call = { id = "u1", name = "mcp_write_probe", arguments = {} }
            local info = { side_effect = "write", unconfined = true }
            local v = P:check(call, info)
            return P:preset(), v.decision
            "#,
        )
        .await;
        assert_eq!(before_preset, "full-access");
        assert_eq!(before_vote, "allow");

        // Tighten this VM's PolicyHandle directly (exact mechanism used by parent -> child VM).
        let handle = mare_binding::ossandbox::policy_handle(h.lua()).expect("policy_handle installed");
        let ro = mare_binding::ossandbox::Policy {
            level: mare_binding::ossandbox::Level::ReadOnly,
            root: env.cwd.clone(),
            extra: vec![],
            root_writable: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            writable: vec![],
            readable: vec![],
            deny_read: vec![],
            restrict_read: false,
            reads_pinned: false,
            network: mare_binding::ossandbox::Network::Open,
            profile: None,
            unix_sockets: vec![],
            accept_partial: vec![],
            project_protected: vec![],
        };
        assert!(handle.tighten_ceiling(&ro), "expected tighten_ceiling to report change");

        let (after_preset, after_vote): (String, String) = eval(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            local call = { id = "u2", name = "mcp_write_probe", arguments = {} }
            local info = { side_effect = "write", unconfined = true }
            local v = P:check(call, info)
            return P:preset(), v.decision
            "#,
        )
        .await;
        assert_eq!(after_preset, "read-only", "external tighten to read-only must demote Lua preset");
        assert_eq!(
            after_vote, "ask",
            "unconfined write tool must no longer auto-allow after external tighten to read-only"
        );
        h.shutdown().await;
    })
    .await;
}

/// If a VM's host-enforced inherited sandbox ceiling is stricter than a preset passed to
/// `permissions:set` (for example when a parent tightens a child while it is starting up),
/// `permissions:set` must reconcile `P:preset()` with `mare.proc.sandbox.current()` so
/// authorization (`vote.lua`) and sandbox enforcement (`exec_ctx` / `request.lua`) stay aligned.
#[tokio::test]
async fn set_preset_reconciles_lua_preset_when_host_ceiling_clamps_sandbox() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["permissions"])
            .project_trusted(true)
            .no_session()
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" }
                }
            }))
            .faux(json!([]))
            .build_exposed()
            .await;

        let ro = mare_binding::ossandbox::Policy {
            level: mare_binding::ossandbox::Level::ReadOnly,
            root: env.cwd.clone(),
            extra: vec![],
            root_writable: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            writable: vec![],
            readable: vec![],
            deny_read: vec![],
            restrict_read: false,
            reads_pinned: false,
            network: mare_binding::ossandbox::Network::Open,
            profile: None,
            unix_sockets: vec![],
            accept_partial: vec![],
            project_protected: vec![],
        };
        mare_binding::ossandbox::set_inherited_ceiling(h.lua(), Some(ro));

        let (returned, live_preset, vote_decision): (String, String, String) = eval(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            local ret = assert(P:set("full-access"))
            local v = P:check({ id = "w1", name = "write", arguments = { path = "out.txt", content = "hi" } }, { side_effect = "write" })
            return type(ret) == "table" and tostring(ret.preset or "") or tostring(ret), P:preset(), v.decision
            "#,
        )
        .await;

        assert_eq!(returned, "read-only");
        assert_eq!(live_preset, "read-only");
        assert_eq!(vote_decision, "ask");
        h.shutdown().await;
    })
    .await;
}

/// Custom presets whose approval behavior is stricter than `auto` (such as `approval-read`:
/// sandbox `danger-full-access`, approval `like = "read-only"`) must never be demoted to `auto`
/// when their sandbox is clamped to `workspace-write`, because doing so would relax workspace
/// write approvals from `ask` to `allow`.
#[tokio::test]
async fn clamping_custom_approval_read_preset_to_workspace_write_preserves_write_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["permissions"])
            .project_trusted(true)
            .no_session()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "approval-read",
                        "presets": {
                            "approval-read": { "like": "read-only", "level": "danger-full-access" }
                        }
                    }
                }
            }))
            .faux(json!([]))
            .build_exposed()
            .await;

        let ws_spec: mlua::Value = h
            .lua()
            .load(r#"return { level = "workspace-write", network = true, accept_partial = { "fs_protect" } }"#)
            .eval()
            .unwrap();
        let ws = mare_binding::ossandbox::Policy::from_lua(h.lua(), &ws_spec, &env.cwd).unwrap().unwrap();

        // 1. External tightening while running `approval-read` clamps sandbox to workspace-write.
        let handle = mare_binding::ossandbox::policy_handle(h.lua()).expect("policy_handle installed");
        assert!(handle.tighten_ceiling(&ws));
        let (ext_preset, ext_level, ext_vote): (String, String, String) = eval(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            local v = P:check({ id = "w1", name = "write", arguments = { path = "out.txt", content = "hi" } }, { side_effect = "write" })
            return P:preset(), mare.proc.sandbox.current().level, v.decision
            "#,
        )
        .await;
        assert_eq!(ext_preset, "custom", "must not display AUTO when approval-read is clamped");
        assert_eq!(ext_level, "workspace-write");
        assert_eq!(ext_vote, "ask", "clamped approval-read must still ask before workspace writes");

        // 2. Calling P:set("approval-read") under an already-installed workspace-write ceiling
        // must also keep `custom` and preserve `ask` for workspace writes.
        let (set_ret, set_preset, set_vote): (String, String, String) = eval(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            local ret = assert(P:set("approval-read"))
            local v = P:check({ id = "w2", name = "write", arguments = { path = "out2.txt", content = "hi" } }, { side_effect = "write" })
            return tostring(ret.preset), P:preset(), v.decision
            "#,
        )
        .await;
        assert_eq!(set_ret, "custom");
        assert_eq!(set_preset, "custom");
        assert_eq!(set_vote, "ask", "P:set(approval-read) under workspace-write ceiling must still ask on write");
        h.shutdown().await;
    })
    .await;
}
