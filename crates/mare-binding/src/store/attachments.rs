use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use mlua::{Lua, UserData, UserDataMethods};

use mare_shared::lua::rt_err;

use super::host_only;

pub(super) fn attachment_dir(path: &Path) -> PathBuf {
    let mut name = path.with_extension("").into_os_string();
    name.push(".attachments");
    PathBuf::from(name)
}

/// A private staging directory lives until the owner and every in-flight blocking write release
/// it. A cancelled Lua future may detach its blocking task; that task's Arc prevents cleanup
/// until the write has actually stopped using the directory.
pub(super) struct StagingDir {
    path: PathBuf,
    directories: crate::fs::DirectorySyncPlan,
}

impl StagingDir {
    pub(super) fn capture(path: PathBuf) -> std::io::Result<Self> {
        let path = dunce::canonicalize(path)?;
        let boundary = path.ancestors().last().ok_or_else(|| std::io::Error::other("staging path has no root"))?;
        let directories = crate::fs::capture_directory_sync_plan(&path.join("entry"), boundary, &None)?;
        Ok(Self { path, directories })
    }
}

#[cfg(unix)]
fn open_attachment_relative(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(std::io::Error::other("invalid relative attachment name"));
    }
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    #[cfg(target_os = "linux")]
    match crate::fs::open_beneath_fd(directory.as_raw_fd(), Path::new(name), flags, mode) {
        Ok(file) => return Ok(std::fs::File::from(file)),
        Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {}
        Err(error) => return Err(error),
    }
    let name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
    // Only one checked normal component is resolved below the held directory.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
    if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { std::fs::File::from_raw_fd(fd) }) }
}

#[cfg(unix)]
fn same_attachment_file(a: &std::fs::File, b: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
}

#[cfg(any(debug_assertions, test))]
type AttachmentDirectorySyncHook = Box<dyn Fn(&Path) -> std::io::Result<()>>;

#[cfg(any(debug_assertions, test))]
struct BeforeAttachmentDirectorySync(AttachmentDirectorySyncHook);

/// Per-VM fault/ordering seam; not installed into the Lua API.
#[cfg(any(debug_assertions, test))]
pub fn before_attachment_directory_sync(lua: &Lua, callback: impl Fn(&Path) -> std::io::Result<()> + 'static) {
    lua.set_app_data(BeforeAttachmentDirectorySync(Box::new(callback)));
}

pub(super) fn install_staged_attachments(lua: &Lua, stage: &StagingDir, journal: &Path) -> std::io::Result<()> {
    stage.directories.verify()?;
    let entries = std::fs::read_dir(&stage.path)?.collect::<std::io::Result<Vec<_>>>()?;
    for entry in &entries {
        if !entry.file_type()?.is_file() {
            return Err(std::io::Error::other("staged attachment is not a regular file"));
        }
    }
    stage.directories.verify()?;
    let destination = attachment_dir(&dunce::canonicalize(journal)?);
    // The filesystem root is the conservative boundary: the target's intermediate date/
    // session directories may also have been created by the same uncommitted operation.
    let boundary =
        destination.ancestors().last().ok_or_else(|| std::io::Error::other("attachment path has no root"))?;
    let publication = crate::fs::capture_directory_sync_plan(&destination, boundary, &None)?;
    #[cfg(unix)]
    let mut publication = publication;
    publication.verify()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let parent = &publication.directories[0].1;
        let name = destination.file_name().ok_or_else(|| std::io::Error::other("attachment directory has no name"))?;
        let c_name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let target = open_attachment_relative(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        publication.directories.insert(0, (destination.clone(), target));
        publication.verify()?;
        let source = &stage.directories.directories[0].1;
        let target = &publication.directories[0].1;
        for entry in entries {
            let name = entry.file_name();
            let file = open_attachment_relative(source, &name, libc::O_RDONLY, 0)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("staged attachment is not a regular file"));
            }
            let c_name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
            if unsafe { libc::renameat(source.as_raw_fd(), c_name.as_ptr(), target.as_raw_fd(), c_name.as_ptr()) } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let installed = open_attachment_relative(target, &name, libc::O_RDONLY, 0)?;
            if !same_attachment_file(&file, &installed)? {
                return Err(std::io::Error::other("attachment identity changed during installation"));
            }
            file.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    {
        mare_shared::private::create_dir_new(&destination)?;
        for entry in entries {
            let installed = destination.join(entry.file_name());
            std::fs::rename(entry.path(), &installed)?;
            std::fs::OpenOptions::new().write(true).open(installed)?.sync_all()?;
        }
    }
    #[cfg(any(debug_assertions, test))]
    if let Some(callback) = lua.remove_app_data::<BeforeAttachmentDirectorySync>() {
        (callback.0)(journal)?;
    }
    #[cfg(not(any(debug_assertions, test)))]
    let _ = lua;
    publication.sync()?;
    stage.directories.sync()
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if let Err(error) = self.directories.verify() {
            tracing::warn!("staged attachment directory changed before cleanup {}: {error}", self.path.display());
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("could not remove staged attachments {}: {e}", self.path.display());
        }
    }
}

