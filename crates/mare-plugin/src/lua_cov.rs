//! Runtime line-coverage collector for built-in preset Lua files.
//!
//! Enabled only when `MARE_LUA_COV_DIR` is set to a non-empty directory path. Each `Lua` VM
//! created via [`crate::new_lua`] attaches an `every_line` hook and flushes covered
//! `(repo_relative_path, lines)` records on VM drop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mlua::Lua;

static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct CovState {
    files: HashMap<String, HashSet<usize>>,
}

struct LuaCovGuard {
    dir: PathBuf,
    seq: u64,
    state: Arc<Mutex<CovState>>,
}

impl LuaCovGuard {
    fn flush(&self) {
        let Ok(guard) = self.state.lock() else { return };
        if guard.files.is_empty() {
            return;
        }
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let path = self.dir.join(format!("{}-{}.luacov", std::process::id(), self.seq));
        let mut out = String::new();
        for (file, lines) in &guard.files {
            if lines.is_empty() {
                continue;
            }
            let mut sorted: Vec<usize> = lines.iter().copied().collect();
            sorted.sort_unstable();
            out.push_str(file);
            out.push('\t');
            for (i, line) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&line.to_string());
            }
            out.push('\n');
        }
        let _ = std::fs::write(path, out);
    }
}

impl Drop for LuaCovGuard {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Flush any recorded Lua line coverage for `lua` to disk immediately.
pub(crate) fn flush(lua: &Lua) {
    if let Some(guard) = lua.app_data_ref::<LuaCovGuard>() {
        guard.flush();
    }
}

/// Map a Lua chunk `source` name to a repo-relative `crates/mare-plugin/preset/...` path.
pub(crate) fn normalize_preset_source(source: &str) -> Option<String> {
    let s = source.strip_prefix('@')?;
    if let Some(rest) = s.strip_prefix("preset/") {
        return Some(format!("crates/mare-plugin/preset/{rest}"));
    }
    if let Some(rest) = s.strip_prefix("builtin/lua/") {
        return Some(format!("crates/mare-plugin/preset/plugins/lua/{rest}"));
    }
    if let Some(rest) = s.strip_prefix("builtin/") {
        return Some(format!("crates/mare-plugin/preset/plugins/{rest}"));
    }
    let normalized = s.replace('\\', "/");
    if let Some(idx) = normalized.find("crates/mare-plugin/preset/") {
        return Some(normalized[idx..].to_string());
    }
    None
}

/// Attach a line coverage hook to `lua` if `MARE_LUA_COV_DIR` is set and non-empty.
pub(crate) fn maybe_attach(lua: &Lua) -> mlua::Result<()> {
    let Some(dir) = std::env::var_os("MARE_LUA_COV_DIR") else {
        return Ok(());
    };
    if dir.is_empty() {
        return Ok(());
    }
    attach_with_dir(lua, PathBuf::from(dir))
}

fn record_line(ctx: *mut std::ffi::c_void, source: &str, line: usize) {
    if let Some(path) = normalize_preset_source(source) {
        let state = unsafe { &*(ctx as *const Mutex<CovState>) };
        if let Ok(mut guard) = state.lock() {
            guard.files.entry(path).or_default().insert(line);
        }
    }
}

fn attach_with_dir(lua: &Lua, dir: PathBuf) -> mlua::Result<()> {
    let state = Arc::new(Mutex::new(CovState::default()));
    let ctx = Arc::as_ptr(&state) as *mut std::ffi::c_void;
    unsafe {
        mare_shared::lua::install_cov_hook(lua, record_line, ctx)?;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    lua.set_app_data(LuaCovGuard { dir, seq, state });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_preset_source_maps_known_prefixes() {
        assert_eq!(
            normalize_preset_source("@preset/ui_prelude.lua").as_deref(),
            Some("crates/mare-plugin/preset/ui_prelude.lua")
        );
        assert_eq!(
            normalize_preset_source("@builtin/agent.lua").as_deref(),
            Some("crates/mare-plugin/preset/plugins/agent.lua")
        );
        assert_eq!(
            normalize_preset_source("@builtin/lua/agent/loop.lua").as_deref(),
            Some("crates/mare-plugin/preset/plugins/lua/agent/loop.lua")
        );
        assert_eq!(
            normalize_preset_source("@/tmp/mare-cov/crates/mare-plugin/preset/plugins/llm.lua").as_deref(),
            Some("crates/mare-plugin/preset/plugins/llm.lua")
        );
        assert_eq!(normalize_preset_source("@core/freeze"), None);
        assert_eq!(normalize_preset_source("=eval"), None);
    }

    #[test]
    fn line_hook_collects_covered_lines_on_drop() {
        let dir = std::env::temp_dir().join(format!("mare-lua-cov-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let lua = Lua::new();
            attach_with_dir(&lua, dir.clone()).unwrap();
            lua.load("local a = 1\nlocal b = 2\nreturn a + b\n").set_name("@preset/test_cov.lua").exec().unwrap();
            let f = lua
                .load("local c = 3\nreturn c * 2\n")
                .set_name("@preset/test_thread_cov.lua")
                .into_function()
                .unwrap();
            let th = lua.create_thread(f).unwrap();
            mare_shared::lua::attach_cov_hook(&lua, &th);
            let res: i64 = th.resume(()).unwrap();
            assert_eq!(res, 6);
        }
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(entries.len(), 1);
        let text = std::fs::read_to_string(&entries[0]).unwrap();
        assert!(text.contains("crates/mare-plugin/preset/test_cov.lua\t1,2,3"), "{text}");
        assert!(text.contains("crates/mare-plugin/preset/test_thread_cov.lua\t1,2"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
