//! The single integration-test binary for mare-cli (Codex `core/tests/all.rs` layout): one link
//! instead of one per file. Suites live in `suite/<feature>.rs`, shared support in `common/`.
//! Run one suite with `cargo nextest run -p mare-cli -E 'test(/^suite::agent::/)'` or
//! `cargo test -p mare-cli --test all agent::`.

mod common;
mod suite;
