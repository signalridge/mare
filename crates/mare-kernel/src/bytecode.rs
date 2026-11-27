//! The Lua bytecode cache (Neovim `vim.loader` style) behind third-party and user Lua loading,
//! and its maintenance (`mare.loader.reset` / `prune`).

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mlua::{Function, Lua, Table};

const RACY_MTIME_WINDOW: Duration = Duration::from_secs(2);

/// Load a Lua file directly into an executable [`Function`] through the bytecode cache, scoped by
/// optional `owner_path`. Corrupt or incompatible cached bytecode falls back to compiling source
/// and replaces the cache entry atomically.
pub fn load_cached_chunk(
    lua: &Lua,
    owner_path: Option<&Path>,
    path: &Path,
    chunk_name: &str,
    env: Option<Table>,
) -> anyhow::Result<Function> {
    load_cached_chunk_at(lua, owner_path, path, chunk_name, env, &luac_dir())
}

fn is_racy_mtime(meta: &std::fs::Metadata) -> bool {
    let Ok(mtime) = meta.modified() else {
        return true;
    };
    match SystemTime::now().duration_since(mtime) {
        Ok(age) => age < RACY_MTIME_WINDOW,
        Err(_) => true,
    }
}

fn is_private_owned_dir(dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid || (meta.permissions().mode() & 0o077) != 0 {
            return false;
        }
    }
    #[cfg(windows)]
    if mare_shared::private::check(dir).is_err() {
        return false;
    }
    true
}

fn is_private_owned_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid || (meta.permissions().mode() & 0o077) != 0 {
            return false;
        }
    }
    #[cfg(windows)]
    if mare_shared::private::check(path).is_err() {
        return false;
    }
    true
}

fn has_non_dir_ancestor(path: &Path) -> bool {
    path.ancestors().skip(1).any(|ancestor| match std::fs::symlink_metadata(ancestor) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::metadata(ancestor).is_ok_and(|m| !m.is_dir()),
        Ok(meta) => !meta.is_dir(),
        Err(_) => false,
    })
}

fn read_cache_if_trusted(directory: &Path, cache: &Path) -> (Option<Vec<u8>>, bool) {
    match std::fs::symlink_metadata(directory) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !has_non_dir_ancestor(directory) => return (None, true),
        Ok(_) => {
            if !is_private_owned_dir(directory) {
                tracing::warn!(
                    path = %directory.display(),
                    "Lua bytecode cache directory is not private and user-owned; compiling source"
                );
                return (None, false);
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %directory.display(),
                error = %e,
                "cannot inspect Lua bytecode cache directory; compiling source"
            );
            return (None, false);
        }
    }
    match std::fs::symlink_metadata(cache) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, true),
        Ok(_) => {
            if !is_private_owned_file(cache) {
                tracing::warn!(
                    path = %cache.display(),
                    "Lua bytecode cache entry is not private and user-owned; compiling source"
                );
                return (None, false);
            }
        }
        Err(e) => {
            tracing::warn!(path = %cache.display(), error = %e, "cannot inspect Lua bytecode cache; compiling source");
            return (None, false);
        }
    }
    match std::fs::read(cache) {
        Ok(data) => (Some(data), true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, true),
        Err(e) => {
            tracing::warn!(path = %cache.display(), error = %e, "cannot read Lua bytecode cache; compiling source");
            (None, false)
        }
    }
}

struct SourceRead {
    bytes: Vec<u8>,
    stable: Option<(u128, u64)>,
}

fn read_source_with_meta(path: &Path) -> std::io::Result<SourceRead> {
    let mut file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata().ok();
    let stable = after
        .filter(|a| {
            !is_racy_mtime(a)
                && modified_nanos(a) == modified_nanos(&before)
                && a.len() == before.len()
                && a.len() == bytes.len() as u64
        })
        .map(|a| (modified_nanos(&a), a.len()));
    Ok(SourceRead { bytes, stable })
}

