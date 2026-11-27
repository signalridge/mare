//! Local IPC endpoints: Unix domain sockets, named pipes on Windows.

use std::io;

#[cfg(windows)]
use super::pipe;
use super::{Host, IpcBackend};

#[cfg(unix)]
static IPC_BIND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(unix)]
impl IpcBackend for Host {
    type Stream = tokio::net::UnixStream;
    type Listener = tokio::net::UnixListener;
    async fn connect(path: &str) -> io::Result<Self::Stream> {
        tokio::net::UnixStream::connect(path).await
    }
    fn bind(path: &std::path::Path, mode: u32) -> io::Result<Self::Listener> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        if let Some(d) = path.parent() {
            mare_shared::private::create_dir_all(d)?;
            let dm = std::fs::symlink_metadata(d)?;
            if dm.file_type().is_symlink() || !dm.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} must be a non-symlink directory", d.display()),
                ));
            }
            let me = unsafe { libc::geteuid() };
            if dm.uid() != me {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} belongs to uid {} (not {me})", d.display(), dm.uid()),
                ));
            }
        }
        let _guard = IPC_BIND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.file_type().is_socket() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} is not a socket", path.display()),
                    ));
                }
                let me = unsafe { libc::geteuid() };
                if meta.uid() != me {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{} belongs to uid {} (not {me})", path.display(), meta.uid()),
                    ));
                }
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("{}: already in use", path.display()),
                    ));
                }
                std::fs::remove_file(path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let l = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(l)
    }
    async fn accept(l: &Self::Listener) -> io::Result<Self::Stream> {
        l.accept().await.map(|(s, _)| s)
    }
}

#[cfg(windows)]
impl IpcBackend for Host {
    type Stream = pipe::PipeStream;
    type Listener = pipe::PipeListener;
    async fn connect(path: &str) -> io::Result<Self::Stream> {
        pipe::connect(path).await
    }
    fn bind(path: &std::path::Path, _mode: u32) -> io::Result<Self::Listener> {
        pipe::bind(path)
    }
    async fn accept(l: &Self::Listener) -> io::Result<Self::Stream> {
        pipe::accept(l).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipc_bind_rejects_symlink_dir_and_non_socket_and_replaces_stale_socket() {
        let dir = tempfile::tempdir().unwrap();

        // Symlink parent directory must be refused.
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let sym_dir = dir.path().join("sym");
        std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();
        let err = <Host as IpcBackend>::bind(&sym_dir.join("a.sock"), 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");

        // Regular file at socket path must be refused (not silently unlinked).
        let regular = real_dir.join("not_a_sock");
        std::fs::write(&regular, "keep").unwrap();
        let err = <Host as IpcBackend>::bind(&regular, 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(&regular).unwrap(), "keep");

        // Symlink at socket path must also be refused.
        let sock_link = real_dir.join("link.sock");
        std::os::unix::fs::symlink(&regular, &sock_link).unwrap();
        let err = <Host as IpcBackend>::bind(&sock_link, 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");

        // Stale (closed) Unix socket is replaced cleanly; active socket is rejected with AddrInUse.
        let sock = real_dir.join("live.sock");
        let l1 = <Host as IpcBackend>::bind(&sock, 0o600).unwrap();
        let err = <Host as IpcBackend>::bind(&sock, 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");
        drop(l1);
        let l2 = <Host as IpcBackend>::bind(&sock, 0o600).unwrap();
        drop(l2);
    }
}
