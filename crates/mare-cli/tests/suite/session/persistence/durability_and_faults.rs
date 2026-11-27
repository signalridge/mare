//! Persistence listeners, durable execution acknowledgements, and persistence faults.

use super::*;

#[tokio::test]
async fn persistent_agent_refuses_tools_without_a_persistence_listener() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "unsafe.txt", "content": "changed" } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).config(json!({ "plugins": { "session": { "enabled": false } } })).build().await;
        global(&h, "A", h.service_table("agent").unwrap());
        h.prompt("write the file", json!({})).await.unwrap();
        let results = tool_results(&h).await;
        assert!(!env.cwd.join("unsafe.txt").exists(), "an unlogged write ran: {results}");
        assert!(results.contains("persist") || results.contains("session"), "{results}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicitly_ephemeral_agent_can_run_without_a_persistence_listener() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "ephemeral.txt", "content": "changed" } }] },
            { "text": "done" }
        ]);
        let h = env
            .host()
            .faux(script)
            .config(json!({ "plugins": { "session": { "enabled": false } } }))
            .no_session()
            .build()
            .await;
        h.prompt("write the file", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(env.cwd.join("ephemeral.txt")).unwrap(), "changed");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn third_party_session_providers_can_acknowledge_agent_records_via_spi() {
    local(async {
        let env = TestEnv::new().await;
        let plugin = env.config_dir().join("plugins/persistence-probe");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("init.lua"),
            r#"return { name = 'persistence-probe', provide = { 'session' }, apply = function(ctx)
                 mare._probe_records = {}
                 local seq = 0
                 local function next_id() seq = seq + 1; return 'probe-' .. seq end
                 local cur = { session_id = 'probe-sess', session_epoch = 'probe-gen', path = nil, count = 0 }
                 local S = require('cordis').class('ProbeSession', mare.Service)
                 function S:constructor(c) S.super.constructor(self, c, 'session') end
                 for _, name in ipairs({
                   'set_sandbox_mode', 'short_id', 'load_transcript',
                   'set_leaf', 'update', 'label_item', 'archive', 'unarchive', 'delete',
                   'search', 'search_items', 'get_item', 'trace', 'trace_item',
                   'rewind', 'snapshot', 'snapshot_context', 'commit', 'append_compaction', 'append_prune',
                 }) do
                   S[name] = function() return true end
                 end
                 function S:get() return { session_id = cur.session_id, cwd = mare.cwd, loaded = true, created_at_ms = 0, updated_at_ms = 0 } end
                 function S:list_items() return { data = {} } end
                 function S:add_record() return { record_id = 'probe-r', durable = false } end
                 function S:list_records() return { data = {} } end
                 function S:entries() return {} end
                 function S:path_to() return {} end
                 function S:children() return {} end
                 function S:list() return { data = {} } end
                 function S:info() return cur end
                 function S:available() return true end
                 function S:switching() return false end
                 function S:leaf() return nil end
                 function S:child() return nil end
                 function S:transition() return { status = 'rejected', error = 'unsupported' } end
                 function S:baseline_cut() return { parts = {} } end
                 function S:persist_message(ev)
                   local id = next_id()
                   mare._probe_records[#mare._probe_records + 1] = { method = 'persist_message', id = id }
                   return id
                 end
                 function S:record_tool_state(ev)
                   local id = ev.id or next_id()
                   mare._probe_records[#mare._probe_records + 1] = { method = 'record_tool_state', state = ev.state, id = id }
                   return id
                 end
                 function S:record_request(ev)
                   local id = ev.id or next_id()
                   mare._probe_records[#mare._probe_records + 1] = { method = 'record_request', id = id }
                   return id
                 end
                 function S:record_request_end(ev)
                   local id = ev.id or next_id()
                   mare._probe_records[#mare._probe_records + 1] = { method = 'record_request_end', id = id }
                   return id
                 end
                 function S:record_request_aborted(ev)
                   local id = ev.id or next_id()
                   mare._probe_records[#mare._probe_records + 1] = { method = 'record_request_aborted', id = id }
                   return id
                 end
                 function S:record_run_end()
                   return true
                 end
                 ctx:plugin(S)
               end }"#,
        )
        .unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "custom.txt", "content": "changed" } }] },
            { "text": "done" }
        ]);
        let h = env
            .host()
            .faux(script)
            .config(json!({ "plugins": { "session": { "enabled": false } } }))
            .user_plugins()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"EMITTED_IDS = {}
               C.cordis:on("item/completed", function(ev)
                   local item = ev and ev.item
                   local id = item and (item.entry_id or item.result_entry_id) or nil
                   if id then EMITTED_IDS[#EMITTED_IDS + 1] = id end
               end)"#,
        )
        .await;
        h.prompt("write the file", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(env.cwd.join("custom.txt")).unwrap(), "changed");
        let (states, all_probe_ids): (String, bool) = eval(
            &h,
            r#"local out = {}
               for _, record in ipairs(mare._probe_records) do
                 if record.method == 'record_tool_state' then out[#out + 1] = record.state end
               end
               local ok_ids = #EMITTED_IDS >= 4
               for _, id in ipairs(EMITTED_IDS) do
                 if type(id) ~= 'string' or not id:find('^probe%-') then ok_ids = false end
               end
               return table.concat(out, ','), ok_ids"#,
        )
        .await;
        assert_eq!(states, "prepared,started,completed");
        assert!(all_probe_ids, "item/completed.entry_id must propagate SPI-returned provider id");
        // The probe intentionally acknowledges in memory: the host cannot verify that an
        // arbitrary third-party provider actually made its acknowledgement durable.
        h.shutdown().await;
    })
    .await
}

