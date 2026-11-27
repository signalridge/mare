//! Public result retention contract, independent of the external spill policy plugin.
use crate::common::*;

#[tokio::test]
async fn public_output_registry_routes_owned_backends_and_withdraws_disposed_ones() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["spill"]).no_session().build().await;
        expose(&h);
        let valid: bool = eval(
            &h,
            r#"
            local spill = C.cordis:get("spill")
            local changes = {}
            C.cordis:on("spill/backends-changed", function(event) changes[#changes+1] = event.name .. ":" .. event.kind end)
            local backend = mare.spill.new_local({root="registered-recovery",cleanup_period_days=0})
            local unregister = spill:register("archive", backend)
            assert(not pcall(spill.register, spill, "archive", backend))
            local saved = spill:save_owned("producer", "registered text", {backend="archive"})
            assert(saved.backend == "archive")
            local old_custom = backend.save("producer", "unwrapped native handle")
            assert(not pcall(spill.read_owned, spill, "producer", old_custom.spill_id))
            assert(not pcall(spill.read_bytes, spill, "producer", old_custom.spill_id))
            assert(not pcall(spill.delete_owned, spill, "producer", old_custom.spill_id))
            backend.delete(old_custom.spill_id, { session_id = "producer" })
            local listing = spill:list_backends().data
            listing[1].name = "changed copy"
            assert(spill:list_backends().data[1].name == "archive")
            assert(spill:read_owned("producer", saved.spill_id).content == "registered text")
            assert(not pcall(spill.read_owned, spill, "other-session", saved.spill_id))
            local image = spill:save_bytes("producer", string.char(0,255), {backend="archive"})
            assert(spill:read_bytes("producer", image.spill_id).bytes == string.char(0,255))
            spill:delete_owned("producer", saved.spill_id)
            spill:delete_owned("producer", image.spill_id)
            unregister()
            unregister()
            assert(not pcall(spill.read_owned, spill, "producer", saved.spill_id))
            local replacement = mare.spill.new_local({root="different-archive",cleanup_period_days=0})
            local invoked = false
            local underlying_read = replacement.read
            replacement.read = function(...) invoked=true; return underlying_read(...) end
            local unmount = spill:register("archive", replacement)
            local ok, mismatch = pcall(spill.read_owned, spill, "producer", saved.spill_id)
            assert(not ok and tostring(mismatch):find("namespace changed",1,true) and not invoked)
            unmount()
            assert(not pcall(spill.save_owned, spill, "producer", "x", {backend="missing"}))
            local broken = mare.spill.new_local({root="broken-recovery",cleanup_period_days=0})
            local invalid_ref
            local implementation = {}
            for key, value in pairs(broken) do implementation[key] = value end
            implementation.save = function(owner, content)
                invalid_ref = broken.save(owner, content)
                invalid_ref.size = -1
                return invalid_ref
            end
            local withdraw = spill:register("broken", implementation)
            assert(not pcall(spill.save_owned, spill, "producer", "unadvertised", {backend="broken"}))
            assert(not pcall(broken.read, invalid_ref.spill_id, { session_id = "producer" }))
            withdraw()
            assert(table.concat(changes, "|") == "archive:registered|archive:removed|archive:registered|archive:removed|broken:registered|broken:removed")
            -- a raw spill_id of the default spill store reads through spill; deleting stays with its producer
            local bare = mare.spill.save("producer", "bare text")
            assert(spill:read_owned("producer", bare.spill_id).content == "bare text")
            assert(spill:read_bytes("producer", bare.spill_id).bytes == "bare text")
            assert(not pcall(spill.read_owned, spill, "other", bare.spill_id))
            assert(not pcall(spill.delete_owned, spill, "producer", bare.spill_id))
            mare.spill.delete(bare.spill_id, { session_id = "producer" })
            return true
        "#,
        )
        .await;
        assert!(valid);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn local_backend_startup_sweep_and_dispose_are_public_and_preserve_new_saves() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let path: String = eval(
            &h,
            r#"
            local store = mare.spill.new_local({root="startup-recovery", cleanup_period_days=0})
            local item = store.save("producer", "old output")
            stale_spill_id = item.spill_id
            store.dispose()
            return item.path
        "#,
        )
        .await;
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86400);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let valid: bool = eval(
            &h,
            r#"
            local store = mare.spill.new_local({root="startup-recovery"})
            local fresh = store.save("producer", "fresh output")
            local report = store.dispose()
            assert(report.removed == 1 and #report.warnings == 0)
            local reopened = mare.spill.new_local({root="startup-recovery", cleanup_period_days=0})
            assert(not pcall(reopened.read, stale_spill_id, { session_id = "producer" }))
            assert(reopened.read(fresh.spill_id, { session_id = "producer" }).content == "fresh output")
            reopened.delete(fresh.spill_id, { session_id = "producer" })
            reopened.dispose()
            return true
        "#,
        )
        .await;
        assert!(valid);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn configurable_local_backend_shares_text_and_bytes_and_has_owned_lifecycle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let valid: bool = eval(
            &h,
            r#"
            assert(not pcall(mare.spill.new_local, {unknown=true}))
            assert(not pcall(mare.spill.new_local, {cleanup_period_days=-1}))
            assert(not pcall(mare.spill.new_local, {cleanup_period_days=1.5}))
            assert(not pcall(mare.spill.new_local, {cancel="invalid"}))
            local canceled = mare.task.token()
            canceled:cancel()
            assert(not pcall(mare.spill.new_local, {cancel=canceled}))
            local options = {root="local-recovery", cleanup_period_days=0}
            local store = mare.spill.new_local(options)
            local item = store.save("producer", "α\nbeta\n")
            assert(item.path:sub(1, #mare.cwd) == mare.cwd and item.path:find("local-recovery", 1, true))
            assert(store.read(item.spill_id, { session_id = "producer" }).content == "α\nbeta\n")
            assert(not pcall(store.read, item.spill_id, { session_id = "stranger" }))
            local binary = store.save_bytes("producer", string.char(0,255,1))
            assert(store.read_bytes(binary.spill_id, { session_id = "producer" }).bytes == string.char(0,255,1))
            local cached_save = store.save
            assert(store.dispose().removed == 0)
            assert(not pcall(cached_save, "producer", "after disposal"))
            assert(not pcall(store.read, item.spill_id, { session_id = "producer" }))
            local reopened = mare.spill.new_local(options)
            assert(reopened.read(item.spill_id, { session_id = "producer" }).content == "α\nbeta\n")
            reopened.delete(item.spill_id, { session_id = "producer" })
            reopened.delete(binary.spill_id, { session_id = "producer" })
            reopened.dispose()
            return true
        "#,
        )
        .await;
        assert!(valid);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn result_waterfall_observes_full_text_before_truncation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (full, intact, bounded): (bool, bool, bool) = eval(
            &h,
            r#"
            local body = string.rep("line\n", 20000)
            local saw, recovery, budget_owner = false
            C.cordis:on("tool/result", function(data, ev)
                local result, call, budget = data.result, data.call, data.operation
                if call.name ~= "spill-contract" then return ev.next(data) end
                saw = result.content == body and budget.max_bytes > 0
                budget_owner = budget.session_id or "test"
                recovery = mare.spill.save(budget_owner, result.content)
                result.content = "preview"
                result.output_notice = "[recovery: " .. recovery.path .. "]"
                return ev.next(data)
            end)
            C.cordis:get("tools"):register("spill-contract", { description = "test", parameters = {type="object",properties={}},
                side_effect="read", idempotent=true, execute=function() return {content=body} end })
            local r = C.cordis:get("tools"):execute({id="spill-test", name="spill-contract", arguments={}}, {})
            local original = mare.fs.read(recovery.path)
            mare.spill.delete(recovery.spill_id, { session_id = budget_owner })
            return saw, original == body, r.content:find("[recovery:", 1, true) ~= nil and #r.content < 51200
        "#,
        )
        .await;
        assert!(full && intact && bounded);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn result_waterfall_checked_fails_closed_when_listener_throws() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (is_error, error_code, leaked): (bool, String, bool) = eval(
            &h,
            r#"
            local secret_marker = "SECRET_OVERSIZED_TOKEN"
            local body = secret_marker .. "\n" .. string.rep("line\n", 20000)
            C.cordis:on("tool/result", function(data, ev)
                if data.call.name == "spill-fail-closed" then
                    error("spill budget exhausted")
                end
                return ev.next(data)
            end)
            C.cordis:get("tools"):register("spill-fail-closed", {
                description = "test",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                idempotent = true,
                execute = function() return { content = body } end,
            })
            local r = C.cordis:get("tools"):execute({ id = "spill-fail", name = "spill-fail-closed", arguments = {} }, {})
            return r.is_error == true, tostring(r.error_code), r.content:find(secret_marker, 1, true) ~= nil
        "#,
        )
        .await;
        assert!(is_error, "throwing in tool/result must fail closed");
        assert_eq!(error_code, "tool_result_failed");
        assert!(!leaked, "oversized raw content must not leak when tool/result throws");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn result_waterfall_receives_resolved_route_without_route_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (saw_route, wire_route): (String, String) = eval(
            &h,
            r#"
            local llm = assert(C.cordis:get("llm"))
            llm:register_wire({
                name = "route-probe-wire",
                route = "responses",
                encode = function() return {}, {} end,
                decoder = function() return { event = function() return true end, finish = function() return {} end } end,
            })
            llm:register_provider({
                provider = "route-probe-provider",
                is_configured = function() return true end,
                route = "gw-resolved-route",
                routes = {
                    ["gw-resolved-route"] = { wire = "route-probe-wire" },
                },
                models = {
                    { model_id = "m1", name = "m1" },
                },
            })
            assert(llm:set_model("route-probe-provider/m1"))
            local current = assert(llm:current())
            local resolved_gw, _, resolved_wire = llm:route_of(current)
            local observed
            C.cordis:on("tool/result", function(data, ev)
                if data.call.name == "route-probe-tool" then
                    observed = data.operation.route
                end
                return ev.next(data)
            end)
            C.cordis:get("tools"):register("route-probe-tool", {
                description = "test",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                idempotent = true,
                execute = function() return { content = "ok" } end,
            })
            local r = C.cordis:get("tools"):execute({ id = "route-1", name = "route-probe-tool", arguments = {} }, {})
            assert(not r.is_error, tostring(r.content))
            return tostring(observed), tostring(resolved_gw) .. ":" .. tostring(resolved_wire)
        "#,
        )
        .await;
        assert_eq!(saw_route, "gw-resolved-route");
        assert_eq!(wire_route, "gw-resolved-route:responses");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn spill_storage_honors_deny_read_and_restrict_read() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (read_denied, save_denied, remove_denied, restrict_denied): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            local sid = S:info().session_id
            local item = mare.spill.save(sid, "secret\n")
            local function denied_code(fn, expected_code)
                local ran, ok, err = pcall(mare.exec.run, {sandbox={level="read-only",network=true,
                    deny_read={mare.paths.state .. "/spill-results"}, accept_partial={"fs_deny_read"}}}, function()
                    return pcall(fn)
                end)
                if not ran or ok then return false end
                if expected_code then
                    return type(err) == "table" and err.code == expected_code
                end
                return tostring(err):find("sandbox:", 1, true) ~= nil
            end
            local a = denied_code(function()
                return mare.spill.read(item.spill_id)
            end, "denied")
            local b = denied_code(function() return mare.spill.save(sid, "new") end, "denied")
            local c = denied_code(function() return mare.spill.delete(item.spill_id) end, "denied")
            local d_ran, d_res = pcall(mare.exec.run, {sandbox={level="read-only", restrict_read=true, readable={}}}, function()
                local save_ok, save_err = pcall(mare.spill.save, sid, "unreadable-under-policy")
                local bytes_ok, bytes_err = pcall(mare.spill.save_bytes, sid, "unreadable-bytes")
                local read_ok, read_err = pcall(mare.spill.read, item.spill_id)
                return not save_ok
                    and type(save_err) == "table" and save_err.code == "denied"
                    and tostring(save_err):find("not readable", 1, true) ~= nil
                    and not bytes_ok
                    and type(bytes_err) == "table" and bytes_err.code == "denied"
                    and not read_ok
                    and type(read_err) == "table"
                    and read_err.code == "denied"
            end)
            assert(mare.spill.read(item.spill_id).content == "secret\n")
            mare.spill.delete(item.spill_id)
            return a, b, c, d_ran and d_res
        "#,
        )
        .await;
        assert!(read_denied && save_denied && remove_denied && restrict_denied);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn spill_write_failure_and_cancellation_never_claim_recoverable() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let root_file = env.cwd.join("blocked-spill-root");
        std::fs::write(&root_file, "not-a-directory").unwrap();
        let valid: bool = eval(
            &h,
            r#"
            -- Unwritable storage root (file blocking directory creation -> real I/O error):
            -- save / save_bytes must reject explicitly and never return a recoverable handle.
            local broken_root = mare.spill.new_local({root="blocked-spill-root", cleanup_period_days=0})
            local ok_text, err_text = pcall(broken_root.save, "owner", "cannot persist")
            local ok_bytes, err_bytes = pcall(broken_root.save_bytes, "owner", string.char(1, 2, 3))
            assert(not ok_text and err_text ~= nil)
            assert(not ok_bytes and err_bytes ~= nil)
            broken_root.dispose()

            -- Cancelled execution context rejects save and leaves no artifacts behind.
            local tok = mare.task.token()
            tok:cancel()
            local cancel_dir = "canceled-spill-root"
            local store = mare.spill.new_local({root=cancel_dir, cleanup_period_days=0})
            local ran_ok, save_ok = pcall(mare.exec.run, {cancel=tok}, function()
                return pcall(store.save, "owner", "canceled payload")
            end)
            assert(ran_ok and not save_ok)
            local ran_bytes_ok, save_bytes_ok = pcall(mare.exec.run, {cancel=tok}, function()
                return pcall(store.save_bytes, "owner", "canceled bytes")
            end)
            assert(not ran_bytes_ok or not save_bytes_ok)
            store.dispose()
            return true
        "#,
        )
        .await;
        assert!(valid);
        let cancel_root = env.cwd.join("canceled-spill-root");
        if cancel_root.exists() {
            assert!(std::fs::read_dir(&cancel_root).unwrap().next().is_none());
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn binary_output_uses_the_same_owner_permissions_and_release_lifecycle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let passed: bool = eval(
            &h,
            r#"
            local sid = S:info().session_id
            local bytes = string.char(0,255,137,80,78,71,13,10,26,10)
            local item = mare.spill.save_bytes(sid, bytes)
            local first = mare.spill.read_bytes(item.spill_id, {limit=4})
            local tail = mare.spill.read_bytes(item.spill_id, {cursor=first.next_byte})
            assert(first.more and not tail.more and first.bytes .. tail.bytes == bytes)
            assert(tail.size == #bytes and tail.next_byte == #bytes)
            local text_ok, text_err = pcall(mare.spill.read, item.spill_id)
            assert(not text_ok and type(text_err) == "table" and text_err.code == "invalid_arguments", tostring(text_err))
            local foreign_ok, foreign_err = pcall(mare.spill.read_bytes, item.spill_id, {session_id="fork-owner"})
            assert(not foreign_ok and type(foreign_err) == "table" and foreign_err.code == "spill.unknown_spill", tostring(foreign_err))
            local function denied_code(fn, expected_code)
                local ran, ok, err = pcall(mare.exec.run, {sandbox={level="read-only",network=true,
                    deny_read={mare.paths.state .. "/spill-results"}, accept_partial={"fs_deny_read"}}}, function()
                    return pcall(fn)
                end)
                if not ran or ok then return false end
                if expected_code then
                    return type(err) == "table" and err.code == expected_code
                end
                return tostring(err):find("sandbox:", 1, true) ~= nil
            end
            assert(denied_code(function() return mare.spill.read_bytes(item.spill_id) end, "denied"))
            assert(denied_code(function() return mare.spill.save_bytes(sid, bytes) end, "denied"))
            assert(denied_code(function() return mare.spill.delete(item.spill_id) end, "denied"))
            local lim_ok, lim_err = pcall(mare.spill.read_bytes, item.spill_id, {limit=0})
            assert(not lim_ok and type(lim_err) == "table" and lim_err.code == "invalid_arguments", tostring(lim_err))
            local neg_lim_ok, neg_lim_err = pcall(mare.spill.read_bytes, item.spill_id, {limit=-1})
            assert(not neg_lim_ok and type(neg_lim_err) == "table" and neg_lim_err.code == "invalid_arguments", tostring(neg_lim_err))
            local str_cur_ok, str_cur_err = pcall(mare.spill.read_bytes, item.spill_id, {cursor="0"})
            assert(not str_cur_ok and type(str_cur_err) == "table" and str_cur_err.code == "invalid_arguments", tostring(str_cur_err))
            local neg_off_ok, neg_off_err = pcall(mare.spill.read, item.spill_id, {offset=-1})
            assert(not neg_off_ok and type(neg_off_err) == "table" and neg_off_err.code == "invalid_arguments", tostring(neg_off_err))
            local str_lim_ok, str_lim_err = pcall(mare.spill.read, item.spill_id, {limit="10"})
            assert(not str_lim_ok and type(str_lim_err) == "table" and str_lim_err.code == "invalid_arguments", tostring(str_lim_err))
            local cur_ok, cur_err = pcall(mare.spill.read_bytes, item.spill_id, {cursor=11})
            assert(not cur_ok and type(cur_err) == "table" and cur_err.code == "invalid_arguments", tostring(cur_err))
            local opt_ok, opt_err = pcall(mare.spill.read_bytes, item.spill_id, {path=item.path})
            assert(not opt_ok and type(opt_err) == "table" and opt_err.code == "invalid_arguments", tostring(opt_err))
            local big_ok, big_err = pcall(mare.spill.save_bytes, sid, string.rep("x", 8388609))
            assert(not big_ok and type(big_err) == "table" and big_err.code == "invalid_arguments", tostring(big_err))
            mare.spill.delete(item.spill_id)
            local after_ok, after_err = pcall(mare.spill.read_bytes, item.spill_id)
            assert(not after_ok and type(after_err) == "table" and after_err.code == "spill.unknown_spill", tostring(after_err))
            return true
        "#,
        )
        .await;
        assert!(passed);
        h.shutdown().await;
    })
    .await
}

#[test]
fn binary_output_real_rpc_roundtrip_resume_and_fork_owner_boundary() {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use base64::Engine as _;
    use serde_json::json;

    fn walk_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk_files(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = tmp.path().join("faux.json");
    std::fs::write(&script, serde_json::to_string(&json!([{ "text": "durable owner" }])).unwrap()).unwrap();
    let home = tmp.path().join("home");
    let config = home.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let output_root = tmp.path().join("configured-output");
    std::fs::write(
        config.join("init.lua"),
        format!(
            "mare.plugin(\"spill\", {{root={},cleanup_period_days=0}})\n",
            json!(output_root.display().to_string())
        ),
    )
    .unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, &work);
    let init = client.call("initialize", json!({ "protocol_version": "3.0" }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
    let started = client.call("agent.prompt", json!({ "session_id": sid, "input": "create persistent session" }));
    client.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
    assert_eq!(
        client.call("spill.list_backends", json!({})),
        json!({ "data": [{ "name": "local", "selected": true }] })
    );
    let original = client.call("session.fork", json!({ "session_id": sid }));
    assert!(
        original["session_id"].as_str().is_some_and(|s| !s.is_empty())
            && original["path"].as_str().is_some_and(|s| !s.is_empty()),
        "{original}"
    );
    client
        .call_err("spill.save", json!({ "session_id": original["session_id"], "content": "x", "backend": "missing" }));

    let mut data: Vec<u8> = (0u8..=255).cycle().take(256 * 511).collect();
    data.extend_from_slice(b"\x00\xff\x89PNG\r\n\x1a\n");
    let saved = client.call(
        "spill.save",
        json!({
            "session_id": original["session_id"],
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&data),
            "backend": "local"
        }),
    );
    assert!(saved["size"] == data.len() as u64 && saved.get("path").is_none(), "{saved}");

    let mut out_files = Vec::new();
    walk_files(&output_root, &mut out_files);
    assert!(
        out_files
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "txt") && std::fs::read(p).is_ok_and(|b| b == data))
    );

    let mut home_files = Vec::new();
    walk_files(&home, &mut home_files);
    let types: Vec<PathBuf> =
        home_files.into_iter().filter(|p| p.ends_with("types/mare/services/spill.gen.lua")).collect();
    assert!(!types.is_empty() && std::fs::read_to_string(&types[0]).unwrap().contains("mare.SpillService"));

    let mut recovered = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = client.call(
            "spill.read",
            json!({
                "session_id": original["session_id"],
                "spill_id": saved["spill_id"],
                "encoding": "base64",
                "offset": offset,
                "limit": 999
            }),
        );
        let chunk = base64::engine::general_purpose::STANDARD.decode(page["data_base64"].as_str().unwrap()).unwrap();
        recovered.extend_from_slice(&chunk);
        assert_eq!(page["size"], json!(data.len() as u64), "{page}");
        let Some(next_offset) = page["next_offset"].as_u64() else {
            break;
        };
        assert!(next_offset > offset, "{page}");
        offset = next_offset;
    }
    assert_eq!(recovered, data);

    for encoded in ["!", "AA", "AA==\n", "AB=="] {
        client.call_err("spill.save", json!({ "session_id": original["session_id"], "data_base64": encoded }));
    }
    client.call_err(
        "spill.read",
        json!({ "session_id": original["session_id"], "spill_id": saved["spill_id"], "encoding": "base64", "limit": 0 }),
    );
    client.call_err(
        "spill.read",
        json!({ "session_id": original["session_id"], "spill_id": saved["spill_id"], "encoding": "base64", "offset": data.len() + 1 }),
    );
    client.call_err(
        "spill.read",
        json!({ "session_id": original["session_id"], "spill_id": saved["spill_id"], "encoding": "base64", "path": "/secret" }),
    );

    let fork = client.call("session.fork", json!({ "session_id": original["session_id"] }));
    assert_ne!(fork["session_id"], original["session_id"]);
    client.call_err(
        "spill.read",
        json!({ "session_id": fork["session_id"], "spill_id": saved["spill_id"], "encoding": "base64" }),
    );
    client.call("session.load", json!({ "path": original["path"] }));
    let page = client.call(
        "spill.read",
        json!({ "session_id": original["session_id"], "spill_id": saved["spill_id"], "encoding": "base64" }),
    );
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(page["data_base64"].as_str().unwrap()).unwrap(), data);
    client.call("spill.delete", json!({ "session_id": original["session_id"], "spill_id": saved["spill_id"] }));
    client.call_err(
        "spill.read",
        json!({ "session_id": original["session_id"], "spill_id": saved["spill_id"], "encoding": "base64" }),
    );
    client.shutdown_and_wait();
}

