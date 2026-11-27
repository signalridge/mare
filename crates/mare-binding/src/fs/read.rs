// exec-ctx: not a tool entry (read helpers split from fs.rs; gated by callers in fs/lua_api.rs and store/attachments.rs)
//! File reading and binary sniffing helpers for `mare.fs`.

use std::path::Path;

/// A binary format recognized by its magic number.
pub(super) fn sniff_kind(head: &[u8]) -> Option<&'static str> {
    const MAGIC: &[(&[u8], &str)] = &[
        (b"\x89PNG", "PNG image"),
        (b"\xff\xd8\xff", "JPEG image"),
        (b"GIF8", "GIF image"),
        (b"%PDF", "PDF document"),
        (b"PK\x03\x04", "zip archive"),
        (b"\x1f\x8b", "gzip archive"),
        (b"\x7fELF", "ELF executable"),
        (b"\xcf\xfa\xed\xfe", "Mach-O executable"),
        (b"\xca\xfe\xba\xbe", "Mach-O universal binary"),
        (b"MZ", "Windows executable"),
        (b"\0asm", "WebAssembly module"),
        (b"SQLite format 3\0", "SQLite database"),
        (b"(\xb5/\xfd", "zstd archive"),
    ];
    MAGIC.iter().find(|(m, _)| head.starts_with(m)).map(|(_, k)| *k)
}

const MAX_WINDOW_LINE_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_SNIFF_BYTES: usize = 8192;

pub(super) fn open_regular_nonblocking(path: &Path, shown: &str) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?
    };
    #[cfg(not(unix))]
    let f = std::fs::File::open(path)?;
    let meta = f.metadata()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(std::io::Error::other(format!("cannot read {shown}: not a regular file")));
    }
    Ok(f)
}

/// Read an already-opened file, refusing one larger than `max` bytes — checked by the size first,
/// and again while reading (one byte past the cap), so growth after the stat is caught too.
pub fn read_file_capped(f: std::fs::File, max: Option<u64>, shown: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let meta = f.metadata()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(std::io::Error::other(format!("cannot read {shown}: not a regular file")));
    }
    let Some(max) = max else {
        let mut f = f;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        return Ok(buf);
    };
    let too_large = |what: String| {
        std::io::Error::other(format!("cannot read {shown}: {what} exceeds the {max}-byte limit (too_large)"))
    };
    let size = meta.len();
    if size > max {
        return Err(too_large(format!("{size} bytes")));
    }
    #[cfg(any(test, debug_assertions))]
    testing::run_read_capped_after_stat_hook();
    let mut buf = Vec::with_capacity(size as usize);
    f.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(too_large("the content".into()));
    }
    Ok(buf)
}

#[cfg(any(test, debug_assertions))]
pub mod testing {
    use std::sync::{Mutex, OnceLock};

    type AfterStatHook = Box<dyn FnOnce() + Send>;
    static HOOK: OnceLock<Mutex<Option<AfterStatHook>>> = OnceLock::new();

    pub fn set_read_capped_after_stat_hook(hook: impl FnOnce() + Send + 'static) {
        *HOOK.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Box::new(hook));
    }

    pub(super) fn run_read_capped_after_stat_hook() {
        if let Some(lock) = HOOK.get()
            && let Some(hook) = lock.lock().unwrap().take()
        {
            hook();
        }
    }
}

/// Read a whole file, refusing one larger than `max` bytes — checked by the size first, and again
/// while reading (one byte past the cap), so growth after the stat is caught too.
pub fn read_capped(path: &Path, max: Option<u64>, shown: &str) -> std::io::Result<Vec<u8>> {
    read_file_capped(open_regular_nonblocking(path, shown)?, max, shown)
}

pub(super) fn resolve_policies(
    lua: &mlua::Lua,
    opts: Option<&mlua::Table>,
    cwd: &Path,
) -> mlua::Result<Vec<crate::ossandbox::Policy>> {
    let mut out = Vec::new();
    let Some(opts) = opts else {
        return Ok(out);
    };
    let Some(val) = opts.get::<Option<mlua::Value>>("policies")? else {
        return Ok(out);
    };
    let mlua::Value::Table(t) = val else {
        return Err(mlua::Error::RuntimeError("policies must be a table".into()));
    };
    for item in t.sequence_values::<mlua::Value>() {
        if let Some(pol) = crate::ossandbox::Policy::from_lua(lua, &item?, cwd)? {
            out.push(pol);
        }
    }
    Ok(out)
}

