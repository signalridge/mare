//! Lua bytecode cache tests (`$XDG_CACHE_HOME/mare/luac`):
//! - built-in plugins/modules never write to the disk luac cache
//! - third-party/user plugin modules include the owner's path in the cache key
//! - source edits (mtime or size) invalidate the cache
//! - corrupted cache files (garbage, truncated, or invalid Lua VM bytecode) fall back to source
//! - user/project `init.lua` and `lua/` config modules use the cache, invalidate, and recover
//! - non-UTF-8 `init.lua` loads and caches without error
//! - untrusted projects neither execute nor populate `.mare` cache entries
//! - concurrent writers/readers share the cache directory safely

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::common::*;

/// Write a file with an mtime outside the 2-second racy window so the bytecode cache persists it.
fn write_settled(path: &Path, content: impl AsRef<[u8]>) {
    write_at_age(path, content, 10);
}

fn write_at_age(path: &Path, content: impl AsRef<[u8]>, secs_ago: u64) {
    std::fs::write(path, content).unwrap();
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

fn luac_files(luac_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(luac_dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "luac")).collect();
    out.sort();
    out
}

fn fnv64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn normalize_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Extract `(owner, source)` paths from a `mare-luac-v2` cache file header.
fn luac_header_paths(data: &[u8]) -> Option<(PathBuf, PathBuf)> {
    let end = data.iter().position(|&b| b == b'\n')?;
    let fields: Vec<&str> = std::str::from_utf8(&data[..end]).ok()?.split(' ').collect();
    if fields.len() != 9 || fields[0] != "mare-luac-v2" {
        return None;
    }
    let owner_len: usize = fields[5].parse().ok()?;
    let source_len: usize = fields[6].parse().ok()?;
    let payload = &data[end + 1..];
    let (owner, rest) = payload.split_at_checked(owner_len)?;
    let (source, _) = rest.split_at_checked(source_len)?;
    Some((PathBuf::from(std::str::from_utf8(owner).ok()?), PathBuf::from(std::str::from_utf8(source).ok()?)))
}

/// Built-in plugins and built-in `lua/` modules are precompiled in release builds and must never
/// populate the on-disk `$XDG_CACHE_HOME/mare/luac` bytecode cache.
#[tokio::test]
async fn builtin_plugins_and_modules_do_not_use_disk_luac_cache() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["images"]).no_session().build().await;
        let ok: bool = eval(&h, "return type(require('mare_lib')) == 'table'").await;
        assert!(ok);
        h.shutdown().await;

        let luac_dir = env.home.path().join("cache").join("luac");
        let files = luac_files(&luac_dir);
        assert!(files.is_empty(), "built-in plugins and modules must not write to cache/luac, found: {files:?}");
    })
    .await;
}

/// Third-party plugin modules cache bytecode under `$XDG_CACHE_HOME/mare/luac` with the owner's
/// directory path in the cache key, invalidate on both mtime and size edits, and can be cleared
/// via `mare.loader.reset(path)`.
#[tokio::test]
async fn third_party_plugin_module_luac_key_includes_owner_path_and_invalidates_on_edit() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = env.config_dir().join("plugins").join("alpha");
        let alpha_lua = alpha.join("lua");
        std::fs::create_dir_all(&alpha_lua).unwrap();
        let util = alpha_lua.join("util.lua");
        write_settled(&util, "return { ver = 1 }");
        write_settled(
            &alpha.join("init.lua"),
            r#"return { name = "alpha", apply = function()
                mare._luac_ver = require("util").ver
            end }"#,
        );

        let h = env.host().builtins(&[]).user_plugins().no_session().build().await;
        assert_eq!(eval::<i64>(&h, "return mare._luac_ver").await, 1);

        let luac_dir = env.home.path().join("cache").join("luac");
        let files = luac_files(&luac_dir);
        assert!(files.len() >= 2, "expected luac entries for plugin init.lua and lua/util.lua, found: {files:?}");

        let util_norm = normalize_path(&util);
        let owner_norm = normalize_path(&alpha_lua);
        let bare_util_cache = luac_dir.join(format!("{:016x}.luac", fnv64(util_norm.to_string_lossy().as_bytes())));
        assert!(
            !bare_util_cache.exists(),
            "owner-scoped module cache key must include the owner path, not bare file path alone"
        );
        let util_entry = files
            .iter()
            .find(|p| {
                std::fs::read(p)
                    .ok()
                    .and_then(|data| luac_header_paths(&data))
                    .is_some_and(|(owner, source)| owner == owner_norm && source == util_norm)
            })
            .cloned();
        assert!(
            util_entry.is_some(),
            "expected a luac entry recording both owner path `{}` and module path `{}`",
            owner_norm.display(),
            util_norm.display()
        );

        // 1) mtime change invalidates cache.
        write_at_age(&util, "return { ver = 2 }", 5);
        h.kernel.reload(Some("alpha")).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._luac_ver").await, 2);

        // 2) same mtime + different file size invalidates cache.
        let same_mtime = std::fs::metadata(&util).unwrap().modified().unwrap();
        std::fs::write(&util, "return { ver = 333 }").unwrap();
        std::fs::File::options().write(true).open(&util).unwrap().set_modified(same_mtime).unwrap();
        h.kernel.reload(Some("alpha")).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._luac_ver").await, 333);

        // 3) mare.loader.reset(path) drops owner-scoped entries for that source file.
        global(&h, "UTIL_PATH", util.to_string_lossy().into_owned());
        let removed: i64 = eval(&h, "return mare.loader.reset(UTIL_PATH)").await;
        assert_eq!(removed, 1, "loader.reset(path) must drop owner-scoped cache entry");
        h.shutdown().await;
    })
    .await;
}

