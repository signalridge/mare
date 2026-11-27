use super::*;
use std::path::Path;

use mlua::Value;

use super::access::host_of;
use super::caps::narrow;

#[test]
fn independent_lua_vms_keep_their_own_policies_when_threads_interleave() {
    use std::sync::{Arc, Barrier};
    let root = tempfile::tempdir().unwrap();
    let dirs: Vec<_> = (0..3)
        .map(|i| {
            let p = root.path().join(i.to_string());
            std::fs::create_dir(&p).unwrap();
            p
        })
        .collect();
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = dirs
        .into_iter()
        .enumerate()
        .map(|(i, dir)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let lua = Lua::new();
                let proc = lua.create_table().unwrap();
                install(&lua, &proc, dir.to_string_lossy().into_owned()).unwrap();
                lua.globals().set("sandbox", proc.get::<Table>("sandbox").unwrap()).unwrap();
                lua.globals().set("target", dir.join("result.txt").to_string_lossy().into_owned()).unwrap();
                if i == 0 {
                    lua.load(r#"sandbox.set_current({level="read-only"})"#).exec().unwrap();
                }
                barrier.wait();
                if i == 1 {
                    lua.load(r#"sandbox.set_current({level="workspace-write", network=true})"#).exec().unwrap();
                }
                barrier.wait();
                let after_first: String =
                    lua.load("local p = sandbox.current() return p and p.level or 'none'").eval().unwrap();
                let gen_before: u64 = lua.load("return sandbox.generation()").eval().unwrap();
                barrier.wait();
                if i == 2 {
                    lua.load("sandbox.set_current(false)").exec().unwrap();
                }
                barrier.wait();
                let after_second: String =
                    lua.load("local p = sandbox.current() return p and p.level or 'none'").eval().unwrap();
                let gen_after: u64 = lua.load("return sandbox.generation()").eval().unwrap();
                let can_write: bool = lua.load("return sandbox.check('write', target)").eval().unwrap();
                (after_first, after_second, gen_before, gen_after, can_write)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!((&*results[0].0, &*results[0].1), ("read-only", "read-only"), "parent policy changed");
    assert_eq!((&*results[1].0, &*results[1].1), ("workspace-write", "workspace-write"), "first child policy changed");
    assert_eq!(results[2].1, "none", "second child policy changed");
    assert_eq!(results[0].2, results[0].3, "a sibling must not invalidate parent approvals");
    assert_eq!(results[1].2, results[1].3, "a sibling must not invalidate child approvals");
    assert!(!results[0].4 && results[1].4 && results[2].4, "each VM checks its own policy");
}

fn ws(cwd: &Path, extra: &[&Path]) -> Policy {
    // network open: these tests are about file roots (a missing network would be blocked)
    let mut p = Policy::build(Level::WorkspaceWrite, cwd, None, &Value::Boolean(true)).unwrap();
    p.extra = extra.iter().map(|e| validate_root(e).unwrap()).collect();
    p.writable = p.compute_writable();
    p
}

#[test]
fn levels_and_markers() {
    use Level::*;
    assert_eq!(Level::parse("full-access").unwrap(), FullAccess);
    assert!(ReadOnly < WorkspaceWrite && WorkspaceWrite < FullAccess);
    assert_eq!(WorkspaceWrite.marker(), "[sandbox: file access denied under workspace-write mode]");
}

/// the MARE_SANDBOX_CAPS override is intersected with the probed set —
/// declaring a dimension the backend lacks never grants it.
#[test]
fn declared_capabilities_only_narrow_the_probed_set() {
    use Cap::*;
    // bubblewrap's probed set; the override names fs_read and net_proxy it can't enforce
    let bwrap = vec![FsWrite, NetBlock, UnixSocket];
    assert_eq!(narrow(bwrap.clone(), Some(&[FsRead, FsWrite, NetProxy])), vec![FsWrite]);
    assert_eq!(narrow(bwrap.clone(), Some(&[])), vec![]);
    assert_eq!(narrow(bwrap.clone(), None), bwrap);
    // process spawning is not a sandbox dimension: not a capability name
    assert!(Cap::parse_accept("spawn").is_err());
}

#[test]
fn extra_roots_must_be_physical_dirs() {
    let d = tempfile::tempdir().unwrap();
    let real = dunce::canonicalize(d.path()).unwrap();
    std::fs::create_dir(real.join("dir")).unwrap();
    std::fs::write(real.join("file"), "").unwrap();
    std::os::unix::fs::symlink(real.join("dir"), real.join("link")).unwrap();
    assert!(validate_root(&real.join("dir")).is_ok());
    for bad in ["file", "link", "missing"] {
        let e = validate_root(&real.join(bad)).unwrap_err();
        assert!(e.starts_with(SANDBOX_INVALID_ROOT), "{bad}: {e}");
    }
    assert!(validate_root(Path::new("relative/dir")).is_err());
    // swapped for a symlink between calls → refused at the next spawn
    let p = ws(&real, &[&real.join("dir")]);
    assert!(p.revalidate().is_ok());
    std::fs::rename(real.join("dir"), real.join("dir2")).unwrap();
    std::os::unix::fs::symlink(real.join("dir2"), real.join("dir")).unwrap();
    assert!(p.revalidate().is_err());
}

/// the base seccomp set holds even with no socket family blocked (peer batch five item 5)
#[test]
#[cfg(target_os = "linux")]
fn base_seccomp_refuses_ptrace_without_a_socket_block() {
    use std::os::unix::process::CommandExt;
    let f = seccomp_filter(SocketBlock::default()).unwrap();
    let mut c = std::process::Command::new("true");
    // SAFETY: only syscalls in the forked child before exec
    unsafe {
        c.pre_exec(move || {
            child_restrict(None, Some(&f))?;
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == 0 {
                return Err(std::io::Error::other("ptrace allowed"));
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EPERM) => Ok(()),
                _ => Err(std::io::Error::other("ptrace failed for another reason")),
            }
        });
    }
    assert!(c.status().unwrap().success());
}

#[test]
#[cfg(target_os = "linux")]
fn seccomp_allows_netlink_route_only_when_enabled_and_blocks_other_netlink_and_vsock() {
    use std::os::unix::process::CommandExt;
    let f = seccomp_filter(SocketBlock { unix: true, inet: false, netlink_route: true }).unwrap();
    let mut c = std::process::Command::new("true");
    // SAFETY: only syscalls in the forked child before exec
    unsafe {
        c.pre_exec(move || {
            child_restrict(None, Some(&f))?;
            let nl_route = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE);
            if nl_route < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(nl_route);
            if libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_KOBJECT_UEVENT) >= 0 {
                return Err(std::io::Error::other("NETLINK_KOBJECT_UEVENT allowed"));
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                return Err(std::io::Error::other("expected EPERM for non-route netlink"));
            }
            if libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) >= 0 {
                return Err(std::io::Error::other("AF_VSOCK allowed"));
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                return Err(std::io::Error::other("expected EPERM for AF_VSOCK"));
            }
            Ok(())
        });
    }
    assert!(c.status().unwrap().success());
}

