//! Termination requests → graceful exit: frontends race their main loop against this so plugins
//! still see `app/shutdown` and their ctx disposal (e.g. to release external state).
//! Unix: SIGTERM / SIGHUP. Windows: console close, Ctrl+Break, logoff and system shutdown.
//! Interrupts (SIGINT / Ctrl+C) that cancel a turn come through [`Interrupts`].

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A signal gets five seconds for normal teardown, then the process
/// exits even if a synchronous write or a blocked runtime thread cannot make progress.
const FORCE_AFTER: Duration = Duration::from_secs(5);

static DRAINING: AtomicBool = AtomicBool::new(false);
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);
static NATIVE_SIGNALS: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static FORCE_REQUESTED: AtomicBool = AtomicBool::new(false);
static SIGNAL_EPOCH: AtomicUsize = AtomicUsize::new(0);
static SHUTDOWN_STAGE: AtomicU8 = AtomicU8::new(0);

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownStage {
    Running = 0,
    StopAccepting = 1,
    HostShutdown = 2,
    RuntimeDrain = 3,
}

pub fn set_shutdown_stage(stage: ShutdownStage) {
    SHUTDOWN_STAGE.store(stage as u8, Ordering::SeqCst);
}

fn skipped_steps_from(stage: u8) -> &'static [&'static str] {
    match stage {
        0 => &["stop_accepting", "cancel_root_and_cooperative_grace", "process_trees", "runtime_drain"],
        1 => &["cancel_root_and_cooperative_grace", "process_trees", "runtime_drain"],
        2 => &["cooperative_grace_or_process_trees", "runtime_drain"],
        3 => &["runtime_drain"],
        _ => &[],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownAction {
    BeginDrain,
    ForceExit,
}

fn on_signal(draining: &AtomicBool) -> ShutdownAction {
    if draining.swap(true, Ordering::SeqCst) { ShutdownAction::ForceExit } else { ShutdownAction::BeginDrain }
}

#[derive(Clone, Copy)]
struct ShutdownDeadline(Instant);

impl ShutdownDeadline {
    fn from(now: Instant) -> Self {
        Self(now + FORCE_AFTER)
    }

    #[cfg(test)]
    fn remaining(self, now: Instant) -> Duration {
        self.0.saturating_duration_since(now)
    }

    fn expired(self, now: Instant) -> bool {
        now >= self.0
    }
}

/// Call after a frontend receives its first exit signal and before it starts cleanup. A second
/// Ctrl+C/SIGTERM or five seconds elapsing terminates the process outside the async runtime, so a
/// blocked terminal write cannot keep shutdown alive.
pub fn begin_forceable_shutdown(code: i32) {
    EXIT_CODE.store(code, Ordering::SeqCst);
    let _ = SHUTDOWN_STAGE.compare_exchange(
        ShutdownStage::Running as u8,
        ShutdownStage::StopAccepting as u8,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    match on_signal(&DRAINING) {
        ShutdownAction::ForceExit => force_exit_now(code),
        ShutdownAction::BeginDrain => {
            if NATIVE_SIGNALS.load(Ordering::SeqCst) == 0 {
                SIGNAL_EPOCH.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// A turn-only interrupt has completed. It does not request process shutdown.
/// A repeated signal already queued for forced exit cannot be reset by this acknowledgement.
pub fn complete_turn_interrupt() {
    if !DRAINING.load(Ordering::SeqCst)
        && NATIVE_SIGNALS.compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    {
        SIGNAL_EPOCH.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Watchdog {
    epoch: usize,
    deadline: Option<ShutdownDeadline>,
}

impl Watchdog {
    fn due(&mut self, now: Instant, pending: bool, epoch: usize, forced: bool) -> bool {
        if forced {
            return true;
        }
        if !pending {
            self.deadline = None;
        } else if self.deadline.is_none() || self.epoch != epoch {
            self.epoch = epoch;
            self.deadline = Some(ShutdownDeadline::from(now));
        }
        self.deadline.is_some_and(|deadline| deadline.expired(now))
    }
}

/// Install before the first frontend write. Native signal callbacks only touch atomics
/// and request escalation; a dedicated thread owns cleanup and the shutdown deadline.
/// This remains live when a synchronous pipe write blocks the async runtime.
pub fn install_forceable_shutdown() -> std::io::Result<()> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    install_force_exit_handlers()?;
    std::thread::Builder::new().name("mare-shutdown-deadline".into()).spawn(|| {
        let mut watchdog = Watchdog::default();
        loop {
            if watchdog.due(
                Instant::now(),
                DRAINING.load(Ordering::SeqCst) || NATIVE_SIGNALS.load(Ordering::SeqCst) > 0,
                SIGNAL_EPOCH.load(Ordering::SeqCst),
                FORCE_REQUESTED.load(Ordering::SeqCst),
            ) {
                force_exit_now(EXIT_CODE.load(Ordering::SeqCst));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })?;
    Ok(())
}

// Called in a Unix signal handler or Windows console callback. No allocations, locks,
// logging, runtime scheduling or stdio operations are permitted here.
fn native_signal(code: i32) {
    let repeated = NATIVE_SIGNALS.fetch_add(1, Ordering::SeqCst) > 0;
    EXIT_CODE.store(code, Ordering::SeqCst);
    if repeated || DRAINING.load(Ordering::SeqCst) {
        // The supervisor performs process-tree cleanup outside this native callback.
        FORCE_REQUESTED.store(true, Ordering::SeqCst);
    } else {
        SIGNAL_EPOCH.fetch_add(1, Ordering::SeqCst);
    }
}

// Normal completion may legitimately return zero; crossing the force-exit boundary
// means graceful shutdown did not complete and must never report success.
fn forced_exit_code(code: i32) -> i32 {
    // Unix exposes only the low eight exit-status bits. Normalize before testing zero.
    #[cfg(unix)]
    let code = code & 0xff;
    if code == 0 { 1 } else { code }
}

/// Do not allow normal completion to race a native force request into exit zero.
pub fn finish_process(code: i32) -> ! {
    if FORCE_REQUESTED.load(Ordering::SeqCst) {
        force_exit_now(EXIT_CODE.load(Ordering::SeqCst));
    }
    mare_binding::platform::exit(code)
}

/// Force exit after process-tree cleanup and bounded Mare-owned terminal restore, without
/// flushing stdio or waiting for Tokio. Never called in a native signal callback.
pub fn force_exit_now(code: i32) -> ! {
    const FORCE_TERM_CLEANUP_TIMEOUT: Duration = Duration::from_millis(100);
    let stage = SHUTDOWN_STAGE.load(Ordering::SeqCst);
    let skipped = skipped_steps_from(stage);
    tracing::warn!(code, stage, skipped = ?skipped, "forced exit: skipping remaining shutdown steps");
    mare_binding::trees::force_cleanup();
    #[cfg(unix)]
    crate::askpass::force_cleanup();
    if crate::TUI_ACTIVE.swap(false, Ordering::SeqCst) && !mare_tui::modes::handed_off() {
        mare_tui::modes::force_leave(FORCE_TERM_CLEANUP_TIMEOUT);
        mare_tui::tty::leave_raw_best_effort();
    }
    let code = forced_exit_code(code);
    #[cfg(unix)]
    // SAFETY: _exit is async-signal-safe and deliberately skips destructors and stdio flushing.
    unsafe {
        libc::_exit(code)
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // SAFETY: GetCurrentProcess returns the current process pseudo-handle.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: terminating the current process is the intended force-exit path.
        if unsafe { TerminateProcess(process, code as u32) } == 0 {
            std::process::exit(code);
        }
        // TerminateProcess initiates process-wide termination. Do not return to a blocked handler.
        loop {
            std::hint::spin_loop();
        }
    }

    #[cfg(not(any(unix, windows)))]
    std::process::exit(code)
}

#[cfg(unix)]
fn install_force_exit_handlers() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the callback performs only lock-free atomic operations and _exit.
        // The registry chains handlers with Tokio instead of replacing its notification path.
        unsafe {
            signal_hook_registry::register(signal, move || native_signal(128 + signal))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn force_exit_console(ctrl_type: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    match ctrl_type {
        CTRL_C_EVENT => native_signal(130),
        CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => native_signal(1),
        _ => {}
    }
    0
}

#[cfg(windows)]
fn install_force_exit_handlers() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // Windows invokes handlers in reverse registration order. Initialize Tokio's process-wide
    // handler first, then install ours so it observes every signal before Tokio handles it.
    let _ = tokio::signal::windows::ctrl_c()?;
    let _ = tokio::signal::windows::ctrl_break()?;
    let _ = tokio::signal::windows::ctrl_close()?;
    let _ = tokio::signal::windows::ctrl_logoff()?;
    let _ = tokio::signal::windows::ctrl_shutdown()?;
    // SAFETY: the callback has the required system ABI and process lifetime.
    if unsafe { SetConsoleCtrlHandler(Some(force_exit_console), 1) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn install_force_exit_handlers() -> std::io::Result<()> {
    Ok(())
}

/// Resolves when the process is asked to terminate; never resolves otherwise.
pub async fn terminate() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};
        let (Ok(mut close), Ok(mut brk), Ok(mut logoff), Ok(mut shutdown)) =
            (ctrl_close(), ctrl_break(), ctrl_logoff(), ctrl_shutdown())
        else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = close.recv() => {}
            _ = brk.recv() => {}
            _ = logoff.recv() => {}
            _ = shutdown.recv() => {}
        }
    }
    #[cfg(not(any(unix, windows)))]
    std::future::pending::<()>().await
}

/// SIGINT / Ctrl+C as a listener that lives as long as the frontend's loop. A fresh
/// `tokio::signal::ctrl_c()` per `select!` iteration loses interrupts: tokio delivers a signal
/// only to the listeners that exist when it arrives, and the iteration that receives one can
/// still be won by another ready branch (a tap event, a timer tick) — its listener is then
/// dropped with the interrupt in it, and the next iteration's new one never sees it. One
/// listener for the whole loop keeps an interrupt until it is taken.
pub struct Interrupts {
    #[cfg(unix)]
    sig: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    sig: Option<tokio::signal::windows::CtrlC>,
}

impl Interrupts {
    /// Start listening (and install the handler: from now on SIGINT no longer kills mare).
    pub fn new() -> Self {
        Self::listen(true)
    }

    /// `listen = false`: a listener that never fires and installs nothing (SIGINT keeps its
    /// default action).
    pub fn listen(listen: bool) -> Self {
        #[cfg(unix)]
        let sig =
            listen.then(|| tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok()).flatten();
        #[cfg(windows)]
        let sig = listen.then(|| tokio::signal::windows::ctrl_c().ok()).flatten();
        #[cfg(any(unix, windows))]
        return Self { sig };
        #[cfg(not(any(unix, windows)))]
        {
            let _ = listen;
            Self {}
        }
    }

    /// The next interrupt (cancel safe: one that arrives while nobody waits is kept). Never
    /// resolves when the handler couldn't be installed.
    pub async fn recv(&mut self) {
        #[cfg(any(unix, windows))]
        if let Some(sig) = self.sig.as_mut()
            && sig.recv().await.is_some()
        {
            return;
        }
        std::future::pending::<()>().await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An interrupt that arrives while nobody awaits it (the loop is busy with another branch)
    /// is kept for the next `recv` — `tokio::signal::ctrl_c()` made fresh each time loses it.
    #[test]
    fn an_interrupt_between_receives_is_kept() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut sigint = Interrupts::new();
            unsafe { libc::raise(libc::SIGINT) };
            // let the runtime's signal driver take the signal while nothing awaits `recv`
            tokio::time::sleep(Duration::from_millis(100)).await;
            // the old way, for contrast: a listener made after the signal was taken misses it
            assert!(
                tokio::time::timeout(Duration::from_millis(200), tokio::signal::ctrl_c()).await.is_err(),
                "a listener made after the signal never sees it"
            );
            assert!(
                tokio::time::timeout(Duration::from_secs(5), sigint.recv()).await.is_ok(),
                "the interrupt was lost"
            );
        });
    }
}

#[cfg(test)]
mod shutdown_tests {
    #[test]
    fn turn_cancellation_disarms_old_deadline_and_a_new_epoch_gets_its_own_grace() {
        let start = std::time::Instant::now();
        let mut watchdog = super::Watchdog::default();
        assert!(!watchdog.due(start, true, 1, false));
        assert!(!watchdog.due(start + std::time::Duration::from_secs(1), false, 2, false));
        assert!(!watchdog.due(start + std::time::Duration::from_secs(6), false, 2, false));
        assert!(!watchdog.due(start + std::time::Duration::from_secs(7), true, 3, false));
        assert!(!watchdog.due(start + std::time::Duration::from_secs(11), true, 3, false));
        assert!(watchdog.due(start + std::time::Duration::from_secs(12), true, 3, false));
        assert!(watchdog.due(start, false, 0, true));
    }

    #[test]
    fn forced_exit_never_maps_a_success_code_to_success() {
        assert_eq!(super::forced_exit_code(0), 1);
        for code in [1, 130, 143] {
            assert_eq!(super::forced_exit_code(code), code);
        }
        #[cfg(unix)]
        {
            assert_eq!(super::forced_exit_code(256), 1);
            assert_eq!(super::forced_exit_code(-256), 1);
            assert_eq!(super::forced_exit_code(-1), 255);
        }
    }

    use super::*;

    #[test]
    fn first_forceable_signal_begins_drain_and_second_escalates() {
        let draining = AtomicBool::new(false);
        assert_eq!(on_signal(&draining), ShutdownAction::BeginDrain);
        assert_eq!(on_signal(&draining), ShutdownAction::ForceExit);
    }

    #[test]
    fn force_exit_deadline_is_five_seconds_from_drain_start() {
        let start = Instant::now();
        let deadline = ShutdownDeadline::from(start);
        assert_eq!(deadline.remaining(start), Duration::from_secs(5));
        assert!(!deadline.expired(start + Duration::from_millis(4_999)));
        assert!(deadline.expired(start + Duration::from_secs(5)));
    }
}
