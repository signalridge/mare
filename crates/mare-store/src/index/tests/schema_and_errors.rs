use super::*;

#[test]
fn trust_project_root_skips_empty_git_directory() {
    let dir = OwnedTempDir::new();
    let repo = dir.path().join("repo");
    let child = repo.join("nested");
    std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
    std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(child.join(".git")).unwrap();
    assert_eq!(project_root(&child.to_string_lossy()), repo.to_string_lossy());
}

#[test]
fn a_schema_v3_index_is_rebuilt_with_physical_file_identity() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let session = root.join("--w--/session.jsonl");
    append(&session, &[header("session", "/w"), user("1", "body", 1)]);
    let c = Connection::open(root.join(DB_NAME)).unwrap();
    c.execute_batch("CREATE TABLE sessions (id, path, indexed_len, indexed_mtime); PRAGMA user_version = 3;").unwrap();
    drop(c);

    let s = ready(root).unwrap();
    let mut ix = s.lock().unwrap();
    assert!(matches!(ix.status(), Status::Rebuilt(_)));
    assert!(ix.take_notice().unwrap().contains("schema version 3"));
    let rows = all(&ix);
    assert_eq!(ids(&rows), ["session"]);
    let stamp = crate::stamp(&session).unwrap();
    assert_eq!((rows[0].indexed_dev as u64, rows[0].indexed_ino as u64), (stamp.dev, stamp.ino));
}

#[test]
fn a_schema_v4_index_is_rebuilt_with_rollback_revision_watermarks() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let session = root.join("--w--/session.jsonl");
    append(&session, &[header("session", "/w"), user("1", "body", 1)]);
    let c = Connection::open(root.join(DB_NAME)).unwrap();
    c.execute_batch(
        "CREATE TABLE sessions (id, path, indexed_len, indexed_mtime, indexed_dev, indexed_ino); PRAGMA user_version = 4;",
    )
    .unwrap();
    drop(c);

    let s = ready(root).unwrap();
    let mut ix = s.lock().unwrap();
    assert!(matches!(ix.status(), Status::Rebuilt(_)));
    assert!(ix.take_notice().unwrap().contains("schema version 4"));
    let row = all(&ix).remove(0);
    let rollback = scan::rollback_revision(&session).unwrap();
    assert_eq!((row.indexed_lock_dev, row.indexed_lock_ino, row.indexed_lock_len), rollback);
}

#[test]
fn a_schema_v5_index_is_discarded_and_rebuilt_from_session_jsonl() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("session", "/header"), user("1", "body", 1)]);
    let mut index = Index::open(root).unwrap();
    index.ensure_built().unwrap();
    index.update(&path, Some("/writer-override")).unwrap();
    index.set_checkpoint(&path, &checkpoint(&path)).unwrap();
    assert!(index.checkpoint(&path).unwrap().is_some());
    drop(index);

    Connection::open(root.join(DB_NAME)).unwrap().execute_batch("PRAGMA user_version = 5;").unwrap();
    let mut upgraded = Index::open(root).unwrap();
    assert!(matches!(upgraded.status(), Status::Rebuilt(why) if why.contains("schema version 5")));
    assert!(all(&upgraded).is_empty(), "the old projection is discarded before rebuilding");
    assert!(upgraded.ensure_built().unwrap(), "the projection is rebuilt from session JSONL");
    let rows = all(&upgraded);
    assert_eq!(ids(&rows), ["session"]);
    assert_eq!(rows[0].cwd, "/header", "writer-only metadata is not migrated");
    assert_eq!(rows[0].message_count, 1);
    assert_eq!(rows[0].last_input.as_deref(), Some("body"));
    assert_eq!(upgraded.checkpoint(&path).unwrap(), None);
    assert_eq!(
        Connection::open(root.join(DB_NAME))
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
}

/// An index of an older schema (with the dropped `cost_usd` / `branch_count` columns) is set
/// aside and rebuilt from the files, with one notice naming the reason.
#[test]
fn a_schema_bump_rebuilds_the_index_with_one_notice() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let root = dir.path();
    append(&root.join("--w--/a.jsonl"), &[header("a", "/w"), user("1", "x", 1)]);
    let c = Connection::open(root.join(DB_NAME)).unwrap();
    c.execute_batch("CREATE TABLE sessions (id, cost_usd, branch_count); PRAGMA user_version = 1;").unwrap();
    drop(c);
    let s = ready(root).unwrap();
    let mut ix = s.lock().unwrap();
    assert!(matches!(ix.status(), Status::Rebuilt(_)));
    let why = ix.take_notice().expect("a notice");
    assert!(why.contains("schema version 1"), "{why}");
    assert_eq!(ix.take_notice(), None);
    assert_eq!(ids(&all(&ix)), ["a"]);
    let cols: Vec<String> = ix
        .conn
        .prepare("SELECT name FROM pragma_table_info('sessions')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert!(!cols.iter().any(|c| c == "cost_usd" || c == "branch_count"), "{cols:?}");
    drop(ix);
    // reopened: the current schema, nothing to report
    let again = Index::open(root).unwrap();
    assert_eq!(again.status(), &Status::Ok);
}

