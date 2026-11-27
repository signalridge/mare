//! Deterministic `/proc` and kernel answers: no real permissions, no root-dependent chmod.
use super::{Member, groups_in, tree_alive_in};
use std::{collections::HashMap, io};

/// A complete record exactly as the kernel writes it (52 fields, trailing newline).
fn stat(pid: i32, comm: &str, state: &str, pgid: i32, sid: u32) -> Vec<u8> {
    let tail = vec!["0"; 52 - 7].join(" ");
    format!("{pid} ({comm}) {state} 1 {pgid} {sid} 0 {tail}\n").into_bytes()
}

enum Entry {
    Stat(Vec<u8>),
    Err(io::Error),
}

struct Fake {
    entries: HashMap<i32, Entry>,
    kernel: HashMap<i32, Member>,
}

impl Fake {
    fn new() -> Self {
        Self { entries: HashMap::new(), kernel: HashMap::new() }
    }
    fn stat(mut self, pid: i32, bytes: Vec<u8>) -> Self {
        self.entries.insert(pid, Entry::Stat(bytes));
        self
    }
    fn err(mut self, pid: i32, err: io::Error) -> Self {
        self.entries.insert(pid, Entry::Err(err));
        self
    }
    fn kernel(mut self, pid: i32, member: Member) -> Self {
        self.kernel.insert(pid, member);
        self
    }
    fn pids(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = self.entries.keys().copied().collect();
        pids.sort();
        pids
    }
    fn read(&self, pid: i32) -> io::Result<Vec<u8>> {
        match &self.entries[&pid] {
            Entry::Stat(bytes) => Ok(bytes.clone()),
            Entry::Err(err) => Err(io::Error::from_raw_os_error(err.raw_os_error().unwrap())),
        }
    }
    fn alive(&self, root: u32) -> bool {
        tree_alive_in(&self.pids(), root, &mut |p| self.read(p), &mut |p| {
            self.kernel.get(&p).copied().unwrap_or(Member::Unknown)
        })
    }
    fn groups(&self, session: u32) -> Vec<i32> {
        groups_in(&self.pids(), session, &mut |p| self.read(p), &mut |p| {
            self.kernel.get(&p).copied().unwrap_or(Member::Unknown)
        })
        .into_keys()
        .collect()
    }
}

fn eacces() -> io::Error {
    io::Error::from_raw_os_error(libc::EACCES)
}

#[test]
fn truncated_stat_whose_command_name_contains_a_paren_is_not_read_as_a_zombie() {
    // A running member named `a) Z 1 1 1`; the read stopped inside the name, so the prefix
    // parses as a zombie in session 1 if completeness is not checked.
    let full = stat(101, "a) Z 1 1 1", "S", 42, 42);
    let truncated = b"101 (a) Z 1 1 1".to_vec();
    assert!(Fake::new().stat(101, truncated.clone()).alive(42), "unplaceable truncated record must count as live");
    // The kernel can still place it: in the tree -> live; confirmed elsewhere -> not.
    let placed = Fake::new().stat(101, truncated.clone()).kernel(101, Member::Ids { sid: 42, pgid: 42 });
    assert!(placed.alive(42));
    let elsewhere = Fake::new().stat(101, truncated).kernel(101, Member::Ids { sid: 7, pgid: 7 });
    assert!(!elsewhere.alive(42));
    // The complete record with the same name parses by the last `)`.
    assert!(Fake::new().stat(101, full).alive(42));
    assert!(!Fake::new().stat(101, stat(101, "a) S 1 42 42", "Z", 42, 42)).alive(42), "complete zombie is excluded");
}

#[test]
fn stat_truncated_after_the_needed_fields_is_not_trusted() {
    // `state pgid sid` are present but the record is cut short: fall back to the kernel.
    let short = b"104 (worker) Z 1 42 42 0".to_vec();
    assert!(Fake::new().stat(104, short.clone()).alive(42));
    assert!(!Fake::new().stat(104, short).kernel(104, Member::Gone).alive(42));
}

#[test]
fn an_unreadable_unrelated_process_does_not_keep_an_exited_tree_alive() {
    // hidepid=1: another user's entry is listed but its stat is EACCES. The target tree
    // (session 42) has already exited; only a zombie of it remains.
    let fake = Fake::new()
        .err(500, eacces())
        .kernel(500, Member::Ids { sid: 900, pgid: 901 })
        .stat(103, stat(103, "zomb", "Z", 42, 42));
    assert!(!fake.alive(42), "a confirmed-unrelated unreadable process must not keep the tree alive");
}

#[test]
fn an_unreadable_tree_member_or_unplaceable_process_keeps_the_tree_alive() {
    let member = Fake::new().err(500, eacces()).kernel(500, Member::Ids { sid: 900, pgid: 42 });
    assert!(member.alive(42), "an unreadable member of the root's group is live");
    let unknown = Fake::new().err(500, io::Error::from_raw_os_error(libc::EMFILE));
    assert!(unknown.alive(42), "EMFILE with no kernel answer is unknown, never exited");
    let gone = Fake::new().err(500, io::Error::from_raw_os_error(libc::ENOENT));
    assert!(!gone.alive(42), "a vanished entry is gone");
    let live = Fake::new().stat(104, stat(104, "worker", "S", 77, 42));
    assert!(live.alive(42), "a readable member of the session is live");
}

#[test]
fn an_unreadable_session_member_in_its_own_group_still_receives_the_signal() {
    let fake = Fake::new()
        .stat(42, stat(42, "sh", "S", 42, 42))
        .err(60, io::Error::from_raw_os_error(libc::EMFILE))
        .kernel(60, Member::Ids { sid: 42, pgid: 60 })
        .err(70, eacces())
        .kernel(70, Member::Ids { sid: 900, pgid: 70 });
    assert_eq!(fake.groups(42), vec![42, 60], "the background job's group must not be skipped");
}

#[test]
fn the_running_kernels_stat_record_is_accepted_as_complete() {
    // Too strict a completeness check would push every entry to the kernel fallback,
    // which cannot see zombies, and an exited tree with one unreaped child would never end.
    let pid = std::process::id() as i32;
    let real = super::read_stat(pid).unwrap();
    let sid = unsafe { libc::getsid(pid) } as u32;
    let pgid = unsafe { libc::getpgid(pid) };
    assert_eq!(super::parse_stat(pid, &real), Some(Member::Ids { sid, pgid }));
}
