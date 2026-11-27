//! Shell discovery: which shell runs model commands and `!cmd`, plus PowerShell and
//! WSL path helpers. Everything here is a pure function over an injected environment so the
//! Windows rules are unit-tested on every platform.
//!
//! Lua: `mare.proc.shell{shell_path?, model?}` → `{path, args, transport = "argv"|"stdin", kind}`
//! or `nil, err` (the error lists every location checked; `model = true`: the default shell for
//! model commands under the current sandbox, see [`model_shell`]); `mare.proc.powershell()` →
//! `{path, args}` or `nil, err`; `mare.proc.wsl_path(p)` / `mare.proc.windows_path(p)`.

use std::path::{Path, PathBuf};

use mlua::{Lua, MultiValue, Table, Value};

/// How the command reaches the shell: as the last argument (`-c cmd`), or on stdin (the legacy
/// WSL launcher mangles quoting on its command line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Argv,
    Stdin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    pub path: String,
    /// arguments before the command (`-lc` / `-c`; `-s` for stdin transport)
    pub args: Vec<String>,
    pub transport: Transport,
    /// "bash" | "zsh" | "sh" | "wsl" (legacy launcher) | "powershell"
    pub kind: &'static str,
}

/// What discovery may look at; [`Env::system`] reads the real process environment.
pub struct Env<'a> {
    pub windows: bool,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// executables named `name` on PATH, in PATH order
    pub on_path: &'a dyn Fn(&str) -> Option<PathBuf>,
}

impl Env<'_> {
    pub fn system() -> Env<'static> {
        Env {
            windows: cfg!(windows),
            var: &|k| std::env::var(k).ok().filter(|v| !v.is_empty()),
            exists: &|p| p.is_file(),
            on_path: &|n| which::which(n).ok(),
        }
    }
}

fn lower_win(p: &str) -> String {
    p.replace('/', "\\").to_ascii_lowercase()
}

/// `C:\Windows\System32\bash.exe` (or `Sysnative`) is the legacy WSL launcher, not Git Bash.
pub fn is_legacy_wsl_launcher(path: &str, system_root: Option<&str>) -> bool {
    let p = lower_win(path);
    match system_root {
        Some(root) if root.len() >= 3 && root.as_bytes()[1] == b':' => {
            let root = lower_win(root.trim_end_matches(['\\', '/']));
            ["system32", "sysnative"].iter().any(|d| p == format!("{root}\\{d}\\bash.exe"))
        }
        Some(_) => false,
        None => {
            let b = p.as_bytes();
            b.len() > 3 && b[0].is_ascii_alphabetic() && &p[1..] == ":\\windows\\system32\\bash.exe"
                || b.len() > 3 && b[0].is_ascii_alphabetic() && &p[1..] == ":\\windows\\sysnative\\bash.exe"
        }
    }
}

fn kind_of(path: &str) -> &'static str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_ascii_lowercase();
    if name.starts_with("zsh") {
        "zsh"
    } else if name.starts_with("bash") {
        "bash"
    } else {
        "sh"
    }
}

fn bash_config(path: &str, env: &Env, login: bool) -> ShellConfig {
    if env.windows && is_legacy_wsl_launcher(path, (env.var)("SystemRoot").as_deref()) {
        return ShellConfig {
            path: path.to_string(),
            args: vec!["-s".into()],
            transport: Transport::Stdin,
            kind: "wsl",
        };
    }
    // Git Bash and MSYS bash run a login shell fine, but `-l` would cd to $HOME there; keep the
    // Unix behaviour (login shell: picks up mise/asdf/nvm from profiles) on Unix only
    let flag = if login && !env.windows { "-lc" } else { "-c" };
    ShellConfig { path: path.to_string(), args: vec![flag.into()], transport: Transport::Argv, kind: kind_of(path) }
}

