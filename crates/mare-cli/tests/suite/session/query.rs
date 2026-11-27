//! Integration coverage for session & event query (`mare.store.*` + `--rpc` `session.*`).

use std::process::Stdio;

use super::*;

#[test]
fn custom_records_return_record_id_without_forging_item_events() {
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    mare_binding::codec::install(&lua, &mare).unwrap();
    mare_binding::store::install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let mare_lib: mlua::Table =
        lua.load(include_str!("../../../../mare-plugin/preset/plugins/lua/mare_lib.lua")).eval().unwrap();
    let factory: mlua::Function =
        lua.load(include_str!("../../../../mare-plugin/preset/plugins/lua/session/recorder.lua")).eval().unwrap();
    let query_factory: mlua::Function =
        lua.load(include_str!("../../../../mare-plugin/preset/plugins/lua/session/queries.lua")).eval().unwrap();
    let exercise: mlua::Function = lua
        .load(
            r#"
            return function(mare_lib, factory, query_factory)
              local events, written, fail_write = {}, nil, false
              mare.task = { now = function() return 1 end }
              mare.error = function(code, msg) return tostring(code) .. ': ' .. tostring(msg) end
              mare.projection = { project = function(value) return value end }
              package.loaded.mare_lib = mare_lib
              local state = { id = 's1', generation = 'g1', list = {} }
              local store = {
                new_id = function() return 'record-1' end,
                check_new_id = function() return true end,
                check_owner = function() return true end,
                is_reserved_record_kind = function() return false end,
                copy_from_tool = function(value) return value end,
                run_host = function(f) return f(function() return true end) end,
                add = function(entry, persist, sync)
                  if fail_write then return nil, 'fsync failed' end
                  assert(persist and sync)
                  written = entry
                  return true
                end,
              }
              local ctx = { bus = { emit = function(_, name, data) events[#events + 1] = { name, data } end },
                            kernel = { session_epoch = function() return 'epoch-1' end },
                            agent = { activity = function() return { busy = false } end }, on = function() end }
              local full_reads = 0
              local history = { index_update = function() end, ensure_full = function() full_reads = full_reads + 1 end,
                                events_for = function() return state.list end, path_to = function() return state.list end }
              local D = { ctx = ctx, S = state, session = {}, store = store, history = history,
                          dir = 'test-dir', warn_unsupported = function() end }
              query_factory(D)
              factory(D)
              local result = D.session.add_record(nil, nil, 'test/note', { value = 1 }, { sync = true })
              assert(result.record_id == 'record-1' and result.item_id == nil and result.durable == true)
              assert(written.entry_id == result.record_id and written.type == 'custom')
              assert(#events == 1 and events[1][1] == 'session/recorded' and events[1][2].entry == written,
                'custom records must use the record notification, not item events')
              fail_write = true
              local ok, err = pcall(D.session.add_record, nil, nil, 'test/note', {}, { sync = true })
              assert(not ok and tostring(err):find('persist_failed', 1, true))
              assert(#events == 1, 'failed writes must not publish record or item events')
              for i = 1, 5 do state.list[i] = { type = 'custom', kind = 'test/page', entry_id = 'r' .. i, data = { n = i } } end
              local first = D.session.list_records(nil, nil, 'test/page', { branch = 'all', limit = 2 })
              assert(#first.data == 2 and first.data[1].n == 1 and first.data[2].n == 2)
              assert(type(first.next_cursor) == 'string' and first.cursor == nil and full_reads > 0)
              local second = D.session.list_records(nil, nil, 'test/page', { branch = 'all', limit = 2, cursor = first.next_cursor })
              local third = D.session.list_records(nil, nil, 'test/page', { branch = 'all', limit = 2, cursor = second.next_cursor })
              assert(second.data[1].n == 3 and second.data[2].n == 4 and third.data[1].n == 5 and third.next_cursor == nil)
              state.list[6] = { type = 'custom', kind = 'test/page', entry_id = 'r6', data = { n = 6 } }
              local stale, why = pcall(D.session.list_records, nil, nil, 'test/page', { branch = 'all', limit = 2, cursor = first.next_cursor })
              assert(not stale and tostring(why):find('stale_session', 1, true))
              local invalid, reason = pcall(D.session.list_records, nil, nil, 'test/page', { limit = 0 })
              assert(not invalid and tostring(reason):find('invalid_arguments', 1, true))
              local sessions = {}
              for i = 1, 5 do sessions[6-i] = { header = { session_id = 's' .. i, cwd = '/work', created_at_ms = 1 }, mtime = 100 } end
              mare.cwd = '/work'
              mare.fs = { resolve = function(path) return path end }
              mare.contracts = { permitted = function() return true end, is_peer = function() return false end, allows_session_path = function() return true end }
              mare.store.list = function() return sessions, {} end
              mare.store.index_read = function() return {} end
              local page1 = D.session.list(nil, nil, { limit = 2 })
              assert(#page1.data == 2 and page1.data[1].session_id == 's1' and page1.data[2].session_id == 's2')
              assert(type(page1.next_cursor) == 'string' and page1.cursor == nil)
              local page2 = D.session.list(nil, nil, { limit = 2, archived = false, cursor = page1.next_cursor })
              local page3 = D.session.list(nil, nil, { limit = 2, cursor = page2.next_cursor })
              assert(page2.data[1].session_id == 's3' and page2.data[2].session_id == 's4')
              assert(page3.data[1].session_id == 's5' and page3.next_cursor == nil)
              sessions[1].mtime = 101
              local stale_list, list_error = pcall(D.session.list, nil, nil, { limit = 2, cursor = page1.next_cursor })
              assert(not stale_list and tostring(list_error):find('stale_session', 1, true))
              state.list = {
                { type = 'custom', entry_id = 'record', kind = 'note', data = {} },
                { type = 'message', entry_id = 'assistant', message = { role = 'assistant', content = {
                  { type = 'text', text = 'two calls' },
                  { type = 'tool_call', id = 'call-a', name = 'read', arguments = { path = 'a' } },
                  { type = 'tool_call', id = 'call-b', name = 'read', arguments = { path = 'b' } },
                } } },
                { type = 'message', entry_id = 'result-a', message = { role = 'tool_result', tool_call_id = 'call-a', content = {} } },
                { type = 'message', entry_id = 'result-b', message = { role = 'tool_result', tool_call_id = 'call-b', content = {} } },
                { type = 'leaf', target_entry_id = 'assistant' },
              }
              local items1 = D.session.list_items(nil, nil, { limit = 2 })
              assert(#items1.data == 2 and items1.data[1].item_id == 'assistant' and items1.data[1].entry_id == 'assistant')
              assert(items1.data[2].item_id == 'call-a' and items1.data[2].entry_id == 'assistant')
              assert(items1.data[2].result_entry_id == 'result-a' and type(items1.next_cursor) == 'string')
              local items2 = D.session.list_items(nil, nil, { limit = 2, cursor = items1.next_cursor })
              assert(#items2.data == 1 and items2.data[1].item_id == 'call-b' and items2.data[1].result_entry_id == 'result-b')
              assert(items2.next_cursor == nil)
              local calls = D.session.list_items(nil, nil, { types = { 'tool_call' }, direction = 'desc' })
              assert(#calls.data == 2 and calls.data[1].item_id == 'call-b' and calls.data[2].item_id == 'call-a')
              local selected, forwarded
              local function record_call(tag)
                return function(args)
                  selected, forwarded = tag, args
                  return { data = {} }
                end
              end
              mare.store.search_items = record_call('search_items')
              mare.store.read_item = record_call('read_item')
              mare.store.trace_item = record_call('trace_item')
              D.session.search_items(nil, nil, { session_id = 's1', query = 'two' })
              assert(selected == 'search_items', 'session.search_items must use item semantics')
              D.session.get_item(nil, nil, { session_id = 's1', item_id = 'call-a' })
              assert(selected == 'read_item' and forwarded.session_id == 's1' and forwarded.item_id == 'call-a')
              D.session.trace_item(nil, nil, { session_id = 's1', item_id = 'call-a' })
              assert(selected == 'trace_item' and forwarded.session_id == 's1' and forwarded.item_id == 'call-a')
            end
            "#,
        )
        .eval()
        .unwrap();
    exercise.call::<()>((mare_lib, factory, query_factory)).unwrap();
}

/// `session.refresh.reconcile()` must cap total retry time when `index_reconcile` keeps
/// reporting `pending > 0` instead of looping forever and blocking waiting callers.
#[tokio::test]
async fn session_refresh_reconcile_bounds_pending_retry_wait() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (finished, calls): (bool, i64) = eval(
            &h,
            r#"local orig_reconcile = mare.store.index_reconcile
               local orig_notice = mare.store.index_notice
               local calls = 0
               mare.store.index_reconcile = function()
                 calls = calls + 1
                 return { files = 1, updated = 0, removed = 0, failed = 0, pending = 1 }
               end
               mare.store.index_notice = function() return nil end
               local refresh = require("session.refresh")
               local r = refresh({ spawn = function(_, fn) fn() end }, function() end)
               local _, outcome = mare.task.select(
                 function()
                   r.reconcile()
                   return "done"
                 end,
                 function()
                   mare.task.sleep(2800)
                   return "timeout"
                 end
               )
               mare.store.index_reconcile = orig_reconcile
               mare.store.index_notice = orig_notice
               return outcome == "done", calls"#,
        )
        .await;
        assert!(finished, "reconcile() must bound pending retry wait instead of looping forever (calls={calls})");
        assert!(calls >= 2, "reconcile() should still retry transient pending before bounding");
        h.shutdown().await;
    })
    .await;
}

fn session_dir_for(cwd: &std::path::Path) -> std::path::PathBuf {
    let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(&canonical))
}

fn write_persisted_session(dir: &std::path::Path, file_stem: &str, lines: &[Value]) -> std::path::PathBuf {
    let path = dir.join(format!("{file_stem}.jsonl"));
    write_lines(&path, lines);
    path
}

#[tokio::test]
async fn lua_store_query_honors_exec_ctx_deny_read_live_snapshot_and_omits_spill() {
    local(async {
        let env = TestEnv::new().await;
        let sdir = session_dir_for(env.work.path());
        let denied_path = write_persisted_session(
            &sdir,
            "2026-01-01T00-00-00Z_secret-denied",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"secret-denied","cwd":env.work.path(),"created_at_ms":1000}),
                json!({"type":"name","name":"Secret Denied Title"}),
                json!({"type":"message","entry_id":"sd1","created_at_ms":1010,"message":{"role":"user","content":"needle secret-token-denied"}}),
            ],
        );
        write_persisted_session(
            &sdir,
            "2026-01-02T00-00-00Z_allowed-parent",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"allowed-parent","cwd":env.work.path(),"created_at_ms":2000}),
                json!({"type":"name","name":"Allowed Parent Session"}),
                json!({"type":"message","entry_id":"ap1","created_at_ms":2010,"message":{"role":"user","content":"parent needle token"}}),
            ],
        );
        write_persisted_session(
            &sdir,
            "2026-01-03T00-00-00Z_allowed-child",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"allowed-child","cwd":env.work.path(),"created_at_ms":3000,"parent_session_id":"secret-denied"}),
                json!({"type":"name","name":"Child Compaction Session"}),
                json!({"type":"message","entry_id":"c1","created_at_ms":3010,"message":{"role":"user","content":"shadowed needle phrase"}}),
                json!({"type":"compaction","entry_id":"cmp1","parent_entry_id":"c1","created_at_ms":3020,"summary":"compacted summary","first_kept_entry_id":"cmp1"}),
                json!({"type":"message","entry_id":"c2","parent_entry_id":"cmp1","created_at_ms":3030,"message":{"role":"user","content":"current needle phrase"}}),
            ],
        );

        let script = json!([
            {
                "text": "in-flight-step-needle",
                "tool_calls": [{ "id": "call_q1", "name": "probe_history", "arguments": {} }]
            },
            {
                "text": "probe complete"
            }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);

        // No duplicate spill binding on mare.store.
        let has_spill: bool = eval(&h, "return mare.store.spill ~= nil").await;
        assert!(!has_spill, "mare.store.spill must not be exposed (Batch 3 P2)");

        // Outside a tool execution context, query primitives run under the host's frozen policy
        // (and still honor the session sandbox's deny_read once configured below).
        let outside_count: usize = eval(
            &h,
            r#"
            local res = assert(mare.store.search_sessions({ query = "needle" }))
            return #(res.data or {})
            "#,
        )
        .await;
        assert!(outside_count >= 2, "search_sessions outside tool context must query host sessions: {outside_count}");

        exec(
            &h,
            &format!(
                r#"
            mare.proc.sandbox.set_current({{
                level = "read-only",
                network = true,
                deny_read = {{ {} }},
                accept_partial = {{ "fs_deny_read" }},
            }})
            QUERY_REPORT = nil
            T:register("probe_history", {{
                side_effect = "read",
                description = "probe mare.store query APIs inside frozen exec context",
                parameters = {{ type = "object" }},
                execute = function()
                    local live = S:live_snapshot()
                    assert(type(live) == "table" and live.step_boundary ~= nil, "live_snapshot should expose step_boundary during turn")
                    local sessions = mare.store.search_sessions({{
                        query = "needle",
                        sort_key = "updated_at_ms",
                        sort_direction = "desc",
                    }}, {{ live_sessions = {{ live }} }})
                    local current_only = mare.store.search_events({{
                        session_id = "allowed-child",
                        query = "needle",
                        event_filters = {{ surfaces = {{ "current" }} }},
                    }})
                    local traced_session = mare.store.trace_session({{ session_id = "allowed-child" }})
                    local traced_event = mare.store.trace_event({{
                        session_id = "allowed-child",
                        seq = 2,
                    }})
                    local read_res = mare.store.read_event({{
                        event_id = current_only.data[1].event_id,
                        before = 1,
                        after = 1,
                    }})
                    local live_visible = mare.store.search_sessions({{
                        query = "in-flight-step-needle",
                        step_bound = false,
                    }}, {{ live_sessions = {{ live }} }})
                    local live_hidden = mare.store.search_sessions({{
                        query = "in-flight-step-needle",
                        step_bound = true,
                    }}, {{ live_sessions = {{ live }} }})
                    QUERY_REPORT = {{
                        sessions = sessions,
                        current_only = current_only,
                        traced_session = traced_session,
                        traced_event = traced_event,
                        read_res = read_res,
                        live_visible = #live_visible.data,
                        live_hidden = #live_hidden.data,
                    }}
                    return {{ content = "ok" }}
                end,
            }})
            "#,
                serde_json::to_string(&denied_path.to_string_lossy()).unwrap()
            ),
        )
        .await;

        let res = h.prompt("check-live-probe please", json!({})).await.unwrap();
        assert_eq!(res["text"], "probe complete", "{res}");

        let report_str: String = eval(&h, "return mare.json.encode(QUERY_REPORT)").await;
        let report: Value = serde_json::from_str(&report_str).unwrap();
        let report_json = report.to_string();
        assert!(!report_json.contains("secret-denied"), "deny_read leaked secret session id: {report_json}");
        assert!(!report_json.contains("Secret Denied Title"), "deny_read leaked title: {report_json}");
        assert!(!report_json.contains("secret-token-denied"), "deny_read leaked body: {report_json}");

        let current_hits = report["current_only"]["data"].as_array().unwrap();
        assert_eq!(current_hits.len(), 1, "surface=current filter failed: {report}");
        assert_eq!(current_hits[0]["surface"], "current");
        assert_eq!(current_hits[0]["position"]["seq"], 4);

        assert_eq!(report["traced_session"]["parent_boundary"], true, "{report}");
        assert_eq!(report["traced_session"]["complete"], false, "{report}");
        assert_eq!(report["traced_event"]["surface"], "shadowed", "{report}");
        assert_eq!(report["traced_event"]["replaced_by"], 3, "{report}");
        assert_eq!(report["read_res"]["neighbors"].as_array().unwrap().len(), 1, "{report}");
        assert_eq!(report["live_visible"], 1, "step_bound=false should include current turn prompt: {report}");
        assert_eq!(report["live_hidden"], 0, "step_bound=true should hide in-flight step prompt: {report}");

        h.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_session_query_methods_cover_search_cursor_chunk_read_trace_live_and_deny_read() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let canon_work = dunce::canonicalize(&work).unwrap();
    let sdir = home.path().join("state/sessions").join(mare_store::cwd_slug(&canon_work));

    // Denied session must never appear in results or lineage.
    let denied_path = write_persisted_session(
        &sdir,
        "2026-01-01T00-00-00Z_rpc-denied",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"rpc-denied","cwd":canon_work,"created_at_ms":1000}),
            json!({"type":"message","entry_id":"rd1","created_at_ms":1010,"message":{"role":"user","content":"rpc needle forbidden"}}),
        ],
    );

    // Root + child session with >20 matching events, compaction lineage, and >64 KiB event.
    write_persisted_session(
        &sdir,
        "2026-01-02T00-00-00Z_rpc-root",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"rpc-root","cwd":canon_work,"created_at_ms":2000}),
            json!({"type":"name","name":"Alpha Root Topic"}),
            json!({"type":"message","entry_id":"rr1","created_at_ms":2010,"message":{"role":"user","content":"root event needle"}}),
        ],
    );

    let mut child_lines = vec![
        json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"rpc-child","cwd":canon_work,"created_at_ms":3000,"parent_session_id":"rpc-root"}),
        json!({"type":"name","name":"Beta Child Topic"}),
        json!({"type":"message","entry_id":"rc0","created_at_ms":3005,"message":{"role":"user","content":"before compaction needle"}}),
        json!({"type":"compaction","entry_id":"rcmp","parent_entry_id":"rc0","created_at_ms":3008,"summary":"rolled up","first_kept_entry_id":"rcmp"}),
    ];
    for i in 1..=25 {
        child_lines.push(json!({
            "type": "message",
            "entry_id": format!("rm{i}"),
            "parent_entry_id": if i == 1 { "rcmp".to_string() } else { format!("rm{}", i - 1) },
            "created_at_ms": 3010 + i,
            "message": { "role": "user", "content": format!("paginated needle entry {i}") }
        }));
    }
    let large_payload = format!("oversize marker {}", "x".repeat(70_000));
    child_lines.push(json!({
        "type": "message",
        "entry_id": "rlarge",
        "parent_entry_id": "rm25",
        "created_at_ms": 3100,
        "message": { "role": "user", "content": large_payload }
    }));
    let child_path = write_persisted_session(&sdir, "2026-01-03T00-00-00Z_rpc-child", &child_lines);

    write_persisted_session(
        &sdir,
        "2026-01-04T00-00-00Z_rpc-archived",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"rpc-archived","cwd":canon_work,"created_at_ms":4000}),
            json!({"type":"name","name":"Archived Needle Topic"}),
            json!({"type":"message","entry_id":"ra1","created_at_ms":4010,"message":{"role":"user","content":"archived needle"}}),
            json!({"type":"archive","archived":true}),
        ],
    );

    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        format!("mare.setup {{ sandbox = {{ deny_read = {{ {:?} }} }} }}\n", denied_path.to_string_lossy()),
    )
    .unwrap();

    let faux_path = home.path().join("faux.json");
    std::fs::write(&faux_path, json!([{ "text": "live-rpc-reply-marker" }]).to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);

    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(init["result"]["protocol_version"].is_string(), "{init}");

    // 1. Title-only search + snake_case filters.
    let titled = c.call_raw(
        "session.search",
        json!({
            "title_query": "Beta Child",
            "filters": { "archived": false },
            "sort_key": "updated_at_ms",
            "sort_direction": "desc"
        }),
        |_| json!(null),
    );
    let titled_hits = titled["result"]["data"].as_array().unwrap_or_else(|| panic!("{titled}"));
    assert_eq!(titled_hits.len(), 1, "{titled}");
    assert_eq!(titled_hits[0]["session_id"], "rpc-child");
    assert!(!titled.to_string().contains("rpc-denied"), "deny_read leaked: {titled}");

    // 2. Archived filter parity + usable `item_id` (and no unusable `event_id`) on `session.search`.
    let archived =
        c.call_raw("session.search", json!({ "query": "needle", "filters": { "archived": true } }), |_| json!(null));
    let arch_hits = archived["result"]["data"].as_array().unwrap_or_else(|| panic!("{archived}"));
    assert_eq!(arch_hits.len(), 1, "{archived}");
    assert_eq!(arch_hits[0]["session_id"], "rpc-archived");
    assert!(arch_hits[0].get("event_id").is_none(), "session.search must not return unusable event_id: {archived}");
    let arch_item_id = arch_hits[0]["item_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("session.search hit must include usable item_id: {archived}"));
    let arch_item =
        c.call_raw("session.get_item", json!({ "session_id": "rpc-archived", "item_id": arch_item_id }), |_| {
            json!(null)
        });
    assert!(arch_item["error"].is_null(), "session.get_item must accept session.search item_id: {arch_item}");
    assert_eq!(arch_item["result"]["item"]["item_id"], arch_item_id, "{arch_item}");
    let arch_trace =
        c.call_raw("session.trace_item", json!({ "session_id": "rpc-archived", "item_id": arch_item_id }), |_| {
            json!(null)
        });
    assert!(arch_trace["error"].is_null(), "session.trace_item must accept session.search item_id: {arch_trace}");
    assert_eq!(arch_trace["result"]["item_id"], arch_item_id, "{arch_trace}");

    // 3. Invalid params (-32602) when session.search_items omits session_id.
    let missing_sid = c.call_raw("session.search_items", json!({ "query": "needle" }), |_| json!(null));
    assert_eq!(missing_sid["error"]["code"], -32602, "{missing_sid}");

    // 4. Paginated event search with stable cursor and surface filter.
    let page1 = c.call_raw(
        "session.search_items",
        json!({
            "session_id": "rpc-child",
            "query": "paginated needle",
            "event_filters": { "surfaces": ["current"] }
        }),
        |_| json!(null),
    );
    let p1_hits = page1["result"]["data"].as_array().unwrap_or_else(|| panic!("{page1}"));
    assert_eq!(p1_hits.len(), 20, "{page1}");
    assert!(page1["result"].get("hits").is_none() && page1["result"].get("cursor").is_none(), "{page1}");
    let cursor = page1["result"]["next_cursor"].as_str().unwrap_or_else(|| panic!("{page1}")).to_string();

    let page2 = c.call_raw(
        "session.search_items",
        json!({
            "session_id": "rpc-child",
            "query": "paginated needle",
            "event_filters": { "surfaces": ["current"] },
            "cursor": cursor
        }),
        |_| json!(null),
    );
    let p2_hits = page2["result"]["data"].as_array().unwrap_or_else(|| panic!("{page2}"));
    assert_eq!(p2_hits.len(), 5, "{page2}");
    assert!(page2["result"].get("next_cursor").is_none(), "{page2}");
    assert!(p1_hits[0]["position"]["text_offset"].is_u64(), "{page1}");

    // The query provider wraps the store's next_cursor without changing the page shape.
    let query_page1 = c.call_raw(
        "query.call",
        json!({ "operation": "session.events", "arguments": {
            "session_id": "rpc-child", "query": "paginated needle", "event_filters": { "surfaces": ["current"] }
        } }),
        |_| json!(null),
    );
    assert_eq!(query_page1["result"]["data"].as_array().unwrap().len(), 20, "{query_page1}");
    assert!(query_page1["result"].get("cursor").is_none(), "{query_page1}");
    let query_cursor = query_page1["result"]["next_cursor"].as_str().unwrap().to_owned();
    let query_page2 = c.call_raw(
        "query.call",
        json!({ "operation": "session.events", "arguments": {
            "session_id": "rpc-child", "query": "paginated needle",
            "event_filters": { "surfaces": ["current"] }, "cursor": query_cursor
        } }),
        |_| json!(null),
    );
    assert_eq!(query_page2["result"]["data"].as_array().unwrap().len(), 5, "{query_page2}");
    assert!(query_page2["result"].get("next_cursor").is_none(), "{query_page2}");

    // 5. Large event (>64 KiB) fails inline and succeeds chunked.
    let large_search =
        c.call_raw("session.search_items", json!({ "session_id": "rpc-child", "query": "oversize marker" }), |_| {
            json!(null)
        });
    let large_eid = large_search["result"]["data"][0]["item_id"].as_str().unwrap().to_string();
    let too_large =
        c.call_raw("session.get_item", json!({ "session_id": "rpc-child", "item_id": large_eid }), |_| json!(null));
    assert!(too_large["error"]["message"].as_str().unwrap_or("").contains("QUERY_EVENT_TOO_LARGE"), "{too_large}");

    let chunk1 = c.call_raw(
        "session.get_item",
        json!({ "session_id": "rpc-child", "item_id": large_eid, "offset": 0, "max_bytes": 4096, "before": 1 }),
        |_| json!(null),
    );
    assert!(chunk1["result"]["chunk"].is_string(), "{chunk1}");
    assert_eq!(chunk1["result"]["chunk_offset"], 0, "{chunk1}");
    assert!(chunk1["result"]["next_offset"].as_u64().is_some(), "{chunk1}");
    assert_eq!(chunk1["result"]["neighbors"].as_array().unwrap().len(), 1, "{chunk1}");

    // 6. Lineage & event trace RPCs.
    let st = c.call_raw("session.trace", json!({ "session_id": "rpc-root" }), |_| json!(null));
    assert_eq!(st["result"]["descendants"][0]["session"]["session_id"], "rpc-child", "{st}");
    assert_eq!(st["result"]["complete"], true, "{st}");

    let et = c.call_raw(
        "query.call",
        json!({ "operation": "session.event_trace", "arguments": { "session_id": "rpc-child", "seq": 2 } }),
        |_| json!(null),
    );
    assert_eq!(et["result"]["surface"], "shadowed", "{et}");
    assert_eq!(et["result"]["replaced_by"], 3, "{et}");

    // 7. Mutation invalidates old cursor (stale check).
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(&child_path).unwrap();
    writeln!(
        f,
        "{}",
        json!({"type":"message","entry_id":"appended","created_at_ms":3200,"message":{"role":"user","content":"extra"}})
    )
    .unwrap();
    let stale = c.call_raw(
        "session.search_items",
        json!({
            "session_id": "rpc-child",
            "query": "paginated needle",
            "event_filters": { "surfaces": ["current"] },
            "cursor": page1["result"]["next_cursor"]
        }),
        |_| json!(null),
    );
    assert!(stale["error"]["message"].as_str().unwrap_or("").contains("stale history cursor"), "{stale}");

    // 8. Live session prompt opens tool journal; RPC history query must still succeed and see live session!
    let live_sid = c.call_raw("status.get", json!({}), |_| json!(null))["result"]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let started =
        c.call_raw("agent.prompt", json!({ "session_id": live_sid, "input": "live-rpc-prompt-marker" }), |_| {
            json!(null)
        });
    assert!(started.get("error").is_none(), "{started}");
    let prompted =
        c.call_raw("agent.wait", json!({ "session_id": live_sid, "turn_id": started["result"]["turn_id"] }), |_| {
            json!(null)
        });
    assert!(prompted.get("error").is_none(), "{prompted}");

    let live_res = c.call_raw(
        "session.search",
        json!({
            "query": "live-rpc-reply-marker",
            "filters": { "availability": ["live"] }
        }),
        |_| json!(null),
    );
    let live_hits = live_res["result"]["data"].as_array().unwrap_or_else(|| panic!("{live_res}"));
    assert_eq!(live_hits.len(), 1, "live RPC session should be searchable while journal is active: {live_res}");
    assert_eq!(live_hits[0]["availability"], "live");

    c.shutdown_and_wait();
}