#[tokio::test]
async fn rich_spill_metadata_and_retrieval_hint_preserve_owner_isolation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["spill"]).no_session().build().await;
        expose(&h);
        let passed: bool = eval(
            &h,
            r#"
            local spill = C.cordis:get("spill")
            local a = spill:save_owned("sess-a", "line1\nneedle line\n", {
                suggested_name = "../tool/grep_me.txt",
                source = { kind = "tool", tool_name = "probe", tool_call_id = "call-1", label = "result" },
            })
            local b = spill:save_owned("sess-a", "second", {
                suggested_name = "../tool/grep_me.txt",
                source = { kind = "session-reference", session_id = "sess-0", label = "transcript" },
            })
            assert(a.spill_id ~= b.spill_id and a.path ~= b.path)
            assert(a.path:find("grep_me.txt", 1, true) and not a.path:find("/../", 1, true))
            assert(type(a.retrieval_hint) == "string" and a.retrieval_hint:find(a.path, 1, true))
            assert(a.source.kind == "tool" and a.source.tool_name == "probe" and a.source.tool_call_id == "call-1" and a.source.label == "result")
            assert(b.source.kind == "session-reference" and b.source.session_id == "sess-0" and b.source.label == "transcript")
            assert(spill:read_owned("sess-a", a.spill_id).content:find("needle", 1, true))
            -- Descriptive source metadata never grants cross-session access.
            assert(not pcall(spill.read_owned, spill, "sess-0", b.spill_id))
            assert(not pcall(spill.save_owned, spill, "sess-a", "x", { unknown_opt = true }))
            assert(not pcall(spill.save_owned, spill, "sess-a", "x", { source = { kind = "invalid" } }))
            assert(not pcall(spill.save_owned, spill, "sess-a", "x", { source = { kind = "session_reference", session_id = "sess-0", label = "transcript" } }))
            assert(not pcall(spill.save_owned, spill, "sess-a", "x", { source = { kind = "tool", tool_name = "p" } }))
            assert(not pcall(spill.save_owned, spill, "sess-a", "x", { source = { kind = "tool", tool_name = "p", call_id = "legacy" } }))
            spill:delete_owned("sess-a", a.spill_id)
            spill:delete_owned("sess-a", b.spill_id)
            return true
        "#,
        )
        .await;
        assert!(passed);
        h.shutdown().await;
    })
    .await
}

