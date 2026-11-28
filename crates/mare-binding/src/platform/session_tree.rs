//! A PTY shell moves jobs into separate process groups in its owned session. Treating only
//! the leader's group as the tree leaks those jobs. Enumerate kernel session membership;
//! never infer ownership from a name, user, terminal path, or a caller-supplied group id.
use std::{collections::BTreeMap, io};

/// Fields a complete `/proc/<pid>/stat` record has after the `(comm)` field on every
/// Linux kernel since 2.6.24 (`state` .. `guest_time`); fewer means we cannot trust it.
#[cfg(target_os = "linux")]
const STAT_FIELDS_AFTER_COMM: usize = 42;

/// What is known about one `/proc` entry. `Unknown` is never treated as "gone".
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Member {
    Gone,
    Zombie,
    Ids { sid: u32, pgid: i32 },
    Unknown,
}

/// Parse a record only if it is complete: it names `pid`, ends with the kernel's trailing
/// newline, and carries every field. A command name may itself contain `)` and spaces, so
/// a truncated read can look like a well-formed zombie; completeness is what rules that out.
#[cfg(target_os = "linux")]
fn parse_stat(pid: i32, stat: &[u8]) -> Option<Member> {
    let body = stat.strip_suffix(b"\n")?;
    let open = body.iter().position(|&b| b == b'(')?;
    if std::str::from_utf8(&body[..open]).ok()?.trim_end().parse::<i32>().ok()? != pid {
        return None;
    }
    let end = body.iter().rposition(|&b| b == b')')?;
    let fields: Vec<&str> = std::str::from_utf8(&body[end + 1..]).ok()?.split_whitespace().collect();
    if fields.len() < STAT_FIELDS_AFTER_COMM {
        return None;
    }
    if fields[0] == "Z" {
        return Some(Member::Zombie);
    }
    Some(Member::Ids { pgid: fields[2].parse().ok()?, sid: fields[3].parse().ok()? })
}

/// Ask the kernel directly. `getsid`/`getpgid` are not subject to `/proc` mount options
/// such as `hidepid=1`, so an unreadable entry of another user can still be placed.
#[cfg(target_os = "linux")]
fn kernel_ids(pid: i32) -> Member {
    let gone_or_unknown = || {
        if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) { Member::Gone } else { Member::Unknown }
    };
    let sid = unsafe { libc::getsid(pid) };
    if sid < 0 {
        return gone_or_unknown();
    }
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 {
        return gone_or_unknown();
    }
    Member::Ids { sid: sid as u32, pgid }
}

/// Trust a complete stat record; otherwise (unreadable, truncated) fall back to the kernel.
#[cfg(target_os = "linux")]
fn classify(pid: i32, stat: io::Result<Vec<u8>>, kernel: &mut impl FnMut(i32) -> Member) -> Member {
    match stat {
        Ok(stat) => parse_stat(pid, &stat).unwrap_or_else(|| kernel(pid)),
        Err(err) if err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ESRCH) => Member::Gone,
        Err(_) => kernel(pid),
    }
}

#[cfg(target_os = "linux")]
fn proc_pids() -> io::Result<Vec<i32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        if let Some(pid) = entry?.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(target_os = "linux")]
fn read_stat(pid: i32) -> io::Result<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/stat"))
}

#[cfg(target_os = "linux")]
fn groups_in(
    pids: &[i32],
    session: u32,
    read: &mut impl FnMut(i32) -> io::Result<Vec<u8>>,
    kernel: &mut impl FnMut(i32) -> Member,
) -> BTreeMap<i32, i32> {
    let mut groups = BTreeMap::new();
    for &pid in pids {
        // An unreadable member still has a group to signal; the kernel places it.
        if let Member::Ids { sid, pgid } = classify(pid, read(pid), kernel)
            && sid == session
            && pgid > 0
        {
            groups.insert(pgid, pid);
        }
    }
    groups
}

