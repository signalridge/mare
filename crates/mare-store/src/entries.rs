//! Readers: strict and lenient reads of a session's entries (stat-checked variants), the
//! header alone, and `mare doctor`'s strict scan.

use super::*;
use crate::validate::{blank, complete_lines, scan_file};

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_BOUND_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static AFTER_BOUND_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Strict parse of the complete lines (end frozen when the read starts; an unterminated final
/// entry is still being written or torn and never counts): any line that
/// does not parse is [`CORRUPT`]. Streams the file.
pub(crate) fn parse_lines(path: &Path) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    complete_lines(path, 0, |n, line| {
        if blank(line) {
            return Ok(());
        }
        match parse_line(line) {
            Some(v) => out.push(v),
            None => anyhow::bail!("{CORRUPT}: line {n} of {} does not parse", path.display()),
        }
        Ok(())
    })?;
    Ok(out)
}

/// Read-only view of a damaged file: every parseable entry, plus how many lines were skipped and
/// how many entries come before the first bad line (`intact`). Never silently lenient: callers
/// show the skipped count, and a fork copies only the `intact` prefix.
pub fn read_entries_lenient(path: &Path) -> Result<(Vec<Value>, usize, usize)> {
    crate::format::check_file_header(path)?;
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut intact = None;
    complete_lines(path, 0, |_, line| {
        if blank(line) {
            return Ok(());
        }
        match parse_line(line) {
            Some(v) => out.push(v),
            None => {
                skipped += 1;
                intact.get_or_insert(out.len());
            }
        }
        Ok(())
    })?;
    let intact = intact.unwrap_or(out.len());
    Ok((out, skipped, intact))
}

/// Read all entries, strictly (an unterminated final entry is ignored; any other bad line is
/// [`CORRUPT`]). Read-only and lock-free: never rewrites the file; unsupported versions
/// are rejected.
pub fn read_entries(path: &Path) -> Result<Vec<Value>> {
    crate::format::check_file_header(path)?;
    parse_lines(path)
}

/// `mare doctor`'s full strict scan of one session file: every complete
/// line must parse ([`CORRUPT`] otherwise); returns the bytes of a torn final write (an entry
/// without its '\n', cut by the next writer before it appends), 0 when the file ends cleanly.
/// Read-only, lock-free, streamed (no whole-file buffer).
pub fn check_file(path: &Path) -> Result<u64> {
    crate::format::check_file_header(path)?;
    let (complete, end) = scan_file(path, 0)?;
    Ok(end - complete)
}

/// [`read_entries`] with a stat before and after ([`stable_read`]): `false` when the file still
/// changed on the re-read — mark a read-only view "unstable", never fork from it.
pub fn read_entries_stable(path: &Path) -> Result<(Vec<Value>, bool)> {
    stable_read(path, || {
        crate::format::check_file_header(path)?;
        parse_lines(path)
    })
}

/// [`read_entries_lenient`] with a stat before and after: `(entries, skipped, intact, stable)`.
pub fn read_entries_lenient_stable(path: &Path) -> Result<(Vec<Value>, usize, usize, bool)> {
    let ((e, s, i), stable) = stable_read(path, || read_entries_lenient(path))?;
    Ok((e, s, i, stable))
}

/// Read a complete read-only view through one handle, refusing a changing source.
pub fn read_entries_bound(path: &Path, lenient: bool) -> Result<(Vec<Value>, usize)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let before = crate::stable::stamp_file(&file)?;
    #[cfg(test)]
    if let Some(hook) = BEFORE_BOUND_READ.with(|h| h.borrow_mut().take()) {
        hook();
    }
    let mut entries = Vec::new();
    let mut skipped = 0;
    crate::validate::complete_lines_held(&mut file, 0, |n, line| {
        if n == 1 {
            let header = parse_line(line).ok_or_else(|| anyhow::anyhow!("{CORRUPT}: invalid session header"))?;
            crate::format::check_header(&header)?;
            entries.push(header);
        } else if !blank(line) {
            if let Some(value) = parse_line(line) {
                entries.push(value);
            } else if lenient {
                skipped += 1;
            } else {
                anyhow::bail!("{CORRUPT}: line {n} of {} does not parse", path.display());
            }
        }
        Ok(())
    })?;
    #[cfg(test)]
    if let Some(hook) = AFTER_BOUND_READ.with(|h| h.borrow_mut().take()) {
        hook();
    }
    if entries.is_empty() {
        anyhow::bail!("{CORRUPT}: missing complete session header");
    }
    if crate::stable::stamp_file(&file)? != before
        || crate::stable::stamp(path).ok() != Some(before)
        || crate::stable::forced_unstable(path)
    {
        anyhow::bail!("session file {} changed while it was read; try again", path.display());
    }
    Ok((entries, skipped))
}