pub(super) fn write_staged_attachment(stage: Arc<StagingDir>, name: String, bytes: Vec<u8>) -> std::io::Result<()> {
    stage.directories.verify()?;
    #[cfg(not(unix))]
    let path = stage.path.join(&name);
    #[cfg(unix)]
    let created = open_attachment_relative(
        &stage.directories.directories[0].1,
        std::ffi::OsStr::new(&name),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    );
    #[cfg(not(unix))]
    let created = mare_shared::private::create_new(&path);
    match created {
        Ok(mut file) => {
            let result = file.write_all(&bytes).and_then(|_| file.sync_all()).and_then(|_| stage.directories.verify());
            if result.is_err() {
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    let parent = &stage.directories.directories[0].1;
                    if open_attachment_relative(parent, std::ffi::OsStr::new(&name), libc::O_RDONLY, 0)
                        .and_then(|named| same_attachment_file(&file, &named))
                        .unwrap_or(false)
                        && let Ok(name) = std::ffi::CString::new(name.as_bytes())
                    {
                        let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
                    }
                }
                #[cfg(not(unix))]
                let _ = std::fs::remove_file(&path);
            }
            result
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(unix)]
            let existing = {
                use std::io::Read;
                let mut file = open_attachment_relative(
                    &stage.directories.directories[0].1,
                    std::ffi::OsStr::new(&name),
                    libc::O_RDONLY,
                    0,
                )?;
                let mut data = Vec::new();
                file.read_to_end(&mut data)?;
                data
            };
            #[cfg(not(unix))]
            let existing = std::fs::read(&path)?;
            stage.directories.verify()?;
            if existing == bytes {
                Ok(())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "staged attachment hash collision"))
            }
        }
        Err(e) => Err(e),
    }
}

pub(super) struct AttachmentStage {
    pub(super) inner: Mutex<Option<Arc<StagingDir>>>,
}

impl AttachmentStage {
    pub(super) fn acquire(&self) -> mlua::Result<Arc<StagingDir>> {
        self.inner.lock().unwrap().clone().ok_or_else(|| rt_err("attachment staging is closed"))
    }
}

impl UserData for AttachmentStage {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("close", |lua, this, ()| {
            host_only(lua, "close attachment staging")?;
            this.inner.lock().unwrap().take();
            Ok(())
        });
        m.add_async_method("write", |lua, this, (name, data): (String, mlua::LuaString)| {
            let allowed = host_only(&lua, "write staged attachment");
            let stage = this.acquire();
            let bytes = data.as_bytes().to_vec();
            async move {
                allowed?;
                let stage = stage?;
                let mut parts = Path::new(&name).components();
                if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
                    return Err(rt_err("invalid staged attachment name"));
                }
                crate::blocking::spawn(&lua, move || write_staged_attachment(stage, name, bytes))?
                    .await
                    .map_err(rt_err)?
                    .map_err(rt_err)?;
                Ok(true)
            }
        });
    }
}

fn parse_attachment_sha256(name: &str) -> std::io::Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid attachment name"));
    }
    let Some((hex, ext)) = name.split_once('.') else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "attachment name missing extension"));
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "attachment name missing sha256 digest"));
    }
    if ext.is_empty() || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid attachment extension"));
    }
    Ok(hex)
}

pub(super) fn read_verified_attachment(journal: &Path, name: &str, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let expected_hex = parse_attachment_sha256(name)?;
    let att_dir = attachment_dir(journal);
    let parent = att_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "attachment path has no parent"))?;
    let att_name = att_dir
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "attachment directory has no name"))?;

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        let canonical_parent = dunce::canonicalize(parent)?;
        let parent_dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&canonical_parent)?;
        // Refuse symlinked `<session>.attachments` directory and bind reads to the directory fd.
        let dir_fd = open_attachment_relative(&parent_dir, att_name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        open_attachment_relative(&dir_fd, std::ffi::OsStr::new(name), libc::O_RDONLY, 0)?
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

        let canonical_parent = dunce::canonicalize(parent)?;
        let dir_path = canonical_parent.join(att_name);
        let dir_handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&dir_path)?;
        let dir_meta = dir_handle.metadata()?;
        if dir_meta.file_type().is_symlink()
            || (dir_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || !dir_meta.is_dir()
        {
            return Err(std::io::Error::other("attachment directory cannot be a symlink or junction"));
        }
        let opened_dir = crate::fs::opened_path(&dir_handle)?;
        if !dunce::simplified(&opened_dir).as_os_str().eq_ignore_ascii_case(dunce::simplified(&dir_path).as_os_str()) {
            return Err(std::io::Error::other("attachment directory changed during open"));
        }
        let target_path = opened_dir.join(name);
        let file =
            std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&target_path)?;
        let file_meta = file.metadata()?;
        if file_meta.file_type().is_symlink()
            || (file_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || !file_meta.is_file()
        {
            return Err(std::io::Error::other("attachment is not a regular file"));
        }
        let opened_file = crate::fs::opened_path(&file)?;
        if !dunce::simplified(&opened_file)
            .as_os_str()
            .eq_ignore_ascii_case(dunce::simplified(&target_path).as_os_str())
        {
            return Err(std::io::Error::other("attachment target changed during open"));
        }
        file
    };

    let file_meta = file.metadata()?;
    if file_meta.file_type().is_symlink() || !file_meta.is_file() {
        return Err(std::io::Error::other("attachment is not a regular file"));
    }
    let bytes = crate::fs::read_file_capped(file, Some(max_bytes), name)?;
    let actual_hex: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "attachment content hash mismatch"));
    }
    Ok(bytes)
}
