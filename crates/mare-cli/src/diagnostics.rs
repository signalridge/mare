//! Logging and crash reports: the log file subscriber, the panic hook that prepares a redacted
//! report, and writing it when a panic actually ends mare.

/// `MARE_LOG` sets the level / filter (`info` by default; e.g. `debug`, `mare_binding=trace`).
pub(crate) fn init_logging() {
    // opened on the first log line, not at startup
    let writer = std::sync::Mutex::new(mare_shared::logfile::Redacting(mare_shared::logfile::Lazy::new(
        &mare_shared::paths::log_dir(),
        mare_shared::logfile::MAX_BYTES,
        mare_shared::logfile::KEEP,
    )));
    let filter = tracing_subscriber::EnvFilter::try_from_env("MARE_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(writer).try_init();
}

/// Prepare a crash report on panic (version, platform, panic, backtrace, recent events), redacted.
/// It's only written when the panic actually ends mare ([`write_crash_report`], called by
/// [`CrashGuard`] / the TUI's TermGuard): a panic mlua turns into a Lua error is recovered and
/// leaves no report.
pub(crate) fn install_crash_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let events = mare_kernel::recent_events().join("\n");
        let report = mare_shared::redact::redact_str(&format!(
            "mare {} crash\nos: {} {}\n\n{info}\n\nrecent events:\n{events}\n\nbacktrace:\n{bt}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        tracing::error!("panic: {}", mare_shared::redact::redact_str(&info.to_string()));
        *PENDING_REPORT.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
        // never write to the terminal while the TUI owns it; if the panic ends mare, TermGuard
        // prints the report path after restoring the terminal
        if !TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
            prev(info);
        }
    }));
}

/// The newest panic's report, not yet written (recovered panics never get written).
static PENDING_REPORT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Write the pending crash report to `<state>/crashes/<time>/report.txt` (once); returns its path.
pub(crate) fn write_crash_report() -> Option<std::path::PathBuf> {
    let mut last = LAST_PANIC_REPORT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(report) = PENDING_REPORT.lock().unwrap_or_else(|e| e.into_inner()).take()
        && let Some(path) = write_crash_report_in(&mare_shared::paths::crash_dir(), &report)
    {
        *last = Some(path);
    }
    last.clone()
}

fn write_crash_report_in(crash_root: &std::path::Path, report: &str) -> Option<std::path::PathBuf> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = crash_root.join(stamp.to_string());
    let path = dir.join("report.txt");
    if mare_shared::private::create_dir_all(&dir).is_ok()
        && mare_shared::paths::write_atomic(&path, report.as_bytes(), mare_shared::paths::WriteMode::Private).is_ok()
    {
        return Some(path);
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn diagnostics_crash_report_creates_private_file_and_directories() {
        let root = tempfile::tempdir().unwrap();
        let crashes = root.path().join("crashes");
        let path = write_crash_report_in(&crashes, "private backtrace").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&crashes).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "private backtrace");
    }
}

/// Dropped while the main thread unwinds out of `main` (a fatal panic outside the TUI): writes
/// the crash report and says where it is.
pub(crate) struct CrashGuard;

impl Drop for CrashGuard {
    fn drop(&mut self) {
        if std::thread::panicking()
            && !TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst)
            && let Some(p) = write_crash_report()
        {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr().lock(), "mare: crashed; report saved to {}", p.display());
        }
    }
}

/// Set while the TUI owns the terminal (raw mode): the panic hook stays silent then.
pub(crate) static TUI_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The written crash report (the TUI prints it after restoring the terminal on a fatal panic).
pub(crate) static LAST_PANIC_REPORT: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
