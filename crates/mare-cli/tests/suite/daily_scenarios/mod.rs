#![cfg(unix)]

//! Daily-use acceptance scenarios:
//! End-to-end daily-use acceptance tests using the real `opencode-go` provider (`just live`
//! profile, `glm-5.3-flash` by default, `MARE_LIVE_OPENCODE_KEY` or `OPENCODE_API_KEY` at
//! runtime, never printed) and real `herdr` TUI driving where
//! applicable (`herdr` isolated server in a temp directory so user panes are never disturbed).
//!
//! Marked `#[ignore]` so default `cargo test` / CI stay offline; run individually or all six with:
//! `cargo nextest run -p mare-cli --profile live --run-ignored only --no-capture -E 'test(/suite::daily_scenarios::/)'`

mod cli_scenarios;
mod helpers;
mod tui_and_oauth_scenarios;
