# mare dev tasks

default: check

# format Rust + Lua
fmt:
    cargo fmt
    stylua crates/mare-plugin/preset

# lint without modifying files (what `just accept` runs)
lint:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    stylua --check crates/mare-plugin/preset
    python3 scripts/check_lua_public_api.py --self-test
    python3 scripts/check_lua_public_api.py
    python3 scripts/check_release_gate.py --self-test
    python3 scripts/release_manifests.py --self-test
    python3 scripts/check_release_policy.py --self-test
    python3 scripts/check_release_policy.py

# Nextest is required so each test is started by the isolated-home wrapper.
test *args:
    if ! command -v cargo-nextest >/dev/null; then echo "cargo-nextest 0.9.138 or newer is required" >&2; exit 1; fi
    cargo nextest run --workspace {{args}}
    python3 scripts/nextest_test_home.py --doc

check: lint test

# verify generated options and reference documentation are up to date
docs-check:
    python3 scripts/gen_options.py --check
    cargo build -p mare-cli --bin mare
    python3 scripts/gen_docs.py --check
    python3 -m unittest scripts/test_gen_docs.py scripts/test_docs_keymaps.py

# compare built-in provider presets and models_dev ids against models.dev (developer check; not in CI)
presets-drift *args:
    python3 scripts/presets_drift.py {{args}}

# debug builds read the preset (crates/mare-plugin/preset) from disk; /reload picks up edits
run *ARGS:
    cargo run --bin mare -- {{ARGS}}

release:
    cargo build --release

# unused dependencies (cargo install cargo-shear)
# provenance of the vendored cordis-lua (crates/mare-kernel/lua/cordis/VENDOR); needs a cordis-lua
# checkout ($CORDIS_LUA_REPO or ../cordis-lua), so it is not part of `test`
vendor-check:
    scripts/vendor-cordis.sh --check
# cordis-lua's own test suite on the mlua executor against the vendored copy; needs a cordis-lua
# checkout, so it is not part of `test`
cordis-suite *args:
    scripts/cordis-suite.sh {{args}}

shear:
    cargo shear

# licenses, advisories, bans and sources (deny.toml)
deny:
    if command -v cargo-deny >/dev/null; then cargo deny check; else echo "cargo-deny not installed (cargo install cargo-deny)" >&2; exit 1; fi

# local final acceptance on this platform: fmt, stylua,
# clippy, nextest, doctests, cargo-deny; never runs live tests (`just live`).
# Appends a per-platform section to ${XDG_CACHE_HOME:-~/.cache}/mare/acceptance/<YYYY-MM-DD>.md
accept *args:
    #!/usr/bin/env sh
    exec sh scripts/accept.sh {{args}}

# performance reference values: startup, RSS, frame p99, plugin load — not run in CI
# `just bench --check` fails only when measurements are incomplete or invalid
bench *args:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p mare-cli --bin mare --features faux
    target_dir="${CARGO_TARGET_DIR:-target}"
    case "$(uname -s)" in
        MINGW*|MSYS*) cargo run --release -p mare-cli --example bench_windows --features faux -- "$target_dir/release/mare.exe" {{args}} ;;
        *) python3 scripts/bench.py "$target_dir/release/mare" {{args}} ;;
    esac

# end-to-end tests against a real model: opencode Go `glm-5.3-flash` (override with
# MARE_LIVE_MODEL; deepseek-v4.1-flash is only its per-test fallback when glm is unavailable).
# Requires OPENCODE_API_KEY (or MARE_LIVE_OPENCODE_KEY for `just live`) in the environment; never in CI.
# complex real-model evals through Harbor: evals/tasks (tool-use cases, dev-* development tasks)
# and fixed subsets of Terminal-Bench 2.0, SWE-bench Verified, FeatureBench-lite and Aider
# polyglot (evals/*.txt). One try per task, billed model: the table reports time and cost. Needs
# Docker and `uv tool install harbor`; not in CI.
# group: own | tbench | dev | swebench | featurebench | polyglot | all (default: all)
# eval helpers: sync evals/lib/mare_log.py into every task's tests/ dir (for direct `harbor run`)
eval-sync:
    python3 -c 'import pathlib, shutil; src = pathlib.Path("evals/lib/mare_log.py"); [shutil.copyfile(src, t / "tests" / "mare_log.py") for t in sorted(pathlib.Path("evals/tasks").iterdir()) if (t / "task.toml").is_file()]'

eval group="all" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export OPENCODE_API_KEY="${OPENCODE_API_KEY:-}"
    python3 evals/run.py {{group}} {{args}}

# real terminal through herdr (TUI e2e, local layer): skipped outside a herdr pane
tui-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -p mare-cli
    if [ "${HERDR_ENV:-}" != 1 ] || ! command -v herdr >/dev/null; then
        echo "skipped: not inside a herdr pane"; exit 0
    fi
    export OPENCODE_API_KEY="${OPENCODE_API_KEY:-}"
    python3 scripts/tui_e2e.py

live *args:
    #!/usr/bin/env bash
    set -euo pipefail
    MARE_LIVE_OPENCODE_KEY="${MARE_LIVE_OPENCODE_KEY:-${OPENCODE_API_KEY:-}}"
    if [ -z "$MARE_LIVE_OPENCODE_KEY" ]; then
        echo "error: live key unavailable (set MARE_LIVE_OPENCODE_KEY or OPENCODE_API_KEY)" >&2
        exit 1
    fi
    export MARE_LIVE_OPENCODE_KEY
    filter='test(/suite::live::/)'
    if command -v cargo-nextest >/dev/null; then
        cargo nextest run -p mare-cli --profile live --run-ignored only --no-capture -E "$filter" {{args}}
    else
        cargo test -p mare-cli --test all -- --ignored --nocapture live:: {{args}}
    fi

# re-record an opencode-go cassette (tool call, compaction, session resume) into
# crates/mare-cli/tests/fixtures/cassettes/mare-record/ through mare's HTTP recorder; replaced only
# after it replays. `just record [model] [route]` (default glm-5.3-flash anthropic)
record model="glm-5.3-flash" route="anthropic":
    #!/usr/bin/env bash
    set -euo pipefail
    export OPENCODE_API_KEY="${OPENCODE_API_KEY:-}" MARE_RECORD_MODEL={{model}} MARE_RECORD_ROUTE={{route}}
    cargo nextest run -p mare-cli --profile live --run-ignored only --no-capture -E 'test(=suite::replay_corpus::record_opencode_go_corpus)'