/// Corrupted cache entries (both truncated garbage and header/checksum-valid invalid Lua bytecode)
/// must fall back to compiling from source and heal the cache file on disk.
#[tokio::test]
async fn third_party_module_corrupt_luac_falls_back_to_source() {
    local(async {
        let env = TestEnv::new().await;
        let plug = env.config_dir().join("plugins").join("corruptor");
        let plug_lua = plug.join("lua");
        std::fs::create_dir_all(&plug_lua).unwrap();
        let helper = plug_lua.join("helper.lua");
        write_settled(&helper, "return { val = 77 }");
        write_settled(
            &plug.join("init.lua"),
            r#"return { name = "corruptor", apply = function()
                mare._luac_val = require("helper").val
            end }"#,
        );

        let h1 = env.host().builtins(&[]).user_plugins().no_session().build().await;
        assert_eq!(eval::<i64>(&h1, "return mare._luac_val").await, 77);
        h1.shutdown().await;

        let luac_dir = env.home.path().join("cache").join("luac");
        let helper_norm = normalize_path(&helper);
        let helper_cache = luac_files(&luac_dir)
            .into_iter()
            .find(|p| {
                std::fs::read(p)
                    .ok()
                    .and_then(|data| luac_header_paths(&data))
                    .is_some_and(|(_, source)| source == helper_norm)
            })
            .expect("helper.lua must have a luac cache file");
        let valid_bytes = std::fs::read(&helper_cache).unwrap();

        // 1) Truncated / garbage bytes fall back to source and rebuild.
        std::fs::write(&helper_cache, b"corrupt-garbage").unwrap();
        let h2 = env.host().builtins(&[]).user_plugins().no_session().build().await;
        assert_eq!(eval::<i64>(&h2, "return mare._luac_val").await, 77);
        h2.shutdown().await;
        assert_eq!(std::fs::read(&helper_cache).unwrap(), valid_bytes);

        // 2) Valid header + valid FNV checksum over invalid Lua VM bytecode falls back to source.
        let nl = valid_bytes.iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&valid_bytes[..nl]).unwrap();
        let mut fields: Vec<String> = header.split(' ').map(str::to_string).collect();
        let orig_code_len: usize = fields[fields.len() - 2].parse().unwrap();
        let prefix_len = valid_bytes.len() - orig_code_len;
        let bad_code = b"\x1bLua\x00corrupt-vm-bytecode-payload";
        let code_len_idx = fields.len() - 2;
        let checksum_idx = fields.len() - 1;
        fields[code_len_idx] = bad_code.len().to_string();
        fields[checksum_idx] = format!("{:016x}", fnv64(bad_code));
        let mut forged = fields.join(" ").into_bytes();
        forged.push(b'\n');
        forged.extend_from_slice(&valid_bytes[nl + 1..prefix_len]);
        forged.extend_from_slice(bad_code);
        std::fs::write(&helper_cache, forged).unwrap();

        let h3 = env.host().builtins(&[]).user_plugins().no_session().build().await;
        assert_eq!(eval::<i64>(&h3, "return mare._luac_val").await, 77);
        h3.shutdown().await;
        assert_eq!(
            std::fs::read(&helper_cache).unwrap(),
            valid_bytes,
            "corrupt VM bytecode must be replaced with valid recompiled cache"
        );
    })
    .await;
}

