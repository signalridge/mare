//! Startup first-frame and boot-progress loop while the host initializes.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use super::install_input::StartupInput;
use crate::app::term::probe::TermInit;
use crate::app::*;

pub(in crate::app) enum BootOutcome {
    Ready { host: Arc<Host>, boot_input: Option<Box<StartupInput>> },
    Exit { code: i32, host: Option<Arc<Host>> },
    StrictConfigError { host: Arc<Host>, message: String },
}

pub(in crate::app) async fn boot_host(
    opts: HostOptions,
    ui: &Arc<UiShared>,
    term: &TermInit,
    startuptime: &Option<PathBuf>,
    strict_config: bool,
    mut terminate: Pin<&mut impl Future<Output = ()>>,
    mut interrupt: Pin<&mut impl Future<Output = ()>>,
) -> Result<BootOutcome> {
    let (w, h, margin) = (term.width, term.height, term.margin);
    // project trust still undecided: the overlay comes after the first frame
    let (waiting_tx, mut waiting_rx) = oneshot::channel();
    let startup = Host::with_ui_first_frame(opts, ui.clone(), waiting_tx);
    tokio::pin!(startup);
    let mut boot_input: Option<StartupInput> = None;
    let mut boot_events: Option<tty::Events> = None;
    let mut boot_progress: Option<mpsc::UnboundedReceiver<String>> = None;
    let mut waiting_notice = true;

    let host = loop {
        tokio::select! {
            result = &mut startup => {
                break result?;
            }
            notice = &mut waiting_rx, if waiting_notice => {
                waiting_notice = false;
                if let Ok(mare_host::InstallStart { shown, progress }) = notice {
                    boot_progress = Some(progress);
                    let mut input = StartupInput::new(w, h, margin);
                    input.render(ui);
                    mare_shared::profile::mark("first frame");
                    boot_events = Some(tty::events()?);
                    boot_input = Some(input);
                    // The first frame must have reached the terminal before git can start.
                    mare_tui::out::drained().await;
                    if !strict_config
                        && std::env::var_os("MARE_PROFILE_STARTUP_EXIT").is_some()
                        && let Some(path) = startuptime
                    {
                        std::fs::write(path, mare_shared::profile::render())?;
                        return Ok(BootOutcome::Exit { code: 0, host: None });
                    }
                    let _ = shown.send(());
                }
            }
            line = async {
                match boot_progress.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if boot_progress.is_some() => {
                match line {
                    Some(line) => {
                        if let Some(input) = boot_input.as_mut() {
                            input.progress = Some(line);
                            input.render(ui);
                        }
                    }
                    None => boot_progress = None,
                }
            }
            event = async {
                match boot_events.as_mut() {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            }, if boot_events.is_some() => {
                match event {
                    Some(Ok(event)) => {
                        let input = boot_input.as_mut().expect("startup input exists");
                        if !input.handle(event.event, ui) {
                            return Ok(BootOutcome::Exit { code: 0, host: None });
                        }
                        input.render(ui);
                    }
                    Some(Err(_)) | None => return Ok(BootOutcome::Exit { code: 0, host: None }),
                }
            }
            _ = &mut terminate => {
                crate::signals::begin_forceable_shutdown(0);
                return Ok(BootOutcome::Exit { code: 0, host: None });
            }
            _ = &mut interrupt => {
                crate::signals::begin_forceable_shutdown(130);
                return Ok(BootOutcome::Exit { code: 130, host: None });
            }
        }
    };

    if let (Some(input), Some(events)) = (boot_input.as_mut(), boot_events.as_mut()) {
        while let Some(ev) = events.try_next() {
            match ev {
                Ok(event) => {
                    if !input.handle(event.event, ui) {
                        return Ok(BootOutcome::Exit { code: 0, host: Some(host) });
                    }
                }
                Err(_) => return Ok(BootOutcome::Exit { code: 0, host: Some(host) }),
            }
        }
    }

    if strict_config {
        let strict_err = crate::report_config_warnings(&host, true).err().or_else(|| host.config_error());
        if let Some(message) = strict_err {
            if let Some(input) = boot_input.as_mut() {
                input.renderer.erase();
            }
            return Ok(BootOutcome::StrictConfigError { host, message });
        }
    }

    // a config file failed: its dialog comes before the main UI
    let boot_renderer = boot_input.as_mut().map(|i| &mut i.renderer);
    if !crate::startup_error::before_ui(&host, ui, boot_renderer, boot_events.as_mut()).await? {
        return Ok(BootOutcome::Exit { code: 2, host: Some(host) });
    }
    // Pause and drain any already-read boot events before dropping the reader so pre-read
    // install-screen input is preserved while still letting the post-first-frame probe own stdin.
    if let Some(mut events) = boot_events.take() {
        events.pause_keep_queue();
        let leftover = events.drain_queued();
        drop(events);
        for ev in leftover {
            match ev {
                Ok(event) => {
                    if let Some(input) = boot_input.as_mut()
                        && !input.handle(event.event, ui)
                    {
                        return Ok(BootOutcome::Exit { code: 0, host: Some(host) });
                    }
                }
                Err(_) => return Ok(BootOutcome::Exit { code: 0, host: Some(host) }),
            }
        }
    }
    // Erase the startup screen so the main UI renders cleanly with its own configured Renderer.
    if let Some(input) = boot_input.as_mut() {
        input.renderer.erase();
        mare_tui::out::drained().await;
    }
    Ok(BootOutcome::Ready { host, boot_input: boot_input.map(Box::new) })
}
