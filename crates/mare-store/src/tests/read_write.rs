use super::*;
use crate::session_file::FAIL_NEXT_SYNC;
use serde_json::json;

#[test]
fn fork_origin_record_round_trips_and_rejects_invalid_boundaries() {
    let record = json!({
        "type": "custom", "kind": "session/fork-origin", "data": {
            "source_session_id": "parent", "source_leaf_entry_id": "leaf", "inherited_through_entry_id": "seed"
        }
    });
    crate::record::validate(&record).unwrap();
    let typed: crate::record::Record = serde_json::from_value(record.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), record);
    for data in [
        json!({}),
        json!({"source_session_id":"", "source_leaf_entry_id":"leaf"}),
        json!({"source_session_id":"parent", "source_leaf_entry_id":""}),
        json!({"source_session_id":"parent", "source_leaf_entry_id":"leaf", "inherited_through_entry_id":""}),
        json!({"source_session_id":"parent", "source_leaf_entry_id":"leaf", "inherited_through_entry_id":42}),
    ] {
        let invalid = json!({"type":"custom", "kind":"session/fork-origin", "data":data});
        assert!(crate::record::validate(&invalid).is_err(), "invalid fork origin accepted: {invalid}");
    }
}

/// The M5 R2 decision's measurement (validation stays always on if it adds at most 5 % to an
/// append; measured ~32–35 % in release, so release builds gate it behind `MARE_VALIDATE_RECORDS=1`
/// while debug/test builds keep it on): the median append of a 1 MiB message record, which
/// validates, against the median validation of the same record alone. Run it on a release build
/// with `cargo-slot nextest run --cargo-profile release -p mare-store --run-ignored only --no-capture -E 'test(measure_)'`.
#[test]
#[ignore = "release-profile measurement"]
fn measure_one_mib_append_validation_overhead() {
    use std::time::{Duration, Instant};

    const SIZE: usize = 1024 * 1024;
    const ITERATIONS: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let mut file = SessionFile::open(&dir.path().join("measure.jsonl")).unwrap();
    file.append(&json!({"type":"session", "version":CURRENT_VERSION, "session_id":"s"}), true).unwrap();
    let mut message =
        json!({"type":"message", "entry_id":"m", "message":{"role":"user", "content":[{"type":"text", "text":""}]}});
    let overhead = serde_json::to_vec(&message).unwrap().len();
    message["message"]["content"][0]["text"] = json!("x".repeat(SIZE - overhead));
    assert_eq!(serde_json::to_vec(&message).unwrap().len(), SIZE);
    let load = || std::fs::read_to_string("/proc/loadavg").unwrap_or_default().trim().to_string();
    println!("load before: {}", load());
    let (mut appends, mut checks) = (Vec::with_capacity(ITERATIONS), Vec::with_capacity(ITERATIONS));
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        crate::record::validate(&message).unwrap();
        checks.push(start.elapsed());
        let start = Instant::now();
        file.append_batch(std::slice::from_ref(&message), false).unwrap();
        appends.push(start.elapsed());
    }
    fn median(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }
    let (append, check) = (median(&mut appends), median(&mut checks));
    println!(
        "1 MiB message, {ITERATIONS} iterations: append {append:?} (validation included), validation {check:?}, \
         overhead {:.1} %",
        check.as_secs_f64() / (append - check).as_secs_f64() * 100.0
    );
    println!("load after: {}", load());
}

