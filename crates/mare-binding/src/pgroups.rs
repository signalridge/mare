//! Process groups left behind by a mare that was killed ("mare 被强杀时 Unix 上的
//! 子进程树残留"). On Linux every child gets `PR_SET_PDEATHSIG`; macOS has no such mechanism and
//! `atexit` doesn't run on SIGKILL, so each live background group is also written to an on-disk
//! registry, `<state>/pgroups/<mare pid>`, and the next mare to start ends the groups of
//! registries whose owner is gone. Every group mare starts is registered: `mare.proc.start`
//! jobs, `mare.proc.run`, `mare.proc.spawn`, PTY children and MCP stdio servers ([`crate::trees::Tree`]). mare's own design (none of the reference projects has it).
//!
//! File format, rewritten (temp + rename) on every change, removed when empty:
//!   `owner <pid> <start>` then one `<pgid> <start>` line per group,
//! where `<start>` is the process start time in microseconds. Start times guard against pid
//! reuse: an owner whose pid now names another process counts as gone; a group whose leader pid
//! now names another process is left alone. A group whose leader exited but whose members live
//! on still holds its id (POSIX doesn't reuse a pid while a group of that id exists), so it is
//! ended too. Other platforms: no-ops (Linux: PDEATHSIG; Windows: KILL_ON_JOB_CLOSE jobs).

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// pgid → leader start time, for this process's registry file
    static LIVE: Mutex<BTreeMap<u32, u64>> = Mutex::new(BTreeMap::new());

    /// Tests: registries go to a temp dir instead of the real state dir.
    #[cfg(test)]
    pub static TEST_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub fn dir() -> PathBuf {
        #[cfg(test)]
        if let Some(d) = TEST_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return d;
        }
        mare_shared::paths::state_dir().join("pgroups")
    }

    /// Start time (µs since the epoch) of a live process, None when there is none.
    pub fn start_time(pid: u32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    fn group_exists(pgid: u32) -> bool {
        // signal 0 probes the group; EPERM (someone else's) is never ours to end
        unsafe { libc::kill(-(pgid as i32), 0) == 0 }
    }

    fn write(dir: &Path, live: &BTreeMap<u32, u64>) {
        let me = std::process::id();
        let path = dir.join(me.to_string());
        if live.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let Some(own) = start_time(me) else { return };
        let mut text = format!("owner {me} {own}\n");
        for (pgid, start) in live {
            let _ = writeln!(text, "{pgid} {start}");
        }
        if mare_shared::private::create_dir_all(dir).is_err() {
            return;
        }
        let _ = mare_shared::paths::write_atomic(&path, text.as_bytes(), mare_shared::paths::WriteMode::Private);
    }

    pub fn set(pgid: u32, live: bool) {
        let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        if live {
            let Some(start) = start_time(pgid) else { return };
            g.insert(pgid, start);
        } else if g.remove(&pgid).is_none() {
            return;
        }
        write(&dir(), &g);
    }

    /// At a normal exit (after the groups were killed): nothing left to clean up.
    pub fn clear() {
        let _ = std::fs::remove_file(dir().join(std::process::id().to_string()));
    }

    /// End the groups of every registry in `dir` whose owner is gone; returns the pgids ended.
    pub fn sweep_in(dir: &Path) -> Vec<u32> {
        let mut ended = vec![];
        let Ok(rd) = std::fs::read_dir(dir) else { return ended };
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                // a temp file a crash left mid-write
                if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|d| d.as_secs() > 60)) {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let mut lines = text.lines();
            let owner = lines.next().and_then(|l| {
                let mut f = l.split_whitespace();
                (f.next() == Some("owner")).then_some(())?;
                Some((f.next()?.parse::<u32>().ok()?, f.next()?.parse::<u64>().ok()?))
            });
            let Some((pid, start)) = owner else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            if pid == std::process::id() || start_time(pid) == Some(start) {
                continue; // its mare is still running
            }
            for l in lines {
                let mut f = l.split_whitespace();
                let (Some(Ok(pgid)), Some(Ok(lstart))) =
                    (f.next().map(str::parse::<u32>), f.next().map(str::parse::<u64>))
                else {
                    continue;
                };
                // A corrupt registry must never turn a group signal into a positive PID signal
                // through the u32 -> i32 cast (or overflow when negating i32::MIN).
                if pgid <= 1 || pgid > i32::MAX as u32 {
                    continue;
                }
                let ours = match start_time(pgid) {
                    Some(s) => s == lstart,     // the leader itself still runs (or its pid was reused)
                    None => group_exists(pgid), // leader gone, members left in its group
                };
                if ours && unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) } == 0 {
                    ended.push(pgid);
                }
            }
            let _ = std::fs::remove_file(&path);
        }
        ended
    }
}

/// Record (`live = true`) or drop a background process group in this process's registry.
pub fn set(pgid: u32, live: bool) {
    #[cfg(target_os = "macos")]
    imp::set(pgid, live);
    #[cfg(not(target_os = "macos"))]
    let _ = (pgid, live);
}

