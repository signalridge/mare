## Summary

<!-- Describe the problem, root cause, and fix. -->

Fixes #

## Verification

- [ ] Ran the relevant workspace checks (`just check` or targeted `cargo nextest`, `cargo fmt --check`, `cargo clippy -- -D warnings`, `stylua --check`).
- [ ] **User-reported bug fix:** added a daily scenario test replaying the steps from the issue in `crates/mare-cli/tests/suite/daily_scenarios/post_release_bugs.rs` (or explained below why this PR is not a user-reported bug fix).
- [ ] Updated generated schemas/docs if CLI flags, options, RPC methods, or public Lua APIs changed (`just docs-check`).