/// the host, not a Lua return value, decides that a call's start is durable. A
/// replaced loop whose `on_state` claims success without writing is refused by
/// `mare.exec.run` while the session journal is open; an ack minted by an fsynced journal
/// write admits the call exactly once.
#[tokio::test]
async fn the_host_refuses_a_tool_whose_start_was_never_made_durable() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        h.prompt("open the session journal", json!({})).await.unwrap();
        expose(&h);
        let (code, text, raw, other_ok, other_err, still, runs, again): (String, String, String, bool, String, String, i64, String) = eval(
            &h,
            r#"local res = T:execute({ id = "rogue-1", name = "write",
                   arguments = { path = "rogue.txt", content = "x" } },
                 { on_state = function() return true end }) -- claims durable, writes nothing
               local runs = 0
               local function body() runs = runs + 1 end
               local _, raw = pcall(mare.exec.run, { tool = "probe", tool_call_id = "raw-1" }, body)
               local f = mare.store.open(mare.paths.state .. "/probe-journal.jsonl")
               local other_ok, other_err = pcall(f.append, f,
                 { type = "tool_state", tool_call_id = "raw-1", state = "started" }, { ack = "raw-1" })
               local _, still = pcall(mare.exec.run, { tool = "probe", tool_call_id = "raw-1" }, body)
               assert(type(S:record_tool_state({ tool_call_id = "raw-1", name = "probe", state = "started" })) == "string")
               mare.exec.run({ tool = "probe", tool_call_id = "raw-1" }, body)
               local _, again = pcall(mare.exec.run, { tool = "probe", tool_call_id = "raw-1" }, body)
               -- many calls of one response, every start acked before any runs: none is lost
               for i = 1, 300 do
                 assert(type(S:record_tool_state({ tool_call_id = "p" .. i, name = "probe", state = "started" })) == "string")
               end
               for i = 1, 300 do mare.exec.run({ tool = "probe", tool_call_id = "p" .. i }, body) end
               f:close()
               return tostring(res.error_code), tostring(res.content), tostring(raw), other_ok, tostring(other_err), tostring(still), runs, tostring(again)"#,
        )
        .await;
        assert!(!env.cwd.join("rogue.txt").exists(), "a call without a durable start ran");
        assert_eq!(code, "persist_failed");
        assert!(text.contains("call rogue-1 has no durable start record"), "{text}");
        assert!(raw.contains("EXEC_NOT_DURABLE"), "{raw}");
        assert!(!other_ok && other_err.contains("active session journal"), "{other_err}");
        assert!(still.contains("EXEC_NOT_DURABLE"), "the other journal admitted the call: {still}");
        assert_eq!(runs, 301, "each acked start admits its call once, however many wait");
        assert!(again.contains("EXEC_NOT_DURABLE"), "an ack is single-use: {again}");
        h.shutdown().await;
    })
    .await
}

/// types/mare/proc.lua `mare.exec.run`: `lifetime = "ephemeral-read-only"` with an explicit
/// read-only sandbox that blocks every network skips journal admission — top-level and nested in
/// an admitted call alike — while anything short of that still needs its durable start.
#[tokio::test]
async fn an_ephemeral_read_only_run_needs_no_durable_start() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        h.prompt("open the session journal", json!({})).await.unwrap();
        expose(&h);
        let (top, nested, no_sandbox, open_net, no_lifetime): (bool, bool, String, String, String) = eval(
            &h,
            r#"local ro = { level = "read-only" }
               local function read() return true end
               local top = mare.exec.run({ tool = "inspect", lifetime = "ephemeral-read-only", sandbox = ro }, read)
               assert(type(S:record_tool_state({ tool_call_id = "outer-1", name = "probe", state = "started" })) == "string")
               local nested = mare.exec.run({ tool = "probe", tool_call_id = "outer-1" }, function()
                 return mare.exec.run({ tool = "inspect", lifetime = "ephemeral-read-only", sandbox = ro }, read)
               end)
               local _, no_sandbox = pcall(mare.exec.run, { tool = "inspect", lifetime = "ephemeral-read-only" }, read)
               local _, open_net = pcall(mare.exec.run,
                 { tool = "inspect", lifetime = "ephemeral-read-only", sandbox = { level = "read-only", network = true } }, read)
               local _, no_lifetime = pcall(mare.exec.run, { tool = "inspect", sandbox = ro }, read)
               return top, nested, tostring(no_sandbox), tostring(open_net), tostring(no_lifetime)"#,
        )
        .await;
        assert!(top, "a top-level ephemeral read-only run");
        assert!(nested, "an ephemeral read-only run nested in an admitted call");
        for (what, err) in [("no sandbox", no_sandbox), ("an open network", open_net), ("no lifetime", no_lifetime)] {
            assert!(err.contains("EXEC_NOT_DURABLE"), "{what} still needs a durable start: {err}");
        }
        h.shutdown().await;
    })
    .await
}

/// Acks belong to one active journal and cannot survive switching or closing it.
#[tokio::test]
async fn acknowledgements_do_not_cross_journals_or_session_switches() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "plugins": { "session": { "enabled": false } } })).build().await;
        let (wrong, old_ack, no_active, aux_ack, aux_activate, runs): (String, String, String, String, String, i64) = eval(
            &h,
            r#"local dir = mare.paths.state
               local a = mare.store.open(dir .. "/gate-a.jsonl")
               local b = mare.store.open(dir .. "/gate-b.jsonl")
               local aux = mare.store.open(dir .. "/gate-aux.jsonl", { auxiliary = true })
               for _, f in ipairs({ a, b, aux }) do
                 f:append({ type = "session", version = mare.store.format_version, session_id = "gate" })
               end
               local function start(f, id)
                 return f:append({ type = "tool_state", state = "started", tool_call_id = id }, { ack = id })
               end
               local _, wrong = pcall(start, b, "one")
               local _, aux_ack = pcall(start, aux, "aux")
               local _, aux_activate = pcall(aux.activate, aux)
               start(a, "one")
               b:activate()
               local runs = 0
               local function body() runs = runs + 1 end
               local _, old_ack = pcall(mare.exec.run, { tool = "probe", tool_call_id = "one" }, body)
               start(b, "one")
               mare.exec.run({ tool = "probe", tool_call_id = "one" }, body)
               b:close()
               local _, no_active = pcall(mare.exec.run, { tool = "probe", tool_call_id = "other" }, body)
               a:activate()
               start(a, "two")
               mare.exec.run({ tool = "probe", tool_call_id = "two" }, body)
               a:close()
               -- Auxiliary files alone do not gate an explicitly ephemeral session.
               mare.exec.run({ tool = "probe", tool_call_id = "three" }, body)
               aux:close()
               return tostring(wrong), tostring(old_ack), tostring(no_active), tostring(aux_ack), tostring(aux_activate), runs"#,
        )
        .await;
        assert!(wrong.contains("active session journal"), "{wrong}");
        assert!(old_ack.contains("EXEC_NOT_DURABLE"), "{old_ack}");
        assert!(no_active.contains("no active session journal"), "{no_active}");
        assert!(aux_ack.contains("auxiliary"), "{aux_ack}");
        assert!(aux_activate.contains("auxiliary"), "{aux_activate}");
        assert_eq!(runs, 3);
        h.shutdown().await;
    })
    .await
}