#[test]
fn every_core_record_type_round_trips_and_a_bad_one_rejects_the_batch() {
    use crate::record::CoreRecordType::*;
    let kinds = crate::record::CoreRecordType::ALL;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    let mut file = SessionFile::open(&path).unwrap();
    let mut records: Vec<_> = kinds
        .iter()
        .copied()
        .map(|kind| match kind {
            Session => json!({"type":"session", "version":CURRENT_VERSION, "session_id":"s", "future":{"x":1}}),
            Message => json!({"type":"message", "entry_id":"m", "parent_entry_id":"m0", "created_at_ms":1000, "message":{"role":"assistant", "content":[{"type":"text", "text":"hello", "future":true}, {"type":"tool_call", "id":"c", "name":"tool", "arguments":{}}, {"type":"vendor_block", "text":42}], "usage":{"input":12, "output":3.5, "future":true}, "future":true}}),
            ToolState => json!({"type":"tool_state", "entry_id":"ts1", "state":"started", "tool_call_id":"c", "created_at_ms":1001}),
            State => json!({"type":"state", "state":"closed", "created_at_ms":1002}),
            Name => json!({"type":"name", "name":"title"}),
            Label => json!({"type":"label", "target_entry_id":"m", "label":"tag", "created_at_ms":1003}),
            Archive => json!({"type":"archive", "archived":true, "created_at_ms":1004}),
            Compaction => json!({"type":"compaction", "entry_id":"c", "first_kept_entry_id":"m", "created_at_ms":1005}),
            Custom => json!({"type":"custom", "kind":"agent/run-end", "created_at_ms":1006, "data":{"run_id":"r", "leaf_entry_id":"m"}}),
        })
        .collect();
    records.extend([
        json!({"type":"message", "entry_id":"m-str", "message":{"role":"user", "content":"plain"}}),
        json!({"type":"custom", "kind":"vendor/opaque", "data":42}),
        json!({"type":"extension", "arbitrary":[1,2,3]}),
    ]);
    for record in records.iter().take(kinds.len() + 1) {
        let typed: crate::record::Record = serde_json::from_value(record.clone()).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), *record, "typed records retain unknown fields");
    }
    let blocks_msg = serde_json::from_value::<crate::record::MessageRecord>(json!({
        "type":"message", "entry_id":"m2", "message":{"content":[{"type":"tool_call","id":"c","name":"t"},{"type":"text","text":"after"}]}
    }))
    .unwrap();
    assert_eq!(
        blocks_msg.message.as_ref().and_then(|m| m.content.as_ref()).and_then(|c| c.first_text()),
        Some("after")
    );
    let text_msg = serde_json::from_value::<crate::record::MessageRecord>(
        json!({"type":"message", "message":{"content":"plain"}}),
    )
    .unwrap();
    assert_eq!(text_msg.message.as_ref().and_then(|m| m.content.as_ref()).and_then(|c| c.first_text()), Some("plain"));
    assert!(serde_json::from_value::<crate::record::ToolState>(records[2].clone()).unwrap().is_started_for("c"));
    assert!(serde_json::from_value::<crate::record::State>(records[3].clone()).unwrap().is_closed());
    assert!(serde_json::from_value::<crate::record::Custom>(records[8].clone()).unwrap().is_run_end());
    file.append_batch(&records, false).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(read_entries(&path).unwrap(), records);
    let expected: String =
        records.iter().map(|record| format!("{}\n", serde_json::to_string(record).unwrap())).collect();
    assert_eq!(before, expected.as_bytes());

    let invalid = [json!({"type":"name", "name":"not written"}), json!({"type":"archive", "archived":"bad"})];
    let error = file.append_batch(&invalid, false).expect_err("invalid record must reject the whole batch").to_string();
    assert!(
        error.contains("session.invalid_record")
            && error.contains('1')
            && error.contains("archive")
            && error.contains("archived"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);

    for (record, field) in [
        (json!({"type":"session", "version":"bad"}), "version"),
        (json!({"type":"message", "message":{"role":42}}), "role"),
        (json!({"type":"message", "message":{"content":42}}), "content"),
        (json!({"type":"message", "message":{"content":{"bad":1}}}), "content"),
        (json!({"type":"message", "message":{"content":[42]}}), "content"),
        (json!({"type":"message", "message":{"content":[{"type":"text", "text":42}]}}), "text"),
        (json!({"type":"message", "message":{"content":[{"type":"tool_call", "id":42, "name":"tool"}]}}), "id"),
        (json!({"type":"tool_state", "tool_call_id":42}), "tool_call_id"),
        (json!({"type":"state", "state":42}), "state"),
        (json!({"type":"name", "name":42}), "name"),
        (json!({"type":"label", "label":42}), "label"),
        (json!({"type":"compaction", "first_kept_entry_id":42}), "first_kept_entry_id"),
        (json!({"type":"message", "created_at_ms":1.5}), "created_at_ms"),
        (json!({"type":"archive", "archived":true, "created_at_ms":"bad"}), "created_at_ms"),
        (json!({"type":"custom", "kind":"agent/run-end", "data":{"run_id":42}}), "run_id"),
        (json!({"type":"custom", "kind":"agent/run-end", "data":{"leaf_entry_id":42}}), "leaf_entry_id"),
    ] {
        let kind = record["type"].as_str().unwrap().to_string();
        let error = file.append(&record, false).expect_err("malformed core record").to_string();
        assert!(error.contains("session.invalid_record") && error.contains(&kind) && error.contains(field), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    drop(file);
    OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"type\":\"partial").unwrap();
    let torn = std::fs::read(&path).unwrap();
    let mut file = SessionFile::open(&path).unwrap();
    assert!(file.append_batch(&invalid, false).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), torn, "validation must precede torn-tail repair");
}

