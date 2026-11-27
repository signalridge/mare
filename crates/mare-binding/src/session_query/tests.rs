use super::*;
use crate::session_filters::{EventFilters, SessionFilters as Filters};
use corpus::{corpus, corpus_with_live, validate_live};
use reader::read;
use search::{build_eligible_sources, pattern, search as run_search, search_sources, snippet};
use serde_json::json;
use trace::trace_event_in_corpus;

impl Search {
    fn clone_with_cursor(&self, cursor: Option<String>) -> Self {
        Self {
            query: self.query.clone(),
            title_query: self.title_query.clone(),
            sort_key: self.sort_key.clone(),
            sort_direction: self.sort_direction.clone(),
            filters: self.filters.clone(),
            event_filters: self.event_filters.clone(),
            session_id: self.session_id.clone(),
            cursor,
            limit: self.limit,
            step_bound: self.step_bound,
        }
    }
}

fn search_args(query: &str, session_id: Option<&str>, cursor: Option<String>) -> Search {
    Search { query: query.into(), session_id: session_id.map(str::to_owned), cursor, ..Default::default() }
}

fn search(c: Corpus, args: Search, events: bool) -> mlua::Result<Value> {
    run_search(c, args, events, false, &CancellationToken::new())
}

fn fixture() -> Corpus {
    let mut entries =
        vec![json!({"type":"session","version":mare_store::CURRENT_VERSION,"cwd":"/mine","session_id":"one"})];
    entries.extend((0..25).map(
        |i| json!({"type":"message","entry_id":format!("m{i}"),"message":{"role":"assistant","content":"needle 世界"}}),
    ));
    Corpus { sessions: vec![entries], live_ids: HashSet::new(), incomplete: false }
}

fn request(cursor: Option<String>) -> Search {
    search_args("needle", Some("one"), cursor)
}

fn semantic_fixture() -> Corpus {
    let entries = vec![
        json!({"type":"session","version":mare_store::CURRENT_VERSION,"cwd":"/mine","session_id":"one"}),
        json!({"type":"message","entry_id":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"needle"},
            {"type":"tool_call","id":"call-a","name":"read","arguments":{"path":"needle"}},
            {"type":"tool_call","id":"call-b","name":"read","arguments":{"path":"needle"}}
        ]}}),
        json!({"type":"message","entry_id":"result-a","message":{"role":"tool_result","tool_call_id":"call-a","content":[]}}),
        json!({"type":"message","entry_id":"result-b","message":{"role":"tool_result","tool_call_id":"call-b","content":[]}}),
        json!({"type":"custom","entry_id":"record","kind":"needle","data":{}}),
    ];
    Corpus { sessions: vec![entries], live_ids: HashSet::new(), incomplete: false }
}

#[test]
fn semantic_item_search_pages_do_not_expose_event_tokens() {
    let mut ids = HashSet::new();
    let mut count = 0;
    let mut cursor = None;
    loop {
        let args = Search {
            query: "needle".into(),
            session_id: Some("one".into()),
            limit: Some(2),
            cursor,
            ..Default::default()
        };
        let page = run_search(semantic_fixture(), args, true, true, &CancellationToken::new()).unwrap();
        for item in page["data"].as_array().unwrap() {
            count += 1;
            assert!(item.get("event_id").is_none(), "{item}");
            assert_eq!(item["entry_id"], "assistant", "{item}");
            ids.insert(item["item_id"].as_str().unwrap().to_owned());
        }
        cursor = page.get("next_cursor").and_then(Value::as_str).map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, HashSet::from(["assistant".to_owned(), "call-a".to_owned(), "call-b".to_owned()]));
    assert_eq!(count, 3, "item pages must not repeat any item");
}