/// The project scope lists the sessions of every directory under the repository's root, and
/// only those; the cwd scope stays exact.
#[test]
fn project_scope_includes_subdirectories_of_the_repository() {
    let dir = OwnedTempDir::new();
    let repo = dir.path().join("repo");
    let sub = repo.join("crates/x");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
    std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let root = dir.path().join("sessions");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let (r, sb, o) = (s(&repo), s(&sub), s(&other));
    append(&root.join("--r--/a.jsonl"), &[header("a", &r), user("1", "x", 1)]);
    append(&root.join("--s--/b.jsonl"), &[header("b", &sb), user("1", "y", 2)]);
    append(&root.join("--o--/c.jsonl"), &[header("c", &o), user("1", "z", 3)]);
    let mut ix = Index::open(&root).unwrap();
    ix.ensure_built().unwrap();
    let list = |q: Query| ix.list(&Query { kinds: Kinds::Any, ..q }).unwrap().0;
    // asked from the subdirectory, the repository root's sessions come along
    let here = list(Query { project_root: Some(project_root(&sb)), ..Default::default() });
    assert_eq!(ids(&here), ["b", "a"]);
    assert_eq!(list(Query { cwd: Some(sb.clone()), ..Default::default() }).len(), 1);
    assert_eq!(ids(&list(Query { project_root: Some(project_root(&o)), ..Default::default() })), ["c"]);
}

#[test]
fn unknown_schema_is_discarded_and_rebuilt_from_sessions_without_backup() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    for version in [0, 1, 99] {
        let dir = OwnedTempDir::new();
        let root = dir.path();
        let session = root.join("--w--/s.jsonl");
        append(&session, &[header("s", "/w"), user("m", "hello", 1)]);
        let before = std::fs::read(&session).unwrap();
        let db = root.join(DB_NAME);
        let c = Connection::open(&db).unwrap();
        c.execute_batch(&format!("CREATE TABLE old (x); PRAGMA user_version = {version};")).unwrap();
        drop(c);
        let mut ix = Index::open(root).unwrap();
        ix.ensure_built().unwrap();
        assert_eq!(ids(&all(&ix)), ["s"]);
        assert_eq!(std::fs::read(&session).unwrap(), before);
        assert!(!std::fs::read_dir(root).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains(".bad-")));
    }
}

#[test]
fn incompatible_session_is_skipped_without_changing_any_session() {
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let good = root.join("--w--/a.jsonl");
    let bad = root.join("--w--/future.jsonl");
    append(&good, &[header("a", "/w")]);
    append(&bad, &[json!({"type":"session","version":99,"session_id":"future"})]);
    let good_before = std::fs::read(&good).unwrap();
    let bad_before = std::fs::read(&bad).unwrap();
    let mut ix = Index::open(root).unwrap();
    assert!(ix.ensure_built().unwrap());
    assert_eq!(ids(&all(&ix)), ["a"]);
    assert_eq!(ix.unsupported(), [norm(&bad)]);
    let (rows, _, warnings) = scan_list_with_unsupported(root, &Query::default()).unwrap();
    assert_eq!(ids(&rows), ["a"]);
    assert_eq!(warnings, [norm(&bad)]);
    assert_eq!(std::fs::read(&good).unwrap(), good_before);
    assert_eq!(std::fs::read(&bad).unwrap(), bad_before);
}

#[test]
fn reconcile_removes_stale_row_when_session_becomes_unsupported() {
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let good = root.join("--w--/good.jsonl");
    let changed = root.join("--w--/changed.jsonl");
    append(&good, &[header("good", "/w")]);
    append(&changed, &[header("changed", "/w")]);
    let index = Arc::new(Mutex::new(Index::open(root).unwrap()));
    index.lock().unwrap().ensure_built().unwrap();
    std::fs::write(&changed, "{\"type\":\"session\",\"version\":99,\"session_id\":\"changed\"}\n").unwrap();
    let original = std::fs::read(&changed).unwrap();
    let result = reconcile(&index).unwrap();
    assert_eq!(result.removed, 1);
    let index = index.lock().unwrap();
    assert_eq!(ids(&all(&index)), ["good"]);
    assert_eq!(index.unsupported(), [norm(&changed)]);
    assert_eq!(std::fs::read(&changed).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn rebuilding_retries_when_an_unsupported_path_is_atomically_replaced() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("old-id", "/old"), user("1", "old row", 1)]);
    let shared = ready(&root).unwrap();
    std::fs::write(&path, "{\"type\":\"session\",\"version\":99,\"session_id\":\"future\"}\n").unwrap();

    let replacement = path.with_extension("replacement");
    append(&replacement, &[header("new-id", "/new"), user("1", "replacement row", 2)]);
    let source = path.clone();
    after_next_build_scan(move || std::fs::rename(&replacement, &source).unwrap());

    let rebuilt = shared.lock().unwrap().rebuild().unwrap();
    assert_eq!(rebuilt, 1);
    let index = shared.lock().unwrap();
    let rows = all(&index);
    assert_eq!(ids(&rows), ["new-id"]);
    assert_eq!(rows[0].preview.as_deref(), Some("replacement row"));
    assert!(index.unsupported().is_empty());
}