#[cfg(target_os = "linux")]
fn groups(session: u32) -> io::Result<BTreeMap<i32, i32>> {
    Ok(groups_in(&proc_pids()?, session, &mut read_stat, &mut kernel_ids))
}

#[cfg(target_os = "macos")]
fn groups(session: u32) -> io::Result<BTreeMap<i32, i32>> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0i32; count as usize + 1024];
    let got =
        unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), std::mem::size_of_val(pids.as_slice()) as i32) };
    if got < 0 || got as usize >= pids.len() {
        return Err(io::Error::other("incomplete process snapshot"));
    }
    let mut groups = BTreeMap::new();
    for pid in pids.into_iter().take(got as usize) {
        if pid > 0 && unsafe { libc::getsid(pid) } == session as i32 {
            let group = unsafe { libc::getpgid(pid) };
            if group > 0 {
                groups.insert(group, pid);
            }
        }
    }
    Ok(groups)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn groups(_session: u32) -> io::Result<BTreeMap<i32, i32>> {
    Err(io::Error::other("session inspection unsupported"))
}

pub(crate) fn alive(session: u32) -> bool {
    // Inspection failure is unknown, never confirmed stopped.
    groups(session).map_or(true, |g| !g.is_empty())
}

#[cfg(target_os = "linux")]
pub(super) fn tree_alive(root: u32) -> io::Result<bool> {
    Ok(tree_alive_in(&proc_pids()?, root, &mut read_stat, &mut kernel_ids))
}

/// Only a process confirmed gone, a zombie, or confirmed outside the root's session and
/// group is excluded; anything that cannot be placed keeps the tree alive.
#[cfg(target_os = "linux")]
fn tree_alive_in(
    pids: &[i32],
    root: u32,
    read: &mut impl FnMut(i32) -> io::Result<Vec<u8>>,
    kernel: &mut impl FnMut(i32) -> Member,
) -> bool {
    let root_pgid = i32::try_from(root).ok();
    pids.iter().any(|&pid| match classify(pid, read(pid), kernel) {
        Member::Gone | Member::Zombie => false,
        Member::Ids { sid, pgid } => sid == root || root_pgid == Some(pgid),
        Member::Unknown => true,
    })
}

fn signal_groups_with<Check, Send>(
    groups: BTreeMap<i32, i32>,
    exclude_pgid: Option<i32>,
    mut still_in_group: Check,
    mut send_signal: Send,
) -> io::Result<()>
where
    Check: FnMut(i32, i32) -> bool,
    Send: FnMut(i32) -> io::Result<()>,
{
    let mut first_error = None;
    for (pgid, member) in groups {
        if exclude_pgid == Some(pgid) {
            continue;
        }
        // Recheck kernel identity immediately before each group signal.
        if !still_in_group(member, pgid) {
            continue;
        }
        if let Err(e) = send_signal(pgid)
            && e.raw_os_error() != Some(libc::ESRCH)
        {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn signal_groups(session: u32, signal: i32, exclude_pgid: Option<i32>) -> io::Result<()> {
    let groups = groups(session)?;
    signal_groups_with(
        groups,
        exclude_pgid,
        |member, pgid| unsafe { libc::getsid(member) } == session as i32 && unsafe { libc::getpgid(member) } == pgid,
        |pgid| {
            if unsafe { libc::kill(-pgid, signal) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        },
    )
}

pub(super) fn signal(session: u32, signal: i32) -> io::Result<()> {
    signal_groups(session, signal, None)
}

/// Signal every process group in `session` except the leader's own group (`pgid == session`).
/// Callers that subsequently signal `-(session as i32)` directly use this so polite signals
/// (`SIGINT`, `SIGTERM`, etc.) are delivered to the leader's group once instead of twice.
pub(super) fn signal_other_groups(session: u32, signal: i32) -> io::Result<()> {
    signal_groups(session, signal, i32::try_from(session).ok())
}

#[cfg(all(test, target_os = "linux"))]
#[path = "session_tree_tests.rs"]
mod tests;