/// User `init.lua`, `<config>/lua/` modules, project `.mare/init.lua`, and `.mare/lua/` modules
/// use the `$XDG_CACHE_HOME/mare/luac` bytecode cache, invalidate on edit, and rewrite corrupted
/// cache files on recovery.
#[tokio::test]
async fn user_init_and_config_modules_use_luac_cache_invalidate_and_recover() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir();
        let project = env.cwd.join(".mare");
        std::fs::create_dir_all(user.join("lua")).unwrap();
        std::fs::create_dir_all(project.join("lua")).unwrap();
        let user_helper = user.join("lua").join("cfg_helper.lua");
        let proj_helper = project.join("lua").join("proj_helper.lua");
        let user_init = user.join("init.lua");
        let proj_init = project.join("init.lua");

        write_settled(&user_helper, "return { editor = 'ed-v1' }");
        write_settled(&proj_helper, "return { theme = 'th-v1' }");
        write_settled(&user_init, "mare.setup { editor = require('cfg_helper').editor }");
        write_settled(&proj_init, "mare.setup { theme_name = require('proj_helper').theme }");

        let cfg1 = mare_plugin::config_load::load(&env.cwd, true).await.unwrap();
        assert_eq!(cfg1.value["editor"], "ed-v1");
        assert_eq!(cfg1.value["theme_name"], "th-v1");

        let luac_dir = env.home.path().join("cache").join("luac");
        let files = luac_files(&luac_dir);
        let mut original_bytes = Vec::new();
        for expected in [&user_init, &user_helper, &proj_init, &proj_helper] {
            let expected_norm = normalize_path(expected);
            let entry = files
                .iter()
                .find(|p| {
                    std::fs::read(p)
                        .ok()
                        .and_then(|data| luac_header_paths(&data))
                        .is_some_and(|(_, source)| source == expected_norm)
                })
                .cloned()
                .unwrap_or_else(|| {
                    panic!("expected luac cache entry for `{}`, found: {files:?}", expected_norm.display())
                });
            let raw = std::fs::read(&entry).unwrap();
            original_bytes.push((entry, raw));
        }

        // Corrupt every config luac file and verify source fallback succeeds AND rewrites each file.
        for (f, _) in &original_bytes {
            std::fs::write(f, b"corrupt-config-luac").unwrap();
        }
        let cfg2 = mare_plugin::config_load::load(&env.cwd, true).await.unwrap();
        assert_eq!(cfg2.value["editor"], "ed-v1");
        assert_eq!(cfg2.value["theme_name"], "th-v1");
        for (f, expected_raw) in &original_bytes {
            assert_eq!(
                std::fs::read(f).unwrap(),
                *expected_raw,
                "corrupted config luac entry must be rewritten: {}",
                f.display()
            );
        }

        // Edit user helper (mtime bump) and project helper (same mtime, different size) -> invalidate.
        write_at_age(&user_helper, "return { editor = 'ed-v2' }", 5);
        let proj_mtime = std::fs::metadata(&proj_helper).unwrap().modified().unwrap();
        std::fs::write(&proj_helper, "return { theme = 'th-v2-resized' }").unwrap();
        std::fs::File::options().write(true).open(&proj_helper).unwrap().set_modified(proj_mtime).unwrap();

        let cfg3 = mare_plugin::config_load::load(&env.cwd, true).await.unwrap();
        assert_eq!(cfg3.value["editor"], "ed-v2");
        assert_eq!(cfg3.value["theme_name"], "th-v2-resized");
    })
    .await;
}