#[cfg(unix)]
#[test]
fn reconcile_does_not_delete_a_new_row_after_an_unsupported_scan() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("old-id", "/old"), user("1", "old row", 1)]);
    let shared = ready(&root).unwrap();
    std::fs::write(&path, "{\"type\":\"session\",\"version\":99,\"session_id\":\"future\"}\n").unwrap();

    let replacement = path.with_extension("replacement");
    append(&replacement, &[header("new-id", "/new"), user("1", "replacement row", 2)]);
    let path_for_hook = path.clone();
    let shared_for_hook = shared.clone();
    after_next_unsupported_update(move || {
        std::fs::rename(&replacement, &path_for_hook).unwrap();
        shared_for_hook.lock().unwrap().update(&path_for_hook, None).unwrap();
    });

    let result = reconcile(&shared).unwrap();
    let index = shared.lock().unwrap();
    let rows = all(&index);
    assert_eq!(ids(&rows), ["new-id"]);
    assert_eq!(rows[0].preview.as_deref(), Some("replacement row"));
    assert!(index.unsupported().is_empty());
    assert_eq!(result.removed, 0, "the scan must not delete the replacement's row");
}

#[test]
fn listing_checks_a_stale_page_before_reconcile() {
    let dir = OwnedTempDir::new();
    let good = dir.path().join("--w--/good.jsonl");
    let changed = dir.path().join("--w--/changed.jsonl");
    append(&good, &[header("good", "/w")]);
    append(&changed, &[header("changed", "/w")]);
    let mut index = Index::open(dir.path()).unwrap();
    index.ensure_built().unwrap();
    std::fs::write(&changed, "{\"type\":\"session\",\"version\":99,\"session_id\":\"changed\"}\n").unwrap();
    let query = Query { kinds: Kinds::Any, ..Default::default() };
    let (rows, total) = index.list_checked(&query).unwrap();
    assert_eq!(ids(&rows), ["good"]);
    assert_eq!(total, 1);
    assert_eq!(index.unsupported(), [norm(&changed)]);
}

#[test]
fn a_live_index_guard_prevents_replacing_an_unsupported_schema() {
    // This test reopens an exclusive lock immediately after dropping the live connection. A
    // concurrent fork can briefly inherit the shared-lock descriptor between fork and exec, so
    // coordinate with the subprocess tests instead of serializing the whole test binary.
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let session = root.join("--w--/s.jsonl");
    append(&session, &[header("s", "/w")]);
    let ix = Index::open(root).unwrap();
    ix.conn.pragma_update(None, "user_version", 99).unwrap();
    let err = match Index::open(root) {
        Ok(_) => panic!("live index was replaced"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("in use") && err.contains("cannot rebuild"), "{err}");
    assert_eq!(ix.conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 99);
    drop(ix);
    let mut rebuilt = Index::open(root).unwrap();
    rebuilt.ensure_built().unwrap();
    assert_eq!(ids(&all(&rebuilt)), ["s"]);
}

#[test]
fn schema_version_committed_in_wal_is_detected() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let live = dir.path().join("live");
    let snapshot = dir.path().join("snapshot");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&snapshot).unwrap();
    let db = live.join(DB_NAME);
    let c = Connection::open(&db).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE x(a); PRAGMA user_version=99;")
        .unwrap();
    std::fs::copy(&db, snapshot.join(DB_NAME)).unwrap();
    std::fs::copy(live.join("index.sqlite-wal"), snapshot.join("index.sqlite-wal")).unwrap();
    drop(c);
    append(&snapshot.join("--w--/s.jsonl"), &[header("s", "/w")]);
    let mut ix = Index::open(&snapshot).unwrap();
    assert!(matches!(ix.status(), Status::Rebuilt(why) if why.contains("schema version 99")));
    ix.ensure_built().unwrap();
    assert_eq!(ids(&all(&ix)), ["s"]);
}

