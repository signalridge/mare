# Contributing to mare

## Requirements

- Rust `1.100.0+` (matching `Cargo.toml`)
- `cargo-nextest` (`0.9.138+`), `cargo-deny`, `stylua`, `python3`, and `just`
- Node.js (`>=22.23.4`) and npm (`>=9.6.5`) for `docs-site/` builds

## Common Checks

Run the workspace lint, type/schema guards, and test suite before opening a pull request:

```sh
just check       # fmt, clippy -D warnings, stylua, API/schema checks, nextest
just docs-check  # generated options, CLI/RPC/Lua reference docs, unit tests
just run         # run the debug binary (reads crates/mare-plugin/preset from disk)
```

For documentation site previews:

```sh
cd docs-site && npm ci && npm run build
```

## Guidelines

- Keep process sandboxing, trust checks, session persistence, and credential isolation in Rust; keep high-level agent, tool, provider, and UI behavior in built-in Lua plugins (`crates/mare-plugin/preset/`) using public `mare.*` and service APIs.
- Regenerate schemas/docs (`python3 scripts/gen_options.py`, `python3 scripts/gen_docs.py`) whenever options, CLI flags, RPC methods, or public Lua APIs change.
- Run `cargo fmt` and `stylua crates/mare-plugin/preset` before committing.

## Commits and pull requests

Use Conventional Commits for commits and release-input pull request titles. CI reports title drift; temporary WIP titles can be replaced before merge when the title should feed Release Please:

- `feat`
- `fix`
- `perf`
- `refactor`
- `deps`
- `security`
- `revert`
- `docs`
- `style`
- `chore`
- `test`
- `ci`
- `build`

Accepted forms are `<type>: <description>` and `<type>(<scope>): <description>`, for example `feat: add session export` or `fix(rpc): reject missing session_id`. Mark a breaking change with `!` (such as `feat!: unify service contracts`) and explain it with a `BREAKING CHANGE:` footer when the pull request body needs more detail.

Keep pull requests focused and squash-merge through `main` with a Conventional Commit title. A title is release input, not only style: Release Please classifies it into the changelog and version proposal.

## Release automation

Release Please owns `CHANGELOG.md`, `.release-please-manifest.json`, workspace versions in `Cargo.toml` / `Cargo.lock`, the release pull request, and `v*` tags. Do not hand-edit release versions or create release tags as part of an ordinary contribution.

After changes reach `main`, Release Please opens or updates one release pull request (`chore: release <version>`). Before merging, ensure `crates/mare-plugin/preset/NEWS.md` has a matching `## <version>` section (checked by CI on the release PR) and review `CHANGELOG.md` and the version bump together. Merging that pull request creates the matching `v*` tag and a draft GitHub Release using `GH_PAT`. The tag-triggered `release.yaml` workflow validates the version, waits for the same-commit `ci.yml` gate to succeed, then builds, signs, uploads assets, and publishes the release. Full three-platform release acceptance runs on a schedule or manually via `accept-nightly.yml`. A missing tag or GitHub Release for the manifest version is an operational metadata mismatch to reconcile.
