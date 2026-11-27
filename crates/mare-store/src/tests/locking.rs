use super::*;
use crate::lock::BETWEEN_OPEN_AND_LOCK;
use serde_json::json;

#[test]
fn session_lock_is_exclusive_across_handles() {
    // a child forked by another test inherits our fds until it execs, keeping the flock alive
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    let mut a = SessionFile::open(&p).unwrap();
    // a second open in the same process is refused (in-process registry), never interleaved
    let err = SessionFile::open(&p).err().expect("second open refused").to_string();
    assert!(err.contains(OPEN_HERE), "{err}");
    a.append(&json!({"type":"session","version":CURRENT_VERSION,"session_id":"a"}), true).unwrap();
    drop(a);
    // once closed, it can be opened again
    drop(SessionFile::open(&p).unwrap());
    // another holder (stands in for another process): opening fails with LOCKED
    let other = std::fs::File::open(lock_path(&p)).unwrap();
    other.try_lock().unwrap();
    let err = SessionFile::open(&p).err().unwrap().to_string();
    assert!(err.contains(LOCKED), "{err}");
    drop(other);
    SessionFile::open(&p).unwrap();
}

/// Child half of `one_writer_across_processes`: open the session, report, hold it a while.
#[test]
fn session_writer_child() {
    let Ok(path) = std::env::var("MARE_SESSION_CHILD") else { return };
    match SessionFile::open(Path::new(&path)) {
        Ok(mut f) => {
            f.append(&json!({"type":"message","entry_id":std::process::id().to_string()}), true).unwrap();
            println!("RESULT writer");
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        Err(e) if e.to_string().contains(LOCKED) => println!("RESULT locked"),
        Err(e) => println!("RESULT error {e}"),
    }
}

#[test]
fn one_writer_across_processes() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var("MARE_SESSION_CHILD").is_ok() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    // Whoever wins takes the lock; the loser must still be refused
    std::fs::write(&p, SESSION).unwrap();
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..2)
        .map(|_| {
            std::process::Command::new(&exe)
                .args(["--exact", "tests::locking::session_writer_child", "--test-threads=1", "--nocapture"])
                .env("MARE_SESSION_CHILD", &p)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let results: Vec<String> = children
        .into_iter()
        .map(|c| {
            let out = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
            out.lines().find_map(|l| l.split_once("RESULT ").map(|(_, r)| r.trim().to_string())).unwrap_or_default()
        })
        .collect();
    let writers = results.iter().filter(|r| *r == "writer").count();
    let locked = results.iter().filter(|r| *r == "locked").count();
    assert_eq!((writers, locked), (1, 1), "{results:?}");
    assert_eq!(read_entries(&p).unwrap().len(), 2, "exactly one writer appended");
}

/// Child half of `reader_freezes_the_end_and_skips_an_entry_still_being_written`: the
/// writer, mid-way through writing an entry, finishes it when told to (stdin).
#[test]
fn half_written_child() {
    let Ok(path) = std::env::var("MARE_HALF_CHILD") else { return };
    let mut f = SessionFile::open(Path::new(&path)).unwrap();
    f.append_batch(
        &[json!({"type":"session","version":3,"session_id":"m0"}), json!({"type":"message","entry_id":"m1"})],
        true,
    )
    .unwrap();
    // an entry in flight: the first half reached the file, the rest hasn't yet
    let mut raw = OpenOptions::new().append(true).open(&path).unwrap();
    raw.write_all(b"{\"type\":\"message\",").unwrap();
    println!("MARK half");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    raw.write_all(b"\"entry_id\":\"m2\"}\n").unwrap();
    raw.sync_data().unwrap();
    println!("MARK done");
    line.clear();
    std::io::stdin().read_line(&mut line).unwrap();
    drop(f);
}

#[test]
fn reader_freezes_the_end_and_skips_an_entry_still_being_written() {
    use std::io::BufRead;
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var("MARE_HALF_CHILD").is_ok() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::locking::half_written_child", "--test-threads=1", "--nocapture"])
        .env("MARE_HALF_CHILD", &p)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let wait_for = |out: &mut std::io::BufReader<std::process::ChildStdout>, mark: &str| {
        let mut line = String::new();
        while out.read_line(&mut line).unwrap() > 0 {
            if line.contains(mark) {
                return;
            }
            line.clear();
        }
        panic!("child ended before {mark}");
    };
    let ids = |v: Vec<Value>| {
        v.iter()
            .map(|e| e.get("session_id").or_else(|| e.get("entry_id")).unwrap().as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    wait_for(&mut out, "MARK half");
    let before = std::fs::read(&p).unwrap();
    // lock-free readers while the writer holds the lock: only complete entries count
    assert_eq!(ids(read_entries(&p).unwrap()), ["m0", "m1"]);
    assert_eq!(ids(reverse::read_from_compaction(&p).unwrap().entries), ["m0", "m1"]);
    assert_eq!(ids(read_entries_lenient(&p).unwrap().0), ["m0", "m1"]);
    assert_eq!(std::fs::read(&p).unwrap(), before, "readers never touch the file");
    // a reverse scan freezes its end when it starts: later appends are not seen
    let mut frozen = reverse::ReverseLines::open(&p).unwrap();
    writeln!(stdin, "finish").unwrap();
    wait_for(&mut out, "MARK done");
    assert!(frozen.all(|l| !String::from_utf8_lossy(&l.unwrap()).contains("m2")));
    assert_eq!(ids(read_entries(&p).unwrap()), ["m0", "m1", "m2"]);
    writeln!(stdin, "exit").unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn lock_is_retried_when_the_lock_file_was_replaced_before_locking() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.jsonl");
    let lp = lock_path(&p);
    std::fs::write(&lp, "").unwrap();
    // between our open and our flock, someone unlinks the lock file, recreates it and holds
    // the new one: our flock on the old inode succeeds, but the re-check must catch it
    let held = std::rc::Rc::new(std::cell::RefCell::new(None));
    let h2 = held.clone();
    let lp2 = lp.clone();
    BETWEEN_OPEN_AND_LOCK.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::remove_file(&lp2).unwrap();
            let f = OpenOptions::new().create(true).truncate(false).write(true).open(&lp2).unwrap();
            f.try_lock().unwrap();
            *h2.borrow_mut() = Some(f);
        }));
    });
    let err = SessionFile::open(&p).err().expect("the live lock file is held").to_string();
    assert!(err.contains(LOCKED), "{err}");
    drop(held.borrow_mut().take());
    SessionFile::open(&p).unwrap();
}
