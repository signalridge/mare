use super::*;
use serde_json::json;

fn request(revision: u64, value: Json) -> StateCompareExchangeRequest {
    StateCompareExchangeRequest { namespace: "example".into(), expected_revision: revision, value, delete: false }
}
fn ok(result: StateCompareExchangeResult) -> StateSnapshot {
    match result {
        StateCompareExchangeResult::Ok { snapshot } => snapshot,
        other => panic!("{other:?}"),
    }
}

#[test]
fn isolated_namespace_preserves_json_and_delete_tombstone_prevents_aba() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("namespace.json");
    let legacy = dir.path().join("state.json");
    let original = br#"{"unrelated":null,"empty":{},"list":[]}"#;
    std::fs::write(&legacy, original).unwrap();
    let value = json!({"null":null,"object":{},"array":[],"nested":{"x":[null,{},[]]}});
    let first = ok(exchange_at(&file, request(0, value.clone()), CancellationToken::new()));
    assert_eq!(first.revision, 1);
    assert_eq!(first.value, value);
    assert_eq!(read_at(&file, "example").unwrap(), first);
    assert_eq!(std::fs::read(&legacy).unwrap(), original);
    let mut delete = request(1, Json::Null);
    delete.delete = true;
    let deleted = ok(exchange_at(&file, delete, CancellationToken::new()));
    assert!(!deleted.exists && deleted.revision == 2 && deleted.value.is_null());
    assert!(
        matches!(exchange_at(&file,request(0,json!("stale")),CancellationToken::new()),StateCompareExchangeResult::Conflict{snapshot} if snapshot.revision==2)
    );
    assert_eq!(ok(exchange_at(&file, request(2, json!("new")), CancellationToken::new())).revision, 3);
}

#[test]
fn simultaneous_compare_exchange_has_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("namespace.json");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads = (0..8)
        .map(|n| {
            let file = file.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                exchange_at(&file, request(0, json!({"writer":n})), CancellationToken::new())
            })
        })
        .collect::<Vec<_>>();
    let mut wins = 0;
    let mut conflicts = 0;
    for thread in threads {
        match thread.join().unwrap() {
            StateCompareExchangeResult::Ok { snapshot } => {
                wins += 1;
                assert_eq!(snapshot.revision, 1);
            }
            StateCompareExchangeResult::Conflict { snapshot } => {
                conflicts += 1;
                assert_eq!(snapshot.revision, 1);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((wins, conflicts), (1, 7));
}

#[test]
fn corruption_and_unknown_versions_are_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("namespace.json");
    for bytes in [
        "",
        "{broken",
        r#"{"version":2}"#,
        r#"{"version":1,"namespace":"other","revision":1,"exists":true,"value":{}}"#,
    ] {
        std::fs::write(&file, bytes).unwrap();
        assert!(matches!(
            exchange_at(&file, request(0, json!({})), CancellationToken::new()),
            StateCompareExchangeResult::Rejected { .. }
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), bytes);
    }
}

#[tokio::test]
async fn cancelling_or_dropping_a_waiting_operation_cannot_publish() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("namespace.json");
    let held = acquire(&file, &CancellationToken::new()).unwrap();
    let token = CancellationToken::new();
    let child = token.clone();
    let path = file.clone();
    let pending = tokio::spawn(async move { execute(path, request(0, json!(1)), child).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    token.cancel();
    assert!(matches!(
        pending.await.unwrap(),
        StateCompareExchangeResult::Rejected { error: StateFailure { code: StateErrorCode::Cancelled, .. } }
    ));
    let token = CancellationToken::new();
    let child = token.clone();
    let path = file.clone();
    let pending = tokio::spawn(async move { execute(path, request(0, json!(2)), child).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(token.is_cancelled());
    drop(held);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!file.exists());
}

#[test]
fn final_atomic_checkpoint_preserves_old_value_and_cleans_temp() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("namespace.json");
    std::fs::write(&file, "old").unwrap();
    let result =
        mare_shared::paths::write_atomic_checked(&file, b"new", mare_shared::paths::WriteMode::Private, || {
            Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancel"))
        });
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn namespace_names_and_revision_exhaustion_fail_closed() {
    for namespace in ["", "../escape", ".hidden", "a/b", "a\\b", "C:", "name "] {
        assert!(path(namespace).is_err(), "{namespace}");
    }
    assert!(path("CON").unwrap().file_name().unwrap().to_string_lossy().starts_with("ns-"));
    assert_ne!(path("Case").unwrap(), path("case").unwrap());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("namespace.json");
    let entry =
        Envelope { version: 1, namespace: "example".into(), revision: MAX_REVISION, exists: true, value: json!({}) };
    std::fs::write(&file, serde_json::to_vec(&entry).unwrap()).unwrap();
    assert!(matches!(
        exchange_at(&file, request(MAX_REVISION, json!("x")), CancellationToken::new()),
        StateCompareExchangeResult::Rejected { error: StateFailure { code: StateErrorCode::RevisionExhausted, .. } }
    ));
}