#[test]
fn title_only_item_search_uses_the_same_pagination_shape() {
    let mut fixture = semantic_fixture();
    fixture.sessions[0].insert(1, json!({"type":"name","name":"Needle Session"}));
    let args = Search {
        session_id: Some("one".into()),
        title_query: Some("needle".into()),
        limit: Some(2),
        ..Default::default()
    };
    let first = run_search(
        Corpus { sessions: fixture.sessions.clone(), live_ids: HashSet::new(), incomplete: false },
        args,
        true,
        true,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(first["data"].as_array().unwrap().len(), 2, "{first}");
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let next = Search {
        session_id: Some("one".into()),
        title_query: Some("needle".into()),
        limit: Some(2),
        cursor: Some(cursor),
        ..Default::default()
    };
    let second = run_search(fixture, next, true, true, &CancellationToken::new()).unwrap();
    assert_eq!(second["data"].as_array().unwrap().len(), 1, "{second}");
    assert!(second.get("next_cursor").is_none(), "{second}");
}

#[test]
fn semantic_item_reads_are_session_bound_and_reject_event_tokens() {
    let projector = crate::projection::JsonProjector::default();
    let args = |sid: &str, item: &str| ReadItem {
        session_id: sid.into(),
        item_id: item.into(),
        before: Some(1),
        after: None,
        offset: None,
        max_bytes: None,
    };
    let item = reader::read_item_projected(&mut semantic_fixture(), args("one", "call-a"), &projector).unwrap();
    assert!(item["corpus_revision"].as_str().is_some_and(|s| !s.is_empty()), "{item}");
    assert!(item.get("generation").is_none(), "{item}");
    assert_eq!(item["item"]["entry_id"], "assistant");
    assert_eq!(item["item"]["result_entry_id"], "result-a");
    assert_eq!(item["neighbors"][0]["item_id"], "assistant");
    let trace = trace::trace_item_in_corpus_projected(&mut semantic_fixture(), "one", "call-a", &projector).unwrap();
    assert_eq!(trace["item_id"], "call-a");
    assert_eq!(trace["result_entry_id"], "result-a");
    assert!(trace.get("event_id").is_none(), "{trace}");
    assert!(trace.get("timestamp").is_none(), "{trace}");
    let item_hits = search::search(
        semantic_fixture(),
        Search { query: "needle".into(), session_id: Some("one".into()), ..Default::default() },
        true,
        true,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(item_hits["corpus_revision"].as_str().is_some_and(|s| !s.is_empty()), "{item_hits}");
    assert!(item_hits.get("generation").is_none(), "{item_hits}");
    let hit = &item_hits["data"][0];
    assert!(hit["session_created_at_ms"].is_i64(), "{hit}");
    assert!(hit["updated_at_ms"].is_i64(), "{hit}");
    assert!(hit.get("created_at").is_none() && hit.get("created_at_ms").is_none(), "{hit}");
    assert!(hit.get("updated_at").is_none() && hit.get("timestamp").is_none(), "{hit}");
    let mut other = semantic_fixture();
    other.sessions.push(vec![json!({"type":"session","session_id":"two","cwd":"/mine"})]);
    let missing = reader::read_item_projected(&mut other, args("two", "call-a"), &projector).unwrap_err();
    assert_eq!(
        missing.chain().find_map(|cause| cause.downcast_ref::<mare_shared::errors::Coded>()).unwrap().code,
        "session.unknown_item"
    );
    let event_token =
        encode(&Position { generation: digest(&semantic_fixture().sessions[0]), session_id: "one".into(), seq: 1 });
    let error =
        reader::read_item_projected(&mut semantic_fixture(), args("one", &event_token), &projector).unwrap_err();
    assert_eq!(
        error.chain().find_map(|cause| cause.downcast_ref::<mare_shared::errors::Coded>()).unwrap().code,
        "session.unknown_item"
    );
}

#[test]
fn search_pages_use_requested_limit_and_bind_cursor_to_query() {
    // Search pages use requested limit and bind cursor to query.
    let mut cursor = None;
    let mut seqs = Vec::new();
    loop {
        let args: Search = serde_json::from_value(json!({
            "query": "needle", "session_id": "one", "cursor": cursor, "limit": 7, "step_bound": true,
        }))
        .unwrap();
        let page = search(fixture(), args, true).unwrap();
        assert!(page.get("hits").is_none() && page.get("cursor").is_none(), "{page}");
        let data = page["data"].as_array().unwrap();
        assert!(data.len() <= 7);
        seqs.extend(data.iter().map(|item| item["position"]["seq"].as_u64().unwrap()));
        cursor = page.get("next_cursor").map(|value| value.as_str().unwrap().to_owned());
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seqs.len(), 25);
    assert_eq!(seqs.iter().copied().collect::<HashSet<_>>().len(), 25);

    let first: Search = serde_json::from_value(json!({
        "query": "needle", "session_id": "one", "limit": 2, "step_bound": true,
    }))
    .unwrap();
    let first = search(fixture(), first, true).unwrap();
    let token = first["next_cursor"].as_str().unwrap();
    let resized: Search = serde_json::from_value(json!({
        "query": "needle", "session_id": "one", "limit": 3, "step_bound": true, "cursor": token,
    }))
    .unwrap();
    assert_eq!(search(fixture(), resized, true).unwrap()["data"].as_array().unwrap().len(), 3);
    let changed: Search = serde_json::from_value(json!({
        "query": "needle", "session_id": "one", "limit": 3, "step_bound": false, "cursor": token,
    }))
    .unwrap();
    assert!(search(fixture(), changed, true).unwrap_err().to_string().contains("stale"));
    let zero: Search = serde_json::from_value(json!({ "query": "needle", "session_id": "one", "limit": 0 })).unwrap();
    let error = search(fixture(), zero, true).unwrap_err();
    assert_eq!(
        error.chain().find_map(|cause| cause.downcast_ref::<mare_shared::errors::Coded>()).unwrap().code,
        "invalid_arguments"
    );
}

#[test]
fn history_cursor_cannot_cross_workspace_or_live_session_epoch() {
    let root = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let mut scopes = Vec::new();
    for cwd in ["/aaa", "/bbb"] {
        let dir = root.path().join(&cwd[1..]);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("same.jsonl");
        let mut lines = vec![
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"same","cwd":cwd}).to_string(),
        ];
        lines.extend((0..5).map(|i| {
            json!({"type":"message","entry_id":format!("m{i}"),"message":{"role":"user","content":"needle"}})
                .to_string()
        }));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            )
            .unwrap();
        scopes.push(SourceScope { cwd: cwd.into(), owner: "host".into(), dir, policy: None });
    }
    let run = |scope: &SourceScope, args: Search, live: &[LiveSessionInput]| {
        let predicate = args.filters.clone().unwrap_or_default();
        let (eligible, headers, disk, incomplete) =
            build_eligible_sources(scope, &cancel, &args, true, live, &predicate).unwrap();
        search_sources(
            eligible,
            &headers,
            &disk,
            incomplete,
            args,
            true,
            false,
            &scope.owner,
            &cancel,
            &crate::projection::JsonProjector::default(),
        )
    };
    let args = Search { query: "needle".into(), session_id: Some("same".into()), limit: Some(2), ..Default::default() };
    let first = run(&scopes[0], args.clone_with_cursor(None), &[]).unwrap();
    let token = first["next_cursor"].as_str().unwrap().to_owned();
    assert!(run(&scopes[1], args.clone_with_cursor(Some(token)), &[]).unwrap_err().to_string().contains("stale"));

    let entries = fixture().sessions.remove(0);
    let live_scope =
        SourceScope { cwd: "/mine".into(), owner: "host".into(), dir: root.path().join("empty"), policy: None };
    std::fs::create_dir(&live_scope.dir).unwrap();
    let live = |epoch| {
        serde_json::from_value::<LiveSessionInput>(json!({
            "path": live_scope.dir.join("live.jsonl"), "entries": entries, "session_epoch": epoch,
        }))
        .unwrap()
    };
    let live_args =
        Search { query: "needle".into(), session_id: Some("one".into()), limit: Some(2), ..Default::default() };
    let first = run(&live_scope, live_args.clone_with_cursor(None), &[live("epoch-a")]).unwrap();
    let token = first["next_cursor"].as_str().unwrap().to_owned();
    assert!(
        run(&live_scope, live_args.clone_with_cursor(Some(token)), &[live("epoch-b")])
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

#[test]
fn search_indexes_forked_history_only_under_its_source_session() {
    let parent = vec![
        json!({"type":"session","session_id":"parent","cwd":"/mine"}),
        json!({"type":"message","entry_id":"p1","message":{"role":"user","content":"inheriteduniquetext"}}),
    ];
    let child = vec![
        json!({"type":"session","session_id":"child","cwd":"/mine","parent_session_id":"parent"}),
        json!({"type":"custom","kind":"session/fork-origin","provenance":"session-host","data":{"source_session_id":"parent","source_leaf_entry_id":"p1","inherited_through_entry_id":"c1"}}),
        json!({"type":"message","entry_id":"c1","message":{"role":"user","content":"inheriteduniquetext"}}),
        json!({"type":"message","entry_id":"c2","message":{"role":"user","content":"childuniquetext"}}),
    ];
    let grandchild = vec![
        json!({"type":"session","session_id":"grandchild","cwd":"/mine","parent_session_id":"child"}),
        json!({"type":"custom","kind":"session/fork-origin","provenance":"session-host","data":{"source_session_id":"child","source_leaf_entry_id":"c2","inherited_through_entry_id":"g2"}}),
        json!({"type":"message","entry_id":"g1","message":{"role":"user","content":"inheriteduniquetext"}}),
        json!({"type":"message","entry_id":"g2","message":{"role":"user","content":"childuniquetext"}}),
        json!({"type":"message","entry_id":"g3","message":{"role":"user","content":"grandchilduniquetext"}}),
    ];
    let corpus = || Corpus {
        sessions: vec![parent.clone(), child.clone(), grandchild.clone()],
        live_ids: HashSet::new(),
        incomplete: false,
    };
    let inherited = search(corpus(), search_args("inheriteduniquetext", None, None), false).unwrap();
    let inherited_hits = inherited["data"].as_array().unwrap();
    assert_eq!(inherited_hits.len(), 1, "inherited content must not be indexed twice: {inherited}");
    assert_eq!(inherited_hits[0]["session_id"], "parent");
    let own = search(corpus(), search_args("childuniquetext", None, None), false).unwrap();
    assert_eq!(own["data"].as_array().unwrap().len(), 1, "nested fork must not reindex child history: {own}");
    assert_eq!(own["data"][0]["session_id"], "child");
    let newest = search(corpus(), search_args("grandchilduniquetext", None, None), false).unwrap();
    assert_eq!(newest["data"][0]["session_id"], "grandchild");
    let mut only_child = corpus();
    only_child.sessions.remove(0);
    assert!(
        search(only_child, search_args("inheriteduniquetext", None, None), false).unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty(),
        "removing the source must not reindex the copied prefix"
    );
    let grandchild_only = Corpus { sessions: vec![grandchild], live_ids: HashSet::new(), incomplete: false };
    assert!(
        search(grandchild_only, search_args("childuniquetext", None, None), false).unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty(),
        "removing the child source must not reindex its inherited text in the grandchild"
    );
}

#[test]
fn search_skips_corrupt_or_untrusted_fork_origins_without_losing_healthy_results() {
    let healthy = vec![
        json!({"type":"session","session_id":"healthy","cwd":"/mine"}),
        json!({"type":"message","entry_id":"p1","message":{"role":"user","content":"needle in healthy session"}}),
    ];
    let damaged = vec![
        json!({"type":"session","session_id":"damaged","cwd":"/mine","parent_session_id":"healthy"}),
        json!({"type":"custom","kind":"session/fork-origin","provenance":"session-host","data":{"source_session_id":"healthy","source_leaf_entry_id":"p1","inherited_through_entry_id":"missing"}}),
        json!({"type":"message","entry_id":"d1","message":{"role":"user","content":"needle in damaged session"}}),
    ];
    let forged = vec![
        json!({"type":"session","session_id":"forged","cwd":"/mine","parent_session_id":"healthy"}),
        json!({"type":"custom","kind":"session/fork-origin","provenance":"plugin","data":{"source_session_id":"healthy","source_leaf_entry_id":"p1","inherited_through_entry_id":"f1"}}),
        json!({"type":"message","entry_id":"f1","message":{"role":"user","content":"inherited"}}),
        json!({"type":"message","entry_id":"f2","message":{"role":"user","content":"needle in forged session"}}),
    ];
    for bad in [damaged, forged] {
        let c = Corpus { sessions: vec![healthy.clone(), bad], live_ids: HashSet::new(), incomplete: false };
        let result = search(c, search_args("needle", None, None), false).unwrap();
        assert_eq!(result["incomplete"], true, "{result}");
        assert_eq!(result["data"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(result["data"][0]["session_id"], "healthy", "{result}");
    }
}

#[test]
fn positions_cursors_neighbors_and_stale_rejection() {
    let first = search(fixture(), request(None), true).unwrap();
    assert_eq!(first["data"].as_array().unwrap().len(), 20);
    let next = first["next_cursor"].as_str().unwrap().to_owned();
    assert_eq!(search(fixture(), request(Some(next.clone())), true).unwrap()["data"].as_array().unwrap().len(), 5);
    let event_id = first["data"][1]["event_id"].as_str().unwrap().to_owned();
    let event = read(
        fixture(),
        Read { event_id: event_id.clone(), before: Some(1), after: Some(1), offset: None, max_bytes: None },
    )
    .unwrap();
    assert_eq!(event["seq"], 24);
    assert_eq!(event["neighbors"].as_array().unwrap().len(), 2);
    let mut changed = fixture();
    changed.sessions[0][1]["message"]["content"] = json!("changed");
    assert!(search(changed, request(Some(next)), true).unwrap_err().to_string().contains("stale"));
    let mut changed = fixture();
    changed.sessions[0][1]["message"]["content"] = json!("changed");
    assert!(
        read(changed, Read { event_id, before: None, after: None, offset: None, max_bytes: None })
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

#[test]
fn excluded_live_session_does_not_invalidate_history_positions() {
    let mut initial = fixture();
    initial.sessions.push(vec![json!({"type":"session","session_id":"live","cwd":"/mine"})]);
    let args = || Search {
        query: "needle".into(),
        filters: Some(Filters { exclude_session_id: Some("live".into()), ..Default::default() }),
        ..Default::default()
    };
    let old = search(initial, args(), false).unwrap();
    let mut changed = fixture();
    changed.sessions.push(vec![
        json!({"type":"session","session_id":"live","cwd":"/mine"}),
        json!({"type":"message","message":{"content":"new request"}}),
    ]);
    let new = search(changed, args(), false).unwrap();
    assert_eq!(old["corpus_revision"], new["corpus_revision"]);
    assert_eq!(old["data"][0]["event_id"], new["data"][0]["event_id"]);
}

#[test]
fn scope_without_exec_ctx_enforces_host_policy_and_poison_barrier() {
    let tmp = tempfile::tempdir().unwrap();
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    let proc_t = lua.create_table().unwrap();
    crate::ossandbox::install(&lua, &proc_t, tmp.path().to_string_lossy().into_owned()).unwrap();
    mare.set("proc", proc_t).unwrap();
    mare.set("cwd", tmp.path().to_string_lossy().into_owned()).unwrap();
    lua.globals().set("mare", mare).unwrap();

    // Without exec_ctx, scope() succeeds using host cwd + current sandbox policy.
    let s = scope(&lua).unwrap();
    assert_eq!(Path::new(&s.cwd), tmp.path());

    // When the active sandbox policy denies reading the sessions directory, scope() refuses.
    let pol = crate::ossandbox::Policy::from_lua(
        &lua,
        &mare_shared::lua::json_to_lua(
            &lua,
            &json!({
                "level": "read-only",
                "network": true,
                "deny_read": [s.dir.to_string_lossy()],
                "accept_partial": ["fs_deny_read"],
            }),
        )
        .unwrap(),
        tmp.path(),
    )
    .unwrap();
    crate::ossandbox::set_current(&lua, pol).unwrap();
    assert!(scope(&lua).is_err(), "scope() without exec_ctx must honor deny_read");

    // A poisoned session refuses scope() even without an active exec_ctx.
    crate::ossandbox::set_current(&lua, None).unwrap();
    crate::exec_gate::poison(&lua, "poisoned for test");
    assert!(scope(&lua).is_err(), "scope() without exec_ctx must check_unpoisoned");
}

#[test]
fn store_install_exposes_query_api_without_duplicate_spill() {
    let lua = Lua::new();
    let store = lua.create_table().unwrap();
    install(&lua, &store).unwrap();
    for key in [
        "search_sessions",
        "search_events",
        "search_items",
        "trace_session",
        "trace_event",
        "trace_item",
        "read_event",
        "read_item",
    ] {
        assert!(!store.get::<mlua::Value>(key).unwrap().is_nil(), "missing {key}");
    }
    assert!(store.get::<mlua::Value>("spill").unwrap().is_nil(), "mare.store.spill must not exist");
}

#[test]
fn literal_whitespace_query_preserves_original_excerpt() {
    let event = json!({"type":"message","message":{"content":"Before Mixed\n  Case 世界 After"}});
    let (excerpt, offset) = snippet(&event, &pattern("mixed case 世界").unwrap()).unwrap();
    assert!(excerpt.contains("Mixed\n  Case 世界"));
    assert_eq!(offset, 7);
    assert!(pattern("\0").is_err());
    assert!(pattern("  ").is_err());
}

#[test]
fn complete_catalogue_beyond_200_and_actionable_budget_errors() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..221 {
        let id = format!("s{n}");
        let body = if n == 220 { "needle" } else { "other" };
        let source = format!(
            "{}\n{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":id,"cwd":"/work"}),
            json!({"type":"message","entry_id":"m","message":{"role":"assistant","content":body}})
        );
        std::fs::write(dir.path().join(format!("{n:04}.jsonl")), source).unwrap();
    }
    let nested = dir.path().join("agents/run");
    std::fs::create_dir_all(&nested).unwrap();
    let child = format!(
        "{}\n{}\n",
        json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"nested-child","parent_session_id":"s0","cwd":"/work"}),
        json!({"type":"message","message":{"role":"assistant","content":"child"}})
    );
    std::fs::write(nested.join("child.jsonl"), &child).unwrap();
    let attachments = dir.path().join("s0.attachments");
    std::fs::create_dir_all(&attachments).unwrap();
    std::fs::write(attachments.join("pretend.jsonl"), &child).unwrap();
    let scope = SourceScope { cwd: "/work".into(), owner: "host".into(), dir: dir.path().into(), policy: None };
    let cancel = CancellationToken::new();
    let view = corpus(&scope, &cancel, None, None).unwrap();
    assert_eq!(view.sessions.len(), 222);
    assert!(view.sessions.iter().any(|s| s[0]["session_id"] == "nested-child"));
    assert!(!view.incomplete);
    let hit = search(view, search_args("needle", None, None), false).unwrap();
    assert_eq!(hit["data"][0]["session_id"], "s220");
    let large = dir.path().join("large.jsonl");
    std::fs::write(
        &large,
        format!(
            "{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"large","cwd":"/work"})
        ),
    )
    .unwrap();
    std::fs::OpenOptions::new().write(true).open(&large).unwrap().set_len(mare_store::bounded::MAX_BYTES + 1).unwrap();
    assert!(corpus(&scope, &cancel, None, None).err().unwrap().to_string().contains("QUERY_BUDGET_EXCEEDED"));
    let filtered = Filters { session_ids: Some(vec!["s220".into()]), ..Default::default() };
    assert_eq!(corpus(&scope, &cancel, None, Some(&filtered)).unwrap().sessions.len(), 1);
}