#[test]
fn query_service_and_agent_activity_contracts_work_across_tools_and_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    let plug = home.join("config/plugins/query-activity-probe");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&plug).unwrap();

    std::fs::write(
        plug.join("init.lua"),
        r#"
return {
	name = "query-activity-probe",
	inject = { "query", "agent", "tools", "commands" },
	apply = function(ctx)
		ctx.agent:register_activity("probe", function()
			return { ok = true, nested = { count = 2 } }
		end)
		local release
		release = ctx.query:register("probe-mut", {
			source = { capability = "in_process" },
			operations = {
				["probe.mutate"] = function()
					mare.exec.check_mutation("probe mutation")
					return {}
				end,
				["probe.unregister"] = function()
					release()
					return {}
				end,
				["probe.register"] = function()
					ctx.query:register("probe-late", { source = { capability = "in_process" }, operations = { ["probe.late"] = function() return {} end } })
					return {}
				end,
				["probe.activity"] = function()
					local act = ctx.agent:activity()
					return {
						version = act.version,
						busy = act.busy,
						session_id = act.session_id,
						extra = act.extra,
					}
				end,
			},
		})
		ctx.commands:register("probe-release", {
			run = function()
				release()
			end,
		})
		ctx.commands:register("probe-register", {
			run = function()
				ctx.query:register("probe-late", { source = { capability = "in_process" }, operations = { ["probe.late"] = function() return {} end } })
			end,
		})
	end,
}
"#,
    )
    .unwrap();

    let script = dir.path().join("faux.json");
    std::fs::write(&script, serde_json::to_string(&json!([{"text":"activity-ok"}])).unwrap()).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();

    let canon_work = dunce::canonicalize(&work).unwrap();
    let sdir = home.join("state/sessions").join(mare_store::cwd_slug(&canon_work));
    write_persisted_session(
        &sdir,
        "2026-01-01T00-00-00Z_q-one",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"q-one","cwd":canon_work,"created_at_ms":1000}),
            json!({"type":"name","name":"Query One"}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1010,"message":{"role":"user","content":"query_service_needle"}}),
        ],
    );

    let mut cmd = mare_cmd();
    cmd.args(["-C", work.to_str().unwrap(), "--yolo", "--rpc"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::inherit());
    let mut c = RpcClient::spawn_cmd(&mut cmd);

    c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));

    let providers = c.call_raw("query.list_providers", json!({}), |_| json!(null));
    let names: Vec<&str> =
        providers["result"]["data"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(names.contains(&"persisted-sessions"), "{providers}");
    assert!(names.contains(&"live-sessions"), "{providers}");

    let search = c.call_raw(
        "query.call",
        json!({ "operation": "session.search", "arguments": { "query": "query_service_needle" } }),
        |_| json!(null),
    );
    assert_eq!(search["result"]["provider"], "live-sessions", "{search}");
    assert_eq!(search["result"]["coverage"]["persisted"], true, "{search}");
    assert_eq!(search["result"]["data"][0]["session_id"], "q-one", "{search}");

    let step_bound_persisted = c.call_raw(
        "query.call",
        json!({
            "operation": "session.search",
            "arguments": { "query": "query_service_needle", "step_bound": true },
            "provider": "persisted-sessions"
        }),
        |_| json!(null),
    );
    assert!(
        step_bound_persisted["error"]["message"].as_str().unwrap_or("").contains("QUERY_CAPABILITY_UNAVAILABLE"),
        "{step_bound_persisted}"
    );

    let mut_err = c.call_raw("query.call", json!({ "operation": "probe.mutate", "arguments": {} }), |_| json!(null));
    assert!(mut_err["error"]["message"].as_str().unwrap_or("").contains("ephemeral-read-only"), "{mut_err}");

    // a read-only query cannot release a provider registration (the release runs in the caller's
    // execution context, not the registrant's), a command can
    let listed = |c: &mut RpcClient, name: &str| {
        let providers = c.call_raw("query.list_providers", json!({}), |_| json!(null));
        providers["result"]["data"].as_array().unwrap().iter().any(|p| p["name"] == name)
    };
    let revoke = c.call_raw("query.call", json!({ "operation": "probe.unregister", "arguments": {} }), |_| json!(null));
    assert!(revoke["error"]["message"].as_str().unwrap_or("").contains("ephemeral-read-only"), "{revoke}");
    assert!(listed(&mut c, "probe-mut"), "a refused release keeps the provider");
    // nor register one
    let late = c.call_raw("query.call", json!({ "operation": "probe.register", "arguments": {} }), |_| json!(null));
    assert!(late["error"]["message"].as_str().unwrap_or("").contains("ephemeral-read-only"), "{late}");
    assert!(!listed(&mut c, "probe-late"), "a refused registration adds no provider");

    let act = c.call_raw("query.call", json!({ "operation": "probe.activity", "arguments": {} }), |_| json!(null));
    assert_eq!(act["result"]["version"], 1, "{act}");
    assert_eq!(act["result"]["busy"], false, "{act}");
    assert!(act["result"]["session_id"].as_str().is_some_and(|s| !s.is_empty()), "{act}");
    assert_eq!(act["result"]["extra"]["probe"]["ok"], true, "{act}");

    c.call_raw("commands.run", json!({ "line": "/probe-release" }), |_| json!(null));
    assert!(!listed(&mut c, "probe-mut"), "a command releases the provider");
    c.call_raw("commands.run", json!({ "line": "/probe-register" }), |_| json!(null));
    assert!(listed(&mut c, "probe-late"), "a command registers a provider");

    c.shutdown_and_wait();
}

