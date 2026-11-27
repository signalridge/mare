//! The runner's arguments and the executable that serves as the runner.

use std::path::PathBuf;

/// What the runner needs: the level, the capability SIDs to restrict to, the working directory
/// and the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerArgs {
    /// "read-only" | "workspace-write"
    pub mode: String,
    pub sids: Vec<String>,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    /// enhanced tier: run the inner runner as this sandbox account ("offline" | "online")
    pub as_user: Option<String>,
    /// enhanced tier, inner runner: the sandbox account's SID (`--account <SID>`). The runner
    /// checks it is the token user, then adds it to the restricting SIDs so MSYS2 (Git Bash) can
    /// open the signal pipe it creates with a {user, Administrators, SYSTEM} DACL. Never the real
    /// user's SID: that would make every place the user can write writable again.
    pub account: Option<String>,
    /// PTY mode (`--pty <cols>x<rows>`): the runner attaches the command to a ConPTY it creates
    /// and bridges it over its stdio (see [`crate::ptybridge`])
    pub pty: Option<(u16, u16)>,
}

impl RunnerArgs {
    /// Arguments after [`RUNNER_ARG`].
    ///
    /// [`RUNNER_ARG`]: crate::winsandbox::RUNNER_ARG
    pub fn to_args(&self) -> Vec<String> {
        let mut a = vec!["--mode".into(), self.mode.clone(), "--cwd".into(), self.cwd.to_string_lossy().into_owned()];
        if let Some(u) = &self.as_user {
            a.push("--as".into());
            a.push(u.clone());
        }
        if let Some(s) = &self.account {
            a.push("--account".into());
            a.push(s.clone());
        }
        if let Some((c, r)) = self.pty {
            a.push("--pty".into());
            a.push(format!("{c}x{r}"));
        }
        for s in &self.sids {
            a.push("--sid".into());
            a.push(s.clone());
        }
        a.push("--".into());
        a.extend(self.argv.iter().cloned());
        a
    }

    pub fn parse(raw: &[String]) -> Result<Self, String> {
        let (mut mode, mut cwd, mut sids, mut as_user, mut account, mut pty) = (None, None, vec![], None, None, None);
        let mut it = raw.iter();
        loop {
            let Some(k) = it.next() else { return Err("missing `--` before the command".into()) };
            if k == "--" {
                break;
            }
            let v = it.next().ok_or_else(|| format!("missing value after {k}"))?;
            match k.as_str() {
                "--mode" => mode = Some(v.clone()),
                "--cwd" => cwd = Some(PathBuf::from(v)),
                "--sid" => sids.push(v.clone()),
                "--as" if v == "offline" || v == "online" => as_user = Some(v.clone()),
                "--account" if v.starts_with("S-1-5-") => account = Some(v.clone()),
                "--pty" => pty = Some(crate::ptybridge::parse_size(v).ok_or_else(|| format!("bad --pty size {v}"))?),
                _ => return Err(format!("unknown argument {k}")),
            }
        }
        let argv: Vec<String> = it.cloned().collect();
        let mode = mode.ok_or("missing --mode")?;
        match mode.as_str() {
            "read-only" if sids.is_empty() && as_user.is_none() && account.is_none() => {}
            "read-only" if sids.len() == 1 && (as_user.is_some() || account.is_some()) => {}
            "read-only" => {
                return Err("enhanced read-only needs one ungranted capability; default read-only takes none".into());
            }
            "workspace-write" if !sids.is_empty() => {}
            "workspace-write" => return Err("workspace-write needs at least one --sid".into()),
            m => return Err(format!("unknown mode {m}")),
        }
        if argv.is_empty() {
            return Err("missing command after --".into());
        }
        if as_user.is_some() && account.is_some() {
            // the outer runner (the real user) never restricts to an account SID itself
            return Err("--account belongs to the inner runner, not with --as".into());
        }
        Ok(Self { mode, sids, cwd: cwd.ok_or("missing --cwd")?, argv, as_user, account, pty })
    }
}

fn find_helper_near(exe: &std::path::Path) -> Option<PathBuf> {
    let name = format!("mare{}", std::env::consts::EXE_SUFFIX);
    // Test binaries may live in `target/<profile>/deps` or nested `build/<crate>/<hash>/out`.
    for ancestor in exe.ancestors().skip(1).take(6) {
        for cand in [ancestor.join(&name), ancestor.join("deps").join(&name)] {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// The executable that serves as the runner: `MARE_SANDBOX_HELPER`, `CARGO_BIN_EXE_mare`, else
/// this process when it is `mare`, else a `mare` in an ancestor or `deps/` directory of this
/// executable (covering `cargo test` harnesses under `deps/` or `build-dir` layouts).
pub fn helper_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("MARE_SANDBOX_HELPER").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_mare").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mare"));
    if exe.file_stem().is_some_and(|s| s == "mare") {
        return exe;
    }
    find_helper_near(&exe).unwrap_or(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_helper_near_resolves_nested_build_dir_test_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let target_debug = dir.path().join("target").join("debug");
        let nested_out = target_debug.join("build").join("mare-cli").join("abc123").join("out");
        std::fs::create_dir_all(&nested_out).unwrap();
        let mare_bin = target_debug.join(format!("mare{}", std::env::consts::EXE_SUFFIX));
        let harness_bin = nested_out.join(format!("mare_cli-deadbeef{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&mare_bin, b"").unwrap();
        std::fs::write(&harness_bin, b"").unwrap();
        assert_eq!(find_helper_near(&harness_bin).as_deref(), Some(mare_bin.as_path()));
    }
}
