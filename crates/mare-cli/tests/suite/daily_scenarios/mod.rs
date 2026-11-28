//! Daily-use acceptance scenarios:
//!
//! 1. Offline post-1.0.0 user-reported bug scenarios (`post_release_bugs`): deterministic daily
//!    scenarios replaying the steps from user-reported issues so regressions are caught on every
//!    standard `cargo nextest` run.
//! 2. Live real-model scenarios (`cli_scenarios`, `tui_and_oauth_scenarios`, Unix-only):
//!    end-to-end daily-use acceptance tests using the real `opencode-go` provider (`just live`
//!    profile, `glm-5.3-flash` by default, `MARE_LIVE_OPENCODE_KEY` or `OPENCODE_API_KEY` at
//!    runtime, never printed) and real `herdr` TUI driving where applicable (`herdr` isolated
//!    server in a temp directory so user panes are never disturbed).
//!
//! Live scenarios are marked `#[ignore]` so default `cargo test` / CI stay offline; run them with:
//! `cargo nextest run -p mare-cli --profile live --run-ignored only --no-capture -E 'test(/suite::daily_scenarios::/)'`

#[cfg(unix)]
mod cli_scenarios;
#[cfg(unix)]
mod helpers;
mod post_release_bugs;
#[cfg(unix)]
mod tui_and_oauth_scenarios;