#[test]
fn text_only() {
    assert!(snippet(&json!({"type":"name","name":"needle"}), &pattern("needle").unwrap()).is_none());
    assert!(snippet(&fixture().sessions[0][1], &pattern("世界").unwrap()).is_some());
}

#[test]
fn surface_filter_event_trace_and_chunked_oversize_read() {
    let big = "Z".repeat(70_000);
    let entries = vec![
        json!({"type":"session","version":mare_store::CURRENT_VERSION,"cwd":"/mine","session_id":"surf","created_at_ms":100}),
        json!({"type":"message","entry_id":"m1","created_at_ms":110,"message":{"role":"user","content":"shadow needle"}}),
        json!({"type":"compaction","entry_id":"cmp1","parent_entry_id":"m1","created_at_ms":120,"summary":"compacted needle"}),
        json!({"type":"message","entry_id":"m2","parent_entry_id":"cmp1","created_at_ms":130,"message":{"role":"assistant","content":format!("current needle {big}")}}),
    ];
    let c = Corpus { sessions: vec![entries], live_ids: HashSet::new(), incomplete: false };
    let shadowed = search(
        Corpus { sessions: c.sessions.clone(), live_ids: HashSet::new(), incomplete: false },
        Search {
            query: "needle".into(),
            session_id: Some("surf".into()),
            event_filters: Some(EventFilters { surfaces: Some(vec!["shadowed".into()]), ..Default::default() }),
            ..Default::default()
        },
        true,
    )
    .unwrap();
    assert_eq!(shadowed["data"].as_array().unwrap().len(), 1);
    assert_eq!(shadowed["data"][0]["position"]["seq"], 1);
    assert_eq!(shadowed["data"][0]["surface"], "shadowed");

    let traced = trace_event_in_corpus(
        Corpus { sessions: c.sessions.clone(), live_ids: HashSet::new(), incomplete: false },
        "surf",
        1,
        None,
    )
    .unwrap();
    assert_eq!(traced["surface"], "shadowed");
    assert_eq!(traced["replaced_by"], 2);
    assert_eq!(traced["replacement_chain"], json!([2]));
    assert_eq!(traced["derived_event_seqs"], json!([2]));

    // > 64 KiB read refuses inline without chunking, succeeds via chunked reads
    let cur = search(
        Corpus { sessions: c.sessions.clone(), live_ids: HashSet::new(), incomplete: false },
        Search {
            query: "needle".into(),
            session_id: Some("surf".into()),
            event_filters: Some(EventFilters { surfaces: Some(vec!["current".into()]), ..Default::default() }),
            ..Default::default()
        },
        true,
    )
    .unwrap();
    let big_eid = cur["data"].as_array().unwrap().iter().find(|h| h["position"]["seq"] == 3).unwrap()["event_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        read(
            Corpus { sessions: c.sessions.clone(), live_ids: HashSet::new(), incomplete: false },
            Read { event_id: big_eid.clone(), before: Some(1), after: None, offset: None, max_bytes: None },
        )
        .unwrap_err()
        .to_string()
        .contains("QUERY_EVENT_TOO_LARGE")
    );
    let mut off = 0usize;
    let mut buf = String::new();
    loop {
        let chunk = read(
            Corpus { sessions: c.sessions.clone(), live_ids: HashSet::new(), incomplete: false },
            Read {
                event_id: big_eid.clone(),
                before: Some(1),
                after: None,
                offset: Some(off),
                max_bytes: Some(65_536),
            },
        )
        .unwrap();
        buf.push_str(chunk["chunk"].as_str().unwrap());
        if let Some(next) = chunk["next_offset"].as_u64() {
            off = next as usize;
        } else {
            break;
        }
    }
    let parsed: Value = serde_json::from_str(&buf).unwrap();
    assert!(parsed["message"]["content"].as_str().unwrap().contains(&big));
}

