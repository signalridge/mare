//! Windows named pipes behind the path-addressed IPC API.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

pub enum PipeStream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

impl PipeStream {
    /// The connecting process of an accepted connection (`GetNamedPipeClientProcessId`): the
    /// pipe's counterpart of a Unix socket's peer credentials. `None` on the client side.
    pub fn client_pid(&self) -> Option<u32> {
        use std::os::windows::io::AsRawHandle;
        let Self::Server(s) = self else { return None };
        let mut pid = 0u32;
        let ok = unsafe { windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(s.as_raw_handle(), &mut pid) };
        (ok != 0 && pid != 0).then_some(pid)
    }
}

impl AsyncRead for PipeStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(c) => Pin::new(c).poll_read(cx, buf),
            Self::Server(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PipeStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Client(c) => Pin::new(c).poll_write(cx, b),
            Self::Server(s) => Pin::new(s).poll_write(cx, b),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(c) => Pin::new(c).poll_flush(cx),
            Self::Server(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(c) => Pin::new(c).poll_shutdown(cx),
            Self::Server(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub struct PipeListener {
    name: String,
    next: tokio::sync::Mutex<Option<NamedPipeServer>>,
    /// the lock file, exclusively locked while the listener lives: a second `bind` of the same
    /// path fails, and a crashed owner releases it automatically
    _lock: std::fs::File,
}

const PREFIX: &str = r"\\.\pipe\mare-";

/// How many random names `bind` tries before it gives up (each refused one is held by another
/// process; a squatter can't predict the next).
const NAME_TRIES: usize = 8;

/// The lock file of the endpoint at `path` (`<path>.lock`). The endpoint file itself only names
/// the pipe and stays readable, so clients can find it; the lock lives next to it.
pub fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    p.into()
}

/// A fresh pipe name: 128 random bits, so another local user can't create it first (pipe names
/// are global; a name derived from the path could be squatted before the session starts).
fn random_name(_attempt: usize) -> String {
    // test hook (debug builds): the first draw, to exercise the collision retry
    #[cfg(debug_assertions)]
    if _attempt == 0
        && let Ok(n) = std::env::var("MARE_TEST_PIPE_FIRST_NAME")
    {
        return n;
    }
    let mut b = [0u8; 16];
    rand::fill(&mut b[..]);
    let hex: String = b.iter().map(|b| format!("{b:02x}")).collect();
    format!("{PREFIX}{hex}")
}

fn valid_name(n: &str) -> bool {
    n.strip_prefix(PREFIX).is_some_and(|h| !h.is_empty() && h.len() <= 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// The pipe of the endpoint at `path`, read from its endpoint file (what [`bind`] wrote). A mare
/// from before random names keeps that file locked (`ERROR_LOCK_VIOLATION`) and listens on the
/// name derived from the path ([`super::pipe_name`]).
pub fn resolve(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if valid_name(s.trim()) => Ok(s.trim().to_string()),
        Ok(_) => {
            Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: not a mare pipe endpoint", path.display())))
        }
        Err(e) if e.raw_os_error() == Some(33) => Ok(super::pipe_name(path)),
        Err(e) => Err(e),
    }
}

/// The pipe's security descriptor: owned by the current user's SID, full control for that SID and
/// SYSTEM, nobody else (the counterpart of mode 0600 on a socket file); remote clients are
/// rejected. The user's SID rather than `OW` (OWNER RIGHTS) and the default owner: an elevated
/// process's objects are owned by Administrators, so `OW` would admit every elevated
/// administrator and refuse the same user's non-elevated `mare peer send` — the token's user is
/// the same either way, like the uid a Unix socket checks. The explicit owner is what clients
/// verify ([`connect`]).
fn sddl() -> io::Result<String> {
    let me = mare_shared::private::user_sid()?;
    Ok(format!("O:{me}D:P(A;;GA;;;{me})(A;;GA;;;SY)"))
}

fn create(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let sddl: Vec<u16> = sddl()?.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let r = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, &mut sa as *mut _ as *mut core::ffi::c_void);
        LocalFree(sd as _);
        r
    }
}

/// Connect, then make sure the server is this user's: pipe names are global, so another local
/// user could have created this one (squatting a session's name, or taking it over after the
/// session exits) — Unix needs no such check, its sockets live in a 0700 directory. The pipe's
/// owner must be the current user's SID (what [`create`] sets; only this user or an administrator
/// can make an object owned by it; checking ownership works also when the server is elevated and
/// the client not, whose token it may not open).
pub async fn connect(path: &str) -> io::Result<PipeStream> {
    use std::os::windows::io::AsRawHandle;
    let name = resolve(Path::new(path))?;
    let mut tries = 0;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(c) => {
                if let Err(e) = mare_shared::private::check_owner_handle(c.as_raw_handle()) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{path}: refusing the named pipe {name}: {e} (another user's process may hold its name)"
                        ),
                    ));
                }
                return Ok(PipeStream::Client(c));
            }
            // all instances busy: the server creates the next one right after accepting
            Err(e) if e.raw_os_error() == Some(231) && tries < 50 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn bind(path: &Path) -> io::Result<PipeListener> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    // liveness: the owner holds a lock on the lock file (probing with a client would steal the
    // listening instance); first_pipe_instance below is a second line of defence
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path(path))?;
    let in_use = || io::Error::new(io::ErrorKind::AddrInUse, format!("{}: already in use", path.display()));
    if lock.try_lock().is_err() {
        return Err(in_use());
    }
    // a mare from before random names locks the endpoint file itself
    if std::fs::read(path).is_err_and(|e| e.raw_os_error() == Some(33)) {
        return Err(in_use());
    }
    // ERROR_ACCESS_DENIED / ERROR_PIPE_BUSY on a first instance: another process holds this name
    // (no session of ours: the lock was free) — draw another; a squatter can't predict it. Only
    // when every draw is taken, say so loudly instead of "in use".
    let mut attempt = 0;
    let (name, server) = loop {
        let name = random_name(attempt);
        match create(&name, true) {
            Ok(s) => break (name, s),
            Err(e) if matches!(e.raw_os_error(), Some(5 | 231)) => {
                tracing::warn!(
                    "{}: the named pipe {name} is held by another process; drawing another name",
                    path.display()
                );
                attempt += 1;
                if attempt == NAME_TRIES {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "{}: {NAME_TRIES} random named pipes were all held by other processes (not mare sessions of this user; possibly another user squatting them)",
                            path.display()
                        ),
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    };
    // the endpoint file names the pipe for clients ([`resolve`]): private, replaced atomically
    // so a reader never sees half a name; cleaned up by path, like a socket file
    mare_shared::paths::write_atomic(path, name.as_bytes(), mare_shared::paths::WriteMode::Private)?;
    Ok(PipeListener { name, next: tokio::sync::Mutex::new(Some(server)), _lock: lock })
}

pub async fn accept(l: &PipeListener) -> io::Result<PipeStream> {
    let mut g = l.next.lock().await;
    loop {
        let server = match g.take() {
            Some(s) => s,
            None => create(&l.name, false)?,
        };
        match server.connect().await {
            Ok(()) => {
                // the next client gets a fresh instance
                *g = Some(create(&l.name, false)?);
                return Ok(PipeStream::Server(server));
            }
            // a client connected and left before we accepted it (e.g. a liveness probe):
            // ERROR_NO_DATA / ERROR_BROKEN_PIPE — drop that instance and keep listening
            Err(e) if matches!(e.raw_os_error(), Some(232 | 109)) => {
                *g = Some(create(&l.name, false)?);
            }
            Err(e) => return Err(e),
        }
    }
}