/// A raw `mare.spill` handle (what a producer gets from `mare.spill.save` / `save_bytes`) is read
/// through the `spill` service and the UI's artifact image resolver like a `spill:v1:` locator;
/// the handle stays opaque to the caller (flip batch: plugin producers return raw spill handles).
#[tokio::test]
async fn raw_spill_handles_read_through_the_output_service_and_ui_images() {
    local(async {
        let env = TestEnv::new().await;
        // an ephemeral session: the image resolver below reads outside any tool call
        let h = env.host().builtins(&["spill"]).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local info = S:info()
            BYTES = "\0\255\137PNG raw spill bytes"
            local text = mare.spill.save(info.session_id, "raw spill text")
            local bin = mare.spill.save_bytes(info.session_id, BYTES)
            local spill = C.cordis:get("spill")
            assert(spill:read_owned(info.session_id, text.spill_id).content == "raw spill text")
            local page = spill:read_bytes(info.session_id, bin.spill_id)
            assert(page.bytes == BYTES and page.next_offset == nil, "bytes differ")
            -- the session owner is still enforced (by mare.spill)
            assert(not pcall(spill.read_owned, spill, "other-session", text.spill_id))
            IMAGE = C.cordis:get("ui"):artifact_image({ session_id = info.session_id, spill_id = bin.spill_id,
                sha256 = mare.crypto.sha256(BYTES), width = 4, height = 2 }).resource
            "#,
        )
        .await;
        let value: mlua::Table = h.lua().globals().get("IMAGE").unwrap();
        let reference = mare_shared::image_resource::Reference::new(
            value.get("resolver_id").unwrap(),
            value.get("key").unwrap(),
            4,
            2,
        )
        .unwrap();
        let loaded = tokio::task::spawn_blocking(move || reference.load()).await.unwrap().unwrap();
        assert_eq!(&loaded[..], b"\0\xff\x89PNG raw spill bytes");
        h.shutdown().await;
    })
    .await;
}