/// A plugin cannot mint an execution acknowledgement with unrelated data or another call's start.
#[tokio::test]
async fn acknowledgement_requires_a_matching_started_record() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "plugins": { "session": { "enabled": false } } })).build().await;
        let (errors, denied, before, after, runs): (String, String, i64, i64, i64) = eval(
            &h,
            r#"local path = mare.paths.state .. "/verified-start.jsonl"
               local f = mare.store.open(path)
               f:append({ type = "session", version = mare.store.format_version, session_id = "verified-start" })
               local invalid = {
                 { type = "message", state = "started", tool_call_id = "bad" },
                 { type = "tool_state", state = "prepared", tool_call_id = "bad" },
                 { type = "tool_state", state = "started", tool_call_id = "another" },
                 { type = "tool_state", state = "started" },
               }
               local errors = {}
               for _, entry in ipairs(invalid) do
                 local ok, err = pcall(f.append, f, entry, { ack = "bad" })
                 assert(not ok)
                 errors[#errors + 1] = tostring(err)
               end
               local ok, err = pcall(f.append_batch, f, invalid, { ack = "bad" })
               assert(not ok)
               errors[#errors + 1] = tostring(err)
               local runs = 0
               local function body() runs = runs + 1 end
               local _, denied = pcall(mare.exec.run, { tool = "probe", tool_call_id = "bad" }, body)
               local before = #mare.store.read(path)
               f:append({ type = "custom", data = "still allowed" })
               f:append({ type = "tool_state", state = "started", tool_call_id = "one" }, { ack = "one" })
               mare.exec.run({ tool = "probe", tool_call_id = "one" }, body)
               f:append_batch({ { type = "custom" },
                   { type = "tool_state", state = "started", tool_call_id = "two" } }, { ack = "two" })
               mare.exec.run({ tool = "probe", tool_call_id = "two" }, body)
               local after = #mare.store.read(path)
               f:close()
               return table.concat(errors, "\n"), tostring(denied), before, after, runs"#,
        )
        .await;
        assert_eq!(
            errors.matches("ack requires a tool_state started record with the same tool_call_id").count(),
            5,
            "{errors}"
        );
        assert!(denied.contains("EXEC_NOT_DURABLE"), "{denied}");
        assert_eq!(before, 1, "rejected acknowledgements must not write any records beyond the header");
        assert_eq!(after, 5, "header, custom records and correctly acknowledged starts are retained");
        assert_eq!(runs, 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn append_batch_async_and_flush_async_preserve_durable_ack_and_ordering() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let path = env.home.path().join("async-ack.jsonl");
        let (denied, bad_err, runs, after): (String, String, i64, i64) = eval(
            &h,
            &format!(
                r#"
                local path = {path:?}
                local f = mare.store.open(path)
                f:append_batch_async({{
                    {{ type = "session", version = mare.store.format_version, session_id = "async-ack" }},
                }}, {{ sync = true }})
                local ok_bad, err_bad = pcall(f.append_batch_async, f, {{
                    {{ type = "custom", data = "no-start" }},
                }}, {{ ack = "bad-async" }})
                assert(not ok_bad)
                local runs = 0
                local function body() runs = runs + 1 end
                local _, denied = pcall(mare.exec.run, {{ tool = "probe", tool_call_id = "bad-async" }}, body)
                local ok_w, lossy = f:append_batch_async({{
                    {{ type = "custom", data = "before-start" }},
                    {{ type = "tool_state", state = "started", tool_call_id = "async-1" }},
                }}, {{ ack = "async-1" }})
                assert(ok_w == true and lossy == false)
                mare.exec.run({{ tool = "probe", tool_call_id = "async-1" }}, body)
                f:append_batch_async({{ {{ type = "custom", data = "buffered" }} }}, {{ sync = false }})
                f:flush_async()
                local after = #mare.store.read(path)
                f:close()
                return tostring(denied), tostring(err_bad), runs, after
                "#
            ),
        )
        .await;
        assert!(denied.contains("EXEC_NOT_DURABLE"), "{denied}");
        assert!(bad_err.contains("ack requires a tool_state started record"), "{bad_err}");
        assert_eq!(runs, 1);
        assert_eq!(after, 4, "header + custom + tool_state + buffered custom must be durable in order");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn every_entry_is_written_exactly_once() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        h.prompt("one", json!({})).await.unwrap();
        h.prompt("two", json!({})).await.unwrap();
        expose(&h);
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let ids: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["id"].as_str().map(str::to_string))
            .collect();
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(ids.len(), uniq.len(), "duplicate entries: {ids:?}");
    })
    .await
}

#[tokio::test]
async fn failed_session_write_blocks_side_effecting_tools() {
    local(async {
        let env = TestEnv::new().await;
        // a regular file where the sessions dir belongs: every session write fails (also as root)
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        std::fs::write(env.home.path().join("state/sessions"), "").unwrap();
        let marker = env.cwd.join("tool-ran");
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": create_file_cmd(&marker) } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).build().await;
        let res = h.prompt("touch it", json!({})).await.unwrap();
        assert!(res["error"]["message"].as_str().unwrap_or("").contains("session write failed"), "{res}");
        assert!(!marker.exists(), "a tool with side effects ran although the session could not be persisted");
        h.shutdown().await;
    })
    .await
}

/// During a persistence fault no fallback model answers (the retry layer,
/// other credentials and other models must not run on their own while the record is stale).
#[tokio::test]
async fn a_persistence_fault_never_switches_to_a_fallback_model() {
    local(async {
        let env = TestEnv::new().await;
        let quota = json!({ "error": "quota exhausted", "kind": "quota", "retryable": false });
        let h = env
            .host()
            .faux(json!([quota, { "text": "from the fallback" }]))
            .config(json!({ "fallback_models": ["faux/echo2"] }))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"FAIL, FALLBACKS = true, 0
               local orig_persist = S.persist_message
               S.persist_message = function(...)
                 if FAIL then require("mare_lib").fail("persist_failed", "disk full") end
                 return orig_persist(...)
               end
               C.cordis:on("llm/fallback", function() FALLBACKS = FALLBACKS + 1 end)"#,
        )
        .await;
        let res = h.prompt("first", json!({})).await.unwrap();
        assert!(res["error"]["message"].is_string() && res["text"] == "", "the failed request stands: {res}");
        assert_eq!(eval::<i64>(&h, "return FALLBACKS").await, 0, "no fallback during the fault");
        // the fallback's scripted answer was never consumed: the next (healthy) request gets it
        exec(&h, "FAIL = false").await;
        let res = h.prompt("second", json!({})).await.unwrap();
        assert_eq!(res["text"], "from the fallback", "{res}");
        h.shutdown().await;
    })
    .await
}

/// A persistence fault stops every tool — read-only ones too — and is cleared only after the
/// records that failed are written again, in order; a later successful write
/// alone doesn't clear it.
#[tokio::test]
async fn persistence_fault_stops_all_tools_until_missing_records_are_rewritten() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] },
            { "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] },
            { "text": "recovered" }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        exec(
            &h,
            r#"FAIL = true
            local orig_persist, orig_ts = S.persist_message, S.record_tool_state
            S.persist_message = function(...)
                if FAIL then require("mare_lib").fail("persist_failed", "disk full") end
                return orig_persist(...)
            end
            S.record_tool_state = function(...)
                if FAIL then require("mare_lib").fail("persist_failed", "disk full") end
                return orig_ts(...)
            end
            RESULTS = {}
            C.cordis:on("item/completed", function(e)
                if e and e.item and e.item.type == "tool_call" then
                    RESULTS[#RESULTS + 1] = e.item
                end
            end)"#,
        )
        .await;
        let res = h.prompt("first", json!({})).await.unwrap();
        assert!(res["error"]["message"].as_str().unwrap_or("").contains("can't be saved"), "{res}");
        let (count, result_status, execution_status): (i64, String, String) =
            eval(&h, "return #RESULTS, RESULTS[1].result_status, RESULTS[1].execution_status").await;
        assert_eq!((count, result_status.as_str(), execution_status.as_str()), (1, "persist_failed", "unknown"));
        exec(&h, "FAIL = false").await;
        let res = h.prompt("second", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let msgs: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["type"] == "message")
            .map(|e| e["message"].clone())
            .collect();
        let text = |m: &Value| {
            m["content"]
                .as_array()
                .map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<String>())
                .unwrap_or_default()
        };
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // the failed records came back first, in their original order, then the new ones
        assert_eq!(&roles[..4], ["user", "assistant", "tool_result", "user"], "{roles:?}");
        assert_eq!(text(&msgs[0]), "first");
        assert!(text(&msgs[2]).starts_with("Not run: the session can't be saved"), "read-only ls was stopped too");
        assert_eq!(text(&msgs[3]), "second");
        let second_ls = msgs.iter().filter(|m| m["role"] == "tool_result").nth(1).expect("second ls result");
        assert!(!text(second_ls).starts_with("Not run"), "tools run again after recovery: {}", text(second_ls));
    })
    .await
}

