#!/usr/bin/env bash
# Run cordis-lua's test suite on mare's mlua executor (`mare test`) against the vendored copy in
# crates/mare-kernel/lua/cordis/. Needs a cordis-lua checkout
# ($CORDIS_LUA_REPO or ../cordis-lua) for the specs at the VENDOR commit, so it is not part of
# `just test`. Usage: scripts/cordis-suite.sh [mare binary] (default: target/debug/mare)
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
mare_bin="${1:-${MARE_BIN:-$repo_root/target/debug/mare}}"
[[ -x "$mare_bin" ]] || { echo "mare binary not found: $mare_bin" >&2; exit 2; }
case "$mare_bin" in
	/*|[A-Za-z]:/*|[A-Za-z]:\\*) ;;
	*) mare_bin="$(cd -- "$(dirname -- "$mare_bin")" && pwd)/$(basename -- "$mare_bin")" ;;
esac
src="${CORDIS_LUA_REPO:-}"
if [[ -z "$src" ]]; then
	for candidate in "$repo_root/../cordis-lua" "$(ghq root 2>/dev/null || echo /nonexistent)/github.com/signalridge/cordis-lua"; do
		[[ -e "$candidate/.git" ]] && { src="$candidate"; break; }
	done
fi
[[ -n "$src" && -e "$src/.git" ]] || { echo "cordis-lua repository not found; set CORDIS_LUA_REPO" >&2; exit 2; }
commit="$(sed -n 's/^commit //p' "$repo_root/crates/mare-kernel/lua/cordis/VENDOR")"
tmp_root="${MARE_TMPDIR:-${TMPDIR:-${TEMP:-${TMP:-/tmp}}}}"
tmp="$(mktemp -d "$tmp_root/mare-cordis-suite.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT INT TERM
git -C "$src" archive "$commit" test | tar -x -C "$tmp"
mkdir -p "$tmp/home/config" "$tmp/run"
cp "$repo_root/scripts/cordis-suite/cordis_suite_spec.lua" "$tmp/run/"
cd "$tmp/run"
MARE_HOME="$tmp/home" CORDIS_SUITE_TESTS="$tmp/test" CORDIS_SUITE_DIR="$repo_root/scripts/cordis-suite" \
	CORDIS_SUITE_REPORT="${CORDIS_SUITE_REPORT:-$tmp/report.txt}" "$mare_bin" test --clean cordis_suite_spec.lua