#[test]
fn query_contract_validates_params_wraps_providers_and_preserves_error_codes() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join("config/plugins/query-cursor-probe")).unwrap();
    std::fs::write(
        home.join("config/plugins/query-cursor-probe/init.lua"),
        r#"
return {
	name = "query-cursor-probe",
	inject = { "query" },
	apply = function(ctx)
		ctx.query:register("cursor-probe", {
			source = { capability = "in_process" },
			operations = {
				["probe.page"] = function(req)
					if req.cursor == "p2" then
						return { data = { "two" } }
					end
					return { data = { "one" }, next_cursor = "p2" }
				end,
				["probe.retired_page"] = function()
					return { data = { "old" }, cursor = "legacy" }
				end,
			},
		})
	end,
}
"#,
    )
    .unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();
    let script = dir.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-C", work.to_str().unwrap(), "--yolo", "--rpc"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    let providers = c.call_raw("query.list_providers", json!({}), |_| json!(null));
    assert!(providers["result"]["data"].is_array(), "query.list_providers must return {{data}}: {providers}");

    let bad_args =
        c.call_raw("query.call", json!({ "operation": "session.search", "arguments": {}, "extra": true }), |_| {
            json!(null)
        });
    assert_eq!(bad_args["error"]["data"]["code"], json!("invalid_arguments"), "{bad_args}");

    let missing_op = c.call_raw("query.call", json!({ "operation": "no.such.op", "arguments": {} }), |_| json!(null));
    assert_eq!(missing_op["error"]["data"]["code"], json!("unavailable"), "{missing_op}");

    let stale_cursor = c.call_raw(
        "query.call",
        json!({ "operation": "session.search", "arguments": { "query": "x", "cursor": "not-json" } }),
        |_| json!(null),
    );
    assert_eq!(stale_cursor["error"]["data"]["code"], json!("query.stale_cursor"), "{stale_cursor}");

    let p1 = c.call_raw("query.call", json!({ "operation": "probe.page", "arguments": {} }), |_| json!(null));
    assert!(p1["result"].get("cursor").is_none(), "query.call must return next_cursor, not cursor: {p1}");
    let next_cursor = p1["result"]["next_cursor"].as_str().unwrap_or_else(|| panic!("{p1}")).to_string();
    let p2 =
        c.call_raw("query.call", json!({ "operation": "probe.page", "arguments": { "cursor": next_cursor } }), |_| {
            json!(null)
        });
    assert_eq!(p2["result"]["data"], json!(["two"]), "{p2}");
    assert!(p2["result"].get("next_cursor").is_none(), "{p2}");

    let retired =
        c.call_raw("query.call", json!({ "operation": "probe.retired_page", "arguments": {} }), |_| json!(null));
    assert_eq!(retired["error"]["data"]["code"], "invalid_result", "{retired}");

    c.shutdown_and_wait();
}