#[tokio::test]
async fn prepared_session_journal_does_not_activate_before_commit() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "plugins": { "session": { "enabled": false } } })).build().await;
        let (before, after): (String, bool) = eval(
            &h,
            r#"
            local f = mare.store.open(mare.paths.state .. "/prepared.jsonl", {defer_activation=true})
            f:append({type="session", version=mare.store.format_version, session_id="prepared"})
            local entry = {type="tool_state", state="started", tool_call_id="p"}
            local _, err = pcall(f.append, f, entry, {ack="p"})
            f:activate()
            local ok = pcall(f.append, f, entry, {ack="p"})
            f:close()
            return tostring(err), ok
        "#,
        )
        .await;
        assert!(before.contains("active session journal"), "{before}");
        assert!(after);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn multibyte_prompt_crossing_changes_turn_cap_persists_cleanly() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["changes"]).faux(script).build().await;
        expose(&h);
        let prompt = format!("{}中tail", "a".repeat(1999));
        let res = h.prompt(&prompt, json!({})).await.unwrap();
        assert!(res["error"].is_null(), "unexpected error: {res}");
        assert_eq!(res["text"], "done", "{res}");
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let entries: Vec<serde_json::Value> =
            std::fs::read_to_string(&path).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let turn = entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "changes/turn")
            .expect("missing changes/turn entry");
        assert_eq!(turn["data"]["text"], "a".repeat(1999));
    })
    .await
}

#[tokio::test]
async fn non_utf8_object_key_is_rejected_and_lossy_value_persists_with_warning() {
    local(async {
        let env = TestEnv::new().await;
        let mut opts = env.host().opts;
        opts.mode = mare_plugin::Mode::Tui;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        expose(&h);
        while rx.try_recv().is_ok() {}

        let ok_clean: bool = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local ok = pcall(sess.add_record, sess, "test/clean", { raw = "valid-utf8" }, { sync = true })
               return ok == true"#,
        )
        .await;
        assert!(ok_clean);
        let mut clean_notices = vec![];
        while let Ok(cmd) = rx.try_recv() {
            if let mare_shared::ui::UiCmd::Notify { text, level, .. } = cmd
                && text.contains("non-UTF-8")
            {
                clean_notices.push((text, level));
            }
        }
        assert!(clean_notices.is_empty(), "unexpected non-UTF-8 warning on valid UTF-8 write: {clean_notices:?}");

        let (bad_val_ok, bad_val_err, bad_key_ok, bad_key_err, nan_ok, nan_err, path): (
            bool,
            Option<String>,
            bool,
            Option<String>,
            bool,
            Option<String>,
            String,
        ) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local ok1, err1 = pcall(sess.add_record, sess, "test/bytes", { raw = "ab\228\184" }, { sync = true })
               local ok2, err2 = pcall(sess.add_record, sess, "test/bad_key", { ["k\228\184"] = "v" }, { sync = true })
               local ok3, err3 = pcall(sess.add_record, sess, "test/nan", { n = 0/0 }, { sync = true })
               return ok1 == true, (not ok1) and tostring(err1) or nil, ok2 == true, (not ok2) and tostring(err2) or nil, ok3 == true, (not ok3) and tostring(err3) or nil, S:info().path"#,
        )
        .await;
        assert!(bad_val_ok, "non-UTF-8 value must persist lossily instead of failing: {bad_val_err:?}");
        assert!(!bad_key_ok, "non-UTF-8 key must fail");
        assert!(bad_key_err.as_deref().is_some_and(|e| e.contains("UTF-8")), "unexpected err: {bad_key_err:?}");
        assert!(nan_ok, "non-finite number must convert to null and persist: {nan_err:?}");

        let mut lossy_notices = vec![];
        while let Ok(cmd) = rx.try_recv() {
            if let mare_shared::ui::UiCmd::Notify { text, level, .. } = cmd
                && text.contains("non-UTF-8")
            {
                lossy_notices.push((text, level));
            }
        }
        assert_eq!(lossy_notices.len(), 1, "expected visible warning notice on lossy session write: {lossy_notices:?}");
        assert_eq!(lossy_notices[0].1, "warning");

        h.shutdown().await;
        let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let rec_bytes = entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "test/bytes")
            .expect("missing test/bytes entry");
        assert_eq!(rec_bytes["data"]["raw"], "ab\u{FFFD}");
        let rec_nan = entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "test/nan")
            .expect("missing test/nan entry");
        assert_eq!(rec_nan["data"]["n"], serde_json::Value::Null);
        assert!(
            !entries.iter().any(|e| e["type"] == "custom" && e["kind"] == "test/bad_key"),
            "rejected non-UTF-8 key record must not be persisted: {entries:?}"
        );
    })
    .await
}