#[test]
fn empty_content_map_refusal_appends_and_scans() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj/filtered.jsonl");
    let mut file = SessionFile::open(&path).unwrap();
    let header =
        json!({"type":"session", "version":CURRENT_VERSION, "session_id":"s_filtered", "cwd":"/w", "created_at_ms":1});
    let filtered = json!({
        "type": "message",
        "entry_id": "m_filtered",
        "created_at_ms": 2,
        "message": {
            "role": "assistant",
            "model_id": "claude-sonnet-5-5",
            "provider_id": "anthropic",
            "content": {},
            "stop_reason": "filtered",
            "raw_stop_reason": "sensitive",
            "usage": { "input": 12, "output": 0 }
        }
    });
    file.append_batch(&[header.clone(), filtered.clone()], true).unwrap();
    assert_eq!(read_entries(&path).unwrap(), vec![header, filtered]);
    let (rows, total) = crate::index::scan_list(dir.path(), &crate::index::Query::default()).unwrap();
    assert_eq!(total, 1);
    let row = &rows[0];
    assert_eq!(row.message_count, 1);
    assert_eq!(row.tokens_in, 12);
    assert_eq!(row.tokens_out, 0);
    assert_eq!(row.model.as_deref(), Some("claude-sonnet-5-5"));
}

#[test]
fn roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a/b.jsonl");
    let mut f = SessionFile::open(&p).unwrap();
    let entries = vec![
        json!({"type":"session","session_id":"s1","cwd":"/x","version":3}),
        json!({"type":"message","entry_id":"m1","parent_entry_id":null,"message":{"role":"user","content":[{"type":"text","text":"你好\nworld"}]}}),
        json!({"type":"message","entry_id":"m2","parent_entry_id":"m1","message":{"role":"assistant","content":[]}}),
    ];
    for e in &entries {
        f.append(e, true).unwrap();
    }
    drop(f);
    // torn trailing write is tolerated
    std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{\"type\":\"mess").unwrap();
    let back = read_entries(&p).unwrap();
    assert_eq!(back, entries);
    assert_eq!(read_header(&p).unwrap()["session_id"], "s1");
    // reopen and append continues the same file
    let mut f = SessionFile::open(&p).unwrap();
    f.append(&json!({"type":"leaf","target_entry_id":"m1"}), false).unwrap();
    assert!(f.flush().unwrap());
    let back = read_entries(&p).unwrap();
    // the torn line is skipped; the new entry starts on its own line
    assert_eq!(&back[..3], &entries[..]);
    assert_eq!(back.len(), 4);
    assert_eq!(back[3]["type"], "leaf");
    let list = list_sessions(&dir.path().join("a"));
    assert_eq!(list.len(), 1);
}

#[test]
fn session_dirs_and_files_are_private() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(dir.path());
    let p = dir.path().join("proj/s.jsonl");
    let mut f = SessionFile::open(&p).unwrap();
    f.append(&json!({"type":"session","session_id":"s","version":CURRENT_VERSION}), true).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("proj")), 0o700);
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(&lock_path(&p)), 0o600);
    }
    #[cfg(windows)]
    for p in [dir.path().join("proj"), p.clone(), lock_path(&p)] {
        assert_eq!(mare_shared::private::check(&p), Ok(()));
    }
}