#[cfg(unix)]
#[test]
fn inaccessible_index_directory_reports_error_and_preserves_session() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    use std::os::unix::fs::PermissionsExt;
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let session = root.join("--w--/s.jsonl");
    append(&session, &[header("s", "/w")]);
    let before = std::fs::read(&session).unwrap();
    let db = root.join(DB_NAME);
    let c = Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE old(x); PRAGMA user_version=99;").unwrap();
    drop(c);
    std::fs::write(root.join("index.sqlite.lock"), "").unwrap();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = Index::open(root);
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = match result {
        Ok(_) => panic!("read-only index directory accepted"),
        Err(e) => e,
    };
    assert!(must_report_error(&error), "{error:#}");
    assert!(format!("{error:#}").contains("opening") || format!("{error:#}").contains("removing"), "{error:#}");
    assert_eq!(std::fs::read(&session).unwrap(), before);
}

#[test]
fn failing_to_remove_index_sidecar_is_reported_without_touching_files() {
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let db = root.join(DB_NAME);
    let session = root.join("s.jsonl");
    std::fs::write(&db, "derived index").unwrap();
    std::fs::write(&session, "session bytes").unwrap();
    std::fs::create_dir(root.join("index.sqlite-wal")).unwrap();
    let err = remove_index(&db).unwrap_err().to_string();
    assert!(err.contains("removing session index") && err.contains("index.sqlite-wal"), "{err}");
    assert_eq!(std::fs::read_to_string(&db).unwrap(), "derived index");
    assert_eq!(std::fs::read_to_string(&session).unwrap(), "session bytes");
}

#[cfg(unix)]
#[test]
fn listing_preserves_a_valid_replacement_in_the_unsupported_delete_window() {
    let dir = OwnedTempDir::new();
    let path = dir.path().join("--w--/session.jsonl");
    append(&path, &[header("same", "/w"), user("1", "valid", 1)]);
    let mut index = Index::open(dir.path()).unwrap();
    index.ensure_built().unwrap();
    index.set_checkpoint(&path, &checkpoint(&path)).unwrap();
    let before = all(&index);
    let bytes = std::fs::read(&path).unwrap();
    let replacement = path.with_extension("replacement");
    std::fs::rename(&path, &replacement).unwrap();
    append(&path, &[json!({"type":"session","version":99,"session_id":"future"})]);
    let source = path.clone();
    BEFORE_STALE_DELETE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(replacement, source).unwrap();
        }))
    });
    let (rows, total) = index.list_checked(&Query::default()).unwrap();
    assert_eq!(total, 1);
    assert_eq!(rows, before, "the restored source retains order and checkpoint");
    assert!(index.unsupported().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn reconcile_preserves_a_writer_in_the_missing_delete_window() {
    let dir = OwnedTempDir::new();
    let path = dir.path().join("--w--/session.jsonl");
    append(&path, &[header("same", "/w"), user("1", "valid", 1)]);
    let shared = ready(dir.path()).unwrap();
    let replacement = path.with_extension("replacement");
    std::fs::rename(&path, &replacement).unwrap();
    let source = path.clone();
    let other = shared.clone();
    let expected = Arc::new(Mutex::new(Vec::new()));
    let saved = expected.clone();
    BEFORE_STALE_DELETE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(replacement, &source).unwrap();
            append(&source, &[user("2", "writer update", 2)]);
            let mut index = other.lock().unwrap();
            index.update(&source, None).unwrap();
            index.set_checkpoint(&source, &checkpoint(&source)).unwrap();
            *saved.lock().unwrap() = all(&index);
        }))
    });
    let result = reconcile(&shared).unwrap();
    assert_eq!(result.removed, 0);
    assert_eq!(all(&shared.lock().unwrap()), *expected.lock().unwrap());
    assert!(shared.lock().unwrap().checkpoint(&path).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn stale_deletion_rolls_back_if_the_source_returns_after_sql_delete() {
    for unsupported in [false, true] {
        let dir = OwnedTempDir::new();
        let path = dir.path().join("--w--/session.jsonl");
        append(&path, &[header("same", "/w"), user("1", "valid", 1)]);
        let shared = ready(dir.path()).unwrap();
        shared.lock().unwrap().set_checkpoint(&path, &checkpoint(&path)).unwrap();
        let before = all(&shared.lock().unwrap());
        let replacement = path.with_extension("replacement");
        std::fs::rename(&path, &replacement).unwrap();
        if unsupported {
            append(&path, &[json!({"type":"session","version":99,"session_id":"future"})]);
        }
        let source = path.clone();
        AFTER_STALE_DELETE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || std::fs::rename(replacement, source).unwrap()));
        });
        if unsupported {
            assert_eq!(shared.lock().unwrap().list_checked(&Query::default()).unwrap(), (before.clone(), 1));
        } else {
            assert_eq!(reconcile(&shared).unwrap().removed, 0);
        }
        assert_eq!(all(&shared.lock().unwrap()), before);
        assert!(shared.lock().unwrap().unsupported().is_empty());
    }
}