#[tokio::test]
async fn valid_complete_session_records_roundtrip_entry_by_entry() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (ok1, err1, ok2, err2, path): (bool, Option<String>, bool, Option<String>, String) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local payload1 = {
                   text = "hello 世界 🚀",
                   [""] = "empty-key",
                   ["__proto__"] = { safe = true, count = 42 },
                   ["键🔑"] = mare.json.array({ 1, 2, 3 }),
                   empty_arr = mare.json.array({}),
                   nullable = mare.json.null,
                   flag = false,
               }
               local payload2 = {
                   i64_max = math.maxinteger,
                   i64_min = math.mininteger,
                   tiny = 1e-308,
                   nested = { arr = mare.json.array({ "a", 1, true, { k = "v" } }) },
               }
               local ok1, err1 = pcall(sess.add_record, sess, "test/full-1", payload1, { sync = true })
               local ok2, err2 = pcall(sess.add_record, sess, "test/full-2", payload2, { sync = true })
               return ok1 == true, (not ok1) and tostring(err1) or nil, ok2 == true, (not ok2) and tostring(err2) or nil, S:info().path"#,
        )
        .await;
        assert!(ok1, "record 1 failed: {err1:?}");
        assert!(ok2, "record 2 failed: {err2:?}");
        h.shutdown().await;

        let disk_entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let rec1 = disk_entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "test/full-1")
            .expect("missing test/full-1");
        let rec2 = disk_entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "test/full-2")
            .expect("missing test/full-2");

        let expected1 = json!({
            "text": "hello 世界 🚀",
            "": "empty-key",
            "__proto__": { "safe": true, "count": 42 },
            "键🔑": [1, 2, 3],
            "empty_arr": [],
            "nullable": null,
            "flag": false,
        });
        let expected2 = json!({
            "i64_max": i64::MAX,
            "i64_min": i64::MIN,
            "tiny": 1e-308_f64,
            "nested": { "arr": ["a", 1, true, { "k": "v" }] },
        });
        assert_eq!(rec1["data"], expected1);
        assert_eq!(rec2["data"], expected2);

        // Read back via mare.store.read_tail and compare every entry against disk_entries.
        let h2 = env.host().build().await;
        expose(&h2);
        let reloaded_json: String = eval(
            &h2,
            &format!(
                r#"local tail = assert(mare.store.read_tail({:?}, 100))
                   return mare.json.encode(tail)"#,
                path
            ),
        )
        .await;
        h2.shutdown().await;
        let reloaded_entries: Vec<serde_json::Value> = serde_json::from_str(&reloaded_json).unwrap();
        assert_eq!(reloaded_entries.len(), disk_entries.len());
        for (idx, (read_back, on_disk)) in reloaded_entries.iter().zip(disk_entries.iter()).enumerate() {
            // json_to_lua omits null-valued object keys (e.g. `nullable = nil`).
            let mut expected = on_disk.clone();
            if let Some(data) = expected.get_mut("data").and_then(|d| d.as_object_mut()) {
                data.retain(|_, v| !v.is_null());
            }
            assert_eq!(read_back, &expected, "entry {idx} mismatch between disk and read_tail");
        }
    })
    .await
}

#[tokio::test]
async fn tool_result_with_non_utf8_bytes_saves_resumes_and_emits_warning() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "raw_bytes_tool", "arguments": {} }] },
            { "text": "turn 1 done" }
        ]);
        let mut opts = env.host().faux(script).opts;
        opts.mode = mare_plugin::Mode::Tui;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        expose(&h);
        eval::<()>(
            &h,
            r#"local tools = C.cordis:get("tools")
               tools:register("raw_bytes_tool", {
                   description = "test tool returning non-UTF-8 bytes",
                   parameters = { type = "object", properties = {} },
                   side_effect = "read",
                   execute = function()
                       return { content = "ok" }
                   end,
               })
               C.cordis:on("tool/after", function(data, ev)
                   if data.call.name == "raw_bytes_tool" then
                       data.result.details = { raw = "prefix\228\184suffix" }
                   end
                   return ev.next(data)
               end)"#,
        )
        .await;
        while rx.try_recv().is_ok() {}

        let res = h.prompt("run raw tool", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "unexpected error: {res}");
        assert_eq!(res["text"], "turn 1 done", "{res}");

        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;

        let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        assert!(
            entries.iter().any(|e| {
                e["type"] == "message"
                    && matches!(e["message"]["role"].as_str(), Some("tool_result" | "toolResult"))
                    && e["message"]["details"]["raw"] == "prefix\u{FFFD}suffix"
            }),
            "expected lossily persisted tool_result in session entries: {entries:?}"
        );

        let mut lossy_notices = vec![];
        while let Ok(cmd) = rx.try_recv() {
            if let mare_shared::ui::UiCmd::Notify { text, level, .. } = cmd
                && text.contains("non-UTF-8")
            {
                lossy_notices.push((text, level));
            }
        }
        assert!(
            !lossy_notices.is_empty(),
            "expected visible warning notice when tool result contains non-UTF-8 bytes: {lossy_notices:?}"
        );
        assert!(lossy_notices.iter().all(|(_, level)| level == "warning"), "{lossy_notices:?}");

        let h2 = env.host().faux(json!([{ "text": "resumed ok" }])).resume().build().await;
        let res2 = h2.prompt("after resume", json!({})).await.unwrap();
        assert!(res2["error"].is_null(), "unexpected resume error: {res2}");
        assert_eq!(res2["text"], "resumed ok", "{res2}");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn oversized_session_record_exceeding_node_budget_is_rejected_without_persisting() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (ok, err, path): (bool, Option<String>, Option<String>) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local big = mare.json.array()
               for i = 1, 1000005 do
                   big[i] = 1
               end
               local ok, err = pcall(sess.add_record, sess, "test/oversized", { items = big }, { sync = true })
               return ok == true, (not ok) and tostring(err) or nil, S:info().path"#,
        )
        .await;
        assert!(!ok, "oversized session write exceeding HISTORY_LOSSLESS_MAX_NODES must fail");
        assert!(
            err.as_deref().is_some_and(|e| e.contains("exceeds resource limit")),
            "expected resource limit error, got: {err:?}"
        );
        h.shutdown().await;
        if let Some(path) = path
            && std::path::Path::new(&path).exists()
        {
            let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
            assert!(
                !entries.iter().any(|e| e["type"] == "custom" && e["kind"] == "test/oversized"),
                "oversized record must not be persisted: {entries:?}"
            );
        }
    })
    .await
}

