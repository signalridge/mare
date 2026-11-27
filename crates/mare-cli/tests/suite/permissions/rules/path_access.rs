//! Path rules through the real guarded tool pipeline, including on-disk effects.

use super::*;

fn write_rules(env: &TestEnv, rules: Value) -> String {
    let path = env.home.path().join("state/permissions.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = json!({ "version": 1, "rules": rules, "note": "preserve me" }).to_string();
    std::fs::write(path, &original).unwrap();
    original
}

// A denied path must not cause any write, as in reference implementation
// packages/fs/fs-sandbox/tests/fs-sandbox.spec.ts:78 and
// packages/fs/tool-fs/tests/patch/preflight.spec.ts:1-28.
#[tokio::test]
async fn path_allow_does_not_cover_sibling() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("docs")).unwrap();
        std::fs::create_dir_all(env.cwd.join("src")).unwrap();
        let original = write_rules(
            &env,
            json!([
                { "tool": "write", "path": "docs/**", "op": "read", "decision": "allow" },
                { "tool": "write", "path": "docs/**", "op": "write", "decision": "allow" }
            ]),
        );
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (allowed, denied): (bool, bool) = eval(
            &h,
            r#"
            local inside = T:execute({ id = "inside", name = "write",
                arguments = { path = "docs/inside.txt", content = "inside" } })
            local outside = T:execute({ id = "outside", name = "write",
                arguments = { path = "src/outside.txt", content = "outside" } })
            return inside.is_error ~= true, outside.refused == true
            "#,
        )
        .await;
        h.shutdown().await;
        assert!(allowed, "a fully covered write must be allowed");
        assert_eq!(std::fs::read_to_string(env.cwd.join("docs/inside.txt")).unwrap(), "inside");
        assert!(denied, "a sibling must not inherit the path-scoped allow");
        assert!(!env.cwd.join("src/outside.txt").exists());
        assert_eq!(std::fs::read_to_string(env.home.path().join("state/permissions.json")).unwrap(), original);
    })
    .await
}

#[tokio::test]
async fn multi_access_needs_every_entry_and_operation() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("docs")).unwrap();
        std::fs::create_dir_all(env.cwd.join("src")).unwrap();
        std::fs::write(env.cwd.join("docs/a.txt"), "old a\n").unwrap();
        std::fs::write(env.cwd.join("src/b.txt"), "old b\n").unwrap();
        write_rules(
            &env,
            json!([
                { "tool": "apply_patch", "path": "docs/**", "op": "read", "decision": "allow" },
                { "tool": "apply_patch", "path": "docs/**", "op": "write", "decision": "allow" }
            ]),
        );
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (first_ok, denied, asked): (bool, bool, i64) = eval(
            &h,
            r#"
            for i, path in ipairs({ "docs/a.txt", "src/b.txt" }) do
                local r = T:execute({ id = "read-" .. i, name = "read", arguments = { path = path } })
                assert(r.is_error ~= true, tostring(r.content))
            end
            local first = T:execute({ id = "patch-inside", name = "apply_patch", arguments = { input =
                "*** Begin Patch\n*** Update File: docs/a.txt\n@@\n-old a\n+new a\n*** End Patch" } })
            local asked = 0
            local both = T:execute({ id = "patch-both", name = "apply_patch", arguments = { input =
                "*** Begin Patch\n*** Update File: docs/a.txt\n@@\n-new a\n+again a\n*** Update File: src/b.txt\n@@\n-old b\n+new b\n*** End Patch" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return first.is_error ~= true, both.refused == true, asked
            "#,
        )
        .await;
        h.shutdown().await;
        assert!(first_ok, "the completely covered patch must execute");
        assert!(denied && asked == 1, "a partially covered patch must be presented for approval");
        assert_eq!(std::fs::read_to_string(env.cwd.join("docs/a.txt")).unwrap(), "new a\n");
        assert_eq!(std::fs::read_to_string(env.cwd.join("src/b.txt")).unwrap(), "old b\n");
    })
    .await
}