/// Read only the first entry (session header) cheaply.
pub fn read_header(path: &Path) -> Option<Value> {
    let f = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// Observe a fork source from its last fsynced prefix. Each attempt reads one
/// open file description at its initial size, validating complete lines even past
/// the durable offset, but returns only the fsynced prefix. A legacy journal
/// without a watermark is initialized once under its writer lock. If the path,
/// lock generation, or metadata changes, retry once; on a second change retain
/// that attempt's initial prefix. Complete
/// bad lines are never skipped, and the expected session identity must match.
pub fn read_fork_snapshot(path: &Path, expected_id: Option<&str>) -> Result<Vec<Value>> {
    use std::io::Read;

    if crate::lock::read_committed_prefix(path)?.is_none() {
        // Upgrade an older journal once under its writer lock. New writers publish this marker
        // before returning from open, so lock-free readers never infer a boundary from file size.
        // A lock error must not hide complete corrupt lines in an older journal.
        read_entries_bound(path, false)?;
        let _writer = crate::SessionFile::open_existing(path, None)
            .with_context(|| format!("session has no durable fork boundary: {}", path.display()))?;
    }

    let mut bytes = Vec::new();
    let mut durable_len = 0;
    for attempt in 0..crate::stable::STABLE_READ_TRIES {
        let point = crate::lock::read_committed_prefix(path)?
            .ok_or_else(|| anyhow::anyhow!("session has no durable fork boundary: {}", path.display()))?;
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let before = crate::stable::fork_stamp_file(&file)?;
        if !commit_point_matches(path, &file, point)? || point.len > before.len {
            if attempt + 1 == crate::stable::STABLE_READ_TRIES {
                anyhow::bail!("session durable fork boundary no longer matches {}", path.display());
            }
            continue;
        }
        #[cfg(test)]
        FORK_READ_HOOK.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(attempt);
            }
        });
        bytes.clear();
        durable_len = point.len as usize;
        // Validate every complete line in this observation, including bytes beyond the
        // durable boundary. Those bytes cannot be copied, but corruption there still
        // makes the source damaged.
        (&mut file).take(before.len).read_to_end(&mut bytes)?;
        let stable = crate::stable::fork_stamp_file(&file).ok() == Some(before)
            && crate::stable::fork_stamp(path).ok() == Some(before)
            && crate::lock::read_committed_prefix(path).ok().flatten() == Some(point)
            && commit_point_matches(path, &file, point).unwrap_or(false)
            && !crate::stable::forced_unstable(path);
        if stable || attempt + 1 == crate::stable::STABLE_READ_TRIES {
            break;
        }
    }
    if bytes.get(durable_len.wrapping_sub(1)) != Some(&b'\n') {
        anyhow::bail!("{CORRUPT}: durable fork boundary is not a complete line in {}", path.display());
    }
    let mut entries = Vec::new();
    let mut offset = 0;
    for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        if line.last() != Some(&b'\n') {
            break;
        }
        let in_prefix = offset < durable_len;
        offset += line.len();
        let line = &line[..line.len() - 1];
        if i > 0 && blank(line) {
            continue;
        }
        let value = parse_line(line)
            .ok_or_else(|| anyhow::anyhow!("{CORRUPT}: line {} of {} does not parse", i + 1, path.display()))?;
        if i == 0 {
            crate::format::check_header(&value).with_context(|| format!("session header in {}", path.display()))?;
        }
        // Only the fsynced prefix may become fork history.
        if in_prefix {
            entries.push(value);
        }
    }
    let header = entries
        .first()
        .ok_or_else(|| anyhow::anyhow!("{CORRUPT}: missing complete session header in {}", path.display()))?;
    if header
        .get("session_id")
        .and_then(Value::as_str)
        .is_none_or(|id| expected_id.is_some_and(|expected| id != expected))
    {
        anyhow::bail!("session source id mismatch in {}", path.display());
    }
    Ok(entries)
}

fn commit_point_matches(path: &Path, file: &File, point: crate::lock::CommittedPrefix) -> Result<bool> {
    if crate::lock::rollback_pending(path)? {
        return Ok(false);
    }
    let lock = match crate::stamp(&crate::lock_path(path)) {
        Ok(stamp) => (stamp.dev, stamp.ino, stamp.len),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, 0, 0),
        Err(error) => return Err(error.into()),
    };
    Ok(crate::lock::file_id(file)? == (point.dev, point.ino)
        && lock == (point.lock_dev, point.lock_ino, point.revision))
}

#[cfg(test)]
type ForkReadHook = Box<dyn FnMut(usize)>;