/// Resolve the shell for model commands. `shell_path` is the user's setting (`tools.shell`).
///
/// - Unix: `shell_path`, else `$SHELL` when it is bash or zsh, else `/bin/bash`, `bash` on PATH,
///   then `sh`.
/// - Windows: `shell_path` → `%ProgramFiles%\Git\bin\bash.exe` →
///   `%ProgramFiles(x86)%\Git\bin\bash.exe` → `bash.exe` on PATH. The legacy WSL launcher gets
///   the command on stdin. Nothing found → an error listing every location checked.
pub fn find_shell(shell_path: Option<&str>, env: &Env) -> Result<ShellConfig, String> {
    let mut checked = vec![];
    if let Some(p) = shell_path.filter(|p| !p.is_empty()) {
        if (env.exists)(Path::new(p)) || (env.on_path)(p).is_some() {
            let p = if (env.exists)(Path::new(p)) {
                p.to_string()
            } else {
                (env.on_path)(p).map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string())
            };
            return Ok(bash_config(&p, env, true));
        }
        checked.push(format!("{p} (shell setting)"));
    }
    if env.windows {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = (env.var)(var) {
                let p = format!("{}\\Git\\bin\\bash.exe", root.trim_end_matches(['\\', '/']));
                if (env.exists)(Path::new(&p)) {
                    return Ok(bash_config(&p, env, true));
                }
                checked.push(p);
            }
        }
        if let Some(p) = (env.on_path)("bash.exe") {
            return Ok(bash_config(&p.to_string_lossy(), env, true));
        }
        checked.push("bash.exe on PATH".into());
        return Err(format!(
            "no bash found. Install Git for Windows (https://git-scm.com/download/win), put a bash \
             (MSYS2, Cygwin) on PATH, or set the shell with mare.plugin(\"tools\", {{ shell = \"C:\\\\path\\\\to\\\\bash.exe\" }}).\n\
             Checked:\n{}",
            checked.iter().map(|c| format!("  {c}")).collect::<Vec<_>>().join("\n")
        ));
    }
    if let Some(sh) = (env.var)("SHELL")
        && matches!(kind_of(&sh), "bash" | "zsh")
    {
        return Ok(bash_config(&sh, env, true));
    }
    if (env.exists)(Path::new("/bin/bash")) {
        return Ok(bash_config("/bin/bash", env, true));
    }
    if let Some(p) = (env.on_path)("bash") {
        return Ok(bash_config(&p.to_string_lossy(), env, true));
    }
    Ok(ShellConfig { path: "sh".into(), args: vec!["-c".into()], transport: Transport::Argv, kind: "sh" })
}

/// PowerShell arguments; the command follows `-Command`.
pub const POWERSHELL_ARGS: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"];

/// PowerShell 7 (`pwsh.exe` on PATH, then `%ProgramFiles%\PowerShell\7`), else Windows
/// PowerShell (`powershell.exe` on PATH, then `%SystemRoot%\System32\WindowsPowerShell\v1.0`).
pub fn find_powershell(env: &Env) -> Result<ShellConfig, String> {
    let cfg = |p: String| ShellConfig {
        path: p,
        args: POWERSHELL_ARGS.iter().map(|s| s.to_string()).collect(),
        transport: Transport::Argv,
        kind: "powershell",
    };
    let pwsh = if env.windows {
        (env.on_path)("pwsh.exe")
    } else {
        (env.on_path)("pwsh.exe").or_else(|| (env.on_path)("pwsh"))
    };
    if let Some(p) = pwsh {
        return Ok(cfg(p.to_string_lossy().into_owned()));
    }
    if let Some(pf) = (env.var)("ProgramFiles") {
        let p = format!("{}\\PowerShell\\7\\pwsh.exe", pf.trim_end_matches(['\\', '/']));
        if (env.exists)(Path::new(&p)) {
            return Ok(cfg(p));
        }
    }
    if let Some(p) = (env.on_path)("powershell.exe") {
        return Ok(cfg(p.to_string_lossy().into_owned()));
    }
    if let Some(root) = (env.var)("SystemRoot") {
        let p = format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", root.trim_end_matches(['\\', '/']));
        if (env.exists)(Path::new(&p)) {
            return Ok(cfg(p));
        }
    }
    Err("no PowerShell found: install PowerShell 7 or put pwsh.exe / powershell.exe on PATH".into())
}