#[test]
fn title_search_updated_at_sort_gz_and_over_32mib_corpus() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    // 6 sessions of ~6 MiB each (~36 MiB total > 32 MiB), target in the 6th compressed rollout!
    let filler = "x".repeat(600_000);
    for n in 0..5 {
        let p = dir.path().join(format!("s{n:02}.jsonl"));
        let mut content = format!(
            "{}\n{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":format!("bulk-{n}"),"cwd":"/work","created_at_ms":100 + n}),
            json!({"type":"name","name":format!("Bulk {n}")})
        );
        for m in 0..10 {
            use std::fmt::Write as _;
            writeln!(
                content,
                "{}",
                json!({"type":"message","entry_id":format!("m{m}"),"created_at_ms":200 + n,"message":{"role":"assistant","content":filler}})
            )
            .unwrap();
        }
        std::fs::write(p, content).unwrap();
    }
    let gz_path = dir.path().join("z-tail.jsonl.gz");
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(
        format!(
            "{}\n{}\n{}\n",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"tail-gz","cwd":"/work","created_at_ms":50}),
            json!({"type":"name","name":"Alpha Roadmap"}),
            json!({"type":"message","entry_id":"t1","created_at_ms":999,"message":{"role":"assistant","content":"tail_gz_needle"}})
        )
        .as_bytes(),
    )
    .unwrap();
    std::fs::write(&gz_path, enc.finish().unwrap()).unwrap();

    let scope = SourceScope { cwd: "/work".into(), owner: "host".into(), dir: dir.path().into(), policy: None };
    let cancel = CancellationToken::new();

    // Streamed batched search crosses 32 MiB and reaches z-tail.jsonl.gz via cursor
    let args = Search { query: "tail_gz_needle".into(), ..Default::default() };
    let mut cursor = None;
    let mut found = Vec::new();
    loop {
        let req = args.clone_with_cursor(cursor);
        let predicate = req.filters.clone().unwrap_or_default();
        let (eligible, headers, disk_sources, incomplete) =
            build_eligible_sources(&scope, &cancel, &req, false, &[], &predicate).unwrap();
        let page = search_sources(
            eligible,
            &headers,
            &disk_sources,
            incomplete,
            req,
            false,
            false,
            &scope.owner,
            &cancel,
            &crate::projection::JsonProjector::default(),
        )
        .unwrap();
        found.extend(page["data"].as_array().unwrap().iter().cloned());
        if let Some(c) = page["next_cursor"].as_str() {
            cursor = Some(c.to_owned());
        } else {
            break;
        }
    }
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["session_id"], "tail-gz");

    // Title-only search + updated_at_ms asc/desc
    let title_req = Search {
        title_query: Some("roadmap".into()),
        sort_key: Some("updated_at_ms".into()),
        sort_direction: Some("asc".into()),
        ..Default::default()
    };
    let predicate = Filters::default();
    let (eligible, headers, disk_sources, incomplete) =
        build_eligible_sources(&scope, &cancel, &title_req, false, &[], &predicate).unwrap();
    let page = search_sources(
        eligible,
        &headers,
        &disk_sources,
        incomplete,
        title_req,
        false,
        false,
        &scope.owner,
        &cancel,
        &crate::projection::JsonProjector::default(),
    )
    .unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["title"], "Alpha Roadmap");
    assert_eq!(page["data"][0]["updated_at_ms"], 999);
    assert_eq!(page["data"][0]["session_created_at_ms"], 50);
    assert_eq!(page["data"][0]["matched_at_ms"], 999);
    assert!(page["corpus_revision"].as_str().is_some_and(|s| !s.is_empty()));

    // Non-integer ISO timestamp filters fail with `invalid_arguments` including the field path.
    let lua = Lua::new();
    let bad_val =
        mare_shared::lua::json_to_lua(&lua, &json!({ "filters": { "created_from_ms": "2026-01-01T00:00:00Z" } }))
            .unwrap();
    let err = super::parse_args::<Search>(&lua, bad_val).err().expect("expected invalid_arguments error").to_string();
    assert!(err.contains("filters.created_from_ms"), "expected field path in invalid_arguments error, got: {err}");
}