#[test]
fn lenient_read_reports_skipped_lines_and_the_intact_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    std::fs::write(
        &p,
        "{\"type\":\"session\",\"version\":3,\"session_id\":\"a\"}\n{\"id\":\"b\"}\nnot json\n{\"id\":\"c\"}\n",
    )
    .unwrap();
    let (entries, skipped, intact) = read_entries_lenient(&p).unwrap();
    // the view shows everything readable and says how much was skipped; a fork copies only
    // what precedes the first bad line
    assert_eq!((entries.len(), skipped, intact), (3, 1, 2));
}

#[test]
fn corrupt_middle_line_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    let body = format!(
        "{SESSION}{{\"type\":\"message\",\"entry_id\":\"m1\"}}\nnot json\n{{\"type\":\"message\",\"entry_id\":\"m2\"}}\n"
    );
    std::fs::write(&p, &body).unwrap();
    let err = SessionFile::open(&p).err().expect("not opened writable").to_string();
    assert!(err.contains(CORRUPT) && err.contains("line 3"), "{err}");
    assert!(read_entries(&p).unwrap_err().to_string().contains(CORRUPT));
    assert!(reverse::read_from_compaction(&p).err().unwrap().to_string().contains(CORRUPT));
    // the read-only view still shows what parses, and says how much it skipped
    let (entries, skipped, _) = read_entries_lenient(&p).unwrap();
    assert_eq!((entries.len(), skipped), (3, 1));
    assert_eq!(std::fs::read_to_string(&p).unwrap(), body, "bytes unchanged");
}

#[test]
fn torn_tail_is_ignored_by_readers_and_cut_before_the_next_append() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    // an unterminated final entry is torn even when it happens to parse
    for tail in ["{\"type\":\"mess", "{\"type\":\"message\",\"entry_id\":\"half\"}"] {
        let body = format!("{SESSION}{{\"type\":\"message\",\"entry_id\":\"m1\"}}\n{tail}");
        std::fs::write(&p, &body).unwrap();
        assert_eq!(read_entries(&p).unwrap().len(), 2, "torn final entry not counted");
        assert_eq!(check_file(&p).unwrap(), tail.len() as u64, "doctor sees the torn tail");
        assert_eq!(reverse::read_from_compaction(&p).unwrap().entries.len(), 2);
        let mut f = SessionFile::open(&p).unwrap();
        assert_eq!(f.stats().torn_bytes, tail.len() as u64);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body, "opening alone changes nothing");
        f.append(&json!({"type":"message","entry_id":"m2"}), true).unwrap();
        drop(f);
        let back = read_entries(&p).unwrap();
        assert_eq!(
            back.iter()
                .map(|e| e.get("session_id").or_else(|| e.get("entry_id")).unwrap().as_str().unwrap())
                .collect::<Vec<_>>(),
            ["s", "m1", "m2"]
        );
        assert_eq!(check_file(&p).unwrap(), 0, "repaired by the append");
    }
    // a bad complete line is damage, not a torn tail
    std::fs::write(&p, format!("{SESSION}not json\n")).unwrap();
    assert!(check_file(&p).unwrap_err().to_string().contains(CORRUPT));
}

#[test]
fn reads_never_modify_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("old.jsonl");
    // a torn tail: a writer would repair it — readers must not
    let body =
        "{\"type\":\"session\",\"session_id\":\"s\",\"version\":3}\n{\"type\":\"message\",\"entry_id\":\"m\"}\n{\"ty";
    std::fs::write(&p, body).unwrap();
    read_entries(&p).unwrap();
    read_entries_lenient(&p).unwrap();
    reverse::read_from_compaction(&p).unwrap();
    reverse::read_tail(&p, 5).unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
    assert!(!dir.path().join("old.jsonl.v1.bak").exists(), "no migration backup from a read");
}