fn compile_source_fn(lua: &Lua, src: &[u8], chunk_name: &str, env: Option<Table>) -> anyhow::Result<Function> {
    let mut chunk = lua.load(src).set_mode(mlua::chunk::ChunkMode::Text).set_name(chunk_name);
    if let Some(env) = env {
        chunk = chunk.set_environment(env);
    }
    chunk.into_function().map_err(|e| anyhow::anyhow!("{e}"))
}

fn load_binary_fn(lua: &Lua, code: &[u8], chunk_name: &str, env: Option<Table>) -> mlua::Result<Function> {
    let mut chunk = lua.load(code).set_mode(mlua::chunk::ChunkMode::Binary).set_name(chunk_name);
    if let Some(env) = env {
        chunk = chunk.set_environment(env);
    }
    chunk.into_function()
}

fn write_cache_entry(cache: &Path, owner: &str, source: &str, mtime: u128, source_size: u64, code: &[u8]) {
    if let Some(d) = cache.parent()
        && mare_shared::private::create_dir_all(d).is_ok()
        && is_private_owned_dir(d)
    {
        let mut out = format!(
            "{CACHE_MAGIC} {} {} {mtime} {source_size} {} {} {} {:016x}\n",
            env!("CARGO_PKG_VERSION"),
            lua_version(),
            owner.len(),
            source.len(),
            code.len(),
            fnv(code),
        )
        .into_bytes();
        out.extend_from_slice(owner.as_bytes());
        out.extend_from_slice(source.as_bytes());
        out.extend_from_slice(code);
        let _ = mare_shared::paths::write_atomic_cache(cache, &out, mare_shared::paths::WriteMode::Private);
    }
}

fn load_cached_chunk_at(
    lua: &Lua,
    owner_path: Option<&Path>,
    path: &Path,
    chunk_name: &str,
    env: Option<Table>,
    directory: &Path,
) -> anyhow::Result<Function> {
    mare_shared::exec_ctx::check_process_path(lua, "read", &path.to_string_lossy())?;
    if mare_shared::exec_ctx::current(lua).is_some() {
        let src = std::fs::read(path)?;
        return compile_source_fn(lua, &src, chunk_name, env);
    }
    let meta = std::fs::metadata(path)?;
    let mtime = modified_nanos(&meta);
    let owner = owner_path.map(normalize_cache_path).unwrap_or_default();
    let source = normalize_cache_path(path);
    let cache = cache_path_for(directory, &owner, &source);
    let (cached_bytes, may_write) = read_cache_if_trusted(directory, &cache);
    if let Some(data) = cached_bytes
        && let Some(entry) = parse_entry(&data)
        && entry.owner == owner
        && entry.source == source
        && entry.mtime == mtime
        && entry.source_size == meta.len()
    {
        match load_binary_fn(lua, entry.code, chunk_name, env.clone()) {
            Ok(f) => return Ok(f),
            Err(e) => {
                tracing::warn!(
                    path = %cache.display(),
                    error = %e,
                    "corrupt Lua bytecode cache; recompiling from source"
                );
            }
        }
    }
    let read = read_source_with_meta(path)?;
    let f = compile_source_fn(lua, &read.bytes, chunk_name, env)?;
    if may_write && let Some((stable_mtime, stable_size)) = read.stable {
        let code = f.dump(false);
        write_cache_entry(&cache, &owner, &source, stable_mtime, stable_size, &code);
    }
    Ok(f)
}

const CACHE_MAGIC: &str = "mare-luac-v2";

fn lua_version() -> u32 {
    // SAFETY: Lua 5.5's lua_version ignores its state argument, including a null pointer.
    unsafe { mlua::ffi::lua_version(std::ptr::null_mut()) as u32 }
}

fn modified_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
}

struct CacheEntry<'a> {
    owner: &'a str,
    source: &'a str,
    mtime: u128,
    source_size: u64,
    code: &'a [u8],
}