#[tokio::test]
async fn lua_query_call_accepts_only_cancel_callbacks_including_nil() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["query"]).build().await;
        // The service guard checks the same Lua callback contract as other callers.
        global(&h, "Q", h.service_table("query").unwrap());
        exec(
            &h,
            r#"
            Q:register("callback-form-probe", {
                source = { capability = "in_process" },
                operations = { ["probe.callback_form"] = function(_, opts)
                    assert(opts.cancel and not opts.cancel:is_cancelled())
                    return {}
                end },
            })
            local token = mare.task.token()
            local calls = 0
            Q:call("probe.callback_form", {}, { cancel = function()
                calls = calls + 1
                return token
            end })
            assert(calls == 1 and not token:is_cancelled(), "callback token must remain owned by caller")
            local ok, err = pcall(function()
                return Q:call("probe.callback_form", {}, { cancel = function() return nil end })
            end)
            assert(ok, "a cancellation callback may return nil: " .. tostring(err))
            local accepted_token = pcall(function()
                return Q:call("probe.callback_form", {}, { cancel = token })
            end)
            assert(not accepted_token, "a direct cancellation token must not be accepted")
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn direct_lua_query_uses_ephemeral_read_only_context_at_mare_cwd() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["query"]).build().await;
        global(&h, "Q", h.service_table("query").unwrap());
        exec(
            &h,
            r#"
            Q:register("sandbox-context-probe", {
                source = { capability = "in_process" },
                operations = { ["probe.context"] = function()
                    local current = assert(mare.exec.current(), "query needs an execution context")
                    local mutation_allowed = pcall(mare.exec.check_mutation, "query probe")
                    return { cwd = current.cwd, lifetime = current.lifetime, mutation_allowed = mutation_allowed }
                end },
            })
            assert(mare.exec.current() == nil, "probe must start outside an execution context")
            local result = Q:call("probe.context", {})
            assert(result.cwd == mare.cwd, "query must use mare.cwd without an outer context")
            assert(result.lifetime == "ephemeral-read-only", "query must use an ephemeral read-only lifetime")
            assert(not result.mutation_allowed, "query must refuse mutations")
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await;
}