/// Remove this process's registry (normal exit, after its groups were killed).
pub fn clear() {
    #[cfg(target_os = "macos")]
    imp::clear();
}

/// End the process groups registered by mare processes that are gone (once per process, off the
/// startup path). Returns the pgids ended.
pub fn sweep_stale() -> Vec<u32> {
    #[cfg(target_os = "macos")]
    {
        imp::sweep_in(&imp::dir())
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![]
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::imp::*;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    fn group_child() -> std::process::Child {
        std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap()
    }

    fn exited_within(c: &mut std::process::Child, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if c.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A registry left by a dead mare: its group is ended, a group whose leader pid now has
    /// another start time (pid reused) is not, a live owner's registry is untouched.
    #[test]
    fn groups_of_a_dead_owner_are_ended_and_reused_pids_spared() {
        let dir = tempfile::tempdir().unwrap();
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        dead.wait().unwrap();
        let mut stale = group_child();
        let mut reused = group_child();
        let (sp, rp) = (stale.id(), reused.id());
        let (ss, rs) = (start_time(sp).unwrap(), start_time(rp).unwrap());
        std::fs::write(
            dir.path().join(dead.id().to_string()),
            format!("owner {} 1\n{sp} {ss}\n{rp} {}\n", dead.id(), rs + 1),
        )
        .unwrap();
        let me = std::process::id();
        let mut live_owner = group_child();
        let lp = live_owner.id();
        let live_file = dir.path().join("live");
        std::fs::write(
            &live_file,
            format!("owner {me} {}\n{lp} {}\n", start_time(me).unwrap(), start_time(lp).unwrap()),
        )
        .unwrap();

        assert_eq!(sweep_in(dir.path()), [sp]);
        assert!(exited_within(&mut stale, Duration::from_secs(5)), "the stale group was not ended");
        assert!(reused.try_wait().unwrap().is_none(), "a reused pid must be spared");
        assert!(live_owner.try_wait().unwrap().is_none(), "a running mare's groups are its own");
        assert!(!dir.path().join(dead.id().to_string()).exists(), "the dead owner's registry is removed");
        assert!(live_file.exists());
        for mut c in [reused, live_owner] {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    #[test]
    fn corrupt_group_ids_never_signal_an_unrelated_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut victim = group_child();
        let pid = victim.id();
        let forged = 0u32.wrapping_sub(pid);
        std::fs::write(dir.path().join("stale"), format!("owner 0 0\n{forged} 1\n{} 1\n", 1u32 << 31)).unwrap();
        assert!(sweep_in(dir.path()).is_empty());
        assert!(victim.try_wait().unwrap().is_none(), "a corrupt group ID signalled an unrelated child");
        let _ = victim.kill();
        let _ = victim.wait();
    }

    /// A [`crate::trees::Tree`] (every child mare starts): on disk while the group lives, removed
    /// when it ended normally, kept while a group the owner let go of (and ended) still dies.
    #[test]
    fn registered_groups_stay_on_disk_until_they_end() {
        let dir = tempfile::tempdir().unwrap();
        *TEST_DIR.lock().unwrap() = Some(dir.path().to_path_buf());
        let file = dir.path().join(std::process::id().to_string());
        let listed = |pid: u32| {
            std::fs::read_to_string(&file).is_ok_and(|t| t.lines().any(|l| l.starts_with(&format!("{pid} "))))
        };

        let mut done = group_child();
        let reg = crate::trees::Tree::new(Some(done.id()), None);
        assert!(listed(done.id()), "registered at spawn");
        let _ = done.kill();
        let _ = done.wait();
        drop(reg);
        assert!(!file.exists(), "a group that ended is removed (and an empty registry with it)");

        let mut left = group_child();
        // ended on drop, but not reaped yet: still registered for the next mare to end
        drop(crate::trees::Tree::new(Some(left.id()), None));
        assert!(listed(left.id()), "a group still running stays registered for the next mare to end");
        let _ = left.kill();
        let _ = left.wait();
        super::set(left.id(), false);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn end_all_graceful_removes_lingering_group_from_disk_immediately() {
        let dir = tempfile::tempdir().unwrap();
        *TEST_DIR.lock().unwrap() = Some(dir.path().to_path_buf());
        let file = dir.path().join(std::process::id().to_string());

        let lua = mlua::Lua::new();
        let owner = crate::trees::Owner::of(&lua);
        let mut child = group_child();
        let pid = child.id();
        let tree = crate::trees::Tree::new(Some(pid), Some(owner));
        assert!(file.exists(), "registered at spawn");
        tree.settle(false);

        let unconfirmed = crate::trees::end_all_graceful(&lua).await;
        assert!(unconfirmed.is_empty(), "expected clean stop, got {unconfirmed:?}");
        assert!(!file.exists(), "end_all_graceful must clear the crash-recovery registry file before returning");
        let _ = child.wait();
        *TEST_DIR.lock().unwrap() = None;
    }
}