#[test]
#[cfg(unix)]
fn link_dotdot_resolves_like_the_kernel_and_is_refused() {
    // ws/link -> outside/sub: `ws/link/../x` is outside/x for the kernel, so it must not be
    // checked as ws/x (peer review batch five #1:148-175)
    let d = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(d.path()).unwrap();
    let ws_dir = root.join("ws");
    let outside = root.join("outside");
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::fs::create_dir_all(&ws_dir).unwrap();
    std::os::unix::fs::symlink(outside.join("sub"), ws_dir.join("link")).unwrap();
    assert_eq!(resolve(&ws_dir.join("link/../x")).unwrap(), outside.join("x"));
    assert_eq!(resolve(&ws_dir.join("link/f")).unwrap(), outside.join("sub/f"));
    let p = ws(&ws_dir, &[]);
    // (the temp dir itself is writable in tests, so compare with the physical target: the
    // check must be the one for outside/x, not for ws/x)
    assert_eq!(p.can_write(&ws_dir.join("link/../x")), p.can_write(&outside.join("x")));
    assert!(!p.can_write(&ws_dir.join("link/../../../../../../../etc/mare-x")));
    assert!(!p.can_write(&ws_dir.join("plain/../x")), "a parent after a missing directory is ambiguous");
    // .GIT is.git where the file system is case-insensitive
    std::fs::create_dir_all(ws_dir.join(".git")).unwrap();
    if cfg!(target_os = "macos") {
        assert!(!p.can_write(&ws_dir.join(".GIT/config")));
    }
    assert!(!p.can_write(&ws_dir.join(".git/config")));
    // .mare is protected before it exists and after it is created.
    assert!(!p.can_write(&ws_dir.join(".mare/init.lua")));
    std::fs::create_dir(ws_dir.join(".mare")).unwrap();
    assert!(!p.can_write(&ws_dir.join(".mare/init.lua")));
}