fn spawn_query_probe_rpc(dir: &std::path::Path, plugin_source: &str) -> RpcClient {
    let home = dir.join("home");
    let work = dir.join("work");
    let plug = home.join("config/plugins/query-rpc-probe");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&plug).unwrap();
    std::fs::write(plug.join("init.lua"), plugin_source).unwrap();
    let script = dir.join("faux.json");
    std::fs::write(&script, "[]").unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-C", work.to_str().unwrap(), "--yolo", "--rpc"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::inherit());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    c
}

#[test]
fn remote_query_call_cancel_cancels_provider_token_and_clears_active() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn_query_probe_rpc(
        dir.path(),
        r#"
return {
	name = "query-rpc-probe",
	inject = { "query", "commands" },
	apply = function(ctx)
		local weak = setmetatable({}, { __mode = "v" })
		local captured_token
		local started_tx, started_rx = mare.task.channel()
		ctx.query:register("probe-cancel", {
			source = { capability = "in_process" },
			operations = {
				["probe.wait_cancel"] = function(_, opts)
					captured_token = opts.cancel
					weak.token = opts.cancel
					started_tx:send(true)
					opts.cancel:wait()
					return {}
				end,
				["probe.inspect_cancel"] = function()
					local was_cancelled = captured_token ~= nil and captured_token:is_cancelled()
					captured_token = nil
					collectgarbage("collect")
					collectgarbage("collect")
					return {
						was_cancelled = was_cancelled,
						active_cleared = weak.token == nil,
					}
				end,
			},
		})
		ctx.commands:register("probe-await-cancel-start", {
			run = function()
				assert(started_rx:recv(), "probe.wait_cancel never started")
			end,
		})
	end,
}
"#,
    );

    let cancel_req = c.request("query.call", json!({ "operation": "probe.wait_cancel", "arguments": {} }));
    let start_cmd = c.call_raw("commands.run", json!({ "line": "/probe-await-cancel-start" }), |_| json!(null));
    assert!(start_cmd.get("error").is_none(), "{start_cmd}");
    c.notify("$/cancel_request", json!({ "id": cancel_req }));
    let cancelled_reply = c.wait_raw(cancel_req, |_| json!(null));
    assert_eq!(cancelled_reply["error"]["data"]["code"], json!("cancelled"), "{cancelled_reply}");

    let inspect =
        c.call_raw("query.call", json!({ "operation": "probe.inspect_cancel", "arguments": {} }), |_| json!(null));
    assert_eq!(inspect["result"]["was_cancelled"], json!(true), "{inspect}");
    assert_eq!(inspect["result"]["active_cleared"], json!(true), "{inspect}");

    c.shutdown_and_wait();
}

#[test]
fn remote_query_call_returns_provider_changed_when_unregistered_mid_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn_query_probe_rpc(
        dir.path(),
        r#"
return {
	name = "query-rpc-probe",
	inject = { "query", "commands" },
	apply = function(ctx)
		local started_tx, started_rx = mare.task.channel()
		local unblock_tx, unblock_rx = mare.task.channel()
		local saw_drop_cancel = false
		local release_replace
		release_replace = ctx.query:register("probe-replace", {
			source = { capability = "in_process" },
			operations = {
				["probe.block_replace"] = function(_, opts)
					started_tx:send(true)
					assert(unblock_rx:recv(), "unblock signal lost")
					saw_drop_cancel = opts.cancel:is_cancelled()
					return { ok = true }
				end,
			},
		})
		ctx.query:register("probe-inspector", {
			source = { capability = "in_process" },
			operations = {
				["probe.inspect_drop"] = function()
					return { saw_drop_cancel = saw_drop_cancel }
				end,
			},
		})
		ctx.commands:register("probe-await-block-start", {
			run = function()
				assert(started_rx:recv(), "probe.block_replace never started")
			end,
		})
		ctx.commands:register("probe-drop-and-unblock", {
			run = function()
				release_replace()
				unblock_tx:send(true)
			end,
		})
	end,
}
"#,
    );

    let replace_req = c.request("query.call", json!({ "operation": "probe.block_replace", "arguments": {} }));
    let block_started = c.call_raw("commands.run", json!({ "line": "/probe-await-block-start" }), |_| json!(null));
    assert!(block_started.get("error").is_none(), "{block_started}");
    let dropped = c.call_raw("commands.run", json!({ "line": "/probe-drop-and-unblock" }), |_| json!(null));
    assert!(dropped.get("error").is_none(), "{dropped}");
    let changed_reply = c.wait_raw(replace_req, |_| json!(null));
    assert_eq!(changed_reply["error"]["data"]["code"], json!("query.provider_changed"), "{changed_reply}");

    let inspect =
        c.call_raw("query.call", json!({ "operation": "probe.inspect_drop", "arguments": {} }), |_| json!(null));
    assert_eq!(inspect["result"]["saw_drop_cancel"], json!(true), "{inspect}");

    c.shutdown_and_wait();
}

// Reproduces mare-plugins tests/session-search-rpc.py:152-161: filesystem writes and network
// requests rejected inside `query.call`'s ephemeral-read-only sandbox must surface as Coded
// `denied` instead of being redacted to RPC `-32603` (`internal`).
#[test]
fn query_call_sandbox_denials_surface_as_coded_denied_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn_query_probe_rpc(
        dir.path(),
        r#"
return {
	name = "query-rpc-probe",
	inject = { "query" },
	apply = function(ctx)
		ctx.query:register("probe-sandbox", {
			source = { capability = "in_process" },
			operations = {
				["probe.write"] = function(req)
					mare.fs.write(req.path, "blocked")
					return {}
				end,
				["probe.network"] = function(req)
					mare.http.request({ url = req.url, method = "GET" })
					return {}
				end,
			},
		})
	end,
}
"#,
    );

    let forbidden = dir.path().join("work/query-must-not-write.txt");
    for (operation, arguments) in [
        ("probe.write", json!({ "path": forbidden })),
        ("probe.network", json!({ "url": "http://127.0.0.1:9/no-network" })),
    ] {
        let resp = c.call_raw("query.call", json!({ "operation": operation, "arguments": arguments }), |_| json!(null));
        assert_eq!(resp["error"]["data"]["code"], json!("denied"), "{operation}: {resp}");
        let msg = resp["error"]["message"].as_str().unwrap_or("");
        assert!(msg.contains("denied") && msg.contains("ephemeral-read-only"), "{operation}: {resp}");
    }
    assert!(!forbidden.exists(), "forbidden write must not create file");

    c.shutdown_and_wait();
}