fn spawn_spill_rpc(
    extra_args: &[&str],
    init_lua: Option<&str>,
    plugin: Option<(&str, &str)>,
) -> (tempfile::TempDir, std::path::PathBuf, crate::common::rpc::RpcClient, serde_json::Value) {
    use serde_json::json;
    use std::collections::BTreeMap;

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = tmp.path().join("faux.json");
    std::fs::write(&script, serde_json::to_string(&json!([{ "text": "spill session" }])).unwrap()).unwrap();
    let home = tmp.path().join("home");
    let config = home.join("config");
    std::fs::create_dir_all(config.join("plugins")).unwrap();
    let spill_root = tmp.path().join("configured-spill");
    let default_lua =
        format!("mare.plugin(\"spill\", {{root={},cleanup_period_days=0}})\n", json!(spill_root.display().to_string()));
    std::fs::write(config.join("init.lua"), init_lua.unwrap_or(&default_lua)).unwrap();
    if let Some((filename, body)) = plugin {
        std::fs::write(config.join("plugins").join(filename), body).unwrap();
    }
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut args = vec!["--rpc", "--yolo", "--model", "faux/echo"];
    args.extend_from_slice(extra_args);
    let mut client = crate::common::rpc::RpcClient::spawn(&args, &env, &work);
    let init = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
    let started = client.call("agent.prompt", json!({ "session_id": sid, "input": "create persistent session" }));
    client.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
    (tmp, home, client, init)
}