#[test]
fn failed_batch_is_truncated_back_to_before_it() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    let mut f = SessionFile::open(&p).unwrap();
    f.append(&json!({"type":"session","session_id":"s","version":3}), true).unwrap();
    let before = std::fs::read(&p).unwrap();
    FAIL_NEXT_SYNC.with(|f| f.set(true));
    let batch = [json!({"type":"message","entry_id":"a"}), json!({"type":"message","entry_id":"b"})];
    assert!(f.append_batch(&batch, true).is_err());
    assert_eq!(std::fs::read(&p).unwrap(), before, "the failed batch left nothing behind");
    // the writer's bookkeeping matches the file: retrying the batch gives a consistent file
    f.append_batch(&batch, true).unwrap();
    let c = f.checkpoint().unwrap().unwrap();
    drop(f);
    assert_eq!(read_entries(&p).unwrap().len(), 3);
    // a fresh full open agrees on the anchor
    assert_eq!(SessionFile::open(&p).unwrap().checkpoint().unwrap().unwrap(), c);
}

fn checkpointed(dir: &Path) -> (PathBuf, Checkpoint) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("s.jsonl");
    std::fs::write(&p, SESSION).unwrap();
    let mut f = SessionFile::open(&p).unwrap();
    for i in 0..3 {
        f.append(&json!({"type":"message","entry_id":format!("m{i}"),"text":"hello"}), false).unwrap();
    }
    let c = f.checkpoint().unwrap().unwrap();
    assert_eq!(c.len, std::fs::metadata(&p).unwrap().len());
    (p, c)
}

#[test]
fn matching_checkpoint_validates_only_what_follows_it() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let (p, c) = checkpointed(dir.path());
    // appended after the checkpoint (e.g. by a run that later crashed)
    OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{\"type\":\"leaf\"}\n").unwrap();
    let mut f = SessionFile::open_with(&p, Some(&c)).unwrap();
    // only length + mtime were compared: nothing before the checkpoint was read
    assert_eq!((f.stats().scanned_from, f.stats().checkpoint_mismatch), (c.len, false));
    let c2 = f.checkpoint().unwrap().unwrap();
    drop(f);
    assert_eq!(c2.len, std::fs::metadata(&p).unwrap().len());
}

#[test]
fn checkpoint_mismatch_forces_a_full_scan_before_writing() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // edited outside mare, same length: the mtime moved → full scan (still valid JSON, so it
    // opens; content integrity of valid JSON is out of scope)
    let (p, c) = checkpointed(&dir.path().join("edit"));
    std::thread::sleep(std::time::Duration::from_millis(5));
    let body = std::fs::read_to_string(&p).unwrap().replacen("hello", "HELLO", 1);
    std::fs::write(&p, &body).unwrap();
    let f = SessionFile::open_with(&p, Some(&c)).unwrap();
    assert_eq!((f.stats().scanned_from, f.stats().checkpoint_mismatch), (0, true));
    drop(f);
    // shorter than the checkpoint, with a bad line in the old prefix: caught by the full scan
    let (p, c) = checkpointed(&dir.path().join("bad"));
    let body = std::fs::read_to_string(&p).unwrap().replacen("{\"type\":\"message\"", "{oops", 1);
    std::fs::write(&p, &body).unwrap();
    let err = SessionFile::open_with(&p, Some(&c)).err().unwrap().to_string();
    assert!(err.contains(CORRUPT), "{err}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), body, "left untouched");
}

#[test]
fn readers_retry_while_the_file_changes_and_report_instability() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    std::fs::write(&p, SESSION).unwrap();
    let grow = |p: &Path| OpenOptions::new().append(true).open(p).unwrap().write_all(b"{\"type\":\"leaf\"}\n").unwrap();
    // changed during the first read only (a batch landing): read again, then stable
    let mut calls = 0;
    let (_, stable) = stable_read(&p, || {
        calls += 1;
        if calls == 1 {
            grow(&p);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!((calls, stable), (2, true));
    // shrinking during the read (a writer rolling back its failed batch) also re-reads
    let len = std::fs::metadata(&p).unwrap().len();
    let mut calls = 0;
    let (_, stable) = stable_read(&p, || {
        calls += 1;
        if calls == 1 {
            OpenOptions::new().write(true).open(&p).unwrap().set_len(len - 16).unwrap();
        }
        Ok(())
    })
    .unwrap();
    assert_eq!((calls, stable), (2, true));
    // changing on every try: give up after STABLE_READ_TRIES, marked unstable
    let mut calls = 0;
    let (_, stable) = stable_read(&p, || {
        calls += 1;
        grow(&p);
        Ok(())
    })
    .unwrap();
    assert_eq!((calls, stable), (STABLE_READ_TRIES, false));
    assert!(read_entries_stable(&p).unwrap().1, "quiet file: stable");
}

#[test]
fn resuming_a_missing_file_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("proj/gone.jsonl");
    let err = SessionFile::open_existing(&p, None).err().expect("refused").to_string();
    assert!(err.contains(NOT_FOUND), "{err}");
    assert!(!p.exists() && !lock_path(&p).exists(), "no headerless session, no lock file");
    // an existing one opens; opening alone never touches it (no mtime change)
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, SESSION).unwrap();
    let before = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    drop(SessionFile::open_existing(&p, None).unwrap());
    assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), before);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), SESSION);
}