#[test]
fn session_history_contract_renames_drops_legacy_methods_and_supports_snake_case_rpc() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let canon_work = dunce::canonicalize(&work).unwrap();
    let sdir = home.path().join("state/sessions").join(mare_store::cwd_slug(&canon_work));

    let denied_path = write_persisted_session(
        &sdir,
        "2026-01-01T00-00-00Z_b18b-denied",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"b18b-denied","cwd":canon_work,"created_at_ms":1000}),
            json!({"type":"message","entry_id":"d1","created_at_ms":1010,"message":{"role":"user","content":"secret b18b needle"}}),
        ],
    );
    write_persisted_session(
        &sdir,
        "2026-01-02T00-00-00Z_b18b-root",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"b18b-root","cwd":canon_work,"created_at_ms":2000}),
            json!({"type":"name","name":"B18b Root"}),
            json!({"type":"message","entry_id":"r1","created_at_ms":2010,"message":{"role":"user","content":"root b18b needle"}}),
        ],
    );
    let large_payload = format!("b18b oversize {}", "z".repeat(70_000));
    write_persisted_session(
        &sdir,
        "2026-01-03T00-00-00Z_b18b-child",
        &[
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"b18b-child","cwd":canon_work,"created_at_ms":3000,"parent_session_id":"b18b-root"}),
            json!({"type":"name","name":"B18b Child"}),
            json!({"type":"message","entry_id":"c0","created_at_ms":3005,"message":{"role":"user","content":"compacted b18b needle"}}),
            json!({"type":"compaction","entry_id":"ccmp","parent_entry_id":"c0","created_at_ms":3008,"summary":"compacted","first_kept_entry_id":"ccmp"}),
            json!({"type":"message","entry_id":"c1","parent_entry_id":"ccmp","created_at_ms":3010,"message":{"role":"user","content":"child b18b marker"}}),
            json!({"type":"message","entry_id":"clarge","parent_entry_id":"c1","created_at_ms":3020,"message":{"role":"user","content":large_payload}}),
        ],
    );

    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        format!("mare.setup {{ sandbox = {{ deny_read = {{ {:?} }} }} }}\n", denied_path.to_string_lossy()),
    )
    .unwrap();
    let faux_path = home.path().join("faux.json");
    std::fs::write(&faux_path, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);

    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let advertised: Vec<&str> =
        init["result"]["capabilities"]["methods"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for m in ["session.search", "session.search_items", "session.get_item", "session.trace", "session.trace_item"] {
        assert!(advertised.contains(&m), "missing contract method {m} in {advertised:?}");
    }
    for legacy in [
        "session.search_sessions",
        "session.search_events",
        "session.read_event",
        "session.trace_session",
        "session.trace_event",
    ] {
        assert!(!advertised.contains(&legacy), "legacy {legacy} still advertised");
        let missing = c.call_raw(legacy, json!({}), |_| json!(null));
        assert_eq!(missing["error"]["code"], -32601, "legacy {legacy} must be method_not_found: {missing}");
    }

    let searched = c.call_raw(
        "session.search",
        json!({ "title_query": "B18b Child", "sort_key": "updated_at_ms", "sort_direction": "desc" }),
        |_| json!(null),
    );
    let hits = searched["result"]["data"].as_array().unwrap_or_else(|| panic!("{searched}"));
    assert_eq!(hits.len(), 1, "{searched}");
    assert_eq!(hits[0]["session_id"], "b18b-child", "{searched}");
    assert!(!searched.to_string().contains("b18b-denied"), "deny_read leaked: {searched}");

    let missing_sid = c.call_raw("session.search_items", json!({ "query": "marker" }), |_| json!(null));
    assert_eq!(missing_sid["error"]["code"], -32602, "{missing_sid}");

    let items = c.call_raw(
        "session.search_items",
        json!({ "session_id": "b18b-child", "query": "child b18b marker", "event_filters": { "surfaces": ["current"] } }),
        |_| json!(null),
    );
    let item_hits = items["result"]["data"].as_array().unwrap_or_else(|| panic!("{items}"));
    assert_eq!(item_hits.len(), 1, "{items}");
    let item_id = item_hits[0]["item_id"].as_str().unwrap().to_string();
    assert!(item_hits[0].get("event_id").is_none(), "{items}");
    let raw_event = c.call_raw(
        "query.call",
        json!({ "operation": "session.events", "arguments": {
            "session_id": "b18b-child", "query": "child b18b marker", "event_filters": { "surfaces": ["current"] }
        } }),
        |_| json!(null),
    );
    let event_id = raw_event["result"]["data"][0]["event_id"].as_str().unwrap().to_owned();
    for (sid, invalid_id) in [("b18b-root", item_id.as_str()), ("b18b-child", event_id.as_str())] {
        let invalid =
            c.call_raw("session.get_item", json!({ "session_id": sid, "item_id": invalid_id }), |_| json!(null));
        assert_eq!(invalid["error"]["data"]["code"], "session.unknown_item", "{invalid}");
    }

    let got =
        c.call_raw("session.get_item", json!({ "session_id": "b18b-child", "item_id": item_id, "before": 1 }), |_| {
            json!(null)
        });
    assert_eq!(got["result"]["item"]["item_id"], item_id, "{got}");
    let item_trace =
        c.call_raw("session.trace_item", json!({ "session_id": "b18b-child", "item_id": item_id }), |_| json!(null));
    assert_eq!(item_trace["result"]["item_id"], item_id, "{item_trace}");
    assert!(item_trace["result"].get("event_id").is_none(), "{item_trace}");
    assert_eq!(got["result"]["session_id"], "b18b-child", "{got}");
    assert_eq!(got["result"]["neighbors"].as_array().unwrap().len(), 1, "{got}");

    let via_direct =
        c.call_raw("session.search_items", json!({ "session_id": "b18b-child", "query": "b18b oversize" }), |_| {
            json!(null)
        });
    let large_id = via_direct["result"]["data"][0]["item_id"].as_str().unwrap().to_string();
    let chunked = c.call_raw(
        "session.get_item",
        json!({ "session_id": "b18b-child", "item_id": large_id, "offset": 0, "max_bytes": 1024 }),
        |_| json!(null),
    );
    assert!(chunked["result"]["chunk"].is_string(), "{chunked}");
    assert!(chunked["result"]["next_offset"].is_u64(), "{chunked}");

    let st = c.call_raw("session.trace", json!({ "session_id": "b18b-root" }), |_| json!(null));
    assert_eq!(st["result"]["descendants"][0]["session"]["session_id"], "b18b-child", "{st}");

    let et = c.call_raw(
        "query.call",
        json!({ "operation": "session.event_trace", "arguments": { "session_id": "b18b-child", "seq": 2 } }),
        |_| json!(null),
    );
    assert_eq!(et["result"]["surface"], "shadowed", "{et}");
    assert_eq!(et["result"]["replaced_by"], 3, "{et}");

    // Contract error coding over RPC (never masked as -32603 `internal`):
    let missing_items =
        c.call_raw("session.search_items", json!({ "session_id": "b18b-missing", "query": "b18b" }), |_| json!(null));
    assert_ne!(missing_items["error"]["code"], -32603, "{missing_items}");
    assert_eq!(missing_items["error"]["data"]["code"], "unavailable", "{missing_items}");

    let empty_q = c.call_raw("session.search", json!({ "query": "   " }), |_| json!(null));
    assert_ne!(empty_q["error"]["code"], -32603, "{empty_q}");
    assert_eq!(empty_q["error"]["data"]["code"], "invalid_arguments", "{empty_q}");

    let bad_cursor = c.call_raw("session.search", json!({ "query": "b18b", "cursor": "not-base64!" }), |_| json!(null));
    assert_ne!(bad_cursor["error"]["code"], -32603, "{bad_cursor}");
    assert_eq!(bad_cursor["error"]["data"]["code"], "invalid_arguments", "{bad_cursor}");

    let bad_item =
        c.call_raw("session.get_item", json!({ "session_id": "b18b-child", "item_id": "bad-token" }), |_| json!(null));
    assert_ne!(bad_item["error"]["code"], -32603, "{bad_item}");
    assert_eq!(bad_item["error"]["data"]["code"], "session.unknown_item", "{bad_item}");

    let missing_trace = c.call_raw("session.trace", json!({ "session_id": "b18b-missing" }), |_| json!(null));
    assert_ne!(missing_trace["error"]["code"], -32603, "{missing_trace}");
    assert_eq!(missing_trace["error"]["data"]["code"], "unavailable", "{missing_trace}");

    let bad_seq = c.call_raw(
        "query.call",
        json!({ "operation": "session.event_trace", "arguments": { "session_id": "b18b-child", "seq": 0 } }),
        |_| json!(null),
    );
    assert_ne!(bad_seq["error"]["code"], -32603, "{bad_seq}");
    assert_eq!(bad_seq["error"]["data"]["code"], "invalid_arguments", "{bad_seq}");

    let oob_seq = c.call_raw(
        "query.call",
        json!({ "operation": "session.event_trace", "arguments": { "session_id": "b18b-child", "seq": 9999 } }),
        |_| json!(null),
    );
    assert_ne!(oob_seq["error"]["code"], -32603, "{oob_seq}");
    assert_eq!(oob_seq["error"]["data"]["code"], "unavailable", "{oob_seq}");

    // A journal mutation invalidates raw event tokens, but stable item IDs still identify the item.
    let child_path = sdir.join("2026-01-03T00-00-00Z_b18b-child.jsonl");
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&child_path).unwrap();
        writeln!(
            f,
            "{}",
            json!({"type":"message","entry_id":"c4","parent_entry_id":"c3","created_at_ms":3040,"message":{"role":"user","content":"mutated tail"}})
        )
        .unwrap();
    }
    let stable_item =
        c.call_raw("session.get_item", json!({ "session_id": "b18b-child", "item_id": item_id }), |_| json!(null));
    assert_eq!(stable_item["result"]["item"]["item_id"], item_id, "{stable_item}");

    let stale_trace = c.call_raw(
        "query.call",
        json!({ "operation": "session.event_trace", "arguments": { "event_id": event_id } }),
        |_| json!(null),
    );
    assert_ne!(stale_trace["error"]["code"], -32603, "{stale_trace}");
    assert_eq!(stale_trace["error"]["data"]["code"], "stale_session", "{stale_trace}");

    c.shutdown_and_wait();
}

