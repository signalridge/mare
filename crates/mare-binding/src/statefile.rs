//! `mare.state`: mare's own runtime state (`<state>/state.json`: the chosen model, effort,
//! onboarding, update check, permission preset …), shared by every builtin plugin.
//!
//! - `mare.state.read() -> table | nil, err` — `{}` when the file is missing; a file that isn't a
//!   JSON object is an error (never silently `{}`);
//! - `mare.state.update(fn) -> table` — read-modify-write under a cross-process lock
//!   (`state.json.lock`, 0600), written atomically (temp file + rename, 0600). `fn(state)` edits
//!   the table in place or returns a replacement. A corrupt file is copied to `state.json.bak` and
//!   left alone: the update fails instead of replacing every saved choice with one key;
//! - `mare.state.set(key, value)` — `update` for one key (`nil` removes it).
//!
//! Every settings write goes through one locked, atomic read-modify-write, and a file that
//! can't be parsed is never overwritten.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mlua::{Function, Lua, Table, Value};

use mare_shared::lua::{json_to_lua, json_to_lua_lossless, nil_err, rt_err, strict_lua_to_json};

/// Parse the state file: `Ok(None)` when it is missing, `Err` when it is corrupt.
pub fn read(path: &Path) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    mare_config::state::read(path)
}

/// 0700 / 0600 on Unix, private DACLs on Windows (`mare_shared::private`)
fn private_dir(dir: &Path) -> std::io::Result<()> {
    mare_shared::private::create_dir_all(dir)
}

fn open_private(path: &Path, create_new: bool) -> std::io::Result<std::fs::File> {
    if create_new {
        return mare_shared::private::create_new(path);
    }
    mare_shared::private::open(std::fs::OpenOptions::new().write(true).create(true).truncate(false), path)
}