pub(super) fn check_policies_op(
    policies: &[crate::ossandbox::Policy],
    op: &str,
    path: &Path,
    cwd: &Path,
) -> std::io::Result<()> {
    let target = path.to_string_lossy();
    for pol in policies {
        if let Err(err) = pol.check(op, &target, cwd) {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, pol.denial_marker(&err)));
        }
    }
    Ok(())
}

pub(super) fn check_policies_read(
    policies: &[crate::ossandbox::Policy],
    path: &Path,
    cwd: &Path,
) -> std::io::Result<()> {
    check_policies_op(policies, "read", path, cwd)
}

pub(super) fn check_with_resolved<E>(
    path: &Path,
    physical: &Path,
    mut check: impl FnMut(&Path) -> Result<(), E>,
) -> Result<(), E> {
    check(path)?;
    if physical != path {
        check(physical)?;
    }
    Ok(())
}

pub(super) fn check_read_resolved(
    policies: &[crate::ossandbox::Policy],
    path: &Path,
    physical: &Path,
    cwd: &Path,
) -> std::io::Result<()> {
    check_with_resolved(path, physical, |target| check_policies_read(policies, target, cwd))
}

pub(super) fn check_read_with_symlink(
    policies: &[crate::ossandbox::Policy],
    path: &Path,
    cwd: &Path,
) -> std::io::Result<std::path::PathBuf> {
    let physical = crate::ossandbox::resolve(path)?;
    check_read_resolved(policies, path, &physical, cwd)?;
    Ok(physical)
}

/// Stream `path` line by line, keeping lines `offset..offset+limit` (1-based, without their line
/// breaks — like `str::lines`), counting all lines and hashing the whole content.
pub(super) fn read_window(
    path: &Path,
    offset: usize,
    limit: usize,
    max_bytes: Option<u64>,
    shown: &str,
) -> std::io::Result<(Vec<String>, usize, u64, String)> {
    use sha2::{Digest, Sha256};
    use std::io::BufRead;
    let f = open_regular_nonblocking(path, shown)?;
    let meta = f.metadata()?;
    let too_large = |limit_bytes: u64, what: String| {
        std::io::Error::other(format!("cannot read {shown}: {what} exceeds the {limit_bytes}-byte limit (too_large)"))
    };
    if let Some(max) = max_bytes {
        let len = meta.len();
        if len > max {
            return Err(too_large(max, format!("{len} bytes")));
        }
    }
    let mut r = std::io::BufReader::with_capacity(64 * 1024, f);
    let (mut hasher, mut size, mut total) = (Sha256::new(), 0u64, 0usize);
    let mut lines = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let mut saw_newline = false;
        loop {
            let chunk = r.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            let take = match memchr::memchr(b'\n', chunk) {
                Some(pos) => {
                    saw_newline = true;
                    pos + 1
                }
                None => chunk.len(),
            };
            let new_size = size + take as u64;
            if let Some(max) = max_bytes
                && new_size > max
            {
                return Err(too_large(max, "the content".into()));
            }
            if buf.len() + take > MAX_WINDOW_LINE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "cannot read {shown}: line {} exceeds the {MAX_WINDOW_LINE_BYTES}-byte line limit (too_large)",
                        total + 1
                    ),
                ));
            }
            buf.extend_from_slice(&chunk[..take]);
            r.consume(take);
            size = new_size;
            if saw_newline {
                break;
            }
        }
        if buf.is_empty() {
            break;
        }
        hasher.update(&buf);
        total += 1;
        // a multi-byte UTF-8 character never contains a 0x0A byte, so lines validate on their own
        let body = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        let text = std::str::from_utf8(body).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("cannot read {shown}: invalid UTF-8 text (not_text)"),
            )
        })?;
        if total >= offset && total - offset < limit {
            lines.push(text.to_string());
        }
    }
    Ok((lines, total, size, hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()))
}
