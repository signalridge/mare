//! Startup process hardening (Apache-2.0): no core dumps, no debugger attach (Linux: non-dumpable; macOS
//! release builds: PT_DENY_ATTACH) and no `LD_*` / `DYLD_*` injection into mare or the commands
//! it starts. mare holds credentials in memory, so other processes of the same user shouldn't be
//! able to read them out. Failures are logged, never fatal. `MARE_NO_HARDENING=1` skips it all
//! (debugging, profilers).

/// Must run before any other thread exists (it edits the process environment).
pub fn apply() {
    let skip = std::env::var_os("MARE_NO_HARDENING").is_some_and(|v| !v.is_empty() && v != "0");
    #[cfg(unix)]
    if !skip {
        unix();
    }
    #[cfg(not(unix))]
    let _ = skip;
}

#[cfg(unix)]
fn unix() {
    // core file size 0: a crash must not write memory (tokens) to disk
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) } != 0 {
        eprintln!("mare: setrlimit(RLIMIT_CORE, 0) failed: {}", std::io::Error::last_os_error());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        eprintln!("mare: prctl(PR_SET_DUMPABLE, 0) failed: {}", std::io::Error::last_os_error());
    }
    // debug builds stay debuggable
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    if unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0) } != 0 {
        eprintln!("mare: ptrace(PT_DENY_ATTACH) failed: {}", std::io::Error::last_os_error());
    }
    let prefixes: &[&[u8]] = if cfg!(target_os = "macos") { &[b"DYLD_", b"LD_"] } else { &[b"LD_"] };
    remove_env_with_prefix(prefixes);
}

#[cfg(unix)]
fn remove_env_with_prefix(prefixes: &[&[u8]]) {
    use std::os::unix::ffi::OsStrExt;
    let keys: Vec<_> =
        std::env::vars_os().map(|(k, _)| k).filter(|k| prefixes.iter().any(|p| k.as_bytes().starts_with(p))).collect();
    for k in keys {
        // SAFETY: called at the top of main, before any thread is spawned
        unsafe { std::env::remove_var(k) };
    }
}