#[test]
fn unsupported_session_version_cannot_be_opened_for_writing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("future.jsonl");
    let body = "{\"type\":\"session\",\"session_id\":\"s\",\"version\":99}\n{\"type\":\"message\"}\n{\"torn\"";
    std::fs::write(&p, body).unwrap();
    let sidecar = dir.path().join("future.jsonl.v1.bak");
    std::fs::write(&sidecar, "backup").unwrap();
    let stamp = crate::stamp(&p).unwrap();
    let checkpoint = Checkpoint { len: body.find("{\"torn\"").unwrap() as u64, mtime_ns: stamp.mtime_ns };
    for result in
        [SessionFile::open(&p), SessionFile::open_existing(&p, None), SessionFile::open_with(&p, Some(&checkpoint))]
    {
        assert!(result.is_err(), "unsupported version must be refused");
    }
    assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "backup");
}

#[test]
fn unsupported_versions_are_rejected_by_all_session_paths_without_mutation() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    for (index, version) in
        ["1", "2", "0", "-1", "3.5", "\"3\"", "null", "18446744073709551616", "99", "missing"].into_iter().enumerate()
    {
        let p = dir.path().join(format!("version-{index}.jsonl"));
        let v = if version == "missing" { String::new() } else { format!(",\"version\":{version}") };
        let body = format!("{{\"type\":\"session\",\"session_id\":\"s\"{v}}}\n{{\"type\":\"message\"}}\n{{\"torn\"");
        std::fs::write(&p, &body).unwrap();
        let sidecar = dir.path().join(format!("version-{index}.jsonl.v1.bak"));
        std::fs::write(&sidecar, "keep").unwrap();
        let error = SessionFile::open_existing(&p, None).err().unwrap().to_string();
        assert!(error.contains(UNSUPPORTED_VERSION), "{version}: {error}");
        for result in [
            read_entries(&p).map(|_| ()),
            read_entries_stable(&p).map(|_| ()),
            read_entries_lenient(&p).map(|_| ()),
            read_entries_lenient_stable(&p).map(|_| ()),
            reverse::read_from_compaction(&p).map(|_| ()),
            reverse::read_tail(&p, 3).map(|_| ()),
            reverse::read_head(&p, 3, 1000).map(|_| ()),
            check_file(&p).map(|_| ()),
        ] {
            assert!(result.unwrap_err().to_string().contains(UNSUPPORTED_VERSION), "{version}");
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "keep");
        delete_session(&p).expect("a verified session file can be explicitly deleted even if its version is unknown");
        assert!(!p.exists());
        assert!(sidecar.exists());
    }
}

#[test]
fn failed_first_batch_can_reopen_empty_new_session_and_retry() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("new.jsonl");
    let entries = [json!({"type":"session","version":3,"session_id":"s"}), json!({"type":"message","entry_id":"m"})];
    let mut file = SessionFile::open(&p).unwrap();
    FAIL_NEXT_SYNC.with(|f| f.set(true));
    assert!(file.append_batch(&entries, true).is_err());
    assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
    drop(file);
    assert!(SessionFile::open_existing(&p, None).is_err(), "an empty file is not a resumable session");
    let mut reopened = SessionFile::open(&p).expect("retry the first write after rollback");
    assert!(reopened.append(&json!({"type":"message","entry_id":"bad"}), true).is_err());
    reopened.append_batch(&entries, true).unwrap();
    drop(reopened);
    assert_eq!(read_entries(&p).unwrap(), entries);
}