#[test]
fn live_step_boundary_and_deny_read_guard() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("live.jsonl");
    std::fs::write(&p, "").unwrap();
    let lua = Lua::new();
    let spec = lua.create_table().unwrap();
    spec.set("level", "read-only").unwrap();
    spec.set("deny_read", vec![p.to_string_lossy().into_owned()]).unwrap();
    spec.set("accept_partial", vec!["fs_deny_read"]).unwrap();
    let policy = crate::ossandbox::Policy::from_lua(&lua, &mlua::Value::Table(spec), dir.path()).unwrap();
    let scope = SourceScope { cwd: "/work".into(), owner: "host".into(), dir: dir.path().into(), policy };
    let live = vec![LiveSessionInput {
        path: p.to_string_lossy().into_owned(),
        session_epoch: "epoch-1".into(),
        entries: vec![
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"live-1","cwd":"/work"}),
            json!({"type":"message","entry_id":"m1","message":{"role":"user","content":"old_step_needle"}}),
            json!({"type":"message","entry_id":"m2","message":{"role":"assistant","content":"current_step_needle"}}),
        ],
        step_boundary: Some(2),
    }];
    // Denied path hides live session completely!
    assert!(validate_live(&scope, &live, true).is_empty());

    let open_scope = SourceScope { cwd: "/work".into(), owner: "host".into(), dir: dir.path().into(), policy: None };
    let cancel = CancellationToken::new();
    let c = corpus_with_live(&open_scope, &cancel, Some("live-1"), None, &live).unwrap();
    assert_eq!(c.sessions[0].len(), 2); // clamped to step_boundary=2 (header + m1)
    let res = search(
        c,
        Search { query: "current_step_needle".into(), session_id: Some("live-1".into()), ..Default::default() },
        true,
    )
    .unwrap();
    assert!(res["data"].as_array().unwrap().is_empty());
    let c2 = corpus_with_live(&open_scope, &cancel, Some("live-1"), None, &live).unwrap();
    let res2 = search(
        c2,
        Search { query: "old_step_needle".into(), session_id: Some("live-1".into()), ..Default::default() },
        true,
    )
    .unwrap();
    assert_eq!(res2["data"].as_array().unwrap().len(), 1);
    assert_eq!(res2["data"][0]["availability"], "live");
}