#[tokio::test]
async fn oversized_session_record_stops_copying_before_tail_non_utf8_key() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (ok, err): (bool, Option<String>) = eval(
            &h,
            r#"local sess = C.cordis:get("session")
               local big = mare.json.array()
               for i = 1, 1000005 do
                   big[i] = 1
               end
               big[1000005] = { ["bad\xffkey"] = 1 }
               local ok, err = pcall(sess.add_record, sess, "test/oversized-early-stop", { items = big }, { sync = true })
               return ok == true, (not ok) and tostring(err) or nil"#,
        )
        .await;
        assert!(!ok, "oversized session write must fail");
        assert!(
            err.as_deref().is_some_and(|e| e.contains("exceeds resource limit")),
            "expected early resource limit stop before tail non-UTF-8 key, got: {err:?}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn persistence_spi_returns_the_record_id_and_raises_on_write_failure() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("locked-spi.jsonl", &[msg("m1", None, "user", "LOCKED")]);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();

        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "LOCKED_PATH", path.to_string_lossy().to_string());
        let report: serde_json::Value = eval::<String>(
            &h,
            r#"
            local sess = C.cordis:get("session")
            local info = sess:info()
            local bus_called = false
            C.cordis:on("agent/persist", function() bus_called = true; return true end, { prepend = true })

            local msg_id = sess:persist_message({
                message = { role = "user", content = { { type = "text", text = "spi hello" } } },
                durable = true,
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            assert(type(msg_id) == "string" and msg_id ~= "", "persist_message returns record id")
            assert(sess:leaf() == msg_id, "leaf updates on commit")

            local ts_id = sess:record_tool_state({
                tool_call_id = "spi-call-1",
                name = "probe",
                state = "started",
                side_effect = "external",
                durable = true,
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            assert(type(ts_id) == "string" and ts_id ~= "", "record_tool_state returns record id")
            local ran = false
            mare.exec.run({ tool = "probe", tool_call_id = "spi-call-1" }, function() ran = true end)
            assert(ran, "started tool state must mint host gate ack")

            local req_id = sess:record_request({
                request_id = "spi-req-1",
                attempt = 1,
                durable = true,
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            local req_end_id = sess:record_request_end({
                request_id = "spi-req-1",
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            local req_abort_id = sess:record_request_aborted({
                request_id = "spi-req-2",
                attempt = 2,
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            assert(type(req_id) == "string" and type(req_end_id) == "string" and type(req_abort_id) == "string")

            -- Non-tree entries (tool_state, request markers) are rejected by set_leaf/label on LIVE state too
            for _, non_tree_id in ipairs({ ts_id, req_id, req_abort_id }) do
                local ok_leaf, err_leaf = sess:set_leaf(non_tree_id)
                assert(not ok_leaf and tostring(err_leaf):find("no such session entry", 1, true),
                    "live set_leaf must reject non-tree id " .. tostring(non_tree_id))
                local ok_lbl, err_lbl = pcall(sess.label_item, sess, "x", { item_id = non_tree_id })
                assert(not ok_lbl and type(err_lbl) == "table" and err_lbl.code == "session.unknown_item",
                    "live label_item must reject non-tree id " .. tostring(non_tree_id))
            end

            -- Stale owner raises M8 stale_session across both persist_message and record_tool_state
            local ok_stale, err_stale = pcall(sess.persist_message, sess, {
                message = { role = "user", content = { { type = "text", text = "stale" } } },
                session_id = info.session_id,
                session_epoch = "wrong-gen",
            })
            assert(not ok_stale and type(err_stale) == "table" and err_stale.code == "stale_session",
                "expected M8 stale_session error: " .. tostring(err_stale))
            local ok_stale_ts, err_stale_ts = pcall(sess.record_tool_state, sess, {
                tool_call_id = "stale-ts-1",
                name = "probe",
                state = "prepared",
                session_id = info.session_id,
                session_epoch = "wrong-gen",
            })
            assert(not ok_stale_ts and type(err_stale_ts) == "table" and err_stale_ts.code == "stale_session",
                "expected M8 stale_session error on record_tool_state: " .. tostring(err_stale_ts))

            -- Write failure (duplicate id or attachment write error) raises M8 error
            local ok_dup, err_dup = pcall(sess.persist_message, sess, {
                entry_id = msg_id,
                message = { role = "user", content = { { type = "text", text = "dup" } } },
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            assert(not ok_dup and type(err_dup) == "table" and type(err_dup.code) == "string",
                "expected M8 error on duplicate id: " .. tostring(err_dup))

            -- Real prompt goes through SPI, never agent/persist bus listener
            local res = A:wait(A:prompt("via spi").turn_id)
            assert(res and not res.error, tostring(res and res.error))
            assert(not bus_called, "agent/persist listener must never be invoked")

            -- Read-only view returns status = "read_only"
            mare.ui.select = function() return 2 end
            sess:load({ path = LOCKED_PATH })
            local ro_info = sess:info()
            assert(ro_info.readonly == true)
            local ro_msg = sess:persist_message({
                message = { role = "user", content = { { type = "text", text = "ro" } } },
                session_id = ro_info.session_id,
                session_epoch = ro_info.session_epoch,
            })
            local ro_req = sess:record_request({
                request_id = "ro-req-1",
                attempt = 0,
                session_id = ro_info.id,
                generation = ro_info.generation,
            })
            return mare.json.encode({
                msg_id = msg_id,
                ts_id = ts_id,
                req_id = req_id,
                req_end_id = req_end_id,
                req_abort_id = req_abort_id,
                ro_msg_status = type(ro_msg) == "table" and ro_msg.status or nil,
                ro_req_status = type(ro_req) == "table" and ro_req.status or nil,
            })
        "#,
        )
        .await
        .parse()
        .unwrap();
        drop(lock);
        h.shutdown().await;
        assert_eq!(report["ro_msg_status"], "read_only", "{report}");
        assert_eq!(report["ro_req_status"], "read_only", "{report}");
    })
    .await
}

#[tokio::test]
async fn partial_session_stays_unexpanded_across_agent_writes_and_tool_normalization() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "partial-lazy.jsonl",
            &[
                msg("u0", None, "user", "old turn"),
                json!({
                    "type": "message",
                    "entry_id": "a0",
                    "parent_entry_id": "u0",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": "old reply" }] }
                }),
                json!({
                    "type": "compaction",
                    "entry_id": "cmp1",
                    "parent_entry_id": "a0",
                    "summary": "compacted summary",
                    "first_kept_entry_id": null,
                    "tokens_before": 100
                }),
            ],
        );
        let script = json!([
            { "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (full_reads, tail_len, ok_dup, res_ok): (i64, i64, bool, bool) = eval(
            &h,
            r#"
            assert(S:load({ path = TARGET }))
            local full_reads = 0
            local orig_snap = mare.store.read_snapshot
            local orig_read = mare.store.read
            mare.store.read_snapshot = function(...) full_reads = full_reads + 1; return orig_snap(...) end
            mare.store.read = function(...) full_reads = full_reads + 1; return orig_read(...) end
            local res = A:wait(A:prompt("next turn with tool").turn_id)
            assert(S:add_record("lazy/probe", { x = 1 }, { entry_id = "lazy-custom-1", persist = true }))
            local ok_dup = pcall(S.add_record, S, "lazy/probe", { x = 2 }, { entry_id = "lazy-custom-1" })
            -- Partial entries remain tail-only when expand=false
            local tail_len = #S:entries({ expand = false })
            mare.store.read_snapshot = orig_snap
            mare.store.read = orig_read
            return full_reads, tail_len, ok_dup, res ~= nil and res.error == nil
        "#,
        )
        .await;
        assert!(res_ok, "turn with tool normalization and persistence must succeed");
        assert!(!ok_dup, "duplicate id in known_ids must still fail");
        assert_eq!(full_reads, 0, "agent persistence and tool normalization must not expand partial session");
        assert!(tail_len >= 4, "tail entries accumulated locally without loading u0/a0: {tail_len}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn all_records_reads_pre_compaction_records_without_expanding_partial_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "partial-pre-compaction-records.jsonl",
            &[
                msg("u0", None, "user", "old turn"),
                json!({
                    "type": "custom",
                    "entry_id": "rec-pre-1",
                    "parent_entry_id": "u0",
                    "kind": "test/state",
                    "data": { "revision": 1, "phase": "before-compaction" }
                }),
                json!({
                    "type": "message",
                    "entry_id": "a0",
                    "parent_entry_id": "u0",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": "old reply" }] }
                }),
                json!({
                    "type": "compaction",
                    "entry_id": "cmp1",
                    "parent_entry_id": "a0",
                    "summary": "compacted summary",
                    "first_kept_entry_id": null,
                    "tokens_before": 100
                }),
                json!({
                    "type": "custom",
                    "entry_id": "rec-post-2",
                    "parent_entry_id": "cmp1",
                    "kind": "test/state",
                    "data": { "revision": 2, "phase": "after-compaction" }
                }),
            ],
        );
        let h = env.host().build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (all_revs, explicit_false_revs, current_revs, loaded_only_revs, full_reads, has_u0): (
            String,
            String,
            String,
            String,
            i64,
            bool,
        ) = eval(
            &h,
            r#"
            assert(S:load({ path = TARGET }))
            local lib = require("mare_lib")
            local full_reads = 0
            local orig_snap = mare.store.read_snapshot
            local orig_read = mare.store.read
            mare.store.read_snapshot = function(...) full_reads = full_reads + 1; return orig_snap(...) end
            mare.store.read = function(...) full_reads = full_reads + 1; return orig_read(...) end
            local function revs(list)
                local out = {}
                for _, item in ipairs(list or {}) do out[#out + 1] = tostring(item.revision) end
                return table.concat(out, ",")
            end
            local r_default = revs(lib.all_records(S, "test/state"))
            local r_false = revs(lib.all_records(S, "test/state", { loaded_only = false }))
            local r_current = revs(lib.all_records(S, "test/state", { branch = "current" }))
            local r_loaded = revs(lib.all_records(S, "test/state", { loaded_only = true }))
            local has_u0 = false
            for _, e in ipairs(S:entries({ expand = false }) or {}) do
                if e.entry_id == "u0" then has_u0 = true end
            end
            mare.store.read_snapshot = orig_snap
            mare.store.read = orig_read
            return r_default, r_false, r_current, r_loaded, full_reads, has_u0
            "#,
        )
        .await;
        assert_eq!(all_revs, "1,2", "default all_records must include records before compaction");
        assert_eq!(explicit_false_revs, "1,2", "explicit loaded_only=false must include records before compaction");
        assert_eq!(current_revs, "1,2", "branch=current must include records before compaction");
        assert_eq!(loaded_only_revs, "2", "explicit loaded_only=true must return only tail records");
        assert_eq!(full_reads, 0, "pre-compaction custom records must be served without extra full reads");
        assert!(!has_u0, "partial session history must remain tail-only when expand=false");
        h.shutdown().await;
    })
    .await
}

/// Cancelling a `session.add_record` service export while its async disk append is already in
/// flight must not drop the Lua coroutine before `publish_entries` runs: memory (`list_records`
/// and `known_ids`) and the session JSONL file must agree.
#[tokio::test]
async fn cancelled_add_record_export_mid_append_keeps_memory_and_disk_in_sync() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "seeded" }])).build().await;
        expose(&h);
        let seed = h.prompt("seed open session file", json!({})).await.unwrap();
        assert_eq!(seed["text"], "seeded");
        let path: String = eval(&h, "return assert(S:info().path)").await;

        let caller = h.kernel.open_owner("rpc-add-record-cancel", "root").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let h_call = h.clone();
        let cancel_call = cancel.clone();
        let task = tokio::task::spawn_local(async move {
            h_call
                .kernel
                .call_service_export(
                    caller,
                    None,
                    "session",
                    "add_record",
                    json!({
                        "kind": "test/mid-cancel",
                        "data": { "v": 42 },
                        "sync": true
                    }),
                    cancel_call,
                )
                .await
        });

        // One local yield advances `add_record` synchronously up to `append_batch_async`
        // (right after `mark_call_committed()`), so `cancel()` fires mid-append.
        tokio::task::yield_now().await;
        cancel.cancel();

        let res = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("committed add_record must finish promptly")
            .unwrap();
        let committed = res.expect("in-flight committed add_record must finish publication instead of splitting state");
        let record_id = committed["record_id"].as_str().expect("add_record must return record_id").to_string();

        let (mem_count, dup_rejected): (usize, bool) = eval(
            &h,
            &format!(
                r#"
                local recs = S:list_records("test/mid-cancel").data
                local ok_dup = pcall(S.add_record, S, "test/mid-cancel", {{ v = 99 }}, {{
                    entry_id = {:?},
                    sync = true,
                }})
                return #recs, not ok_dup
                "#,
                record_id
            ),
        )
        .await;
        assert_eq!(mem_count, 1, "committed record must be published in memory");
        assert!(dup_rejected, "known_ids must reject duplicate entry_id after mid-append cancel");
        h.shutdown().await;

        let disk = std::fs::read_to_string(&path).unwrap();
        let disk_matches = disk.matches(&record_id).count();
        assert_eq!(disk_matches, 1, "disk must contain the committed record exactly once: {disk}");
    })
    .await
}

#[tokio::test]
async fn record_run_end_on_readonly_session_appends_in_memory_boundary() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("ro-boundary.jsonl", &[msg("u0", None, "user", "seed")]);
        let before_disk = std::fs::read_to_string(&path).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().faux(json!([{ "text": "readonly reply" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (is_ro, ret_true, has_boundary): (bool, bool, bool) = eval(
            &h,
            r#"
            mare.ui.select = function() return 2 end
            assert(S:load({ path = TARGET }))
            local info = S:info()
            local res = S:record_run_end({ run_id = "ro-run-1", session_id = info.session_id, session_epoch = info.session_epoch })
            local found = false
            for _, e in ipairs(S:entries()) do
                if e.type == "custom" and e.kind == "agent/run-end" and e.data and e.data.run_id == "ro-run-1" then
                    found = true
                end
            end
            return info.readonly == true, res == true, found
            "#,
        )
        .await;
        assert!(is_ro, "session must be loaded readonly");
        assert!(ret_true, "record_run_end on readonly session must return true (not read_only)");
        assert!(has_boundary, "readonly session must record agent/run-end in memory for fork boundaries");
        let after_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after_disk, before_disk, "readonly record_run_end must not write to disk");
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn aborted_commit_waiter_does_not_strand_later_writes_or_session_switch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        let (cancelled_rounds, switched_ok, final_leaf, path): (i64, bool, String, String) = eval(
            &h,
            r#"
            local info = S:info()
            local seed = S:persist_message({
                message = { role = "user", content = { { type = "text", text = "seed" } } },
                durable = true,
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            assert(type(seed) == "string" and S:leaf() == seed)
            local path = S:info().path
            local payload = string.rep("x", 262144)
            local cancelled_rounds = 0

            for r = 1, 6 do
                local e1_tx, e1_rx = mare.task.channel()
                local e2_tx, e2_rx = mare.task.channel()
                local e3_tx, e3_rx = mare.task.channel()
                local cancel_tx, cancel_rx = mare.task.channel()
                local d1_tx, d1_rx = mare.task.channel()
                local d2_tx, d2_rx = mare.task.channel()
                local d3_tx, d3_rx = mare.task.channel()

                C.cordis:spawn(function()
                    e1_tx:send(true)
                    local ok, res = pcall(S.persist_message, S, {
                        message = { role = "user", content = { { type = "text", text = "w1-" .. r .. "-" .. payload } } },
                        durable = true,
                        session_id = info.session_id,
                        session_epoch = info.session_epoch,
                    })
                    d1_tx:send({ ok = ok, res = res })
                end)

                C.cordis:spawn(function()
                    local winner, tag = mare.task.select(function()
                        e2_tx:send(true)
                        local ok, res = pcall(S.persist_message, S, {
                            message = { role = "user", content = { { type = "text", text = "w2-" .. r } } },
                            durable = true,
                            session_id = info.session_id,
                            session_epoch = info.session_epoch,
                        })
                        return ok and "wrote" or ("err:" .. tostring(res))
                    end, function()
                        cancel_rx:recv()
                        return "cancelled"
                    end)
                    d2_tx:send({ winner = winner, tag = tag })
                end)

                C.cordis:spawn(function()
                    e3_tx:send(true)
                    local ok, res = pcall(S.persist_message, S, {
                        message = { role = "user", content = { { type = "text", text = "w3-" .. r } } },
                        durable = true,
                        session_id = info.session_id,
                        session_epoch = info.session_epoch,
                    })
                    d3_tx:send({ ok = ok, res = res })
                end)

                assert(mare.task.timeout(5000, function()
                    assert(e1_rx:recv())
                    assert(e2_rx:recv())
                    assert(e3_rx:recv())
                    return true
                end), "all three writers must enter round " .. r)
                cancel_tx:send(true)

                local r1, r2, r3
                assert(mare.task.timeout(5000, function()
                    r1 = d1_rx:recv()
                    r2 = d2_rx:recv()
                    r3 = d3_rx:recv()
                    return true
                end), "later commit waiter stranded after middle waiter abort in round " .. r)
                assert(r1 and r1.ok, "writer 1 failed in round " .. r .. ": " .. tostring(r1 and r1.res))
                assert(r3 and r3.ok, "writer 3 failed in round " .. r .. ": " .. tostring(r3 and r3.res))
                if r2 and r2.tag == "cancelled" then
                    cancelled_rounds = cancelled_rounds + 1
                else
                    assert(r2 and r2.tag == "wrote", "unexpected writer 2 outcome: " .. tostring(r2 and r2.tag))
                end
            end

            local final_leaf = S:leaf()
            local switched_ok, switch_err = pcall(S.create, S)
            assert(switched_ok, "session switch must not be blocked after aborted commit waiter: " .. tostring(switch_err))
            return cancelled_rounds, switched_ok, final_leaf, path
            "#,
        )
        .await;
        assert!(cancelled_rounds >= 1, "expected middle waiter to be aborted during fsync in at least one round");
        assert!(switched_ok);
        h.shutdown().await;

        let disk_entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let messages: Vec<&serde_json::Value> = disk_entries.iter().filter(|e| e["type"] == "message").collect();
        assert!(messages.len() >= 13, "expected seed + at least 12 committed messages, got {}", messages.len());
        for pair in messages.windows(2) {
            assert_eq!(
                pair[1]["parent_entry_id"], pair[0]["entry_id"],
                "persisted messages must form an unbroken parent chain"
            );
        }
        assert_eq!(messages.last().unwrap()["entry_id"], final_leaf);
    })
    .await
}

#[tokio::test]
async fn concurrent_session_writes_bind_parent_leaf_under_commit_mutex() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "concurrent reply" }])).build().await;
        expose(&h);
        let report: serde_json::Value = eval::<String>(
            &h,
            r#"
            local info = S:info()
            local seed = S:persist_message({
                message = { role = "user", content = { { type = "text", text = "seed" } } },
                durable = true,
                session_id = info.session_id,
                session_epoch = info.session_epoch,
            })
            assert(type(seed) == "string" and S:leaf() == seed)
            local big = string.rep("z", 131072)

            -- 4 concurrent persist_message calls overlapping async fsync must all succeed and chain cleanly
            local dones = {}
            for i = 1, 4 do
                local tx, rx = mare.task.channel()
                dones[i] = rx
                C.cordis:spawn(function()
                    local ok, res = pcall(S.persist_message, S, {
                        message = { role = "user", content = { { type = "text", text = "burst-" .. i .. "-" .. big } } },
                        durable = true,
                        session_id = info.session_id,
                        session_epoch = info.session_epoch,
                    })
                    tx:send({ ok = ok, res = res })
                end)
            end
            local burst_ids = {}
            for i = 1, 4 do
                local ok_t, r = mare.task.timeout(5000, function() return dones[i]:recv() end)
                assert(ok_t and r, "burst writer timed out")
                assert(r.ok, "concurrent persist_message " .. i .. " failed: " .. tostring(r.res))
                burst_ids[i] = r.res
            end

            -- Concurrent agent:append overlapping a prompt commit must also succeed with no persist_error
            local app_tx, app_rx = mare.task.channel()
            C.cordis:spawn(function()
                local ok, err = A:append({
                    role = "user",
                    content = { { type = "text", text = "appended-" .. big } },
                })
                app_tx:send({ ok = ok, err = err })
            end)
            local turn = A:prompt("prompt during append")
            local turn_res = A:wait(turn.turn_id)
            local ok_app, app_res = mare.task.timeout(5000, function() return app_rx:recv() end)
            assert(ok_app and app_res, "append timed out")
            assert(app_res.ok, "concurrent agent:append failed: " .. tostring(app_res.err))
            assert(turn_res and not turn_res.error, "concurrent prompt failed: " .. tostring(turn_res and turn_res.error))
            assert(A:persist_error() == nil, "unexpected persist_error: " .. tostring(A:persist_error()))

            return mare.json.encode({
                path = S:info().path,
                seed = seed,
                burst_count = #burst_ids,
                leaf = S:leaf(),
            })
            "#,
        )
        .await
        .parse()
        .unwrap();
        h.shutdown().await;

        let path = std::path::Path::new(report["path"].as_str().unwrap());
        let disk_entries = mare_store::read_entries(path).unwrap();
        let messages: Vec<&serde_json::Value> = disk_entries.iter().filter(|e| e["type"] == "message").collect();
        // seed + 4 burst + 1 appended + 1 prompt user + 1 prompt assistant = 8 messages
        assert_eq!(messages.len(), 8, "all concurrent messages must be durably persisted: {messages:?}");
        assert_eq!(messages[0]["entry_id"], report["seed"]);
        for pair in messages.windows(2) {
            assert_eq!(
                pair[1]["parent_entry_id"], pair[0]["entry_id"],
                "concurrent session writes must form a single linear parent chain without forks"
            );
        }
        assert_eq!(messages.last().unwrap()["entry_id"], report["leaf"]);

        // No quarantine files may be written anywhere under state/
        let mut quarantine_files = vec![];
        fn collect_quarantine(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        collect_quarantine(&p, out);
                    } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("quarantine")) {
                        out.push(p);
                    }
                }
            }
        }
        collect_quarantine(&env.home.path().join("state"), &mut quarantine_files);
        assert!(quarantine_files.is_empty(), "unexpected quarantine files created: {quarantine_files:?}");
    })
    .await
}