#[tokio::test]
async fn multi_access_does_not_inherit_another_paths_scope() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.txt");
        std::fs::create_dir_all(env.cwd.join("docs")).unwrap();
        std::fs::write(env.cwd.join("docs/inside.txt"), "old inside\n").unwrap();
        std::fs::write(&outside_file, "old outside\n").unwrap();
        write_rules(
            &env,
            json!([
                { "tool": "apply_patch", "path": "docs/**", "decision": "allow" },
                { "tool": "apply_patch", "path": outside_file.to_string_lossy(), "scope": "all", "decision": "allow" }
            ]),
        );
        let h = env
            .host()
            .config(json!({ "plugins": {
            "preset": "minimal",
            "permissions": { "enabled": true, "preset": "read-only" },
            "tools-apply-patch": { "enabled": true }
        } }))
            .args(json!({ "no_session": true }))
            .project_trusted(true)
            .build_exposed()
            .await;
        global(&h, "OUTSIDE_FILE", outside_file.to_string_lossy().to_string());
        exec(&h, VOTE).await;
        let (single_decision, single_full, mixed_decision, mixed_full, reverse_full): (
            String,
            bool,
            String,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
            local inside = mare.fs.resolve("docs/inside.txt")
            assert(T:get("apply_patch"), "the multi-file tool must be present")
            local function item(path)
                return { kind = "file", op = "write", path = path, logical = path, physical = path, recursive = false }
            end
            local call = { id = "mixed-scopes", name = "apply_patch", arguments = {}, cwd = mare.cwd }
            local function vote(access)
                return VOTE(call, { side_effect = "write", access_state = "declared", access = access })
            end
            local single = vote({ item(OUTSIDE_FILE) })
            local mixed = vote({ item(inside), item(OUTSIDE_FILE) })
            local reverse = vote({ item(OUTSIDE_FILE), item(inside) })
            return single.decision, single.sandbox_grant == "danger-full-access",
                mixed.decision, mixed.sandbox_grant == "danger-full-access", reverse.sandbox_grant == "danger-full-access"
        "#,
        )
        .await;
        assert_eq!(single_decision, "allow");
        assert!(single_full, "the scoped rule alone must grant its explicit scope");
        assert_eq!(mixed_decision, "allow");
        assert!(
            !mixed_full && !reverse_full,
            "one path's unsandboxed scope must not apply to the entire multi-file call"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn malformed_persisted_allow_rules_never_authorize_a_write() {
    local(async {
        for rule in [
            json!({ "tool": "write", "path": {}, "decision": "allow" }),
            json!({ "tool": "write", "op": "execute", "decision": "allow" }),
            json!({ "tool": "write", "future_condition": "required", "decision": "allow" }),
            json!({ "tool": "write", "deny_args": [3], "decision": "allow" }),
        ] {
            let env = TestEnv::new().await;
            let original = write_rules(&env, json!([rule]));
            let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
            let denied: bool = eval(
                &h,
                r#"local r = T:execute({ id = "invalid", name = "write",
                    arguments = { path = "must-not-exist", content = "bad" } })
                   return r.refused == true"#,
            )
            .await;
            h.shutdown().await;
            assert!(denied, "invalid rule must not authorize: {rule}");
            assert!(!env.cwd.join("must-not-exist").exists(), "invalid rule wrote a file: {rule}");
            assert_eq!(std::fs::read_to_string(env.home.path().join("state/permissions.json")).unwrap(), original);
        }
    })
    .await
}

#[tokio::test]
async fn invalid_denial_rules_fail_closed_and_dynamic_source_is_rejected() {
    local(async {
        let env = TestEnv::new().await;
        let original = write_rules(&env, json!([{ "decision": "forbidden", "future_condition": "required" }]));
        let h = host(&env, true, json!({ "preset": "full-access" }), json!({ "no_session": true })).await;
        let (denied, rejected, bad_decision_rejected, missing_decision_rejected): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            local result = T:execute({ id = "invalid-deny", name = "write",
                arguments = { path = "must-not-exist", content = "bad" } })
            local ok = pcall(function()
                P:add_rules({ { tool = "write", decision = "allow", future_condition = "required" } })
            end)
            local ok_typo = pcall(function()
                P:add_rules({ { tool = "write", decision = "forbiden" } })
            end)
            local ok_missing = pcall(function()
                P:add_rules({ { tool = "write" } })
            end)
            return result.refused == true, not ok, not ok_typo, not ok_missing
            "#,
        )
        .await;
        h.shutdown().await;
        assert!(denied, "an unrecognized denial without a known tool must deny all tools");
        assert!(rejected, "a dynamic rule with an unrecognized condition must be rejected immediately");
        assert!(bad_decision_rejected, "a dynamic rule with a misspelled decision must be rejected immediately");
        assert!(missing_decision_rejected, "a dynamic rule without a decision must be rejected immediately");
        assert!(!env.cwd.join("must-not-exist").exists());
        assert_eq!(std::fs::read_to_string(env.home.path().join("state/permissions.json")).unwrap(), original);

        write_rules(&env, json!([{ "tool": "write", "cwd": "[invalid", "decision": "forbidden" }]));
        let h = host(&env, true, json!({ "preset": "full-access" }), json!({ "no_session": true })).await;
        let denied: bool = eval(
            &h,
            r#"local r = T:execute({ id = "invalid-glob", name = "write",
            arguments = { path = "bad-glob.txt", content = "bad" } }); return r.refused == true"#,
        )
        .await;
        h.shutdown().await;
        assert!(denied, "an invalid cwd glob on a deny rule must not be ignored");
        assert!(!env.cwd.join("bad-glob.txt").exists());

        std::fs::write(env.home.path().join("state/permissions.json"), r#"{"version":1,"rules":{}}"#).unwrap();
        let h = host(&env, true, json!({ "preset": "full-access" }), json!({ "no_session": true })).await;
        let denied: bool = eval(
            &h,
            r#"local r = T:execute({ id = "invalid-source", name = "write",
            arguments = { path = "bad-source.txt", content = "bad" } }); return r.refused == true"#,
        )
        .await;
        h.shutdown().await;
        assert!(denied, "a JSON object must not be mistaken for an empty rules array");
        assert!(!env.cwd.join("bad-source.txt").exists());
    })
    .await
}