#[tokio::test]
async fn search_cursor_binds_caller_owner_under_identical_policy() {
    let dir = tempfile::tempdir().unwrap();
    for i in 1..=3 {
        let p = dir.path().join(format!("2026-01-0{i}T00-00-00Z_s{i}.jsonl"));
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n",
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":format!("s{i}"),"cwd":"/work","created_at_ms":1000 + i}),
                json!({"type":"message","entry_id":format!("m{i}"),"created_at_ms":1010 + i,"message":{"role":"user","content":format!("owner_needle {i}")}}),
            ),
        )
        .unwrap();
    }
    let cancel = CancellationToken::new();
    let run_as = |owner: &str, cursor: Option<String>| {
        let scope = SourceScope { cwd: "/work".into(), owner: owner.into(), dir: dir.path().into(), policy: None };
        let args = Search { query: "owner_needle".into(), limit: Some(2), cursor, ..Default::default() };
        let predicate = args.filters.clone().unwrap_or_default();
        let (eligible, headers, disk, incomplete) =
            build_eligible_sources(&scope, &cancel, &args, false, &[], &predicate).unwrap();
        search_sources(
            eligible,
            &headers,
            &disk,
            incomplete,
            args,
            false,
            false,
            &scope.owner,
            &cancel,
            &crate::projection::JsonProjector::default(),
        )
    };
    let page1 = run_as("rpc:peer:1", None).unwrap();
    let token = page1["next_cursor"].as_str().unwrap().to_owned();
    let stolen = run_as("rpc:peer:2", Some(token.clone())).unwrap_err();
    assert!(stolen.to_string().contains("stale"), "{stolen}");
    let own = run_as("rpc:peer:1", Some(token.clone())).unwrap();
    assert_eq!(own["data"].as_array().unwrap().len(), 1);

    let scoped = mare_shared::txn_scope::scope_caller_owner(Some("rpc:peer:7".into()), async {
        mare_shared::txn_scope::current_caller_owner()
    })
    .await;
    assert_eq!(scoped.as_deref(), Some("rpc:peer:7"));
}