#[tokio::test]
async fn session_history_lua_proxy_works_on_host_and_confines_tools() {
    local(async {
        let env = TestEnv::new().await;
        let canon_cwd = dunce::canonicalize(&env.cwd).unwrap();
        let sdir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&canon_cwd));
        let denied_path = write_persisted_session(
            &sdir,
            "2026-01-01T00-00-00Z_lua-denied",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"lua-denied","cwd":canon_cwd,"created_at_ms":1000}),
                json!({"type":"message","entry_id":"ld1","created_at_ms":1010,"message":{"role":"user","content":"hidden lua needle"}}),
            ],
        );
        let host_only_path = write_persisted_session(
            &sdir,
            "2026-01-01T12-00-00Z_lua-host-only",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"lua-host-only","cwd":canon_cwd,"created_at_ms":1500}),
                json!({"type":"message","entry_id":"lho1","created_at_ms":1510,"message":{"role":"user","content":"host only lua needle"}}),
            ],
        );
        write_persisted_session(
            &sdir,
            "2026-01-02T00-00-00Z_lua-allowed",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"lua-allowed","cwd":canon_cwd,"created_at_ms":2000}),
                json!({"type":"name","name":"Allowed Session"}),
                json!({"type":"message","entry_id":"la1","created_at_ms":2010,"message":{"role":"user","content":"visible lua needle"}}),
            ],
        );

        let plug = env.config_dir().join("plugins/b18b-lua-probe.lua");
        std::fs::create_dir_all(plug.parent().unwrap()).unwrap();
        std::fs::write(
            plug,
            format!(
                r#"return {{
                name = "b18b-lua-probe",
                inject = {{ "session", "tools", "policy" }},
                apply = function(ctx)
                    local tool_denied = {tool_denied}
                    ctx.policy:voter("tool/before", function(call)
                        if call.name == "b18b_probe_tool" then
                            return {{
                                decision = "allow",
                                sandbox = {{
                                    level = "read-only",
                                    network = true,
                                    deny_read = {{ tool_denied }},
                                    accept_partial = {{ "fs_deny_read" }},
                                }},
                            }}
                        end
                    end)
                    mare._b18b_host_check = function()
                        local s = ctx.session:search({{ query = "lua needle" }})
                        local items = ctx.session:search_items({{ session_id = "lua-allowed", query = "visible lua" }})
                        local item = ctx.session:get_item({{ session_id = "lua-allowed", item_id = items.data[1].item_id }})
                        local ts = ctx.session:trace("lua-allowed")
                        local ti = ctx.session:trace_item({{ session_id = "lua-allowed", item_id = items.data[1].item_id }})
                        return {{
                            search_count = #s.data,
                            search_sids = (function()
                                local ids = {{}}
                                for _, h in ipairs(s.data) do ids[#ids + 1] = h.session_id end
                                return table.concat(ids, ",")
                            end)(),
                            item_sid = item.session_id,
                            trace_sid = ts.target.session_id,
                            ti_sid = ti.session_id,
                        }}
                    end
                    ctx.tools:register("b18b_probe_tool", {{
                        side_effect = "read",
                        description = "probe session history under tool sandbox",
                        parameters = {{ type = "object" }},
                        execute = function()
                            local s = ctx.session:search({{ query = "lua needle" }})
                            mare._b18b_tool_report = {{
                                count = #s.data,
                                sid = s.data[1] and s.data[1].session_id or "",
                            }}
                            return {{ content = "ok" }}
                        end,
                    }})
                end,
            }}"#,
                tool_denied = serde_json::to_string(&host_only_path.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();

        let h = env
            .host()
            .user_plugins()
            .faux(json!([
                { "tool_calls": [{ "id": "call_b1", "name": "b18b_probe_tool", "arguments": {} }] },
                { "text": "probe done" }
            ]))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            &format!(
                r#"mare.proc.sandbox.set_current({{
                    level = "read-only",
                    network = true,
                    deny_read = {{ {} }},
                    accept_partial = {{ "fs_deny_read" }},
                }})"#,
                serde_json::to_string(&denied_path.to_string_lossy()).unwrap()
            ),
        )
        .await;

        let (sc, ssids, isid, tsid, tisid): (usize, String, String, String, String) = eval(
            &h,
            "local r = mare._b18b_host_check(); return r.search_count, r.search_sids, r.item_sid, r.trace_sid, r.ti_sid",
        )
        .await;
        assert_eq!((sc, isid.as_str(), tsid.as_str(), tisid.as_str()), (2, "lua-allowed", "lua-allowed", "lua-allowed"));
        assert!(ssids.contains("lua-allowed") && ssids.contains("lua-host-only"), "{ssids}");
        assert!(!ssids.contains("lua-denied"), "{ssids}");

        let prompt_res = h.prompt("run probe tool", json!({})).await.unwrap();
        assert_eq!(prompt_res["text"], "probe done", "{prompt_res}");
        let tr = tool_results(&h).await;
        let (has_report, tool_count, tool_sid): (bool, usize, String) = eval(
            &h,
            "local r = mare._b18b_tool_report; if not r then return false, 0, '' end; return true, r.count, r.sid",
        )
        .await;
        assert!(has_report, "probe tool did not populate _b18b_tool_report; tool_results={tr}");
        assert_eq!((tool_count, tool_sid.as_str()), (1, "lua-allowed"));

        h.shutdown().await;
    })
    .await;
}