fn lock(path: &Path) -> Result<std::fs::File, String> {
    let lp = path.with_file_name("state.json.lock");
    if let Some(d) = lp.parent() {
        private_dir(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let f = open_private(&lp, false).map_err(|e| format!("{}: {e}", lp.display()))?;
    let start = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(format!("{}: {e}", lp.display())),
        }
        if start.elapsed() >= Duration::from_secs(5) {
            return Err(format!("timed out waiting for the lock on {}", lp.display()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Write `value` to `path` atomically (exclusive 0600 temp file, fsync, rename).
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    mare_shared::paths::write_atomic(path, text.as_bytes(), mare_shared::paths::WriteMode::Private)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Keep a copy of a corrupt file next to it (once per distinct content) and say where.
fn corrupt_error(path: &Path, err: String) -> String {
    let bak = path.with_file_name("state.json.bak");
    if let Ok(cur) = std::fs::read(path)
        && std::fs::read(&bak).ok().as_deref() != Some(cur.as_slice())
    {
        let _ = mare_shared::paths::write_atomic(&bak, &cur, mare_shared::paths::WriteMode::Private);
    }
    format!("{err}: not overwritten — a copy is in {}; fix the file or delete it", bak.display())
}

/// Synchronous on purpose (callable from any plugin code, not only coroutines): the lock is held
/// for one small read-modify-write, and `fn` must not yield.
fn update(lua: &Lua, path: PathBuf, f: Function) -> mlua::Result<Value> {
    // Update also touches a lock, a possible corrupt backup and a temporary file. With a deny
    // policy, refuse rather than authorizing only state.json and silently touching adjacent paths.
    crate::exec_ctx::refuse_deny_read(lua, "mare.state.update")?;
    let guard = lock(&path).map_err(rt_err)?;
    let res = (|| {
        let cur = read(&path).map_err(|e| rt_err(corrupt_error(&path, e)))?.unwrap_or_default();
        let t = json_to_lua_lossless(lua, &serde_json::Value::Object(cur))?;
        let ret: Value = f.call(t.clone())?;
        let next = match ret {
            Value::Table(_) => ret,
            _ => t,
        };
        let mut j = strict_lua_to_json(lua, next.clone())?;
        let Some(obj) = j.as_object_mut() else {
            return Err(rt_err("mare.state.update: the state must be a table with string keys"));
        };
        if obj.contains_key("version") {
            return Err(rt_err("mare.state.update: 'version' is reserved for the state file format"));
        }
        obj.insert("version".into(), mare_config::state::VERSION.into());
        write_json_atomic(&path, &j).map_err(rt_err)?;
        Ok(next)
    })();
    let _ = guard.unlock();
    res
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "read",
        lua.create_function(|lua, ()| {
            let path = mare_shared::paths::state_file();
            crate::exec_ctx::check(lua, "read", &path.to_string_lossy())?;
            match read(&path) {
                Ok(m) => Ok(mlua::MultiValue::from_vec(vec![json_to_lua(
                    lua,
                    &serde_json::Value::Object(m.unwrap_or_default()),
                )?])),
                Err(e) => nil_err(lua, e),
            }
        })?,
    )?;
    t.set("update", lua.create_function(|lua, f: Function| update(lua, mare_shared::paths::state_file(), f))?)?;
    t.set(
        "set",
        lua.create_function(|lua, (key, value): (String, Value)| {
            if key == "version" {
                return Err(rt_err("mare.state.set: 'version' is reserved for the state file format"));
            }
            let f = lua.create_function(move |_, t: Table| {
                t.set(key.as_str(), value.clone())?;
                Ok(())
            })?;
            update(lua, mare_shared::paths::state_file(), f)?;
            Ok(())
        })?,
    )?;
    t.set("path", mare_shared::paths::state_file().to_string_lossy().into_owned())?;
    crate::state_namespace::install(lua, &t)?;
    mare.set("state", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_process_uses_nextest_private_home() {
        let home = PathBuf::from(std::env::var_os("MARE_HOME").expect("private MARE_HOME"));
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"));
        assert!(mare_shared::paths::host_token_file().starts_with(&home));
        assert!(mare_shared::paths::auth_file().starts_with(home));
    }

    /// Reject schema-invalid state files while preserving their bytes on disk.
    #[test]
    fn state_read_rejects_invalid_versions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let cases = [
            "",
            " ",
            r#"{}"#,
            r#"{"version":null}"#,
            r#"{"version":true}"#,
            r#"{"version":"1"}"#,
            r#"{"version":1.0}"#,
            r#"{"version":-1}"#,
            r#"{"version":0}"#,
            r#"{"version":2}"#,
        ];
        let mut accepted = Vec::new();
        for raw in cases {
            std::fs::write(&p, raw).unwrap();
            if read(&p).is_ok() {
                accepted.push(raw);
            }
            assert_eq!(std::fs::read_to_string(&p).unwrap(), raw);
        }
        assert!(accepted.is_empty(), "accepted invalid state versions: {accepted:?}");
    }

    #[test]
    fn corrupt_state_is_an_error_and_is_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        assert!(read(&p).unwrap().is_none(), "missing = empty");
        std::fs::write(&p, "{\"model\": ").unwrap();
        let e = read(&p).unwrap_err();
        let msg = corrupt_error(&p, e);
        assert!(msg.contains("not overwritten"), "{msg}");
        assert_eq!(std::fs::read_to_string(dir.path().join("state.json.bak")).unwrap(), "{\"model\": ");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"model\": ", "the original stays");
    }

    #[test]
    fn atomic_write_is_private() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        mare_shared::private::testing::open_to_everyone(dir.path());
        let p = dir.path().join("state.json");
        write_json_atomic(&p, &serde_json::json!({"version": 1, "a": 1})).unwrap();
        assert_eq!(read(&p).unwrap().unwrap()["a"], 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        #[cfg(windows)]
        assert_eq!(mare_shared::private::check(&p), Ok(()));
    }
}