/// Non-UTF-8 `init.lua` files (valid in Lua) must load and cache cleanly without UTF-8 errors.
#[tokio::test]
async fn non_utf8_init_lua_loads_and_caches_cleanly() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir();
        std::fs::create_dir_all(&user).unwrap();
        let user_init = user.join("init.lua");
        let mut raw = b"-- non-utf8 comment: \xff\xfe\x80\n".to_vec();
        raw.extend_from_slice(b"mare.setup { editor = 'raw-bytes-ok' }\n");
        write_settled(&user_init, &raw);

        let cfg1 = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert_eq!(cfg1.value["editor"], "raw-bytes-ok");

        let luac_dir = env.home.path().join("cache").join("luac");
        let files = luac_files(&luac_dir);
        let init_norm = normalize_path(&user_init);
        assert!(
            files.iter().any(|p| {
                std::fs::read(p)
                    .ok()
                    .and_then(|data| luac_header_paths(&data))
                    .is_some_and(|(_, source)| source == init_norm)
            }),
            "expected luac cache entry for non-UTF-8 init.lua"
        );

        let cfg2 = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert_eq!(cfg2.value["editor"], "raw-bytes-ok");
    })
    .await;
}

/// Untrusted projects must neither execute `.mare/init.lua` nor create any `.mare` cache entries.
#[tokio::test]
async fn untrusted_project_neither_runs_nor_caches_project_lua() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir();
        let project = env.cwd.join(".mare");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(project.join("lua")).unwrap();
        write_settled(&user.join("init.lua"), "mare.setup { editor = 'user-only' }");
        write_settled(&project.join("lua").join("evil.lua"), "return { theme = 'untrusted' }");
        write_settled(&project.join("init.lua"), "mare.setup { theme_name = require('evil').theme }");

        let cfg = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert_eq!(cfg.value["editor"], "user-only");
        assert!(cfg.value["theme_name"].is_null());

        let luac_dir = env.home.path().join("cache").join("luac");
        let proj_norm = normalize_path(&project);
        for f in luac_files(&luac_dir) {
            let data = std::fs::read(&f).unwrap_or_default();
            if let Some((owner, source)) = luac_header_paths(&data) {
                assert!(
                    !source.starts_with(&proj_norm) && !owner.starts_with(&proj_norm),
                    "untrusted project must not produce cache entries in luac: {}",
                    f.display()
                );
            }
        }
    })
    .await;
}

/// Concurrent threads loading the same third-party Lua module into a shared luac cache directory
/// must never fail, tear bytecode, or leave temp files behind.
#[tokio::test]
async fn concurrent_third_party_module_loads_share_luac_cache_safely() {
    local(async {
        let env = TestEnv::new().await;
        let plug = env.config_dir().join("plugins").join("racer");
        let plug_lua = plug.join("lua");
        std::fs::create_dir_all(&plug_lua).unwrap();
        let mod_file = plug_lua.join("calc.lua");
        write_settled(&mod_file, "local t = 0\nfor i = 1, 20 do t = t + i end\nreturn { sum = t }\n");
        write_settled(
            &plug.join("init.lua"),
            r#"return { name = "racer", apply = function()
                mare._luac_sum = require("calc").sum
            end }"#,
        );

        let threads = 6usize;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (plug_dir, barrier) = (plug.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let ls = tokio::task::LocalSet::new();
                    barrier.wait();
                    rt.block_on(ls.run_until(async {
                        for _ in 0..4 {
                            let k = mare_kernel::Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
                            let mare = k.lua().create_table().unwrap();
                            let task = k.lua().create_table().unwrap();
                            task.set(
                                "token",
                                k.lua()
                                    .create_function(|_, ()| {
                                        Ok(mare_shared::lua::Token(tokio_util::sync::CancellationToken::new()))
                                    })
                                    .unwrap(),
                            )
                            .unwrap();
                            task.set("clock", k.lua().create_function(|_, ()| Ok(0.0_f64)).unwrap()).unwrap();
                            mare.set("task", task).unwrap();
                            mare.set("log", k.lua().create_function(|_, _: mlua::MultiValue| Ok(())).unwrap()).unwrap();
                            k.lua().globals().set("mare", mare).unwrap();

                            let src = mare_kernel::PluginSource {
                                origin: mare_kernel::Origin::User,
                                path: Some(plug_dir.clone()),
                                code: None,
                                name_hint: "racer".to_string(),
                            };
                            let def = k.load_def(&src).unwrap();
                            k.set_defs(vec![def]);
                            k.start().await.unwrap();
                            let sum: i64 = k.lua().load("return mare._luac_sum").eval().unwrap();
                            assert_eq!(sum, 210);
                        }
                    }));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let luac_dir = env.home.path().join("cache").join("luac");
        let temps: Vec<_> = std::fs::read_dir(&luac_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "concurrent luac writes left temp files: {temps:?}");
        assert!(luac_files(&luac_dir).len() >= 2);
    })
    .await;
}
