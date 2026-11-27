use super::*;

#[tokio::test]
async fn rejected_archive_switch_leaves_zero_archive_records() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text": "saved-ok"}])).build().await;
        expose(&h);
        let (path, blocked_err, archived_after_allow): (String, String, bool) = eval(
            &h,
            r#"
            local ag = C.cordis:get("agent")
            local r = ag:wait(ag:prompt('save me before archive', {}).turn_id)
            assert(not r.error, tostring(r.error))
            local before = C.cordis:get("session"):info()
            local last_notice = ""
            mare.ui.notify = function(msg) last_notice = tostring(msg) end

            local block = true
            C.cordis:get("policy"):voter('session/before-switch', function()
                if block then
                    return { decision = 'deny', reason = 'blocked archive switch' }
                end
                return { decision = 'allow' }
            end)

            C.cordis:get("commands"):run('/archive')
            local err = last_notice
            local after_reject = C.cordis:get("session"):info()
            assert(after_reject.session_id == before.session_id and after_reject.path == before.path, 'session must stay active on rejection')
            local archived_rows = mare.store.index_read({ session_id = before.session_id, archived = true })
            assert(#archived_rows == 0, 'rejected archive must not mark index archived')

            block = false
            C.cordis:get("commands"):run('/archive')
            assert(C.cordis:get("session"):info().session_id ~= before.session_id, last_notice)
            local archived_now = mare.store.index_read({ session_id = before.session_id, archived = true })
            return before.path, tostring(err), #archived_now == 1
        "#,
        )
        .await;
        assert!(blocked_err.contains("blocked archive switch"), "{blocked_err}");
        assert!(archived_after_allow);
        let disk = std::fs::read_to_string(&path).unwrap();
        let archive_lines = disk
            .lines()
            .filter(|line| line.contains("\"type\":\"archive\""))
            .count();
        assert_eq!(archive_lines, 1, "expected exactly 1 archive record after allowed switch, got: {disk}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn archive_and_delete_rollback_bypass_before_switch_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text": "saved-ok"}])).build().await;
        expose(&h);
        let (arch_err, del_err, arch_path): (String, String, String) = eval(
            &h,
            r#"
            local ag = C.cordis:get("agent")
            local r = ag:wait(ag:prompt('keep me across rollback', {}).turn_id)
            assert(not r.error, tostring(r.error))
            local before = C.cordis:get("session"):info()
            local tx, rx = mare.task.channel()
            local notices = {}
            mare.ui.notify = function(msg)
                local s = tostring(msg)
                notices[#notices + 1] = s
                if s:find('Session not', 1, true) then
                    tx:send(s)
                end
            end
            mare.ui.confirm = function() return true end

            -- Deny resume switches (which a naive session.load rollback would hit),
            -- and allow toggling denial of new switches to verify archive rejection.
            local block_new = true
            C.cordis:get("policy"):voter('session/before-switch', function(ev)
                if ev and ev.reason == 'resume' then
                    return { decision = 'deny', reason = 'policy blocks resume' }
                end
                if block_new and ev and ev.reason == 'new' then
                    return { decision = 'deny', reason = 'injected archive switch rejection' }
                end
                return { decision = 'allow' }
            end)

            -- If session.new is rejected before close_file, /archive must fail cleanly with zero archive records written.
            C.cordis:get("commands"):run('/archive')
            local arch_err = notices[#notices] or ''
            block_new = false
            local after_arch = C.cordis:get("session"):info()
            assert(after_arch.session_id == before.session_id and after_arch.path == before.path, 'archive failure must keep active session')
            local archived_rows = mare.store.index_read({ session_id = before.session_id, archived = true })
            assert(#archived_rows == 0, 'failed archive must not mark index archived')

            local orig_delete = mare.store.delete
            mare.store.delete = function()
                return nil, 'injected delete failure'
            end
            C.cordis:get("commands"):run('/delete')
            local del_err = rx:recv()
            mare.store.delete = orig_delete
            local after_del = C.cordis:get("session"):info()
            assert(after_del.session_id == before.session_id and after_del.path == before.path, 'delete rollback must restore session')

            return tostring(arch_err), tostring(del_err), before.path
        "#,
        )
        .await;
        assert!(arch_err.contains("injected archive switch rejection"), "{arch_err}");
        assert!(!arch_err.contains("rollback failed"), "{arch_err}");
        assert!(del_err.contains("injected delete failure"), "{del_err}");
        assert!(!del_err.contains("rollback failed"), "{del_err}");
        let disk = std::fs::read_to_string(&arch_path).unwrap();
        let archive_lines = disk
            .lines()
            .filter(|line| line.contains("\"type\":\"archive\""))
            .count();
        assert_eq!(archive_lines, 0, "expected 0 archive records after failed archive, got: {disk}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn archive_rejects_concurrent_switch_during_before_switch_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text": "saved-a"}, {"text": "saved-b"}])).build().await;
        expose(&h);
        let (path_a, path_b, err): (String, String, String) = eval(
            &h,
            r#"
            local ag = C.cordis:get("agent")
            local r_a = ag:wait(ag:prompt('session A', {}).turn_id)
            assert(not r_a.error, tostring(r_a.error))
            local info_a = C.cordis:get("session"):info()

            C.cordis:get("session"):create()
            local r_b = ag:wait(ag:prompt('session B', {}).turn_id)
            assert(not r_b.error, tostring(r_b.error))
            local info_b = C.cordis:get("session"):info()

            local loaded, lerr = C.cordis:get("session"):load({ path = info_a.path })
            assert(loaded, tostring(lerr))
            assert(C.cordis:get("session"):info().session_id == info_a.session_id)

            local switched_concurrently = false
            C.cordis:get("policy"):voter('session/before-switch', function()
                if not switched_concurrently then
                    switched_concurrently = true
                    local ok, serr = C.cordis:get("session"):load({ path = info_b.path })
                    assert(ok, tostring(serr))
                end
                return { decision = 'allow' }
            end)

            local aerr = ''
            mare.ui.notify = function(msg) aerr = tostring(msg) end
            C.cordis:get("commands"):run('/archive')
            local active = C.cordis:get("session"):info()
            assert(active.session_id == info_b.session_id and active.path == info_b.path, 'session B must remain active')
            return info_a.path, info_b.path, tostring(aerr)
        "#,
        )
        .await;
        assert!(err.contains("active session changed before switch"), "unexpected archive error: {err}");
        for (label, path) in [("session A", &path_a), ("session B", &path_b)] {
            let disk = std::fs::read_to_string(path).unwrap();
            assert!(
                disk.lines().all(|line| !line.contains("\"type\":\"archive\"")),
                "{label} must have zero archive records on disk: {disk}"
            );
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn archive_close_entry_write_failure_poisons_and_leaves_zero_archive_records() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text": "saved-ok"}])).build().await;
        expose(&h);
        let (arch_err, arch_path, switching): (String, String, bool) = eval(
            &h,
            r#"
            local orig_open = mare.store.open
            mare.store.open = function(path, opts)
                local f = orig_open(path, opts)
                if not opts or (not opts.auxiliary and not opts.create_new and not opts.existing) then
                    return setmetatable({}, {
                        __index = function(_, k)
                            if k == 'append' then
                                return function(_, entry, append_opts)
                                    if type(entry) == 'table' and entry.type == 'archive' then
                                        return nil, 'injected archive write failure'
                                    end
                                    return f:append(entry, append_opts)
                                end
                            end
                            local v = f[k]
                            if type(v) == 'function' then
                                return function(_, ...)
                                    return v(f, ...)
                                end
                            end
                            return v
                        end,
                    })
                end
                return f
            end

            local ag = C.cordis:get("agent")
            local r = ag:wait(ag:prompt('keep me before write failure', {}).turn_id)
            mare.store.open = orig_open
            assert(not r.error, tostring(r.error))
            local before = C.cordis:get("session"):info()

            local arch_err = ''
            mare.ui.notify = function(msg) arch_err = tostring(msg) end
            C.cordis:get("commands"):run('/archive')
            local after_arch = C.cordis:get("session"):info()
            assert(after_arch.session_id == before.session_id and after_arch.path == before.path, 'failed archive must keep session id/path')
            local archived_rows = mare.store.index_read({ session_id = before.session_id, archived = true })
            assert(#archived_rows == 0, 'failed archive must not mark index archived')

            return tostring(arch_err), before.path, C.cordis:get("session"):switching()
        "#,
        )
        .await;
        assert!(
            arch_err.contains("old session close entry failed: injected archive write failure"),
            "{arch_err}"
        );
        assert!(switching, "close_file failure must activate poison barrier");
        assert!(
            h.prompt("MUST-NOT-RUN", json!({})).await.is_err(),
            "poison barrier must block subsequent turns"
        );
        let disk = std::fs::read_to_string(&arch_path).unwrap();
        let archive_lines = disk
            .lines()
            .filter(|line| line.contains("\"type\":\"archive\""))
            .count();
        assert_eq!(archive_lines, 0, "expected 0 archive records after failed close_file write, got: {disk}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn archive_and_unarchive_recursively_update_child_agent_sessions() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (parent_id, child_archived_after_arch, child_archived_after_unarch): (String, bool, bool) = eval(
            &h,
            r#"
            local s = C.cordis:get("session"):info()
            C.cordis:get("session"):add_record("sync-probe", { ok = true }, { sync = true })
            s = C.cordis:get("session"):info()
            local child_path = string.gsub(s.path, "/[^/]+$", "/agents/run-1/000-explore.jsonl")
            local cf = mare.store.open(child_path)
            cf:append({
                type = "session",
                version = mare.store.format_version,
                session_id = "child-agent-1",
                parent_session_id = s.session_id,
                run_id = "run-1",
                label = "explore",
                cwd = mare.cwd,
            }, { sync = true })
            cf:append({ type = "message", message = { role = "user", content = "subtask" } }, { sync = true })
            cf:close()
            mare.store.index_append({ path = child_path })

            C.cordis:get("session"):create()
            local res, err = C.cordis:get("session"):archive(s.session_id)
            assert(res, tostring(err))
            local arch_rows = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = s.session_id, archived = true })
            local ures, uerr = C.cordis:get("session"):unarchive(s.session_id)
            assert(ures, tostring(uerr))
            local unarch_rows = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = s.session_id, archived = false })
            return s.session_id, #arch_rows == 1, #unarch_rows == 1
            "#,
        )
        .await;
        assert!(!parent_id.is_empty());
        assert!(child_archived_after_arch, "child agent session must be archived when parent is archived");
        assert!(child_archived_after_unarch, "child agent session must be unarchived when parent is unarchived");
        h.shutdown().await;
    })
    .await
}