#[test]
fn new_session_requires_supported_header_before_first_append() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("new.jsonl");
    let mut f = SessionFile::open(&p).unwrap();
    assert!(
        f.append(&json!({"type":"session","version":99}), true).unwrap_err().to_string().contains(UNSUPPORTED_VERSION)
    );
    assert!(f.append(&json!({"type":"message"}), true).is_err());
    assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
    f.append(&json!({"type":"session","version":CURRENT_VERSION,"session_id":"s","future":{"x":1}}), true).unwrap();
    drop(f);
    let mut f = SessionFile::open(&p).unwrap();
    f.append(&json!({"type":"unrecognized","value":42}), true).unwrap();
    drop(f);
    let entries = read_entries(&p).unwrap();
    assert_eq!(entries[0]["future"]["x"], 1);
    assert_eq!(entries[1]["type"], "unrecognized");
}

#[test]
fn a_new_session_file_can_be_opened_written_and_reopened() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new-session.jsonl");
    let mut file = SessionFile::open(&path).unwrap();
    file.append(&json!({"type":"session", "version":CURRENT_VERSION, "session_id":"new"}), true).unwrap();
    drop(file);
    let file = SessionFile::open_existing(&path, None).unwrap();
    assert_eq!(read_entries(&path).unwrap()[0]["session_id"], "new");
    drop(file);
}

#[test]
fn create_new_never_opens_or_changes_an_existing_session() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("existing.jsonl");
    std::fs::write(&p, SESSION).unwrap();
    assert!(SessionFile::create_new(&p).err().unwrap().to_string().contains("already exists"));
    assert_eq!(std::fs::read_to_string(&p).unwrap(), SESSION);
}

#[test]
fn a_create_new_session_file_can_be_opened_written_and_reopened() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new-session.jsonl");
    let mut file = SessionFile::create_new(&path).unwrap();
    file.append(&json!({"type":"session", "version":CURRENT_VERSION, "session_id":"new"}), true).unwrap();
    drop(file);
    let file = SessionFile::open_existing(&path, None).unwrap();
    assert_eq!(read_entries(&path).unwrap()[0]["session_id"], "new");
    drop(file);
}