fn parse_entry(data: &[u8]) -> Option<CacheEntry<'_>> {
    let header_end = data.iter().position(|&b| b == b'\n')?;
    let mut fields = std::str::from_utf8(&data[..header_end]).ok()?.split(' ');
    if fields.next()? != CACHE_MAGIC || fields.next()? != env!("CARGO_PKG_VERSION") {
        return None;
    }
    if fields.next()?.parse::<u32>().ok()? != lua_version() {
        return None;
    }
    let mtime = fields.next()?.parse().ok()?;
    let source_size = fields.next()?.parse().ok()?;
    let owner_len: usize = fields.next()?.parse().ok()?;
    let source_len: usize = fields.next()?.parse().ok()?;
    let code_len: usize = fields.next()?.parse().ok()?;
    let checksum = u64::from_str_radix(fields.next()?, 16).ok()?;
    if fields.next().is_some() {
        return None;
    }
    let payload = &data[header_end + 1..];
    let (owner, rest) = payload.split_at_checked(owner_len)?;
    let (source, code) = rest.split_at_checked(source_len)?;
    if code.len() != code_len || !code.starts_with(mlua::ffi::LUA_SIGNATURE) || fnv(code) != checksum {
        return None;
    }
    Some(CacheEntry {
        owner: std::str::from_utf8(owner).ok()?,
        source: std::str::from_utf8(source).ok()?,
        mtime,
        source_size,
        code,
    })
}

fn luac_dir() -> PathBuf {
    mare_shared::paths::cache_dir().join("luac")
}

#[cfg(test)]
fn cache_path(directory: &Path, source: &str) -> PathBuf {
    cache_path_for(directory, "", &normalize_cache_path(Path::new(source)))
}

fn cache_path_for(directory: &Path, owner: &str, source: &str) -> PathBuf {
    let mut h = fnv_update(0xcbf2_9ce4_8422_2325, env!("CARGO_PKG_VERSION").as_bytes());
    h = fnv_update(h, &[0]);
    h = fnv_update(h, &lua_version().to_le_bytes());
    h = fnv_update(h, &[0]);
    if !owner.is_empty() {
        h = fnv_update(h, owner.as_bytes());
        h = fnv_update(h, &[0]);
    }
    h = fnv_update(h, source.as_bytes());
    directory.join(format!("{h:016x}.luac"))
}

fn normalize_cache_path(path: &Path) -> String {
    if let Ok(canon) = dunce::canonicalize(path) {
        return canon.to_string_lossy().into_owned();
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

/// `mare.loader.reset([path])` (Neovim `vim.loader.reset`): drop the bytecode cache, or all
/// entries for one source file (including owner-scoped entries). Returns how many entries were
/// removed.
pub fn loader_reset(path: Option<&Path>) -> usize {
    loader_reset_at(&luac_dir(), path)
}

fn loader_reset_at(directory: &Path, path: Option<&Path>) -> usize {
    if let Some(p) = path {
        let raw_target = p.to_string_lossy();
        let norm_target = normalize_cache_path(p);
        let mut n = 0;
        let direct = cache_path_for(directory, "", &raw_target);
        if std::fs::remove_file(&direct).is_ok() {
            n += 1;
        }
        if norm_target != raw_target {
            let direct_norm = cache_path_for(directory, "", &norm_target);
            if std::fs::remove_file(&direct_norm).is_ok() {
                n += 1;
            }
        }
        for e in std::fs::read_dir(directory).into_iter().flatten().flatten() {
            let entry_path = e.path();
            if entry_path.extension().is_none_or(|x| x != "luac") {
                continue;
            }
            let Ok(data) = std::fs::read(&entry_path) else {
                continue;
            };
            if let Some(parsed) = parse_entry(&data)
                && (parsed.source == raw_target
                    || parsed.source == norm_target
                    || normalize_cache_path(Path::new(parsed.source)) == norm_target)
                && std::fs::remove_file(&entry_path).is_ok()
            {
                n += 1;
            }
        }
        return n;
    }
    let mut n = 0;
    for e in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "luac") && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// Drop cache entries that can't be used again: another mare version, or a source file that is
/// gone or changed (Neovim `vim.loader` keeps its cache from growing the same way). Returns how
/// many were removed.
pub fn loader_prune() -> usize {
    loader_prune_at(&luac_dir())
}

fn loader_prune_at(directory: &Path) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "luac") {
            // a temporary file of an interrupted write — only an old one: a fresh temp file is
            // a write in progress (this process at startup, or another mare), and removing it
            // makes that write's rename fail
            let stale = || {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(600))
            };
            if p.extension().is_some_and(|x| x.to_string_lossy().starts_with("tmp"))
                && stale()
                && std::fs::remove_file(&p).is_ok()
            {
                n += 1;
            }
            continue;
        }
        let data = match std::fs::read(&p) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "cannot read Lua bytecode cache while pruning");
                continue;
            }
        };
        let fresh = if let Some(entry) = parse_entry(&data) {
            let owner_ok = if entry.owner.is_empty() {
                true
            } else {
                match std::fs::metadata(entry.owner) {
                    Ok(_) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => {
                        tracing::warn!(path = %entry.owner, error = %e, "cannot inspect Lua owner while pruning cache");
                        continue;
                    }
                }
            };
            if !owner_ok {
                false
            } else {
                match std::fs::metadata(entry.source) {
                    Ok(meta) => modified_nanos(&meta) == entry.mtime && meta.len() == entry.source_size,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => {
                        tracing::warn!(path = %entry.source, error = %e, "cannot inspect Lua source while pruning cache");
                        continue;
                    }
                }
            }
        } else {
            false
        };
        if !fresh && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// [`loader_prune`] at most once a day (a `.pruned` stamp in the cache directory).
