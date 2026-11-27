//! Generic presentation lifecycle via public services and native parser/descriptor bindings.
//! No frontend capabilities or mode are replaced; these are headless service tests.

mod fenced_lifecycle;
// Unix-only: asserts raw Kitty graphics APC sequences (`\x1b_G...`) which ConPTY does not pass through.
#[cfg(unix)]
mod pty_graphics;
mod rpc_presenters;
mod tui_artifacts;