/// When archiving or unarchiving a parent session fails on one child agent session,
/// siblings still proceed,
/// and the parent call returns a partial-failure result with `partial = true`, `failed_children`,
/// and an explicit error string instead of silently succeeding.
#[tokio::test]
async fn archive_and_unarchive_report_partial_failure_when_child_fails() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (arch_partial, arch_fail_id, arch_fail_err, arch_err_str, ok_child_archived, unarch_partial, unarch_fail_id, unarch_err_str, ok_child_unarchived): (
            bool,
            String,
            String,
            String,
            bool,
            bool,
            String,
            String,
            bool,
        ) = eval(
            &h,
            r#"
            local s = C.cordis:get("session"):info()
            C.cordis:get("session"):add_record("sync-probe", { ok = true }, { sync = true })
            s = C.cordis:get("session"):info()

            local function make_child(id, rel)
                local dir = string.match(s.path, "^(.*)[/\\][^/\\]+$")
                assert(dir, "expected directory in session path: " .. tostring(s.path))
                local native_rel = string.gsub(rel, "/", mare.os == "windows" and "\\" or "/")
                local path = dir .. native_rel
                local cf = mare.store.open(path)
                cf:append({
                    type = "session",
                    version = mare.store.format_version,
                    session_id = id,
                    parent_session_id = s.session_id,
                    run_id = "run-1",
                    label = id,
                    cwd = mare.cwd,
                }, { sync = true })
                cf:append({ type = "message", message = { role = "user", content = "subtask" } }, { sync = true })
                cf:close()
                mare.store.index_append({ path = path })
                return path
            end

            local fail_path = make_child("child-fail", "/agents/run-1/000-fail.jsonl")
            local _ok_path = make_child("child-ok", "/agents/run-1/001-ok.jsonl")
            local norm_fail = string.gsub(fail_path, "\\", "/")

            local orig_open = mare.store.open
            mare.store.open = function(path, opts)
                if string.gsub(tostring(path), "\\", "/") == norm_fail and type(opts) == "table" and opts.auxiliary then
                    error("injected child archive failure")
                end
                return orig_open(path, opts)
            end

            local sess = C.cordis:get("session")
            sess:create()
            local ok_a, err = pcall(sess.archive, sess, s.session_id)
            assert(not ok_a and type(err) == "table" and err.code == "session.partial_failure", tostring(err))
            local ok_arch_rows = mare.store.index_read({ scope = "all", kind = "agent", session_id = "child-ok", archived = true })

            local ok_u, uerr = pcall(sess.unarchive, sess, s.session_id)
            mare.store.open = orig_open
            assert(not ok_u and type(uerr) == "table" and uerr.code == "session.partial_failure", tostring(uerr))
            local ok_unarch_rows = mare.store.index_read({ scope = "all", kind = "agent", session_id = "child-ok", archived = false })

            local af = type(err.data) == "table" and type(err.data.failed_children) == "table" and err.data.failed_children[1] or {}
            local uf = type(uerr.data) == "table" and type(uerr.data.failed_children) == "table" and uerr.data.failed_children[1] or {}
            return err.code == "session.partial_failure",
                tostring(af.session_id or ""),
                tostring(af.error or ""),
                tostring(err.message or err or ""),
                #ok_arch_rows == 1,
                uerr.code == "session.partial_failure",
                tostring(uf.session_id or ""),
                tostring(uerr.message or uerr or ""),
                #ok_unarch_rows == 1
            "#,
        )
        .await;
        assert!(arch_partial, "archive must mark partial = true when a child fails");
        assert_eq!(arch_fail_id, "child-fail");
        assert!(arch_fail_err.contains("injected child archive failure"), "{arch_fail_err}");
        assert!(arch_err_str.contains("child-fail") && arch_err_str.contains("injected child archive failure"), "{arch_err_str}");
        assert!(ok_child_archived, "sibling child-ok must still be archived when child-fail fails");
        assert!(unarch_partial, "unarchive must mark partial = true when a child fails");
        assert_eq!(unarch_fail_id, "child-fail");
        assert!(unarch_err_str.contains("injected child archive failure"), "{unarch_err_str}");
        assert!(ok_child_unarchived, "sibling child-ok must still be unarchived when child-fail fails");
        h.shutdown().await;
    })
    .await
}