#[cfg(unix)]
#[test]
fn symlinked_metadata_is_protected_after_path_resolution() {
    let d = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(d.path()).unwrap();
    std::fs::create_dir(root.join("actual-mare")).unwrap();
    std::fs::create_dir(root.join("actual-git")).unwrap();
    std::os::unix::fs::symlink(root.join("actual-mare"), root.join(".mare")).unwrap();
    std::os::unix::fs::symlink(root.join("actual-git"), root.join(".git")).unwrap();
    let policy = ws(&root, &[]);
    assert!(!policy.can_write(&root.join(".mare/init.lua")));
    assert!(!policy.can_write(&root.join("actual-mare/init.lua")));
    assert!(!policy.can_write(&root.join(".git/config")));
    assert!(!policy.can_write(&root.join("actual-git/config")));
}

#[test]
fn write_check_and_classify() {
    let d = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(d.path()).unwrap();
    let p = ws(&cwd, &[]);
    assert!(p.can_write(&cwd.join("new/dir/file.txt")));
    assert!(!p.can_write(Path::new("/etc/hosts")));
    assert!(resolve(&cwd.join("a/../b")).is_err(), "parent after a missing directory is rejected");
    assert!(!p.can_write(Path::new("/usr/local/../escape")));
    assert!(p.can_write(Path::new("/dev/null")));
    let d = p.check("write", "/etc/mare-x", &cwd).unwrap_err();
    // the resolved path (`/private/etc/…` on macOS) is named
    let named = d.path.clone().unwrap();
    assert!(named.ends_with("etc/mare-x"), "{named}");
    assert_eq!(p.denial_marker(&d), format!("[sandbox: file access denied under workspace-write mode: write {named}]"));
    let unnamed = Denial { path: None, ..d.clone() };
    assert_eq!(p.denial_marker(&unnamed), "[sandbox: file access denied under workspace-write mode]");
    let ro = p.with_level(Level::ReadOnly).unwrap();
    assert!(ro.check("write", "x.txt", &cwd).is_err());
    assert!(p.with_level(Level::FullAccess).is_none());
    let b = Backend::Seatbelt;
    let d = classify(&p, &cwd, "touch: /etc/mare-x: Operation not permitted", None, b).unwrap();
    assert_eq!((d.kind, d.path.as_deref()), ("write", Some("/etc/mare-x")));
    // bwrap reports EROFS
    let d = classify(&p, &cwd, "touch: cannot touch '/usr/x': Read-only file system", None, Backend::Bwrap);
    assert_eq!(d.unwrap().path.as_deref(), Some("/usr/x"));
    let d = classify(&p, &cwd, "touch: cannot touch '/usr/with space/x': Read-only file system", None, Backend::Bwrap);
    assert_eq!(d.as_ref().map(|d| (d.kind, d.path.as_deref())), Some(("write", Some("/usr/with space/x"))));
    let d = classify(&p, &cwd, "Read-only file system", None, Backend::Bwrap);
    assert_eq!(d.as_ref().map(|d| (d.kind, d.path.as_deref())), Some(("write", None)));
    for line in [
        "zsh: read-only file system: rel.txt",
        "bash: rel.txt: Read-only file system",
        "/bin/bash: line 2: rel.txt: Read-only file system",
        "/bin/sh: 1: cannot create rel.txt: Read-only file system",
        "touch: cannot touch 'rel.txt': Read-only file system",
    ] {
        let d = classify(&ro, &cwd, line, None, Backend::Bwrap).unwrap();
        assert_eq!(d.kind, "write", "{line}");
        assert_eq!(d.path.as_deref(), Some(cwd.join("rel.txt").to_str().unwrap()), "{line}");
    }
    // an EPERM inside the workspace is the program's problem, not the sandbox's
    assert!(classify(&p, &cwd, &format!("chmod: {}/f: Operation not permitted", cwd.display()), None, b).is_none());
    assert!(classify(&p, &cwd, "error: no such file", None, b).is_none());
    // open network: a refused connection is an ordinary failure
    assert!(classify(&p, &cwd, "curl: (7) Failed to connect to localhost port 1", None, b).is_none());
    assert_eq!(host_of("https://user@crates.io:443/api"), "crates.io");
    assert_eq!(host_of("[::1]:80"), "::1");
}

