//! Inside bubblewrap's network namespace (`mare __netns-proxy --uds <sock> --port <p> -- cmd …`):
//! bind 127.0.0.1:`p`, hand the listener to a bridge process (each connection → the host's Unix
//! socket → mare's proxy), then restrict (seccomp) and exec the command (Apache-2.0). Differences: the port is the
//! host proxy's own (the namespace is empty, so it is free — the proxy variables need no
//! rewriting), and the bridge is double-forked so it is not the command's child (a shell's
//! `wait` never waits for it); it dies with bubblewrap's PID namespace when the command ends.
//!
//! Runs single-threaded until the bridge process starts its threads, so `fork` is safe here.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::netns::unreachable_body;
use super::prexec::{SocketBlock, child_restrict, seccomp_filter};

/// Parsed arguments of the inner half.
#[derive(Debug, PartialEq, Eq)]
enum Args {
    /// capability probe: bind a loopback port and install the filter, then exit 0
    Check,
    Run {
        uds: PathBuf,
        port: u16,
        argv: Vec<String>,
    },
}

fn parse(args: &[String]) -> Result<Args, String> {
    if args == ["--check"] {
        return Ok(Args::Check);
    }
    let (mut uds, mut port) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--uds" => uds = it.next().map(PathBuf::from),
            "--port" => port = it.next().and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0),
            "--" => break,
            o => return Err(format!("unknown argument {o}")),
        }
    }
    let argv: Vec<String> = it.cloned().collect();
    if argv.is_empty() {
        return Err("missing command after --".into());
    }
    Ok(Args::Run { uds: uds.ok_or("missing --uds")?, port: port.ok_or("missing or bad --port")?, argv })
}

/// Entry point (`mare __netns-proxy …`): never returns on success (the command replaces this
/// process); on failure prints `mare-sandbox: …` and returns the runner failure code, which
/// [`super::Prepared::start_failure`] reports as "the sandbox failed to start".
pub fn netns_main(args: &[String]) -> i32 {
    let fail = |e: String| {
        eprintln!("{}network bridge: {e}", crate::winsandbox::RUNNER_SIGNATURE);
        crate::winsandbox::RUNNER_FAILURE_EXIT
    };
    let (uds, port, argv) = match parse(args) {
        Ok(Args::Check) => {
            let r = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string()).and_then(|_| restrict());
            return r.map_or_else(fail, |()| 0);
        }
        Ok(Args::Run { uds, port, argv }) => (uds, port, argv),
        Err(e) => return fail(e),
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => return fail(format!("bind 127.0.0.1:{port} in the sandbox's network namespace: {e}")),
    };
    if let Err(e) = spawn_bridge(listener, uds) {
        return fail(format!("start the bridge: {e}"));
    }
    if let Err(e) = restrict() {
        return fail(e);
    }
    use std::os::unix::process::CommandExt;
    let e = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    // the command itself couldn't be started: report it like a shell would
    eprintln!("{}: {e}", argv[0]);
    if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
}

/// The command's filter: no AF_UNIX sockets (it can't reach the host's bridge socket or any
/// local daemon:218-240` ProxyRouted) plus the base set. TCP stays: inside
/// the namespace it only reaches the bridge.
fn restrict() -> Result<(), String> {
    let f = seccomp_filter(SocketBlock { unix: true, inet: false, netlink_route: false }).map_err(|e| e.to_string())?;
    child_restrict(None, Some(&f)).map_err(|e| format!("seccomp: {e}"))
}

/// Double fork: the bridge ends up under bubblewrap's PID 1, not as the command's child; this
/// process waits only for the short-lived middle one.
fn spawn_bridge(listener: TcpListener, uds: PathBuf) -> std::io::Result<()> {
    // SAFETY: single-threaded here; the children only call fork/_exit or run the bridge
    let mid = unsafe { libc::fork() };
    if mid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if mid == 0 {
        // SAFETY: as above
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            quiet_stdio();
            bridge(listener, uds);
        }
        // SAFETY: leave the middle process without running any destructors
        unsafe { libc::_exit(if pid < 0 { 1 } else { 0 }) };
    }
    drop(listener);
    let mut status = 0;
    // SAFETY: waiting for our own child
    if unsafe { libc::waitpid(mid, &mut status, 0) } < 0 || !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(std::io::Error::other("the bridge process did not start"));
    }
    Ok(())
}

/// The bridge holds no terminal or output pipe of the command.
fn quiet_stdio() {
    // SAFETY: plain fd syscalls in the bridge process
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
}

fn bridge(listener: TcpListener, uds: PathBuf) -> ! {
    for tcp in listener.incoming() {
        let Ok(tcp) = tcp else { continue };
        let uds = uds.clone();
        std::thread::spawn(move || match UnixStream::connect(&uds) {
            Ok(unix) => pipe(tcp, unix),
            Err(e) => unreachable(tcp, &format!("the host bridge {} is gone ({e})", uds.display())),
        });
    }
    // SAFETY: end the bridge process without unwinding into the forked parent's state
    unsafe { libc::_exit(0) }
}

/// Copy both ways until each side closes.
fn pipe(mut tcp: TcpStream, mut unix: UnixStream) {
    let (Ok(mut tcp_r), Ok(mut unix_w)) = (tcp.try_clone(), unix.try_clone()) else { return };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut tcp_r, &mut unix_w);
        let _ = unix_w.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut unix, &mut tcp);
    let _ = tcp.shutdown(Shutdown::Write);
    let _ = up.join();
}

/// No host bridge: an HTTP client gets the same `502` the host half sends when the proxy is down.
fn unreachable(mut tcp: TcpStream, why: &str) {
    let mut first = [0u8; 1];
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    if tcp.read(&mut first).is_ok_and(|n| n == 1) && first[0] != 0x05 {
        let body = unreachable_body(why);
        let _ = write!(
            tcp,
            "HTTP/1.1 502 Bad Gateway\r\nx-mare-sandbox: proxy-unreachable\r\ncontent-type: text/plain\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    let _ = tcp.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(parse(&s(&["--check"])), Ok(Args::Check));
        assert_eq!(
            parse(&s(&["--uds", "/r/p.sock", "--port", "8080", "--", "curl", "--", "x"])),
            Ok(Args::Run { uds: "/r/p.sock".into(), port: 8080, argv: s(&["curl", "--", "x"]) })
        );
        for bad in [
            &["--uds", "/p", "--port", "0", "--", "x"][..],
            &["--uds", "/p", "--port", "8080", "--"],
            &["--port", "1", "--", "x"],
            &["--bogus"],
        ] {
            assert!(parse(&s(bad)).is_err(), "{bad:?}");
        }
    }
}