pub fn loader_prune_if_due() -> usize {
    loader_prune_if_due_at(&luac_dir())
}

/// Prune a cache directory captured by the host before submitting background work.
pub fn loader_prune_if_due_at(directory: &Path) -> usize {
    let stamp = directory.join(".pruned");
    let due = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age > Duration::from_secs(24 * 3600));
    if !due || !directory.is_dir() {
        return 0;
    }
    let n = loader_prune_at(directory);
    let _ = mare_shared::paths::write_atomic_cache(&stamp, b"", mare_shared::paths::WriteMode::Private);
    n
}

fn fnv_update(mut h: u64, b: &[u8]) -> u64 {
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn fnv(b: &[u8]) -> u64 {
    fnv_update(0xcbf2_9ce4_8422_2325, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = mare_shared::paths::temp_sibling(&std::env::temp_dir().join("mare-bytecode-test"));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write a source file with an mtime outside the 2-second racy window so caching applies.
    fn write_settled(path: &Path, content: impl AsRef<[u8]>) {
        write_at_age(path, content, 10);
    }

    fn write_at_age(path: &Path, content: impl AsRef<[u8]>, secs_ago: u64) {
        std::fs::write(path, content).unwrap();
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
    }

    fn load_cached_bytes(
        lua: &Lua,
        owner_path: Option<&Path>,
        path: &Path,
        chunk_name: &str,
        directory: &Path,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(load_cached_chunk_at(lua, owner_path, path, chunk_name, None, directory)?.dump(false))
    }

    #[test]
    fn truncated_cache_body_is_recompiled_from_source() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        let original = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        let entry = std::fs::read(&cache).unwrap();
        let body_start = entry.len() - original.len();
        std::fs::write(&cache, &entry[..body_start]).unwrap();

        let recovered = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        assert_eq!(recovered, original, "a truncated cache must be rebuilt from Lua source");
        assert_eq!(lua.load(&recovered).eval::<i64>().unwrap(), 42);
        assert!(std::fs::metadata(cache).unwrap().len() > body_start as u64);
    }

    #[test]
    fn corrupted_cache_body_is_recompiled_from_source() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        let original = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        let mut entry = std::fs::read(&cache).unwrap();
        let last = entry.len() - 1;
        entry[last] ^= 1;
        std::fs::write(&cache, entry).unwrap();

        let recovered = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        assert_eq!(recovered, original, "a flipped byte must not be returned as valid bytecode");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_cache_is_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        let original = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o000)).unwrap();
        let read = std::fs::read(&cache);
        if read.is_ok() {
            // A privileged process can bypass the permission bits.
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        assert_eq!(read.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);

        let recovered = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        assert_eq!(recovered, original);
        assert_eq!(std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0);
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn wrong_length_and_legacy_header_are_rebuilt() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        let original = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        let valid = std::fs::read(&cache).unwrap();
        let end = valid.iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&valid[..end]).unwrap();
        let alter_field = |index, replacement| {
            let mut fields: Vec<_> = header.split(' ').collect();
            fields[index] = replacement;
            let mut entry = fields.join(" ").into_bytes();
            entry.push(b'\n');
            entry.extend_from_slice(&valid[end + 1..]);
            entry
        };
        let wrong_length = alter_field(7, "1");
        let wrong_runtime = alter_field(2, "0");
        let source_meta = std::fs::metadata(&source).unwrap();
        let mut old_format = format!(
            "mare-luac {} {} {} {}\n",
            env!("CARGO_PKG_VERSION"),
            modified_nanos(&source_meta),
            source_meta.len(),
            source.display()
        )
        .into_bytes();
        old_format.extend_from_slice(&original);
        let source_str = source.to_string_lossy();
        let mut v1_format = format!(
            "mare-luac-v1 {} {} {} {} {} {} {:016x}\n",
            env!("CARGO_PKG_VERSION"),
            lua_version(),
            modified_nanos(&source_meta),
            source_meta.len(),
            source_str.len(),
            original.len(),
            fnv(&original),
        )
        .into_bytes();
        v1_format.extend_from_slice(source_str.as_bytes());
        v1_format.extend_from_slice(&original);

        for invalid in [&wrong_length, &wrong_runtime, &old_format, &v1_format] {
            std::fs::write(&cache, invalid).unwrap();
            assert_eq!(load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap(), original);
            assert_eq!(std::fs::read(&cache).unwrap(), valid, "invalid entry was not replaced");
        }
    }

    #[test]
    fn cache_hit_and_source_mtime_and_size_changes() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source with space.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        let original = load_cached_bytes(&lua, None, &source, "@first.lua", &cache_dir).unwrap();
        let direct = lua.load("return 42\n").set_name("@second.lua").into_function().unwrap().dump(false);
        assert_ne!(original, direct, "chunk names distinguish compiling from a cache hit");
        assert_eq!(load_cached_bytes(&lua, None, &source, "@second.lua", &cache_dir).unwrap(), original);

        write_at_age(&source, "return 43\n", 5);
        let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
        let changed = load_cached_bytes(&lua, None, &source, "@first.lua", &cache_dir).unwrap();
        assert_eq!(lua.load(&changed).eval::<i64>().unwrap(), 43);

        std::fs::write(&source, "return 444\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let resized = load_cached_bytes(&lua, None, &source, "@first.lua", &cache_dir).unwrap();
        assert_eq!(lua.load(&resized).eval::<i64>().unwrap(), 444);
    }

    #[test]
    fn prune_keeps_current_entries_and_removes_invalid_and_old_formats() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        load_cached_chunk_at(&lua, None, &source, "@source.lua", None, &cache_dir).unwrap();
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        let invalid = cache_dir.join("invalid.luac");
        let old = cache_dir.join("old.luac");
        let mut truncated = std::fs::read(&cache).unwrap();
        truncated.pop();
        std::fs::write(&invalid, truncated).unwrap();
        std::fs::write(&old, b"mare-luac 0.1.0 0 0 old-source\n").unwrap();

        assert_eq!(loader_prune_at(&cache_dir), 2);
        assert!(cache.is_file());
        assert!(!invalid.exists());
        assert!(!old.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_cache_directory_still_compiles_source() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let probe = cache_dir.join("probe");
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                // A privileged process can bypass the permission bits.
                std::fs::remove_file(probe).unwrap();
                std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                return;
            }
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied),
        }
        let lua = Lua::new();
        let result = load_cached_chunk_at(&lua, None, &source, "@source.lua", None, &cache_dir);
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let func = result.unwrap();
        assert_eq!(func.call::<i64>(()).unwrap(), 42);
        assert_eq!(std::fs::read_dir(cache_dir).unwrap().count(), 0);
    }

    #[test]
    fn luac_cache_dir_and_pruned_stamp_use_private_permissions() {
        let dir = TestDir::new();
        #[cfg(windows)]
        mare_shared::private::testing::open_to_everyone(dir.path());
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        load_cached_chunk_at(&lua, None, &source, "@source.lua", None, &cache_dir).unwrap();
        let _ = loader_prune_if_due_at(&cache_dir);
        let stamp = cache_dir.join(".pruned");
        assert!(stamp.is_file(), ".pruned stamp must be created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&cache_dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "luac cache dir must be 0700"
            );
            assert_eq!(
                std::fs::metadata(cache_dir.parent().unwrap()).unwrap().permissions().mode() & 0o777,
                0o700,
                "parent cache dir created by load_cached_chunk_at must be 0700"
            );
            assert_eq!(
                std::fs::metadata(&stamp).unwrap().permissions().mode() & 0o777,
                0o600,
                ".pruned stamp must be 0600"
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(mare_shared::private::check(&cache_dir), Ok(()));
            assert_eq!(mare_shared::private::check(&stamp), Ok(()));
        }
    }

    #[test]
    fn corrupt_bytecode_with_valid_header_and_checksum_is_recompiled_from_source() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();
        let original = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        let valid = std::fs::read(&cache).unwrap();

        // Craft a cache file whose header, LUA_SIGNATURE prefix, and FNV checksum are all valid,
        // yet whose Lua bytecode body fails to load in the VM.
        let bad_code = b"\x1bLua\x00corrupt-vm-bytecode-payload";
        let meta = std::fs::metadata(&source).unwrap();
        let source_str = normalize_cache_path(&source);
        let mut forged = format!(
            "{CACHE_MAGIC} {} {} {} {} 0 {} {} {:016x}\n",
            env!("CARGO_PKG_VERSION"),
            lua_version(),
            modified_nanos(&meta),
            meta.len(),
            source_str.len(),
            bad_code.len(),
            fnv(bad_code),
        )
        .into_bytes();
        forged.extend_from_slice(source_str.as_bytes());
        forged.extend_from_slice(bad_code);
        std::fs::write(&cache, forged).unwrap();

        let recovered = load_cached_bytes(&lua, None, &source, "@source.lua", &cache_dir).unwrap();
        assert_eq!(recovered, original, "unparseable Lua bytecode must fall back to compiling source");
        assert_eq!(lua.load(&recovered).eval::<i64>().unwrap(), 42);
        assert_eq!(std::fs::read(&cache).unwrap(), valid, "recompiled cache entry must overwrite the corrupt file");
    }

    #[test]
    fn owner_path_partitions_cache_entries_and_reset_clears_them() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let owner_a = dir.path().join("owner_a/lua");
        let owner_b = dir.path().join("owner_b/lua");
        std::fs::create_dir_all(&owner_a).unwrap();
        std::fs::create_dir_all(&owner_b).unwrap();
        let source = dir.path().join("shared.lua");
        write_settled(&source, "return 99\n");

        let lua = Lua::new();
        let code_a = load_cached_bytes(&lua, Some(&owner_a), &source, "@a.lua", &cache_dir).unwrap();
        let code_b = load_cached_bytes(&lua, Some(&owner_b), &source, "@b.lua", &cache_dir).unwrap();
        assert_ne!(
            code_a, code_b,
            "distinct owner paths must not share a cache entry (chunk names prove separate compilations)"
        );

        let luac_count = std::fs::read_dir(&cache_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "luac"))
            .count();
        assert_eq!(luac_count, 2, "each owner path gets its own .luac cache file");

        assert_eq!(loader_reset_at(&cache_dir, Some(&source)), 2);
        let remaining = std::fs::read_dir(&cache_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "luac"))
            .count();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn non_utf8_source_is_compiled_and_cached() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("raw_bytes.lua");
        let mut raw = b"-- raw bytes \xff\xfe\nreturn \"".to_vec();
        raw.extend_from_slice(&[0x80, 0xff]);
        raw.extend_from_slice(b"\"\n");
        write_settled(&source, &raw);

        let lua = Lua::new();
        let f1 = load_cached_chunk_at(&lua, None, &source, "@raw1.lua", None, &cache_dir).unwrap();
        let out1: mlua::LuaString = f1.call(()).unwrap();
        assert_eq!(out1.as_bytes(), &[0x80, 0xff]);

        // Second load hits the cache (chunk name stays @raw1.lua).
        let f2 = load_cached_chunk_at(&lua, None, &source, "@raw2.lua", None, &cache_dir).unwrap();
        assert_eq!(f2.info().source.as_deref(), Some("@raw1.lua"));
        let out2: mlua::LuaString = f2.call(()).unwrap();
        assert_eq!(out2.as_bytes(), &[0x80, 0xff]);
    }

    #[cfg(unix)]
    #[test]
    fn non_private_cache_dir_or_entry_is_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("source.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();

        // 1) Existing group/world-accessible cache dir is refused for both read and write.
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = load_cached_chunk_at(&lua, None, &source, "@open_dir.lua", None, &cache_dir).unwrap();
        assert_eq!(f.call::<i64>(()).unwrap(), 42);
        assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 0, "must not write into non-private cache dir");

        // 2) Tighten dir to 0700 so a valid entry is created, then widen entry to 0644 -> refused.
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let f_cached = load_cached_chunk_at(&lua, None, &source, "@first.lua", None, &cache_dir).unwrap();
        assert_eq!(f_cached.info().source.as_deref(), Some("@first.lua"));
        let cache = cache_path(&cache_dir, &source.to_string_lossy());
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();

        let f_recompiled = load_cached_chunk_at(&lua, None, &source, "@second.lua", None, &cache_dir).unwrap();
        assert_eq!(
            f_recompiled.info().source.as_deref(),
            Some("@second.lua"),
            "world/group-readable cache file must not be trusted; must compile from source"
        );
        assert_eq!(std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn recently_modified_source_is_not_cached() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("fresh.lua");
        std::fs::write(&source, "return 7\n").unwrap();

        let lua = Lua::new();
        let f = load_cached_chunk_at(&lua, None, &source, "@fresh.lua", None, &cache_dir).unwrap();
        assert_eq!(f.call::<i64>(()).unwrap(), 7);
        assert!(!cache_dir.exists(), "source modified within the 2s racy window must not write a cache entry");

        write_settled(&source, "return 7\n");
        let f2 = load_cached_chunk_at(&lua, None, &source, "@settled.lua", None, &cache_dir).unwrap();
        assert_eq!(f2.call::<i64>(()).unwrap(), 7);
        assert!(cache_path(&cache_dir, &source.to_string_lossy()).is_file());
    }

    #[test]
    fn loader_reset_normalizes_dotdot_paths() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let source = dir.path().join("target.lua");
        write_settled(&source, "return 11\n");

        let lua = Lua::new();
        load_cached_chunk_at(&lua, Some(&sub), &source, "@target.lua", None, &cache_dir).unwrap();
        let unnormalized = sub.join("../target.lua");
        assert_eq!(loader_reset_at(&cache_dir, Some(&unnormalized)), 1);
    }

    #[test]
    fn load_normalizes_owner_and_source_paths() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache").join("luac");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let source = dir.path().join("target.lua");
        write_settled(&source, "return 23\n");

        let unnormalized_owner = sub.join(".").join("..").join("sub");
        let unnormalized_source = sub.join("..").join("target.lua");
        let lua = Lua::new();
        let f1 =
            load_cached_chunk_at(&lua, Some(&unnormalized_owner), &unnormalized_source, "@first.lua", None, &cache_dir)
                .unwrap();
        assert_eq!(f1.call::<i64>(()).unwrap(), 23);

        // Loading via the clean paths must hit the exact same cache entry (chunk name stays @first.lua).
        let f2 = load_cached_chunk_at(&lua, Some(&sub), &source, "@second.lua", None, &cache_dir).unwrap();
        assert_eq!(f2.info().source.as_deref(), Some("@first.lua"));

        let luac_entries: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "luac"))
            .collect();
        assert_eq!(luac_entries.len(), 1, "unnormalized and normalized paths must share one .luac file");
        let raw = std::fs::read(&luac_entries[0]).unwrap();
        let entry = parse_entry(&raw).expect("valid .luac header");
        assert_eq!(entry.owner, normalize_cache_path(&sub));
        assert_eq!(entry.source, normalize_cache_path(&source));
    }

    #[test]
    fn concurrent_load_cached_chunk_writers_are_safe() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("concurrent.lua");
        write_settled(&source, "local x = 0\nfor i = 1, 10 do x = x + i end\nreturn x\n");

        let threads = 8usize;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (source, cache_dir, barrier) = (source.clone(), cache_dir.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let lua = Lua::new();
                    barrier.wait();
                    for _ in 0..12 {
                        let f = load_cached_chunk_at(&lua, None, &source, "@concurrent.lua", None, &cache_dir).unwrap();
                        let val: i64 = f.call(()).unwrap();
                        assert_eq!(val, 55);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let leftover_temps: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover_temps.is_empty(), "no temp files left behind: {leftover_temps:?}");
        let luac_count = std::fs::read_dir(&cache_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "luac"))
            .count();
        assert_eq!(luac_count, 1);
    }

    #[test]
    fn bytecode_cache_rejects_untrusted_entries_recovers_corrupt_files_and_prunes_stale() {
        let dir = TestDir::new();
        let cache_dir = dir.path().join("cache/luac");
        let source = dir.path().join("sample.lua");
        write_settled(&source, "return 42\n");
        let lua = Lua::new();

        // Missing paths are rejected by privacy checks; normalizing relative/./.. paths resolves cleanly.
        assert!(!is_private_owned_dir(&dir.path().join("missing_dir")));
        assert!(!is_private_owned_file(&dir.path().join("missing_file")));
        assert!(!is_private_owned_file(dir.path()));
        let norm = normalize_cache_path(Path::new("./a/../b/file.lua"));
        assert!(Path::new(&norm).ends_with(Path::new("b").join("file.lua")), "unexpected normalized path: {norm:?}");

        // Parent path that is a regular file fails symlink_metadata with ENOTDIR.
        struct AcceptAll;
        impl tracing::Subscriber for AcceptAll {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        tracing::subscriber::with_default(AcceptAll, || {
            let file_as_dir = dir.path().join("not_a_dir");
            std::fs::write(&file_as_dir, "x").unwrap();
            let (read_err, trusted_err) =
                read_cache_if_trusted(&file_as_dir.join("child"), &file_as_dir.join("child/x.luac"));
            assert!(read_err.is_none() && !trusted_err);

            // Compile and cache once, then overwrite with a syntactically valid cache envelope
            // containing corrupt Lua bytecode so load_binary_fn fails and recompiles from source.
            let _ = load_cached_chunk_at(&lua, None, &source, "@sample.lua", None, &cache_dir).unwrap();
            let cache = cache_path(&cache_dir, &source.to_string_lossy());
            assert!(cache.is_file());
            let meta = std::fs::metadata(&source).unwrap();
            let mtime = modified_nanos(&meta);
            let source_str = normalize_cache_path(&source);
            write_cache_entry(&cache, "", &source_str, mtime, meta.len(), b"not-valid-lua-bytecode");
            let f = load_cached_chunk_at(&lua, None, &source, "@sample.lua", None, &cache_dir).unwrap();
            assert_eq!(f.call::<i64>(()).unwrap(), 42);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();
                let (data, trusted) = read_cache_if_trusted(&cache_dir, &cache);
                assert!(data.is_none() && !trusted);
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        });

        // Write a stray corrupt .luac file and a stale .luac whose source was removed, then prune.
        let corrupt_stray = cache_dir.join("stray_corrupt.luac");
        std::fs::write(&corrupt_stray, b"bad").unwrap();
        let stale_src = dir.path().join("gone.lua");
        write_settled(&stale_src, "return 1\n");
        let _ = load_cached_chunk_at(&lua, None, &stale_src, "@gone.lua", None, &cache_dir).unwrap();
        std::fs::remove_file(&stale_src).unwrap();
        let pruned = loader_prune_at(&cache_dir);
        assert!(pruned >= 2, "expected corrupt and stale entries to be pruned, got {pruned}");
    }
}