#[test]
fn bwrap_profile() {
    let d = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(d.path()).unwrap();
    let a = ws(&cwd, &[]).bwrap_args(&cwd).join(" ");
    assert!(
        a.starts_with(
            "--ro-bind / / --dev /dev --unshare-pid --proc /proc --die-with-parent --unshare-ipc --cap-drop ALL"
        ),
        "{a}"
    );
    assert!(!a.contains("--new-session"), "bwrap must not pass --new-session (breaks PTY ^C / job control): {a}");
    assert!(a.contains("--tmpfs /tmp"), "{a}");
    // the workspace bind comes after the /tmp tmpfs so a workspace under /tmp stays visible
    assert!(a.find("--tmpfs").unwrap() < a.find(&format!("--bind {}", cwd.display())).unwrap());
    let ro = ws(&cwd, &[]).with_level(Level::ReadOnly).unwrap().bwrap_args(&cwd).join(" ");
    assert!(!ro.contains("--tmpfs /tmp") && !ro.contains("--bind "), "{ro}");
    let token_file = mare_shared::paths::host_token_file();
    let host_dir = token_file.parent().unwrap().to_string_lossy();
    assert!(ro.contains(&format!("--perms 000 --tmpfs {host_dir} --remount-ro {host_dir}")), "{ro}");
}

/// Linux `net_proxy`: through the bridge the network namespace is unshared and the bridge's
/// paths are bound read-only after the /tmp tmpfs and before the read seal; without it (an
/// accepted partial policy) a filtered network keeps the host's namespace.
#[test]
fn bwrap_bridge_unshares_the_network_and_binds_before_the_seal() {
    let d = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(d.path()).unwrap();
    let lua = Lua::new();
    let t: Table = lua.load(r#"return { allow = { "example.com" } }"#).eval().unwrap();
    let mut p = ws(&cwd, &[]);
    p.network = Network::Proxy(NetPolicy::from_lua(&t).unwrap());
    let (sock, helper) = (Path::new("/run/user/1/mare-netproxy-1-0"), Path::new("/usr/bin/mare"));
    for restrict_read in [false, true] {
        p.restrict_read = restrict_read;
        let a = p.bwrap_args_with(&cwd, Some(&[sock, helper]));
        let at = |w: &[&str]| a.windows(w.len()).position(|x| x == w);
        let bind = at(&["--ro-bind", "/run/user/1/mare-netproxy-1-0", "/run/user/1/mare-netproxy-1-0"]).unwrap();
        assert!(at(&["--ro-bind", "/usr/bin/mare", "/usr/bin/mare"]).is_some(), "{a:?}");
        assert!(at(&["--unshare-net"]).is_some(), "{a:?}");
        assert!(at(&["--tmpfs", "/tmp"]).unwrap() < bind, "{a:?}");
        if let Some(seal) = at(&crate::read_mask::BWRAP_SEAL) {
            assert!(bind < seal, "{a:?}");
        }
    }
    p.restrict_read = false;
    assert!(!p.bwrap_args(&cwd).contains(&"--unshare-net".to_string()), "partial: no bridge, no unshare");
    p.network = Network::Proxy(NetPolicy::deny_all());
    assert!(p.bwrap_args(&cwd).contains(&"--unshare-net".to_string()));
}