#[cfg(test)]
thread_local! {
    static FORK_READ_HOOK: std::cell::RefCell<Option<ForkReadHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod fork_snapshot_tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn write(path: &Path, id: &str, value: &str) {
        let bytes =
            format!("{{\"type\":\"session\",\"version\":3,\"session_id\":\"{id}\"}}\n{{\"value\":\"{value}\"}}\n");
        std::fs::write(path, bytes).unwrap();
        // Windows requires a write-capable handle for FlushFileBuffers; a read-only
        // handle happens to work with fsync on Unix, which hid this fixture bug there.
        let file = OpenOptions::new().read(true).write(true).open(path).unwrap();
        file.sync_data().unwrap();
        crate::lock::write_committed_prefix(path, &file, file.metadata().unwrap().len()).unwrap();
    }

    #[test]
    fn retries_a_replaced_source_and_checks_its_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let replacement = dir.path().join("replacement");
        write(&path, "s", "old");
        write(&replacement, "s", "new");
        let p = path.clone();
        FORK_READ_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |attempt| {
                if attempt == 0 {
                    std::fs::rename(&replacement, &p).unwrap();
                    let file = OpenOptions::new().read(true).write(true).open(&p).unwrap();
                    file.sync_data().unwrap();
                    crate::lock::write_committed_prefix(&p, &file, file.metadata().unwrap().len()).unwrap();
                }
            }))
        });
        let result = read_fork_snapshot(&path, Some("s")).unwrap();
        FORK_READ_HOOK.with(|h| *h.borrow_mut() = None);
        assert_eq!(result[1]["value"], "new");
        assert_eq!(read_fork_snapshot(&path, None).unwrap()[0]["session_id"], "s");
        assert!(read_fork_snapshot(&path, Some("other")).unwrap_err().to_string().contains("id mismatch"));
    }

    #[test]
    fn fork_ignores_run_end_until_its_journal_sync_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut writer = crate::SessionFile::open(&path).unwrap();
        writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let fork_path = path.clone();
        crate::session_file::BEFORE_ROLLBACK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let entries =
                    std::thread::spawn(move || read_fork_snapshot(&fork_path, Some("s")).unwrap()).join().unwrap();
                tx.send(entries).unwrap();
            }));
        });
        crate::session_file::FAIL_NEXT_SYNC.with(|fail| fail.set(true));
        assert!(
            writer.append(&json!({"type":"custom","kind":"agent/run-end","data":{"run_id":"failed"}}), true,).is_err()
        );

        let observed_during_failure = rx.recv().unwrap();
        let is_run_end = |entry: &Value| {
            crate::record::core::<crate::record::Custom>(entry).is_some_and(|record| record.is_run_end())
        };
        assert!(!observed_during_failure.iter().any(is_run_end));
        assert!(!read_fork_snapshot(&path, Some("s")).unwrap().iter().any(is_run_end));

        writer.append(&json!({"type":"custom","kind":"agent/run-end","data":{"run_id":"successful"}}), true).unwrap();
        let forked = read_fork_snapshot(&path, Some("s")).unwrap();
        assert!(forked.iter().any(|entry| entry["data"]["run_id"] == "successful"));
    }

    #[test]
    fn validates_complete_lines_beyond_the_durable_prefix_without_copying_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut writer = crate::SessionFile::open(&path).unwrap();
        writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();
        writer.append(&json!({"type":"name","name":"unsynced"}), false).unwrap();
        assert_eq!(read_fork_snapshot(&path, Some("s")).unwrap().len(), 1);
        let mut external = OpenOptions::new().append(true).open(&path).unwrap();
        write!(external, "torn").unwrap();
        assert_eq!(read_fork_snapshot(&path, Some("s")).unwrap().len(), 1);
        writeln!(external, " bad line").unwrap();
        let err = read_fork_snapshot(&path, Some("s")).unwrap_err().to_string();
        assert!(err.contains(CORRUPT) && err.contains(path.to_str().unwrap()), "{err}");
    }

    #[test]
    fn a_locked_legacy_source_requires_a_watermark_and_still_reports_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"session_id\":\"s\"}\n").unwrap();
        let _lock = crate::lock::lock_session(&path).unwrap();
        let err = read_fork_snapshot(&path, Some("s")).unwrap_err().to_string();
        assert!(err.contains("no durable fork boundary"), "{err}");
        writeln!(OpenOptions::new().append(true).open(&path).unwrap(), "invalid json").unwrap();
        let err = read_fork_snapshot(&path, Some("s")).unwrap_err().to_string();
        assert!(err.contains(CORRUPT), "{err}");
    }

    #[test]
    fn changing_twice_uses_second_attempt_initial_size_and_rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        write(&path, "s", "old");
        let p = path.clone();
        FORK_READ_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |attempt| {
                writeln!(OpenOptions::new().append(true).open(&p).unwrap(), "{{\"attempt\":{attempt}}}").unwrap();
                let file = OpenOptions::new().read(true).write(true).open(&p).unwrap();
                file.sync_data().unwrap();
                crate::lock::write_committed_prefix(&p, &file, file.metadata().unwrap().len()).unwrap();
            }))
        });
        let result = read_fork_snapshot(&path, Some("s")).unwrap();
        FORK_READ_HOOK.with(|h| *h.borrow_mut() = None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[2]["attempt"], 0);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "bad").unwrap();
        file.sync_data().unwrap();
        crate::lock::write_committed_prefix(&path, &file, file.metadata().unwrap().len()).unwrap();
        let err = read_fork_snapshot(&path, Some("s")).unwrap_err().to_string();
        assert!(err.contains(CORRUPT) && err.contains(path.to_str().unwrap()));
    }
}