#[test]
fn multihost_session_list_loaded_pagination_and_stale_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let canon_work = dunce::canonicalize(&work).unwrap();
    let sdir = home.join("state/sessions").join(mare_store::cwd_slug(&canon_work));
    for (n, sid) in [(1, "unloaded-one"), (2, "unloaded-two")] {
        write_persisted_session(
            &sdir,
            &format!("2026-01-0{n}T00-00-00Z_{sid}"),
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":sid,"cwd":canon_work,"created_at_ms":1000 + n}),
                json!({"type":"message","entry_id":format!("m{n}"),"created_at_ms":1010 + n,"message":{"role":"user","content":"unloaded"}}),
            ],
        );
    }
    let faux = dir.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "r1" }, { "text": "r2" }, { "text": "r3" }]).to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["-C", work.to_str().unwrap(), "--yolo", "--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &faux)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| json!(null));
    let s0 = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let p0 = rpc.call_raw("agent.prompt", json!({ "session_id": s0, "input": "one" }), |_| json!(null));
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s0, "turn_id": p0["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );

    let mut loaded_ids = std::collections::BTreeSet::from([s0.clone()]);
    for label in ["two", "three"] {
        let created = rpc.call_raw("session.create", json!({}), |_| json!(null));
        let sid = created["result"]["session_id"].as_str().unwrap().to_string();
        let p = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": label }), |_| json!(null));
        assert!(
            rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": p["result"]["turn_id"] }), |_| json!(
                null
            ))
            .get("error")
            .is_none()
        );
        loaded_ids.insert(sid);
    }
    assert_eq!(loaded_ids.len(), 3);

    // Loaded=true across 3 multi-host sessions with limit=2 => 2 pages (2 + 1).
    let lp1 = rpc.call_raw("session.list", json!({ "loaded": true, "limit": 2 }), |_| json!(null));
    let lp1_rows = lp1["result"]["data"].as_array().unwrap_or_else(|| panic!("{lp1}"));
    assert_eq!(lp1_rows.len(), 2, "expected 2 loaded sessions on page 1: {lp1}");
    let lp1_cursor =
        lp1["result"]["next_cursor"].as_str().unwrap_or_else(|| panic!("expected next_cursor: {lp1}")).to_string();
    let lp2 =
        rpc.call_raw("session.list", json!({ "loaded": true, "limit": 2, "cursor": lp1_cursor }), |_| json!(null));
    let lp2_rows = lp2["result"]["data"].as_array().unwrap_or_else(|| panic!("{lp2}"));
    assert_eq!(lp2_rows.len(), 1, "expected 1 loaded session on page 2: {lp2}");
    assert!(lp2["result"].get("next_cursor").is_none(), "final page must omit next_cursor: {lp2}");
    let got_loaded: std::collections::BTreeSet<String> = lp1_rows
        .iter()
        .chain(lp2_rows.iter())
        .map(|r| {
            assert_eq!(r["loaded"], true, "{r}");
            r["session_id"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(got_loaded, loaded_ids, "loaded=true pagination missed or duplicated sessions");

    // Loaded=false across 2 unloaded sessions with limit=1 => 2 pages (1 + 1).
    let up1 = rpc.call_raw("session.list", json!({ "loaded": false, "limit": 1 }), |_| json!(null));
    let up1_rows = up1["result"]["data"].as_array().unwrap_or_else(|| panic!("{up1}"));
    assert_eq!(up1_rows.len(), 1, "{up1}");
    let up1_cursor = up1["result"]["next_cursor"].as_str().unwrap_or_else(|| panic!("{up1}")).to_string();
    let up2 =
        rpc.call_raw("session.list", json!({ "loaded": false, "limit": 1, "cursor": up1_cursor }), |_| json!(null));
    let up2_rows = up2["result"]["data"].as_array().unwrap_or_else(|| panic!("{up2}"));
    assert_eq!(up2_rows.len(), 1, "{up2}");
    assert!(up2["result"].get("next_cursor").is_none(), "{up2}");
    let got_unloaded: std::collections::BTreeSet<String> = up1_rows
        .iter()
        .chain(up2_rows.iter())
        .map(|r| {
            assert_eq!(r["loaded"], false, "{r}");
            r["session_id"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        got_unloaded,
        std::collections::BTreeSet::from(["unloaded-one".to_string(), "unloaded-two".to_string()])
    );

    // Closing a loaded session invalidates both loaded and unloaded cursors via multi-host snapshot.
    let closed = rpc.call_raw("session.close", json!({ "session_id": s0 }), |_| json!(null));
    assert!(closed.get("error").is_none(), "{closed}");
    let stale_loaded =
        rpc.call_raw("session.list", json!({ "loaded": true, "limit": 2, "cursor": lp1_cursor }), |_| json!(null));
    assert_eq!(stale_loaded["error"]["data"]["code"], "stale_session", "{stale_loaded}");
    let stale_unloaded =
        rpc.call_raw("session.list", json!({ "loaded": false, "limit": 1, "cursor": up1_cursor }), |_| json!(null));
    assert_eq!(stale_unloaded["error"]["data"]["code"], "stale_session", "{stale_unloaded}");

    rpc.shutdown_and_wait();
}

#[cfg(unix)]
#[test]
fn rpc_pagination_cursors_bind_caller_owner_under_identical_policy() {
    use futures_util::{SinkExt, StreamExt};
    use std::collections::BTreeMap;
    use tokio_tungstenite::tungstenite::Message;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"mare.setup {
            peer = { allow = { "session.list", "session.list_items", "session.list_records" } },
            webserver = { enabled = true, host = "127.0.0.1", port = 0, token = "test-owner-token-0123456789abcdef" }
        }"#,
    )
    .unwrap();
    let canon_work = dunce::canonicalize(&work).unwrap();
    let sdir = home.join("state/sessions").join(mare_store::cwd_slug(&canon_work));
    for (n, sid) in [(1, "owner-s1"), (2, "owner-s2"), (3, "owner-s3")] {
        write_persisted_session(
            &sdir,
            &format!("2026-01-0{n}T00-00-00Z_{sid}"),
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":sid,"cwd":canon_work,"created_at_ms":1000 + n}),
                json!({"type":"message","entry_id":format!("m{n}"),"created_at_ms":1010 + n,"message":{"role":"user","content":format!("shared_owner_needle {n}")}}),
            ],
        );
    }
    let faux = dir.path().join("faux.json");
    std::fs::write(
        &faux,
        json!([{ "text": "shared_owner_needle live1" }, { "text": "shared_owner_needle live2" }]).to_string(),
    )
    .unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut owner = RpcClient::spawn(&["--rpc", "--peer", "--yolo", "--model", "faux/echo"], &env, &work);
    let init = owner.call("initialize", json!({ "protocol_version": "3.0" }));
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    for prompt in ["shared_owner_needle p1", "shared_owner_needle p2"] {
        let p = owner.call("agent.prompt", json!({ "session_id": sid, "input": prompt }));
        owner.call("agent.wait", json!({ "session_id": sid, "turn_id": p["turn_id"] }));
    }
    for i in 1..=3 {
        owner.call(
            "session.add_record",
            json!({ "session_id": sid, "kind": "owner/test", "data": { "idx": i }, "sync": true }),
        );
    }

    let mut peer_a = crate::common::rpc::PeerClient::connect(&home, owner.pid());
    peer_a.call("initialize", json!({ "protocol_version": init["protocol_version"] }));
    let mut peer_b = crate::common::rpc::PeerClient::connect(&home, owner.pid());
    peer_b.call("initialize", json!({ "protocol_version": init["protocol_version"] }));

    for (method, params) in [
        ("session.list", json!({ "limit": 2 })),
        ("session.list_items", json!({ "session_id": sid, "limit": 2 })),
        ("session.list_records", json!({ "session_id": sid, "kind": "owner/test", "limit": 2 })),
    ] {
        let page1 = peer_a.call(method, params.clone());
        let cursor = page1["result"]["next_cursor"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} missing next_cursor: {page1}"))
            .to_string();
        let mut next_params = params;
        next_params["cursor"] = json!(cursor);

        let stolen = peer_b.call(method, next_params.clone());
        assert_eq!(
            stolen["error"]["data"]["code"], "stale_session",
            "{method}: different caller under identical policy must not reuse cursor: {stolen}"
        );

        let own_next = peer_a.call(method, next_params);
        assert!(
            own_next.get("error").is_none() && !own_next["result"]["data"].as_array().unwrap().is_empty(),
            "{method}: original caller must advance with its own cursor: {own_next}"
        );
    }

    // Two distinct owner connections under identical policy (stdio owner vs WS owner) must not reuse
    // `session.search` / `session.search_items` cursors across caller identities.
    let ws_info = owner.call("webserver.get", json!({}));
    let ws_url = format!("{}?token=test-owner-token-0123456789abcdef", ws_info["ws_url"].as_str().unwrap());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (mut ws_owner, _) = tokio_tungstenite::connect_async(&ws_url).await.expect("WS owner connect failed");
        let mut next_id = 1u64;
        let mut ws_call = async |method: &str, params: Value| -> Value {
            let id = next_id;
            next_id += 1;
            ws_owner
                .send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string().into(),
                ))
                .await
                .unwrap();
            loop {
                let frame = ws_owner.next().await.unwrap().unwrap();
                if let Message::Text(text) = frame {
                    let msg: Value = serde_json::from_str(&text).unwrap();
                    if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return msg;
                    }
                }
            }
        };
        let ws_init =
            ws_call("initialize", json!({ "protocol_version": init["protocol_version"], "capabilities": {} })).await;
        assert!(ws_init.get("error").is_none(), "WS owner initialize failed: {ws_init}");

        for (method, params) in [
            ("session.search", json!({ "query": "shared_owner_needle", "limit": 2 })),
            ("session.search_items", json!({ "session_id": sid, "query": "shared_owner_needle", "limit": 2 })),
        ] {
            let p1 = owner.call_raw(method, params.clone(), |_| json!(null));
            let cursor = p1["result"]["next_cursor"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} missing next_cursor: {p1}"))
                .to_string();
            let mut next_params = params;
            next_params["cursor"] = json!(cursor);

            let stolen = ws_call(method, next_params.clone()).await;
            assert_eq!(
                stolen["error"]["data"]["code"], "stale_session",
                "{method}: different owner under identical policy must not reuse cursor: {stolen}"
            );

            let own_next = owner.call_raw(method, next_params, |_| json!(null));
            assert!(
                own_next.get("error").is_none() && !own_next["result"]["data"].as_array().unwrap().is_empty(),
                "{method}: original owner must advance with its own cursor: {own_next}"
            );
        }
    });

    drop(peer_b);
    drop(peer_a);
    owner.shutdown_and_wait();
}

#[tokio::test]
async fn search_sessions_without_exec_context_honours_deny_read() {
    local(async {
        let env = TestEnv::new().await;
        let sdir = session_dir_for(env.work.path());
        let denied_path = write_persisted_session(
            &sdir,
            "2026-01-01T00-00-00Z_noctx-denied",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"noctx-denied","cwd":env.work.path(),"created_at_ms":1000}),
                json!({"type":"name","name":"Denied NoCtx"}),
                json!({"type":"message","entry_id":"m1","created_at_ms":1010,"message":{"role":"user","content":"unique_noctx_needle_999"}}),
            ],
        );
        write_persisted_session(
            &sdir,
            "2026-01-02T00-00-00Z_noctx-allowed",
            &[
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"noctx-allowed","cwd":env.work.path(),"created_at_ms":2000}),
                json!({"type":"name","name":"Allowed NoCtx"}),
                json!({"type":"message","entry_id":"m2","created_at_ms":2010,"message":{"role":"user","content":"unique_noctx_needle_999"}}),
            ],
        );
        let h = env.host().no_session().build().await;
        global(&h, "DENIED_PATH", denied_path.to_string_lossy().to_string());
        let raw: String = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current({
                level = "read-only",
                network = true,
                deny_read = { DENIED_PATH },
                accept_partial = { "fs_deny_read" },
            })
            local res = mare.store.search_sessions({ query = "unique_noctx_needle_999" })
            return mare.json.encode(res)
            "#,
        )
        .await;
        let v: Value = serde_json::from_str(&raw).unwrap();
        let ids: Vec<&str> = v["data"].as_array().unwrap().iter().filter_map(|r| r["session_id"].as_str()).collect();
        assert!(ids.contains(&"noctx-allowed"), "allowed session must be found without exec_ctx: {v}");
        assert!(!ids.contains(&"noctx-denied"), "deny_read session must be excluded without exec_ctx: {v}");
        h.shutdown().await;
    })
    .await;
}