#[tokio::test]
async fn malformed_rule_reload_does_not_keep_a_stale_allow() {
    local(async {
        let env = TestEnv::new().await;
        write_rules(&env, json!([{ "tool": "write", "decision": "allow" }]));
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let first: bool = eval(
            &h,
            r#"
            local r = T:execute({ id = "before-reload", name = "write",
                arguments = { path = "before.txt", content = "before" } })
            return r.is_error ~= true
        "#,
        )
        .await;
        assert!(first && env.cwd.join("before.txt").exists(), "valid rule must authorize the control write");

        let file = env.home.path().join("state/permissions.json");
        let changed = json!({ "version": 99, "rules": [{ "tool": "write", "decision": "forbidden" }] }).to_string();
        std::fs::write(&file, &changed).unwrap();
        let (refused, decision): (bool, String) = eval(
            &h,
            r#"
            local call = { id = "after-reload", name = "write",
                arguments = { path = "after.txt", content = "after" } }
            local r = T:execute(call)
            local verdict = P:check(call, { side_effect = "write" })
            return r.refused == true, verdict.decision
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(refused, "an unreadable new rule source must not leave the stale allow in effect");
        assert_eq!(decision, "deny", "a retry loop must not mask a stale cached allow");
        assert!(!env.cwd.join("after.txt").exists());
        assert_eq!(std::fs::read_to_string(file).unwrap(), changed);
    })
    .await
}

#[tokio::test]
async fn recursive_overlap_is_conservative() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("docs/private")).unwrap();
        write_rules(&env, json!([{ "tool": "tree_writer", "path": "docs/private/*", "op": "write", "decision": "allow" }]));
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (refused, asked): (bool, i64) = eval(&h, r#"
            T:register("tree_writer", {
                description = "write a file in a tree", side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                access = function(a) return { { kind = "file", op = "write", path = a.path, recursive = true } } end,
                execute = function(a) assert(mare.fs.write(a.path .. "/private/proof", "bad")); return { content = "done" } end,
            })
            local asked = 0
            local r = T:execute({ id = "tree", name = "tree_writer", arguments = { path = "docs" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#).await;
        h.shutdown().await;
        assert!(refused && asked == 1, "a partial child glob cannot authorize recursive access");
        assert!(!env.cwd.join("docs/private/proof").exists());

        write_rules(
            &env,
            json!([
                { "tool": "tree_writer", "path": "docs/**", "op": "write", "decision": "allow" },
                { "tool": "tree_writer", "path": "docs/private/**", "op": "write", "decision": "forbidden" }
            ]),
        );
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (forbid_refused, forbid_asked): (bool, i64) = eval(
            &h,
            r#"
            T:register("tree_writer", {
                description = "write a file in a tree", side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                access = function(a) return { { kind = "file", op = "write", path = a.path, recursive = true } } end,
                execute = function(a) assert(mare.fs.write(a.path .. "/private/proof", "bad")); return { content = "done" } end,
            })
            local asked = 0
            local r = T:execute({ id = "tree-forbid", name = "tree_writer", arguments = { path = "docs" } }, {
                approve = function() asked = asked + 1; return { decision = "allow", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        assert!(forbid_refused && forbid_asked == 0, "recursive allow overlapping child forbidden must deny without asking");
        assert!(!env.cwd.join("docs/private/proof").exists());

        write_rules(
            &env,
            json!([
                { "tool": "tree_writer", "path": "docs/**", "op": "write", "decision": "allow" },
                { "tool": "tree_writer", "path": "docs/private/**", "op": "write", "decision": "prompt" }
            ]),
        );
        let (prompt_refused, prompt_asked): (bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local r = T:execute({ id = "tree-prompt", name = "tree_writer", arguments = { path = "docs" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(prompt_refused && prompt_asked == 1, "recursive allow overlapping child prompt must ask");
        assert!(!env.cwd.join("docs/private/proof").exists());
    })
    .await
}

#[tokio::test]
async fn same_size_same_mtime_rule_replacement_invalidates_allow() {
    local(async {
        let env = TestEnv::new().await;
        let file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let allow = r#"{"version":1,"rules":[{"tool":"write","decision":"allow"}],"pad":"xxxx"}"#;
        let deny = r#"{"version":1,"rules":[{"tool":"write","decision":"forbidden"}],"pad":""}"#;
        assert_eq!(allow.len(), deny.len());
        std::fs::write(&file, allow).unwrap();
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let first: bool = eval(
            &h,
            r#"local r = T:execute({ id = "before", name = "write",
            arguments = { path = "before.txt", content = "before" } }); return r.is_error ~= true"#,
        )
        .await;
        assert!(first && env.cwd.join("before.txt").exists());
        std::fs::write(&file, deny).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let refused: bool = eval(
            &h,
            r#"local r = T:execute({ id = "after", name = "write",
            arguments = { path = "after.txt", content = "after" } }); return r.refused == true"#,
        )
        .await;
        h.shutdown().await;
        assert!(refused, "same-sized replacement with restored mtime must not keep the old allow");
        assert!(!env.cwd.join("after.txt").exists());
    })
    .await
}

#[tokio::test]
async fn unknown_access_and_empty_access_do_not_borrow_path_allow() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("docs")).unwrap();
        write_rules(&env, json!([{ "tool": "opaque", "path": "docs/**", "op": "write", "decision": "allow" }]));
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (unknown, empty, asked): (bool, bool, i64) = eval(&h, r#"
            local function register(access)
                T:register("opaque", { description = "write fixture", side_effect = "write",
                    parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                    access = access, execute = function(a) assert(mare.fs.write(a.path, "bad")); return { content = "done" } end })
            end
            local asked = 0
            local function run(id, path)
                return T:execute({ id = id, name = "opaque", arguments = { path = path } }, {
                    approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
                })
            end
            register(nil)
            local unknown = run("unknown", "docs/unknown.txt")
            register(function() return {} end)
            local empty = run("empty", "docs/empty.txt")
            return unknown.refused == true, empty.refused == true, asked
        "#).await;
        h.shutdown().await;
        assert!(unknown && empty && asked == 2, "unknown and empty access cannot satisfy a path allow");
        assert!(!env.cwd.join("docs/unknown.txt").exists());
        assert!(!env.cwd.join("docs/empty.txt").exists());
    })
    .await
}

#[tokio::test]
async fn create_delete_write_scope_are_distinct() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("docs")).unwrap();
        let rules = |tool: &str, op: &str| json!([
            { "tool": tool, "path": "docs/**", "op": "read", "decision": "allow" },
            { "tool": tool, "path": "docs/**", "op": op, "decision": "allow" }
        ]);
        write_rules(&env, rules("write", "create"));
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (refused, asked): (bool, i64) = eval(&h, r#"
            local asked = 0
            local r = T:execute({ id = "create-only", name = "write", arguments = { path = "docs/a.txt", content = "old\n" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#).await;
        assert!(refused && asked == 1, "create-only cannot authorize a tool that may overwrite");
        assert!(!env.cwd.join("docs/a.txt").exists());
        write_rules(&env, rules("write", "write"));
        let wrote: bool = eval(&h, r#"local r = T:execute({ id = "write-super", name = "write",
            arguments = { path = "docs/a.txt", content = "old\n" } }); return r.is_error ~= true"#).await;
        assert!(wrote && env.cwd.join("docs/a.txt").exists(), "write covers create");
        write_rules(&env, rules("apply_patch", "create"));
        let (refused_delete, asked_delete): (bool, i64) = eval(&h, r#"
            local read = T:execute({ id = "observe", name = "read", arguments = { path = "docs/a.txt" } })
            assert(read.is_error ~= true)
            local asked = 0
            local r = T:execute({ id = "delete-only", name = "apply_patch", arguments = {
                input = "*** Begin Patch\n*** Delete File: docs/a.txt\n*** End Patch" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#).await;
        assert!(refused_delete && asked_delete == 1, "create does not cover delete");
        assert_eq!(std::fs::read_to_string(env.cwd.join("docs/a.txt")).unwrap(), "old\n");
        write_rules(&env, rules("apply_patch", "write"));
        let deleted: bool = eval(&h, r#"local r = T:execute({ id = "delete-super", name = "apply_patch", arguments = {
            input = "*** Begin Patch\n*** Delete File: docs/a.txt\n*** End Patch" } }); return r.is_error ~= true"#).await;
        h.shutdown().await;
        assert!(deleted, "write covers delete");
        assert!(!env.cwd.join("docs/a.txt").exists());
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn aliases_missing_parent_and_swap_are_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("docs/safe")).unwrap();
        std::fs::create_dir_all(env.cwd.join("src")).unwrap();
        std::os::unix::fs::symlink(env.cwd.join("src"), env.cwd.join("docs/outside")).unwrap();
        std::os::unix::fs::symlink(env.cwd.join("missing"), env.cwd.join("docs/dangling")).unwrap();
        std::os::unix::fs::symlink(env.cwd.join("docs/safe"), env.cwd.join("docs/swap")).unwrap();
        write_rules(&env, json!([
            { "tool": "write", "path": "docs/**", "op": "read", "decision": "allow" },
            { "tool": "write", "path": "docs/**", "op": "write", "decision": "allow" }
        ]));
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (outside, dangling): (bool, bool) = eval(&h, r#"
            local outside = T:execute({ id = "alias", name = "write", arguments = { path = "docs/outside/no.txt", content = "bad" } })
            local dangling = T:execute({ id = "dangling", name = "write", arguments = { path = "docs/dangling/no.txt", content = "bad" } })
            return outside.refused == true, dangling.refused == true
        "#).await;
        assert!(outside && dangling, "neither an outside alias nor an unresolved link can borrow an allow");
        assert!(!env.cwd.join("src/no.txt").exists());
        write_rules(&env, json!([]));
        exec(&h, r#"
            ENTER_TX, ENTER_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            ASKED, RESULT = 0, nil
            TASK = mare.task.spawn(function()
                RESULT = T:execute({ id = "swap", name = "write", arguments = { path = "docs/swap/no.txt", content = "bad" } }, {
                    approve = function()
                        ASKED = ASKED + 1
                        if ASKED == 1 then ENTER_TX:send(true); RELEASE_RX:recv(); return { decision = "allow_session", source = "user" } end
                        return { decision = "deny", source = "user" }
                    end,
                })
            end)
            ENTER_RX:recv()
        "#).await;
        std::fs::remove_file(env.cwd.join("docs/swap")).unwrap();
        std::os::unix::fs::symlink(env.cwd.join("src"), env.cwd.join("docs/swap")).unwrap();
        let refused: bool = eval(&h, r#"RELEASE_TX:send(true); TASK:wait(); return RESULT.refused == true"#).await;
        assert!(refused, "a changed symlink cannot inherit an approval for its old target");
        assert!(!env.cwd.join("src/no.txt").exists());
        assert!(!env.cwd.join("docs/safe/no.txt").exists());

        std::fs::remove_file(env.cwd.join("docs/swap")).unwrap();
        std::fs::create_dir(env.cwd.join("docs/swap")).unwrap();
        exec(&h, r#"
            ENTER_TX, ENTER_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            ASKED, RESULT = 0, nil
            TASK = mare.task.spawn(function()
                RESULT = T:execute({ id = "replace-parent", name = "write", arguments = { path = "docs/swap/new.txt", content = "bad" } }, {
                    approve = function()
                        ASKED = ASKED + 1
                        if ASKED == 1 then ENTER_TX:send(true); RELEASE_RX:recv(); return { decision = "allow_session", source = "user" } end
                        return { decision = "deny", source = "user" }
                    end,
                })
            end)
            ENTER_RX:recv()
        "#).await;
        std::fs::rename(env.cwd.join("docs/swap"), env.cwd.join("docs/old-parent")).unwrap();
        std::fs::create_dir(env.cwd.join("docs/swap")).unwrap();
        let refused: bool = eval(&h, r#"RELEASE_TX:send(true); TASK:wait(); return RESULT.refused == true"#).await;
        h.shutdown().await;
        assert!(refused, "a replacement parent directory must invalidate the frozen path identity");
        assert!(!env.cwd.join("docs/swap/new.txt").exists());
        assert!(!env.cwd.join("docs/old-parent/new.txt").exists());
    })
    .await
}

#[tokio::test]
async fn access_snapshot_changed_after_vote_is_rejected() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (accepted, asked): (bool, i64) = eval(&h, r#"
            local asked = 0
            local r = T:execute({ id = "mutate-access", name = "write", arguments = { path = "not-written.txt", content = "bad" } }, {
                approve = function(call, info)
                    asked = asked + 1
                    call.access.entries[1].physical = mare.cwd .. "/other-target"
                    if info.access then info.access[1].physical = mare.cwd .. "/other-target" end
                    return { decision = "allow_session", source = "user" }
                end,
            })
            return r.is_error ~= true, asked
        "#).await;
        h.shutdown().await;
        assert!(accepted && asked == 1, "mutating the detached copy must not redirect the approved call");
        assert_eq!(std::fs::read_to_string(env.cwd.join("not-written.txt")).unwrap(), "bad");
        assert!(!env.cwd.join("other-target").exists());

        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (tamper_ok, tamper_asked, replace_before_refused, replace_before_asked, swap_refused, swap_asked, drift_refused, drift_asked): (
            bool,
            i64,
            bool,
            i64,
            bool,
            i64,
            bool,
            i64,
        ) = eval(
            &h,
            r#"
            local tamper_asked = 0
            local off_voter = C.cordis:get("policy"):voter("tool/before", function(call)
                if call.id == "tamper-before" and call.access and call.access.entries and call.access.entries[1] then
                    call.access.entries[1].physical = mare.cwd .. "/tampered-target.txt"
                elseif call.id == "replace-before" then
                    T:register("replace_before_tool", {
                        description = "replaced in tool/before", side_effect = "write",
                        parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                        access = function() return { { kind = "file", op = "read", path = "other.txt", recursive = false } } end,
                        execute = function() return { content = "done" } end,
                    })
                end
            end)
            local tamper = T:execute({ id = "tamper-before", name = "write", arguments = { path = "tamper-orig.txt", content = "ok" } }, {
                approve = function(call)
                    tamper_asked = tamper_asked + 1
                    assert(call.access.entries[1].physical ~= mare.cwd .. "/tampered-target.txt")
                    return { decision = "allow", source = "user" }
                end,
            })

            T:register("replace_before_tool", {
                description = "replace before vote completes", side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                access = function(a) return { { kind = "file", op = "write", path = a.path, recursive = false } } end,
                execute = function(a) assert(mare.fs.write(a.path, "bad")); return { content = "done" } end,
            })
            local replace_before_asked = 0
            local replace_before = T:execute({ id = "replace-before", name = "replace_before_tool", arguments = { path = "replace-before.txt" } }, {
                approve = function() replace_before_asked = replace_before_asked + 1; return { decision = "allow", source = "user" } end,
            })
            off_voter()

            T:register("swap_access_tool", {
                description = "swap access after vote", side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                access = function(a) return { { kind = "file", op = "write", path = a.path, recursive = false } } end,
                execute = function(a) assert(mare.fs.write(a.path, "bad")); return { content = "done" } end,
            })
            local swap_asked = 0
            local swap = T:execute({ id = "swap-def", name = "swap_access_tool", arguments = { path = "swap-target.txt" } }, {
                approve = function()
                    swap_asked = swap_asked + 1
                    T:register("swap_access_tool", {
                        description = "swap access after vote", side_effect = "write",
                        parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                        access = function() return { { kind = "file", op = "read", path = "other.txt", recursive = false } } end,
                        execute = function() return { content = "done" } end,
                    })
                    return { decision = "allow", source = "user" }
                end,
            })

            local drift_seq = 0
            T:register("drift_access_tool", {
                description = "access changes after approval", side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                access = function(a)
                    return { { kind = "file", op = "write", path = a.path .. "-" .. drift_seq, recursive = false } }
                end,
                execute = function(a) assert(mare.fs.write(a.path, "bad")); return { content = "done" } end,
            })
            local drift_asked = 0
            local drift = T:execute({ id = "drift-def", name = "drift_access_tool", arguments = { path = "drift-target.txt" } }, {
                approve = function()
                    drift_asked = drift_asked + 1
                    drift_seq = drift_seq + 1
                    return { decision = "allow", source = "user" }
                end,
            })
            return tamper.is_error ~= true, tamper_asked,
                replace_before.refused == true, replace_before_asked,
                swap.refused == true, swap_asked,
                drift.refused == true, drift_asked
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(tamper_ok && tamper_asked == 1, "mutating cloned call.access in tool/before must not redirect the call");
        assert!(replace_before_refused && replace_before_asked == 0, "replacing tool definition during tool/before must refuse before approval");
        assert!(swap_refused && swap_asked == 1, "replacing tool definition during approval must reject the call");
        assert!(drift_refused && drift_asked == 3, "changing evaluated def.access output before execution must refuse after voiding");
        assert_eq!(std::fs::read_to_string(env.cwd.join("tamper-orig.txt")).unwrap(), "ok");
        assert!(!env.cwd.join("tampered-target.txt").exists());
        assert!(!env.cwd.join("replace-before.txt").exists());
        assert!(!env.cwd.join("swap-target.txt").exists());
        assert!(!env.cwd.join("drift-target.txt").exists());
    })
    .await
}

#[tokio::test]
async fn readonly_path_allow_writes_missing_subdir_and_preserves_boundaries() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir().unwrap();
        let outside_dir = dunce::canonicalize(outside.path()).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        std::fs::write(env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        std::fs::write(env.cwd.join(".mcp.json"), "{}\n").unwrap();
        std::fs::create_dir_all(env.cwd.join("docs")).unwrap();

        write_rules(&env, json!([{ "tool": "write", "path": "**", "decision": "allow" }]));
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        global(&h, "OUTSIDE_DIR", outside_dir.to_string_lossy().to_string());
        let (nested_ok, git_err, mare_err, mcp_err, out_err): (bool, bool, bool, bool, bool) = eval(
            &h,
            r#"
            local nested = T:execute({ id = "nested", name = "write",
                arguments = { path = "docs/newdir/guide.md", content = "nested ok" } })
            local git = T:execute({ id = "git", name = "write",
                arguments = { path = ".git/hooks/pre-commit", content = "pwn" } })
            local mare_init = T:execute({ id = "mare-init", name = "write",
                arguments = { path = ".mare/init.lua", content = "pwn" } })
            local mcp = T:execute({ id = "mcp", name = "write",
                arguments = { path = ".mcp.json", content = "pwn" } })
            local ext = T:execute({ id = "ext", name = "write",
                arguments = { path = OUTSIDE_DIR .. "/escaped.txt", content = "pwn" } })
            return nested.is_error ~= true, git.is_error == true, mare_init.is_error == true,
                mcp.is_error == true, ext.is_error == true
        "#,
        )
        .await;
        assert!(nested_ok, "read-only + path allow must create missing nested subdirectories in workspace");
        assert_eq!(std::fs::read_to_string(env.cwd.join("docs/newdir/guide.md")).unwrap(), "nested ok");
        assert!(git_err && !env.cwd.join(".git/hooks/pre-commit").exists(), ".git must stay read-only");
        assert!(mare_err && !env.cwd.join(".mare/init.lua").exists(), ".mare must stay read-only");
        assert!(
            mcp_err && std::fs::read_to_string(env.cwd.join(".mcp.json")).unwrap() == "{}\n",
            ".mcp.json must stay read-only"
        );
        assert!(
            out_err && !outside_dir.join("escaped.txt").exists(),
            "path allow without scope must not write outside workspace"
        );

        let outside_glob = format!("{}/**", outside_dir.to_string_lossy().replace('\\', "/"));
        let outside_scope = format!("write:{}", outside_dir.to_string_lossy().replace('\\', "/"));
        write_rules(
            &env,
            json!([{ "tool": "write", "path": outside_glob, "scope": outside_scope, "decision": "allow" }]),
        );
        let scoped_ro_ok: bool = eval(
            &h,
            r#"
            local r = T:execute({ id = "scoped-ro", name = "write",
                arguments = { path = OUTSIDE_DIR .. "/scoped-ro.txt", content = "ro scoped" } })
            return r.is_error ~= true
        "#,
        )
        .await;
        assert!(scoped_ro_ok && std::fs::read_to_string(outside_dir.join("scoped-ro.txt")).unwrap() == "ro scoped");
        assert!(p_set_auto_and_write(&h, &outside_dir).await);
        h.shutdown().await;
    })
    .await
}

async fn p_set_auto_and_write(h: &mare_host::Host, outside_dir: &std::path::Path) -> bool {
    let ok: bool = eval(
        h,
        r#"
        assert(P:set("auto"))
        local r = T:execute({ id = "scoped-auto", name = "write",
            arguments = { path = OUTSIDE_DIR .. "/scoped-auto.txt", content = "auto scoped" } })
        return r.is_error ~= true
    "#,
    )
    .await;
    ok && std::fs::read_to_string(outside_dir.join("scoped-auto.txt")).unwrap() == "auto scoped"
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_workspace_and_home_dual_root_rules() {
    local(async {
        for with_mare_dir in [false, true] {
            let mut env = TestEnv::new().await;
            let base = env.cwd.clone();
            let phys_ws = base.join("phys_ws");
            let link_ws = base.join("link_ws");
            let phys_home = env.home.path().join("phys_home");
            let link_home = env.home.path().join("link_home");
            std::fs::create_dir_all(phys_ws.join("secrets")).unwrap();
            std::fs::create_dir_all(phys_ws.join("docs")).unwrap();
            std::fs::create_dir_all(phys_ws.join("src")).unwrap();
            if with_mare_dir {
                std::fs::create_dir_all(phys_ws.join(".mare")).unwrap();
            }
            std::fs::write(phys_ws.join("secrets/prod.key"), "top-secret\n").unwrap();
            std::fs::write(phys_ws.join(".env"), "ENV_TOKEN=1\n").unwrap();
            std::os::unix::fs::symlink(&phys_ws, &link_ws).unwrap();
            std::os::unix::fs::symlink(phys_ws.join("src"), phys_ws.join("docs/outside")).unwrap();

            std::fs::create_dir_all(phys_home.join(".ssh")).unwrap();
            std::fs::write(phys_home.join(".ssh/id_rsa"), "PRIVATE KEY\n").unwrap();
            std::os::unix::fs::symlink(&phys_home, &link_home).unwrap();

            env.cwd = link_ws.clone();
            env.set_var("HOME", &link_home);
            write_rules(
                &env,
                json!([
                    { "tool": "read", "path": "secrets/**", "op": "read", "decision": "forbidden" },
                    { "tool": "read", "path": "~/.ssh/**", "op": "read", "decision": "forbidden" },
                    { "tool": "write", "path": "docs/**", "decision": "allow" },
                    { "tool": "read", "path": ".env", "op": "read", "decision": "allow" }
                ]),
            );
            let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
            global(&h, "PHYS_WS", phys_ws.to_string_lossy().to_string());
            global(&h, "PHYS_HOME", phys_home.to_string_lossy().to_string());
            let (forbid_ws, forbid_home, allow_docs, ext_symlink_refused, env_allowed, env_asked): (
                bool,
                bool,
                bool,
                bool,
                bool,
                i64,
            ) = eval(
                &h,
                r#"
                local f1 = T:execute({ id = "f-ws", name = "read", arguments = { path = PHYS_WS .. "/secrets/prod.key" } })
                local f2 = T:execute({ id = "f-home", name = "read", arguments = { path = PHYS_HOME .. "/.ssh/id_rsa" } })
                local w = T:execute({ id = "w-docs", name = "write", arguments = { path = "docs/a.md", content = "ok" } })
                local ext = T:execute({ id = "ext-link", name = "write", arguments = { path = "docs/outside/escape.txt", content = "bad" } })
                local asked = 0
                local env_res = T:execute({ id = "read-env", name = "read", arguments = { path = ".env" } }, {
                    approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
                })
                return f1.refused == true, f2.refused == true, w.is_error ~= true,
                    ext.refused == true, env_res.is_error ~= true, asked
            "#,
            )
            .await;
            h.shutdown().await;
            assert!(forbid_ws, "with_mare_dir={with_mare_dir}: physical path must hit relative forbidden rule");
            assert!(forbid_home, "with_mare_dir={with_mare_dir}: physical home path must hit ~ forbidden rule");
            assert!(allow_docs, "with_mare_dir={with_mare_dir}: relative allow must work in symlinked workspace");
            assert_eq!(std::fs::read_to_string(phys_ws.join("docs/a.md")).unwrap(), "ok");
            assert!(ext_symlink_refused && !phys_ws.join("src/escape.txt").exists(), "external symlink must remain refused");
            assert!(env_allowed && env_asked == 0, "with_mare_dir={with_mare_dir}: exact relative sensitive exempt must work");
        }
    })
    .await
}

#[tokio::test]
async fn hidden_path_is_rejected_before_permission_matching() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "full-access" }), json!({ "no_session": true })).await;
        let (refused, asked, message): (bool, i64, String) = eval(&h, r#"
            KEEP_PATH = mare.projection.register("HIDDEN_PATH_FIXTURE")
            T:register("hidden_path", {
                description = "write file", side_effect = "write", sensitive_inputs = { "path" },
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                access = function(a) return { { kind = "file", op = "write", path = a.path, recursive = false } } end,
                execute = function(a) assert(mare.fs.write(a.path, "bad")); return { content = "done" } end,
            })
            local asked = 0
            local r = T:execute({ id = "hidden-path", name = "hidden_path", arguments = { path = "HIDDEN_PATH_FIXTURE" } }, {
                approve = function() asked = asked + 1; return { decision = "allow", source = "user" } end,
            })
            return r.refused == true, asked, tostring(r.content)
        "#).await;
        h.shutdown().await;
        assert!(refused && asked == 0 && message.contains("access path contains a hidden value"), "{message}");
        assert!(!env.cwd.join("HIDDEN_PATH_FIXTURE").exists());
    })
    .await
}