#[test]
fn semantic_item_search_filters_by_semantic_event_types() {
    // Semantic item search filters by semantic event types.
    let tool_args = Search {
        query: "needle".into(),
        session_id: Some("one".into()),
        event_filters: Some(EventFilters { event_types: Some(vec!["tool_call".into()]), ..Default::default() }),
        ..Default::default()
    };
    let tool_res = run_search(semantic_fixture(), tool_args, true, true, &CancellationToken::new()).unwrap();
    let tool_ids: HashSet<_> = tool_res["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            assert_eq!(item["type"], "tool_call", "{item}");
            assert!(item.get("call").is_none(), "tool_call item must not include legacy call object: {item}");
            assert_eq!(item["name"], "read", "{item}");
            assert_eq!(item["arguments"]["path"], "needle", "{item}");
            item["item_id"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(tool_ids, HashSet::from(["call-a".to_owned(), "call-b".to_owned()]));

    let msg_args = Search {
        query: "needle".into(),
        session_id: Some("one".into()),
        event_filters: Some(EventFilters { event_types: Some(vec!["assistant_message".into()]), ..Default::default() }),
        ..Default::default()
    };
    let msg_res = run_search(semantic_fixture(), msg_args, true, true, &CancellationToken::new()).unwrap();
    let msg_ids: Vec<_> =
        msg_res["data"].as_array().unwrap().iter().map(|item| item["item_id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(msg_ids, vec!["assistant".to_owned()]);
}