/// The shell model commands default to: inside the Windows default-tier
/// sandbox (`mechanism` = "windows-acl", a confined policy in effect) Git Bash can't start — its
/// MSYS runtime is denied the signal pipe it creates — so the default is PowerShell; the enhanced
/// tier ("windows-user"), no sandbox, or a configured `shell_path` keep [`find_shell`]. With no
/// PowerShell on the host the bash answer stands (the bash tool then reports
/// SANDBOX_MSYS_UNSUPPORTED itself).
pub fn model_shell(shell_path: Option<&str>, env: &Env, mechanism: Option<&str>) -> Result<ShellConfig, String> {
    let configured = shell_path.is_some_and(|p| !p.is_empty());
    if env.windows
        && !configured
        && mechanism == Some("windows-acl")
        && let Ok(ps) = find_powershell(env)
    {
        return Ok(ps);
    }
    find_shell(shell_path, env)
}

/// `C:\Users\me\x` → `/mnt/c/Users/me/x` (only the default `/mnt/<drive>` mount root).
pub fn to_wsl_path(p: &str) -> Option<String> {
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let rest = p[2..].replace('\\', "/");
        let rest = rest.trim_start_matches('/');
        let drive = (b[0] as char).to_ascii_lowercase();
        return Some(if rest.is_empty() { format!("/mnt/{drive}") } else { format!("/mnt/{drive}/{rest}") });
    }
    None
}

/// `/mnt/c/Users/me/x` → `C:\Users\me\x`; anything outside `/mnt/<drive>` → None.
pub fn from_wsl_path(p: &str) -> Option<String> {
    let rest = p.strip_prefix("/mnt/")?;
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next()?;
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let tail = parts.next().unwrap_or("").replace('/', "\\");
    Some(format!("{}:\\{tail}", drive.to_ascii_uppercase()))
}

fn config_table(lua: &Lua, c: &ShellConfig) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("path", c.path.clone())?;
    t.set("args", c.args.clone())?;
    t.set("transport", if c.transport == Transport::Stdin { "stdin" } else { "argv" })?;
    t.set("kind", c.kind)?;
    Ok(t)
}

fn ret(lua: &Lua, r: Result<ShellConfig, String>) -> mlua::Result<MultiValue> {
    Ok(match r {
        Ok(c) => MultiValue::from_vec(vec![Value::Table(config_table(lua, &c)?)]),
        Err(e) => MultiValue::from_vec(vec![Value::Nil, Value::String(lua.create_string(e)?)]),
    })
}