/// Item 3 (self-written): archiving a parent session after spawning a real subagent (without
/// manually calling `index_append`) must still discover and archive the child transcript.
#[tokio::test]
async fn archive_and_unarchive_cascade_to_real_unindexed_subagent_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "parent reply" },
                { "text": "child reply" }
            ]))
            .build()
            .await;
        expose(&h);
        // Ensure parent session is materialized on disk and initial startup reconcile has run.
        let r = h.prompt("hello parent", json!({})).await.unwrap();
        assert_eq!(r["text"], "parent reply");
        exec(&h, "mare.store.index_reconcile()").await;

        let (parent_id, child_id, archived_count, unarchived_count): (String, String, usize, usize) = eval(
            &h,
            r#"
            local pinfo = C.cordis:get("session"):info()
            -- Start a real subagent via mare.subagent.start (creates child transcript via child_transcript.rs
            -- without manual mare.store.index_append).
            local spawned = mare.subagent.start({
                prompt = "do child work",
                name = "real-child",
                label = "real-child",
                parent_session = pinfo.session_id,
                parent_session_epoch = pinfo.session_epoch,
                children_dir = pinfo.children_dir,
            })
            local waited = mare.subagent.wait("real-child", { timeout_ms = 10000 })
            assert(not waited.timed_out, "child agent timed out")
            assert(waited.status == "completed", "child status: " .. tostring(waited.status))

            C.cordis:get("session"):create()
            local ares, aerr = C.cordis:get("session"):archive(pinfo.session_id)
            assert(ares, tostring(aerr))
            local arch_rows = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = pinfo.session_id, archived = true })

            local ures, uerr = C.cordis:get("session"):unarchive(pinfo.session_id)
            assert(ures, tostring(uerr))
            local unarch_rows = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = pinfo.session_id, archived = false })
            return pinfo.session_id, tostring(arch_rows[1] and arch_rows[1].session_id or ""), #arch_rows, #unarch_rows
            "#,
        )
        .await;
        assert!(!parent_id.is_empty());
        assert!(!child_id.is_empty());
        assert_eq!(archived_count, 1, "real unindexed subagent session must be archived with parent");
        assert_eq!(unarchived_count, 1, "real subagent session must be unarchived with parent");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn archive_reports_partial_failure_when_index_reconcile_fails_or_is_incomplete() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        write_lines(
            &dir.join("2026-01-01T00-00-00_p1.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-throw", "cwd": env.cwd }),
                msg("m1", None, "user", "p1"),
            ],
        );
        write_lines(
            &dir.join("2026-01-01T00-00-01_p2.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-failed", "cwd": env.cwd }),
                msg("m1", None, "user", "p2"),
            ],
        );
        write_lines(
            &dir.join("2026-01-01T00-00-02_p3.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-pending", "cwd": env.cwd }),
                msg("m1", None, "user", "p3"),
            ],
        );
        exec(&h, "mare.store.index_reconcile()").await;

        let (t_partial, t_err, f_partial, f_err, p_partial, p_err): (bool, String, bool, String, bool, String) = eval(
            &h,
            r#"
            local orig_reconcile = mare.store.index_reconcile
            local mode = "throw"
            mare.store.index_reconcile = function()
                if mode == "throw" then
                    error("disk full during reconcile")
                elseif mode == "failed" then
                    return { files = 1, updated = 0, removed = 0, failed = 1, pending = 0 }
                elseif mode == "pending" then
                    return { files = 1, updated = 0, removed = 0, failed = 0, pending = 2 }
                end
                return orig_reconcile()
            end

            local sess = C.cordis:get("session")
            local ok1, e1 = pcall(sess.archive, sess, "p-throw")
            mode = "failed"
            local ok2, e2 = pcall(sess.archive, sess, "p-failed")
            mode = "pending"
            local ok3, e3 = pcall(sess.archive, sess, "p-pending")
            mare.store.index_reconcile = orig_reconcile

            local function is_partial(ok, e)
                return not ok and type(e) == "table" and e.code == "session.partial_failure"
            end
            return is_partial(ok1, e1), tostring(type(e1) == "table" and e1.message or e1),
                is_partial(ok2, e2), tostring(type(e2) == "table" and e2.message or e2),
                is_partial(ok3, e3), tostring(type(e3) == "table" and e3.message or e3)
            "#,
        )
        .await;
        assert!(t_partial, "archive must mark partial=true when index_reconcile throws: {t_err}");
        assert!(t_err.contains("disk full during reconcile"), "error must mention reconcile failure: {t_err}");
        assert!(f_partial, "archive must mark partial=true when index_reconcile reports failed > 0: {f_err}");
        assert!(f_err.contains("failed=1"), "error must mention failed=1: {f_err}");
        assert!(p_partial, "archive must mark partial=true when index_reconcile reports pending > 0: {p_err}");
        assert!(p_err.contains("pending=2"), "error must mention pending=2: {p_err}");
        h.shutdown().await;
    })
    .await
}