/// `spill.read` over RPC accepts a raw `mare.spill` handle (Mermaid's artifact images,
/// read_spill's large events).
#[test]
fn raw_spill_handles_read_over_rpc() {
    use base64::Engine as _;
    use serde_json::json;

    let (_tmp, _home, mut client, _init) = spawn_spill_rpc(
        &[],
        None,
        Some((
            "raw-producer.lua",
            r#"return {name="raw-producer",inject={"session"},provide={"raw-producer"},contracts={["raw-producer"]={methods={save={doc="save",expose="both",
    params={type="object",properties={data={type="string"}},required={"data"},additionalProperties=false},
    order={"data"},result={type="string"}}}}},apply=function(ctx)
    local Raw = require("cordis").class("Raw", mare.Service)
    function Raw:constructor(c) Raw.super.constructor(self, c, "raw-producer") end
    function Raw:save(data)
        return mare.spill.save_bytes(ctx.session:info().session_id, data).spill_id
    end
    ctx:plugin(Raw)
end}
"#,
        )),
    );
    let cur_sid = client.call("session.list", json!({ "loaded": true }))["data"][0]["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let session_id = client.call("session.fork", json!({ "session_id": cur_sid }))["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let handle = client.call("raw-producer.save", json!({ "data": "raw text" }));
    let handle = handle.as_str().expect("handle").to_owned();
    let page = client.call("spill.read", json!({ "session_id": session_id, "spill_id": handle, "encoding": "base64" }));
    let bytes = base64::engine::general_purpose::STANDARD.decode(page["data_base64"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, b"raw text", "{page}");
    let text = client.call("spill.read", json!({ "session_id": session_id, "spill_id": handle }));
    assert_eq!(text["content"], json!("raw text"), "{text}");
    client.shutdown_and_wait();
}

#[test]
fn spill_contract_renames_output_enforces_session_scope_and_base64_limits() {
    use base64::Engine as _;
    use serde_json::json;

    let (_tmp, home, mut client, init) = spawn_spill_rpc(&["--peer"], None, None);

    assert_eq!(
        client.call("spill.list_backends", json!({})),
        json!({ "data": [{ "name": "local", "selected": true }] })
    );

    let missing_sid = client.call_err("spill.save", json!({ "content": "missing sid" }));
    assert_eq!(missing_sid["code"], json!(-32602), "{missing_sid}");

    let original = client.call("session.fork", json!({ "session_id": init["session"]["session_id"] }));
    let sid = original["session_id"].as_str().unwrap().to_owned();
    let text_saved = client.call("spill.save", json!({ "session_id": sid, "content": "hello spill" }));
    assert_eq!(text_saved["size"], json!(11));
    assert!(text_saved.get("path").is_none(), "{text_saved}");
    let text_read = client.call("spill.read", json!({ "session_id": sid, "spill_id": text_saved["spill_id"] }));
    assert_eq!(text_read["content"], json!("hello spill"));
    assert!(text_read.get("next_offset").is_none(), "{text_read}");

    // Text mode multi-page byte offset pagination over a >50 KiB single line.
    let long_utf8 = "界".repeat(25_000);
    let long_saved = client.call("spill.save", json!({ "session_id": sid, "content": long_utf8 }));
    let mut recovered_text = String::new();
    let mut offset: Option<u64> = None;
    let mut pages = 0usize;
    loop {
        let mut params = json!({ "session_id": sid, "spill_id": long_saved["spill_id"] });
        if let Some(byte_pos) = offset {
            params["offset"] = json!(byte_pos);
        }
        let page = client.call("spill.read", params);
        pages += 1;
        recovered_text.push_str(page["content"].as_str().unwrap());
        let Some(next_offset) = page["next_offset"].as_u64() else {
            break;
        };
        offset = Some(next_offset);
    }
    assert!(pages >= 2, "expected multiple pages for 75 KB UTF-8 text, got {pages}");
    assert_eq!(recovered_text, long_utf8);

    // Mid-UTF-8 offset is invalid_arguments, not spill.unknown_spill.
    let mid_utf8 =
        client.call_err("spill.read", json!({ "session_id": sid, "spill_id": long_saved["spill_id"], "offset": 1 }));
    assert_eq!(mid_utf8["data"]["code"], json!("invalid_arguments"), "{mid_utf8}");

    // Multi-line offset/limit pagination and parameter boundary checks.
    let lined = client.call("spill.save", json!({ "session_id": sid, "content": "line1\nline2\nline3\nline4" }));
    let lined_page =
        client.call("spill.read", json!({ "session_id": sid, "spill_id": lined["spill_id"], "offset": 6, "limit": 2 }));
    assert_eq!(lined_page["content"], json!("line2\nline3\n"));
    let next_off = lined_page["next_offset"].as_u64().unwrap();
    let lined_tail = client.call(
        "spill.read",
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "offset": next_off, "limit": 2 }),
    );
    assert_eq!(lined_tail["content"], json!("line4"));
    assert!(lined_tail.get("next_offset").is_none(), "{lined_tail}");

    for bad_read in [
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "limit": 0 }),
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "limit": 2001 }),
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "offset": -1 }),
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "encoding": "base64", "limit": 0 }),
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "encoding": "base64", "limit": 8_388_609 }),
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "encoding": "base64", "offset": -1 }),
        json!({ "session_id": sid, "spill_id": lined["spill_id"], "encoding": "base64", "offset": 100 }),
    ] {
        let err = client.call_err("spill.read", bad_read.clone());
        assert_eq!(err["data"]["code"], json!("invalid_arguments"), "{bad_read} -> {err}");
    }

    // Exact 8 MiB boundary: 8,388,608 decoded bytes succeed; 8,388,609 bytes and 11,184,813 base64 chars fail.
    let exact_max = vec![b'Z'; 8 * 1024 * 1024];
    let max_b64 = base64::engine::general_purpose::STANDARD.encode(&exact_max);
    assert_eq!(max_b64.len(), 11_184_812);
    let max_saved = client.call("spill.save", json!({ "session_id": sid, "data_base64": max_b64 }));
    assert_eq!(max_saved["size"], json!(8_388_608));
    let max_read = client.call(
        "spill.read",
        json!({ "session_id": sid, "spill_id": max_saved["spill_id"], "encoding": "base64", "limit": 8_388_608 }),
    );
    assert!(max_read.get("next_offset").is_none(), "{max_read}");
    assert_eq!(
        base64::engine::general_purpose::STANDARD.decode(max_read["data_base64"].as_str().unwrap()).unwrap().len(),
        8_388_608
    );

    let over_max_b64 = base64::engine::general_purpose::STANDARD.encode(vec![b'Z'; 8 * 1024 * 1024 + 1]);
    let over_err = client.call_err("spill.save", json!({ "session_id": sid, "data_base64": over_max_b64 }));
    assert_eq!(over_err["data"]["code"], json!("invalid_arguments"), "{over_err}");
    let over_chars_err =
        client.call_err("spill.save", json!({ "session_id": sid, "data_base64": "A".repeat(11_184_813) }));
    assert_eq!(over_chars_err["data"]["code"], json!("invalid_arguments"), "{over_chars_err}");

    let payload = b"\x00\xffPNG\n";
    let bin_saved = client.call(
        "spill.save",
        json!({
            "session_id": sid,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(payload),
            "backend": "local"
        }),
    );
    assert_eq!(bin_saved["size"], json!(payload.len()));
    let bin_as_text = client.call_err("spill.read", json!({ "session_id": sid, "spill_id": bin_saved["spill_id"] }));
    assert_eq!(bin_as_text["data"]["code"], json!("invalid_arguments"), "{bin_as_text}");

    let bin_head = client.call(
        "spill.read",
        json!({
            "session_id": sid,
            "spill_id": bin_saved["spill_id"],
            "encoding": "base64",
            "offset": 0,
            "limit": 2
        }),
    );
    assert!(bin_head["next_offset"].as_u64().is_some(), "{bin_head}");
    assert_eq!(bin_head["size"], json!(payload.len()));
    let head_bytes =
        base64::engine::general_purpose::STANDARD.decode(bin_head["data_base64"].as_str().unwrap()).unwrap();
    let bin_tail = client.call(
        "spill.read",
        json!({
            "session_id": sid,
            "spill_id": bin_saved["spill_id"],
            "encoding": "base64",
            "offset": bin_head["next_offset"]
        }),
    );
    assert!(bin_tail.get("next_offset").is_none(), "{bin_tail}");
    let mut recovered = head_bytes;
    recovered.extend_from_slice(
        &base64::engine::general_purpose::STANDARD.decode(bin_tail["data_base64"].as_str().unwrap()).unwrap(),
    );
    assert_eq!(recovered, payload);

    client.call_err("spill.save", json!({ "session_id": sid }));
    client.call_err("spill.save", json!({ "session_id": sid, "content": "a", "data_base64": "YQ==" }));
    for encoded in ["!", "AA", "AA==\n", "AB=="] {
        client.call_err("spill.save", json!({ "session_id": sid, "data_base64": encoded }));
    }

    // Domain error codes: spill.unknown_backend, spill.unknown_spill, and invalid_arguments for malformed locator.
    let unknown_save =
        client.call_err("spill.save", json!({ "session_id": sid, "content": "x", "backend": "no-such-backend" }));
    assert_eq!(unknown_save["data"]["code"], json!("spill.unknown_backend"), "{unknown_save}");
    let unknown_read =
        client.call_err("spill.read", json!({ "session_id": sid, "spill_id": "spill:v1:no-such-backend:eA:eQ" }));
    assert_eq!(unknown_read["data"]["code"], json!("spill.unknown_backend"), "{unknown_read}");
    let bogus_handle = client.call_err("spill.read", json!({ "session_id": sid, "spill_id": "not-a-handle" }));
    assert_eq!(bogus_handle["data"]["code"], json!("invalid_arguments"), "{bogus_handle}");

    let fork = client.call("session.fork", json!({ "session_id": sid }));
    assert_ne!(fork["session_id"], original["session_id"]);
    let fork_read = client.call_err(
        "spill.read",
        json!({ "session_id": fork["session_id"], "spill_id": bin_saved["spill_id"], "encoding": "base64" }),
    );
    assert_eq!(fork_read["data"]["code"], json!("spill.unknown_spill"), "{fork_read}");
    client.call_err("spill.read", json!({ "spill_id": bin_saved["spill_id"], "session_id": "foreign" }));
    client.call("session.load", json!({ "path": original["path"] }));
    client.call_err("spill.read", json!({ "spill_id": bin_saved["spill_id"], "session_id": fork["session_id"] }));
    let resumed_page = client
        .call("spill.read", json!({ "session_id": sid, "spill_id": bin_saved["spill_id"], "encoding": "base64" }));
    assert_eq!(
        base64::engine::general_purpose::STANDARD.decode(resumed_page["data_base64"].as_str().unwrap()).unwrap(),
        payload
    );

    assert_eq!(
        client.call("spill.delete", json!({ "session_id": sid, "spill_id": text_saved["spill_id"] })),
        json!({})
    );
    let removed_read = client.call_err("spill.read", json!({ "session_id": sid, "spill_id": text_saved["spill_id"] }));
    assert_eq!(removed_read["data"]["code"], json!("spill.unknown_spill"), "{removed_read}");
    let removed_again =
        client.call_err("spill.delete", json!({ "session_id": sid, "spill_id": text_saved["spill_id"] }));
    assert_eq!(removed_again["data"]["code"], json!("spill.unknown_spill"), "{removed_again}");

    // access = "primary": peer clients are denied on every spill.* RPC method.
    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    peer.call("initialize", json!({ "protocol_version": init["protocol_version"] }));
    for (method, params) in [
        ("spill.list_backends", json!({})),
        ("spill.save", json!({ "session_id": sid, "content": "from peer" })),
        ("spill.read", json!({ "session_id": sid, "spill_id": bin_saved["spill_id"] })),
        ("spill.delete", json!({ "session_id": sid, "spill_id": bin_saved["spill_id"] })),
    ] {
        let denied = peer.call(method, params);
        assert_eq!(denied["error"]["data"]["code"], json!("denied"), "{method} -> {denied}");
    }

    let old_err = client.call_err("output.backends", json!({}));
    assert_eq!(old_err["code"], json!(-32601), "legacy output.backends must return METHOD_NOT_FOUND: {old_err}");
    client.shutdown_and_wait();
}