pub fn install(lua: &Lua, proc_t: &Table) -> mlua::Result<()> {
    proc_t.set(
        "shell",
        lua.create_function(|lua, opts: Option<Table>| {
            let sp: Option<String> = opts.as_ref().map(|t| t.get("shell_path")).transpose()?.flatten();
            let model: bool =
                opts.as_ref().map(|t| t.get::<Option<bool>>("model")).transpose()?.flatten() == Some(true);
            if !model {
                return ret(lua, find_shell(sp.as_deref(), &Env::system()));
            }
            // the sandbox mechanism of the current policy (Windows only: nothing else changes)
            let mechanism = cfg!(windows)
                .then(|| crate::ossandbox::current(lua))
                .flatten()
                .filter(|p| p.level != crate::ossandbox::Level::FullAccess)
                .map(|_| crate::ossandbox::backend().name());
            ret(lua, model_shell(sp.as_deref(), &Env::system(), mechanism))
        })?,
    )?;
    proc_t.set(
        "powershell",
        lua.create_function(|lua, ()| {
            if !cfg!(windows) {
                return ret(lua, Err("the powershell tool is only available on Windows".into()));
            }
            ret(lua, find_powershell(&Env::system()))
        })?,
    )?;
    proc_t.set("wsl_path", lua.create_function(|_, p: String| Ok(to_wsl_path(&p)))?)?;
    proc_t.set("windows_path", lua.create_function(|_, p: String| Ok(from_wsl_path(&p)))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    type Fakes<'a> = (
        Box<dyn Fn(&str) -> Option<String> + 'a>,
        Box<dyn Fn(&Path) -> bool + 'a>,
        Box<dyn Fn(&str) -> Option<PathBuf> + 'a>,
    );

    fn win_env<'a>(vars: &'a HashMap<&str, &str>, files: &'a [&str], path: &'a HashMap<&str, &str>) -> Fakes<'a> {
        (
            Box::new(move |k: &str| vars.get(k).map(|v| v.to_string())),
            Box::new(move |p: &Path| files.iter().any(|f| Path::new(f) == p)),
            Box::new(move |n: &str| path.get(n).map(PathBuf::from)),
        )
    }

    #[test]
    fn windows_discovery_order_and_errors() {
        let vars = HashMap::from([
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("SystemRoot", r"C:\Windows"),
        ]);
        let none = HashMap::new();
        // Program Files (x86) Git Bash when the 64-bit one is missing
        let files = [r"C:\Program Files (x86)\Git\bin\bash.exe"];
        let (v, e, p) = win_env(&vars, &files, &none);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        let c = find_shell(None, &env).unwrap();
        assert_eq!((c.path.as_str(), c.kind, c.transport), (files[0], "bash", Transport::Argv));
        assert_eq!(c.args, ["-c"]);
        // only the legacy WSL launcher on PATH: used, with the command on stdin
        let path = HashMap::from([("bash.exe", r"C:\Windows\System32\bash.exe")]);
        let (v, e, p) = win_env(&vars, &[], &path);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        let c = find_shell(None, &env).unwrap();
        assert_eq!((c.kind, c.transport, c.args.clone()), ("wsl", Transport::Stdin, vec!["-s".to_string()]));
        // nothing at all: every checked location is listed
        let (v, e, p) = win_env(&vars, &[], &none);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        let err = find_shell(Some(r"D:\nope\bash.exe"), &env).unwrap_err();
        for want in [r"D:\nope\bash.exe", r"C:\Program Files\Git\bin\bash.exe", r"(x86)\Git\bin\bash.exe", "PATH"] {
            assert!(err.contains(want), "{want} missing from: {err}");
        }
    }

    #[test]
    fn legacy_launcher_detection() {
        assert!(is_legacy_wsl_launcher(r"C:\Windows\System32\bash.exe", Some(r"C:\Windows")));
        assert!(is_legacy_wsl_launcher(r"c:/windows/sysnative/BASH.EXE", Some(r"C:\Windows\")));
        assert!(is_legacy_wsl_launcher(r"D:\Windows\System32\bash.exe", None));
        assert!(!is_legacy_wsl_launcher(r"C:\Program Files\Git\bin\bash.exe", Some(r"C:\Windows")));
        assert!(!is_legacy_wsl_launcher(r"C:\Windows\System32\bash.exe", Some(r"D:\Win")));
    }

    #[test]
    fn powershell_prefers_pwsh() {
        let vars = HashMap::from([("SystemRoot", r"C:\Windows")]);
        let files = [r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"];
        let none = HashMap::new();
        let (v, e, p) = win_env(&vars, &files, &none);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        let c = find_powershell(&env).unwrap();
        assert_eq!(c.path, files[0]);
        assert_eq!(c.args, POWERSHELL_ARGS);
        let path = HashMap::from([("pwsh.exe", r"C:\tools\pwsh.exe")]);
        let (v, e, p) = win_env(&vars, &files, &path);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        assert_eq!(find_powershell(&env).unwrap().path, r"C:\tools\pwsh.exe");
        // On Windows, missing `pwsh.exe` must not fall back to bare `pwsh` (which scans PATHEXT).
        let probed = std::cell::RefCell::new(Vec::<String>::new());
        let on_path = |n: &str| {
            probed.borrow_mut().push(n.to_string());
            None
        };
        let (v, e, _) = win_env(&vars, &files, &none);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &on_path };
        assert_eq!(find_powershell(&env).unwrap().path, files[0]);
        assert!(
            !probed.borrow().iter().any(|n| n == "pwsh"),
            "Windows find_powershell must not probe bare `pwsh`: {:?}",
            probed.borrow()
        );
    }

    /// PowerShell by default inside the default-tier sandbox only; Git Bash with the
    /// enhanced tier, without a sandbox, or when the user set the shell.
    #[test]
    fn model_shell_is_powershell_only_in_the_default_tier_sandbox() {
        let vars = HashMap::from([("ProgramFiles", r"C:\Program Files"), ("SystemRoot", r"C:\Windows")]);
        let bash = r"C:\Program Files\Git\bin\bash.exe";
        let ps = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
        let none = HashMap::new();
        let files = [bash, ps];
        let (v, e, p) = win_env(&vars, &files, &none);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        let kind = |sp: Option<&str>, m: Option<&str>| model_shell(sp, &env, m).unwrap().kind;
        assert_eq!(kind(None, Some("windows-acl")), "powershell");
        assert_eq!(model_shell(None, &env, Some("windows-acl")).unwrap().path, ps);
        assert_eq!(kind(None, Some("windows-user")), "bash", "the enhanced tier runs Git Bash");
        assert_eq!(kind(None, None), "bash", "no sandbox: Git Bash as before");
        assert_eq!(kind(Some(bash), Some("windows-acl")), "bash", "a configured shell is the user's choice");
        // no PowerShell at all: the bash answer stands
        let only_bash = [bash];
        let (v, e, p) = win_env(&vars, &only_bash, &none);
        let env = Env { windows: true, var: &v, exists: &e, on_path: &p };
        assert_eq!(model_shell(None, &env, Some("windows-acl")).unwrap().kind, "bash");
        // never on Unix
        let (v, e, p) = win_env(&vars, &files, &none);
        let env = Env { windows: false, var: &v, exists: &e, on_path: &p };
        assert_ne!(model_shell(None, &env, Some("windows-acl")).unwrap().kind, "powershell");
    }

    #[test]
    fn wsl_paths() {
        assert_eq!(to_wsl_path(r"C:\Users\me\x y").as_deref(), Some("/mnt/c/Users/me/x y"));
        assert_eq!(to_wsl_path("D:").as_deref(), Some("/mnt/d"));
        assert_eq!(to_wsl_path("/home/me"), None);
        assert_eq!(from_wsl_path("/mnt/c/Users/me").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(from_wsl_path("/mnt/d").as_deref(), Some(r"D:\"));
        assert_eq!(from_wsl_path("/mnt/wsl/x"), None);
        assert_eq!(from_wsl_path("/home/me"), None);
    }

    #[test]
    fn unix_prefers_bash_or_zsh_shell() {
        let vars = HashMap::from([("SHELL", "/usr/bin/fish")]);
        let files = ["/bin/bash"];
        let none = HashMap::new();
        let (v, e, p) = win_env(&vars, &files, &none);
        let env = Env { windows: false, var: &v, exists: &e, on_path: &p };
        let c = find_shell(None, &env).unwrap();
        assert_eq!((c.path.as_str(), c.args.clone()), ("/bin/bash", vec!["-lc".to_string()]));
        let vars = HashMap::from([("SHELL", "/opt/zsh")]);
        let (v, e, p) = win_env(&vars, &files, &none);
        let env = Env { windows: false, var: &v, exists: &e, on_path: &p };
        assert_eq!(find_shell(None, &env).unwrap().kind, "zsh");
    }
}