#[test]
fn format_v3_rejects_v2_and_normalizes_header_and_index_vocabulary() {
    assert_eq!(CURRENT_VERSION, 3, "session journal format version must be 3");
    assert_eq!(crate::index::SCHEMA_VERSION, 7, "session index schema version must be 7");

    let v2_header = json!({
        "type": "session",
        "version": 2,
        "id": "legacy-v2",
        "cwd": "/work",
        "created": 1700000000000i64,
        "parent_session": "parent-v2",
    });
    let err = check_header(&v2_header).unwrap_err().to_string();
    assert!(err.contains(UNSUPPORTED_VERSION), "v2 header must fail closed: {err}");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let parent_path = root.join("--w--/parent.jsonl");
    let child_path = root.join("--w--/agents/run-1/001-child.jsonl");
    std::fs::create_dir_all(child_path.parent().unwrap()).unwrap();

    let mut parent_file = SessionFile::create_new(&parent_path).unwrap();
    parent_file
        .append_batch(
            &[
                json!({
                    "type": "session",
                    "version": 3,
                    "session_id": "parent-1",
                    "cwd": "/work",
                    "created_at_ms": 1700000000000i64,
                }),
                json!({
                    "type": "message",
                    "entry_id": "m1",
                    "parent_entry_id": null,
                    "created_at_ms": 1700000001000i64,
                    "message": { "role": "user", "content": "hello" },
                }),
                json!({ "type": "archive", "archived": true, "created_at_ms": 1700000002000i64 }),
            ],
            true,
        )
        .unwrap();
    drop(parent_file);

    let mut child_file = SessionFile::create_new(&child_path).unwrap();
    child_file
        .append(
            &json!({
                "type": "session",
                "version": 3,
                "session_id": "child-1",
                "parent_session_id": "parent-1",
                "run_id": "run-1",
                "label": "explore",
                "cwd": "/work",
                "created_at_ms": 1700000003000i64,
            }),
            true,
        )
        .unwrap();
    drop(child_file);

    let snap = read_fork_snapshot(&child_path, Some("child-1")).unwrap();
    assert_eq!(snap[0]["session_id"], "child-1");
    assert_eq!(snap[0]["parent_session_id"], "parent-1");
    assert_eq!(snap[0]["created_at_ms"], 1700000003000i64);

    let mut ix = crate::index::Index::open(root).unwrap();
    ix.ensure_built().unwrap();

    let conn = rusqlite::Connection::open(root.join(crate::index::DB_NAME)).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)").unwrap();
    let cols: Vec<String> = stmt.query_map([], |r| r.get(1)).unwrap().collect::<Result<_, _>>().unwrap();
    for expected in ["session_id", "parent_session_id", "created_at_ms", "updated_at_ms", "archived_at_ms"] {
        assert!(cols.iter().any(|c| c == expected), "missing sqlite column {expected}: {cols:?}");
    }
    for retired in ["id", "parent_id", "created_at", "updated_at", "archived_at"] {
        assert!(!cols.iter().any(|c| c == retired), "retired sqlite column {retired} still present: {cols:?}");
    }

    let (rows, _) = ix.list(&crate::index::Query { kinds: crate::index::Kinds::Any, ..Default::default() }).unwrap();
    assert_eq!(rows.len(), 2);
    let parent_json = rows.iter().find(|r| r.to_json()["session_id"] == "parent-1").unwrap().to_json();
    let child_json = rows.iter().find(|r| r.to_json()["session_id"] == "child-1").unwrap().to_json();

    assert_eq!(parent_json["created_at_ms"], 1700000000000i64);
    assert_eq!(parent_json["updated_at_ms"], 1700000001000i64);
    assert_eq!(parent_json["archived_at_ms"], 1700000002000i64);
    assert_eq!(child_json["parent_session_id"], "parent-1");
    assert_eq!(child_json["created_at_ms"], 1700000003000i64);

    for row_json in [&parent_json, &child_json] {
        for retired in ["id", "parent_session", "created", "updated", "archived_at"] {
            assert!(row_json.get(retired).is_none(), "retired IndexRow key {retired} present: {row_json}");
        }
    }

    delete_session(&parent_path).unwrap();
    assert!(!parent_path.exists());
    assert!(!child_path.exists(), "delete_session must remove child sessions linked via parent_session_id");

    // Verify canonical v3 built-in plugin records use unified vocabulary (no queue_id, follow, bare id, or code).
    let canonical_records = [
        json!({
            "type": "message",
            "entry_id": "m1",
            "parent_entry_id": null,
            "input_id": "in-1",
            "created_at_ms": 1700000001000i64,
            "message": { "role": "user", "content": "hello" },
        }),
        json!({
            "type": "custom",
            "kind": "agent/queue",
            "provenance": "session-host",
            "created_at_ms": 1700000001001i64,
            "data": { "steer": [], "follow_up": [{ "input_id": "in-2", "text": "next" }] },
        }),
        json!({
            "type": "custom",
            "kind": "agent/step-state",
            "created_at_ms": 1700000001002i64,
            "data": { "version": 1, "step_id": "st-1", "turn_id": "t-1", "step": 1, "states": {} },
        }),
        json!({
            "type": "custom",
            "kind": "job",
            "provenance": "session-host",
            "created_at_ms": 1700000001003i64,
            "data": { "event": "end", "job_id": "bash-1", "status": "cancelled", "exit_code": 143 },
        }),
    ];
    for rec in &canonical_records {
        crate::record::validate(rec).unwrap();
        assert!(rec.get("queue_id").is_none(), "message entry must not use queue_id: {rec}");
        if let Some(data) = rec.get("data").and_then(Value::as_object) {
            for retired in ["id", "follow", "code", "leaf", "leaf_id", "source_leaf_id"] {
                assert!(!data.contains_key(retired), "built-in record data must not use retired key {retired}: {rec}");
            }
        }
    }
}