#[tokio::test]
async fn spill_read_and_delete_take_spill_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["spill"]).build().await;
        expose(&h);
        let (content, foreign_code, old_read_code, old_del_code, after_del_ok): (String, String, String, String, bool) =
            eval(
                &h,
                r#"
                local sid = S:info().session_id
                local saved = mare.spill.save(sid, "hello spill\n")
                assert(type(saved.spill_id) == "string" and saved.handle == nil, "save must return spill_id, not handle")
                local page = mare.spill.read(saved.spill_id)

                -- Reading a spill owned by a different session must fail with `spill.unknown_spill`.
                local other = mare.spill.save("other-session-id", "foreign secret\n")
                local f_ok, f_err = pcall(mare.spill.read, other.spill_id)
                assert(not f_ok, "foreign session spill must be refused")
                local foreign_code = type(f_err) == "table" and tostring(f_err.code) or tostring(f_err)

                -- Legacy two-positional-argument signatures must fail with `invalid_arguments`.
                local r2_ok, r2_err = pcall(mare.spill.read, sid, saved.spill_id)
                assert(not r2_ok, "legacy 2-arg mare.spill.read(session_id, id) must fail")
                local old_read_code = type(r2_err) == "table" and tostring(r2_err.code) or tostring(r2_err)

                local d2_ok, d2_err = pcall(mare.spill.delete, sid, saved.spill_id)
                assert(not d2_ok, "legacy 2-arg mare.spill.delete(session_id, id) must fail")
                local old_del_code = type(d2_err) == "table" and tostring(d2_err.code) or tostring(d2_err)

                mare.spill.delete(saved.spill_id)
                local after_ok, after_err = pcall(mare.spill.read, saved.spill_id)
                assert(not after_ok and type(after_err) == "table" and after_err.code == "spill.unknown_spill", tostring(after_err))
                return page.content, foreign_code, old_read_code, old_del_code, after_ok
                "#,
            )
            .await;
        assert_eq!(content, "hello spill\n");
        assert_eq!(foreign_code, "spill.unknown_spill");
        assert_eq!(old_read_code, "invalid_arguments");
        assert_eq!(old_del_code, "invalid_arguments");
        assert!(!after_del_ok, "deleted spill must no longer be readable");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn spill_confined_security_boundaries_and_session_lifecycle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["spill"])
            .faux(serde_json::json!([
                { "tool_calls": [{ "id": "spill-probe-1", "name": "spill_security_probe", "arguments": {} }] },
                { "text": "probe done" }
            ]))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            ORIG_SID = S:info().session_id
            OWN = mare.spill.save(ORIG_SID, "own-secret\n")
            OWN_BIN = mare.spill.save_bytes(ORIG_SID, "\0\1own-bin")
            OTHER_SID = "foreign-session-99"
            FOREIGN = mare.spill.save(OTHER_SID, "foreign-secret\n")
            PROBE_RESULTS = nil
            T:register("spill_security_probe", {
                description = "Verify spill session isolation inside a confined tool call",
                side_effect = "read",
                parameters = { type = "object", properties = {} },
                execute = function()
                    -- 1. Reading own session's spill inside a confined tool call is allowed.
                    local p = mare.spill.read(OWN.spill_id)
                    assert(p.content == "own-secret\n", "confined own read failed")
                    local p_same = mare.spill.read(OWN.spill_id, { session_id = ORIG_SID })
                    assert(p_same.content == "own-secret\n", "confined matching session_id failed")
                    local pb = mare.spill.read_bytes(OWN_BIN.spill_id)
                    assert(pb.bytes == "\0\1own-bin", "confined own read_bytes failed")

                    -- 2. Reading another session's spill fails with `spill.unknown_spill`.
                    local f_ok, f_err = pcall(mare.spill.read, FOREIGN.spill_id)
                    assert(not f_ok and type(f_err) == "table", tostring(f_err))

                    -- 3. Overriding `opts.session_id` to another session inside a confined call fails with `denied`.
                    local o_ok, o_err = pcall(mare.spill.read, FOREIGN.spill_id, { session_id = OTHER_SID })
                    assert(not o_ok and type(o_err) == "table", tostring(o_err))
                    local ob_ok, ob_err = pcall(mare.spill.read_bytes, FOREIGN.spill_id, { session_id = OTHER_SID })
                    assert(not ob_ok and type(ob_err) == "table", tostring(ob_err))
                    local od_ok, od_err = pcall(mare.spill.delete, FOREIGN.spill_id, { session_id = OTHER_SID })
                    assert(not od_ok and type(od_err) == "table", tostring(od_err))

                    PROBE_RESULTS = { f = f_err.code, o = o_err.code, ob = ob_err.code, od = od_err.code }
                    return { content = "probed" }
                end,
            })
            "#,
        )
        .await;
        // Run the tool inside a real persisted turn (confined tool execution context).
        h.prompt("run spill probe", serde_json::json!({})).await.unwrap();
        let summary: String = eval(
            &h,
            r#"
            assert(type(PROBE_RESULTS) == "table", "spill_security_probe tool did not run")
            assert(PROBE_RESULTS.f == "spill.unknown_spill", "expected spill.unknown_spill, got " .. tostring(PROBE_RESULTS.f))
            assert(PROBE_RESULTS.o == "denied", "expected denied on read override, got " .. tostring(PROBE_RESULTS.o))
            assert(PROBE_RESULTS.ob == "denied", "expected denied on read_bytes override, got " .. tostring(PROBE_RESULTS.ob))
            assert(PROBE_RESULTS.od == "denied", "expected denied on delete override, got " .. tostring(PROBE_RESULTS.od))

            local orig_info = S:info()
            local orig_path = assert(orig_info.path, "persisted session must have a path")

            -- 4. Switching to a new session isolates the old session's spill, and resuming restores access.
            S:create()
            local second_sid = S:info().session_id
            assert(second_sid ~= ORIG_SID)
            local second_own = mare.spill.save(second_sid, "second-session\n")
            assert(mare.spill.read(second_own.spill_id).content == "second-session\n")
            local cross_ok, cross_err = pcall(mare.spill.read, OWN.spill_id)
            assert(not cross_ok and type(cross_err) == "table" and cross_err.code == "spill.unknown_spill", tostring(cross_err))

            -- Resume back to the original session: its spill must be readable again.
            S:load({ path = orig_path })
            assert(S:info().session_id == ORIG_SID)
            assert(mare.spill.read(OWN.spill_id).content == "own-secret\n")
            assert(mare.spill.read_bytes(OWN_BIN.spill_id).bytes == "\0\1own-bin")

            mare.spill.delete(OWN.spill_id)
            mare.spill.delete(OWN_BIN.spill_id)
            mare.spill.delete(second_own.spill_id, { session_id = second_sid })
            mare.spill.delete(FOREIGN.spill_id, { session_id = OTHER_SID })
            return "ok"
            "#,
        )
        .await;
        assert_eq!(summary, "ok");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn spill_exec_ctx_session_takes_precedence_over_active_session_after_switch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["spill"])
            .faux(serde_json::json!([
                { "tool_calls": [{ "id": "ctx-prio-1", "name": "spawn_deferred_spill_reader", "arguments": {} }] },
                { "text": "spawned" }
            ]))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            ORIG_SID = S:info().session_id
            OWN_A = mare.spill.save(ORIG_SID, "from-session-a\n")
            SWITCHED_TX, SWITCHED_RX = mare.task.channel()
            DONE_TX, DONE_RX = mare.task.channel()
            T:register("spawn_deferred_spill_reader", {
                description = "Spawn a background task that inherits this tool call's execution context",
                side_effect = "read",
                parameters = { type = "object", properties = {} },
                execute = function()
                    mare.task.spawn(function()
                        assert(SWITCHED_RX:recv())
                        -- Host active session (session_cell) is now SECOND_SID, while our inherited
                        -- exec_ctx still carries ORIG_SID. exec_ctx must take precedence.
                        local page = mare.spill.read(OWN_A.spill_id)
                        local b_ok, b_err = pcall(mare.spill.read, OWN_B.spill_id)
                        local ov_ok, ov_err = pcall(mare.spill.read, OWN_B.spill_id, { session_id = SECOND_SID })
                        DONE_TX:send({
                            a_content = page.content,
                            b_ok = b_ok,
                            b_code = type(b_err) == "table" and b_err.code or tostring(b_err),
                            ov_ok = ov_ok,
                            ov_code = type(ov_err) == "table" and ov_err.code or tostring(ov_err),
                        })
                    end)
                    return { content = "task spawned" }
                end,
            })
            "#,
        )
        .await;
        h.prompt("spawn deferred reader", serde_json::json!({})).await.unwrap();
        let (a_content, b_code, ov_code): (String, String, String) = eval(
            &h,
            r#"
            S:create()
            SECOND_SID = S:info().session_id
            assert(SECOND_SID ~= ORIG_SID, "session switch must produce a different session_id")
            OWN_B = mare.spill.save(SECOND_SID, "from-session-b\n")
            SWITCHED_TX:send(true)
            local res = DONE_RX:recv()
            assert(not res.b_ok, "inherited exec_ctx must not read newly active session B's spill")
            assert(not res.ov_ok, "inherited exec_ctx must not override session_id to session B")
            mare.spill.delete(OWN_A.spill_id, { session_id = ORIG_SID })
            mare.spill.delete(OWN_B.spill_id)
            return res.a_content, res.b_code, res.ov_code
            "#,
        )
        .await;
        assert_eq!(a_content, "from-session-a\n");
        assert_eq!(b_code, "spill.unknown_spill");
        assert_eq!(ov_code, "denied");
        h.shutdown().await;
    })
    .await;
}
