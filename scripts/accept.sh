#!/bin/sh
# Local final acceptance on the current platform (`just accept`). Runs fmt, stylua,
# clippy, nextest, doctests and cargo-deny, and
# appends one per-platform section to ${XDG_CACHE_HOME:-~/.cache}/mare/acceptance/<YYYY-MM-DD>.md.
# Never enables the usage-costing live tests: they are #[ignore]d (`just live` only), and the
# live-model credentials are unset below. Exit status: non-zero if any step failed.
set -u
cd "$(dirname "$0")/.." || exit 2

if [ -n "${MARE_ACCEPT_LEGACY_TMP_DIR+x}" ]; then
	echo "accept: MARE_ACCEPT_LEGACY_TMP_DIR env var is not permitted; use --selftest-legacy-tmp-dir" >&2
	exit 2
fi

# Required tools checked upfront (never silently skipped).
REQUIRED_TOOLS_COMMON="cargo-nextest cargo-deny stylua python3"
REQUIRED_TOOLS_UNIX="lua-language-server ast-grep lua"

accept_mode="standard"
steps_filter=""
no_git=0
allow_no_git=0
sandbox_degraded=0
legacy_tmp_dir="/tmp"
while [ "$#" -gt 0 ]; do
	case "$1" in
		--release)
			accept_mode="release"
			shift
			;;
		--steps)
			[ "$#" -ge 2 ] || { echo "accept: --steps requires an argument" >&2; exit 2; }
			steps_filter=$2
			shift 2
			;;
		--steps=*)
			steps_filter=${1#--steps=}
			shift
			;;
		--no-git)
			no_git=1
			shift
			;;
		--allow-no-git)
			allow_no_git=1
			shift
			;;
		--sandbox-degraded)
			sandbox_degraded=1
			shift
			;;
		--selftest-legacy-tmp-dir)
			[ "$#" -ge 2 ] || { echo "accept: --selftest-legacy-tmp-dir requires an argument" >&2; exit 2; }
			[ -n "$2" ] || { echo "accept: --selftest-legacy-tmp-dir requires a non-empty argument" >&2; exit 2; }
			legacy_tmp_dir=$2
			shift 2
			;;
		--selftest-legacy-tmp-dir=*)
			legacy_tmp_dir=${1#--selftest-legacy-tmp-dir=}
			[ -n "$legacy_tmp_dir" ] || { echo "accept: --selftest-legacy-tmp-dir requires a non-empty argument" >&2; exit 2; }
			shift
			;;
		-h|--help)
			echo "usage: $0 [--release] [--steps STEP1,STEP2,...] [--no-git] [--allow-no-git] [--sandbox-degraded]"
			exit 0
			;;
		*)
			echo "accept: unknown argument: $1" >&2
			exit 2
			;;
	esac
done

# Keep acceptance on its explicit build disk. The mbx shim otherwise remaps the target
# and can place its cache-agent socket beyond the Unix socket path limit.
export MBX_DISABLE=1

# Arch Linux may resolve cargo through the mbx shim; MBX_DISABLE makes it pass through.
[ -d "$HOME/.local/share/mbx/bin" ] && PATH="$HOME/.local/share/mbx/bin:$PATH"
export PATH
export CARGO_INCREMENTAL=0

# fail early when the build disk is nearly full: a full disk makes rustc crash with misleading
# errors (seen on Windows as STATUS_STACK_BUFFER_OVERRUN). MARE_ACCEPT_MIN_FREE_GB overrides 8.
min_gb=${MARE_ACCEPT_MIN_FREE_GB:-8}
target_dir=${CARGO_TARGET_DIR:-target}
mkdir -p "$target_dir" || exit 2
target_dir=$(cd "$target_dir" && pwd -P) || exit 2
export CARGO_TARGET_DIR="$target_dir"
# Socket fixtures append their own directory/name: keep their root short and on tmpfs.
if [ "${MARE_ACCEPT_TMPDIR+x}" = x ]; then
	[ -n "$MARE_ACCEPT_TMPDIR" ] || { echo 'accept: MARE_ACCEPT_TMPDIR is empty' >&2; exit 2; }
	temp_root=$MARE_ACCEPT_TMPDIR
elif [ -n "${TMPDIR:-}" ]; then
	temp_root=$TMPDIR
else
	temp_root=${TEMP:-${TMP:-/tmp}}
fi
case "$(uname -s)" in
	MINGW*|MSYS*|CYGWIN*|Windows_NT) temp_root=$(cygpath -u "$temp_root") || exit 2 ;;
esac
case "$temp_root" in /*) ;; *) echo 'accept: temporary root must be absolute' >&2; exit 2 ;; esac
[ -d "$temp_root" ] && [ -w "$temp_root" ] || { echo 'accept: temporary root is not a writable directory' >&2; exit 2; }
temp_root=$(cd "$temp_root" && pwd -P) || exit 2
case "$temp_root" in /dev|/dev/*) echo 'accept: /dev is not a test temporary root' >&2; exit 2 ;; esac
check_temp_ancestors() (
	ancestor=$1
	while :; do
		if [ -e "$ancestor/.git" ] || [ -L "$ancestor/.git" ]; then
			echo "accept: .git at or above temporary root: $ancestor/.git" >&2
			exit 2
		fi
		[ "$ancestor" = / ] && break
		ancestor=${ancestor%/*}
		[ -n "$ancestor" ] || ancestor=/
	done
)
check_temp_ancestors "$temp_root" || exit 2
if [ "$(uname -s)" = Linux ]; then
	fs_type=$(stat -f -c %T "$temp_root") || exit 2
	[ "$fs_type" = tmpfs ] || { echo "accept: temporary root is not tmpfs: $temp_root ($fs_type)" >&2; exit 2; }
fi
free_kb=$(df -Pk "$target_dir" 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${free_kb:-}" ] && [ "$free_kb" -lt $((min_gb * 1024 * 1024)) ]; then
	echo "accept: only $((free_kb / 1024 / 1024)) GB free under $target_dir (need $min_gb GB);" \
		"free space or share one CARGO_TARGET_DIR across checkouts" >&2
	exit 3
fi
unset MARE_LIVE_OPENCODE_KEY OPENCODE_API_KEY MARE_LIVE_MODEL MARE_LIVE_CANDY_MODEL MARE_LIVE_CANDY_EFFORT

outdir="${XDG_CACHE_HOME:-$HOME/.cache}/mare/acceptance"
mkdir -p "$outdir"
day=$(date +%Y-%m-%d)
report="$outdir/$day.md"
temp_identity() {
	case "$(uname -s)" in
		Darwin|*BSD) stat -f '%d:%i' "$1" ;;
		*) stat -c '%d:%i' "$1" ;;
	esac
}
work=$(mktemp -d "$temp_root/mare-accept.XXXXXX") || exit 2
case "$work" in "$temp_root"/mare-accept.??????) ;; *)
	echo "unexpected temp dir: $work" >&2
	exit 2
	;;
esac
run_tmp=
work_identity=
run_identity=
# Invoked through EXIT trap below.
# shellcheck disable=SC2329
cleanup_accept() {
	rc=$?
	if [ -n "$work_identity" ] && [ ! -L "$work" ] && [ "$(temp_identity "$work" 2>/dev/null || :)" = "$work_identity" ]; then
		if [ -f "$work/faux_target" ]; then
			faux_path=$(cat "$work/faux_target" 2>/dev/null || true)
			if [ -n "$faux_path" ] && ! rm -rf "$faux_path"; then
				echo "accept: failed to remove faux target dir: $faux_path" >&2
				[ "$rc" -eq 0 ] && rc=1
			fi
		fi
		if ! rm -rf "$work"; then
			echo "accept: failed to remove temporary directory: $work" >&2
			[ "$rc" -eq 0 ] && rc=1
		fi
	elif [ -e "$work" ] || [ -L "$work" ]; then
		echo "accept: temporary directory changed; left untouched: $work" >&2
		[ "$rc" -eq 0 ] && rc=1
	fi
	if [ -n "$run_tmp" ]; then
		if [ -n "$run_identity" ] && [ ! -L "$run_tmp" ] && [ "$(temp_identity "$run_tmp" 2>/dev/null || :)" = "$run_identity" ]; then
			if ! rm -rf "$run_tmp"; then
				echo "accept: failed to remove temporary directory: $run_tmp" >&2
				[ "$rc" -eq 0 ] && rc=1
			fi
		elif [ -e "$run_tmp" ] || [ -L "$run_tmp" ]; then
			echo "accept: temporary directory changed; left untouched: $run_tmp" >&2
			[ "$rc" -eq 0 ] && rc=1
		fi
	fi
	exit "$rc"
}
trap cleanup_accept EXIT
verify_owned_temp() {
	[ -d "$1" ] && [ ! -L "$1" ] || { echo "accept: temporary directory changed: $1" >&2; return 2; }
	[ "$(temp_identity "$1")" = "$2" ] || { echo "accept: temporary directory changed: $1" >&2; return 2; }
	physical=$(cd "$1" && pwd -P) || return 2
	parent=${physical%/*}
	[ -n "$parent" ] || parent=/
	[ "$physical" = "$1" ] && [ "$parent" = "$temp_root" ] || {
		echo "accept: temporary directory escaped its root: $1" >&2
		return 2
	}
	check_temp_ancestors "$physical"
}
work_identity=$(temp_identity "$work") || exit 2
verify_owned_temp "$work" "$work_identity" || exit 2
# Own a short namespace: siblings may belong to concurrent acceptance or dev work.
# Keep report bookkeeping outside it so every leftover child resource is a failure.
created_tmp=$(mktemp -d "$temp_root/XXXXXX") || exit 2
case "$created_tmp" in "$temp_root"/??????) run_tmp=$created_tmp ;; *)
	echo "unexpected temp dir: $created_tmp" >&2
	exit 2
	;;
esac
[ "$work" != "$run_tmp" ] || { echo 'accept: temporary directories are not distinct' >&2; exit 2; }
run_identity=$(temp_identity "$run_tmp") || exit 2
verify_owned_temp "$work" "$work_identity" && verify_owned_temp "$run_tmp" "$run_identity" || exit 2
export MARE_TMPDIR="$run_tmp" TMPDIR="$run_tmp" TEMP="$run_tmp" TMP="$run_tmp"
trap 'exit 130' INT
trap 'exit 143' TERM

snapshot_tmp() {
	for entry in "$legacy_tmp_dir"/mare-*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		printf '%s\n' "$entry"
	done
}
snapshot_tmp >"$work/tmp-before"
# Invoked through step below.
# shellcheck disable=SC2329
check_tmp_leaks() {
	leaked=0
	owned_leaked=0
	for entry in "$run_tmp"/* "$run_tmp"/.[!.]* "$run_tmp"/..?*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		echo "accept: owned temporary residue: $entry" >&2
		leaked=1
		owned_leaked=1
	done
	if [ "$owned_leaked" -eq 1 ]; then
		# The EXIT trap removes our namespace: retain bounded names/modes first,
		# never file contents or symlink targets outside the owned root.
		python3 - "$run_tmp" <<'PYMETA' >&2 || echo "accept: residue metadata unavailable" >&2
import os
import stat
import sys

pending = [(sys.argv[1], 0)]
seen = 0
while pending and seen < 80:
    path, depth = pending.pop()
    try:
        info = os.lstat(path)
        print("accept: residue metadata:", stat.filemode(info.st_mode), repr(path))
        seen += 1
        if stat.S_ISDIR(info.st_mode) and depth < 3:
            with os.scandir(path) as entries:
                for index, entry in enumerate(entries):
                    if index >= 80:
                        break
                    pending.append((entry.path, depth + 1))
    except OSError as error:
        print("accept: residue metadata error:", repr(path), str(error))
PYMETA
	fi
	for entry in "$legacy_tmp_dir"/mare-*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		if ! grep -Fqx -- "$entry" "$work/tmp-before"; then
			echo "accept: new $legacy_tmp_dir residue: $entry" >&2
			leaked=1
		fi
	done
	return "$leaked"
}

# a tree shipped as an archive (remote acceptance) has no .git: it carries .shipped-sha instead
tree_sha=""
if [ "$no_git" -eq 0 ] && [ -e .git ]; then
	sha=$(git rev-parse --verify HEAD 2>/dev/null || head -c 40 .shipped-sha 2>/dev/null || echo unknown)
	tree_sha=$(git rev-parse --verify 'HEAD^{tree}' 2>/dev/null || echo unknown)
	if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
		dirty=" (dirty tree)"
	else
		dirty=" (clean checkout)"
	fi
else
	sha=$(head -c 40 .shipped-sha 2>/dev/null || echo unknown)
	dirty=""
fi
host=$(uname -n 2>/dev/null || echo unknown)
platform="$(uname -s) $(uname -m)"
started=$(date '+%Y-%m-%d %H:%M:%S %z')
t_all=$(date +%s)

table="$work/table"
: >"$table"
failed=0
nextest_line=""
doc_line=""
tools_line=""

have() { command -v "$1" >/dev/null 2>&1; }

is_windows=0
case "$(uname -s)" in
	MINGW*|MSYS*|CYGWIN*|Windows_NT) is_windows=1 ;;
esac

should_run() {
	[ -z "$steps_filter" ] && return 0
	case ",$steps_filter," in
		*",$1,"*) return 0 ;;
		*) return 1 ;;
	esac
}

check_required_tools() {
	tools=$REQUIRED_TOOLS_COMMON
	if [ "$is_windows" -eq 0 ]; then
		tools="$tools $REQUIRED_TOOLS_UNIX"
	fi
	missing=0
	versions=""
	for tool in $tools; do
		if ! have "$tool"; then
			echo "accept: missing required tool: $tool (install $tool and ensure it is on PATH)" >&2
			missing=1
			continue
		fi
		if [ "$tool" = "lua" ]; then
			ver=$("$tool" -v 2>&1 | head -n 1)
		else
			ver=$("$tool" --version 2>&1 | head -n 1)
		fi
		echo "$tool: $ver"
		if [ -z "$versions" ]; then
			versions="$ver"
		else
			versions="$versions; $ver"
		fi
	done
	printf '%s' "$versions" >"$work/tools"
	return "$missing"
}

partial=$sandbox_degraded

# step NAME CMD...: stream output, record status and duration in the table, fail acceptance on non-zero exit
step() {
	name=$1
	shift
	should_run "$name" || return 0
	echo "==> $name: $*"
	t0=$(date +%s)
	{
		"$@"
		echo $? >"$work/rc"
	} 2>&1 | tee "$work/last"
	rc=$(cat "$work/rc")
	dur=$(($(date +%s) - t0))
	if [ "$rc" -eq 0 ]; then
		status=pass
	else
		status=FAIL
		failed=1
	fi
	printf '| %s | %s | %ss |\n' "$name" "$status" "$dur" >>"$table"
}

na() {
	should_run "$1" || return 0
	echo "==> $1: n/a ($2)"
	printf '| %s | n/a (%s) | - |\n' "$1" "$2" >>"$table"
}

# missing NAME REASON: a required step that could not run fails the acceptance
missing() {
	echo "==> $1: FAILED ($2)"
	printf '| %s | FAIL (%s) | - |\n' "$1" "$2" >>"$table"
	failed=1
}

sum_results() {
	awk '/^test result:/ { p += $4; f += $6; i += $8 } END { printf "%d passed, %d failed, %d ignored", p, f, i }' "$work/last"
}

step "required tools" check_required_tools
[ -f "$work/tools" ] && tools_line=$(cat "$work/tools")
step "cargo fmt --check" cargo fmt --all --check
step "stylua --check" stylua --check crates/mare-plugin/preset
resolve_lua_api_base() {
	[ "$no_git" -eq 0 ] && [ -e .git ] && have git || return 1
	head_sha=$(git rev-parse --verify HEAD 2>/dev/null) || return 1
	upstream=$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)
	for ref in main "$upstream" origin/main; do
		[ -n "$ref" ] || continue
		git rev-parse --verify "${ref}^{commit}" >/dev/null 2>&1 || continue
		mb=$(git merge-base "$ref" "$head_sha" 2>/dev/null || true)
		if [ -n "$mb" ] && [ "$mb" != "$head_sha" ]; then
			printf '%s\n' "$mb"
			return 0
		fi
	done
	if prev_sha=$(git rev-parse --verify HEAD~1 2>/dev/null); then
		printf '%s\n' "$prev_sha"
	else
		printf '%s\n' "$head_sha"
	fi
}

if [ "$is_windows" -eq 1 ]; then
	na "lua public API selftest" "Linux/macOS only"
	na "lua public API lint" "Linux/macOS only"
else
	lua_api_base=""
	if lua_api_base=$(resolve_lua_api_base); then
		:
	else
		lua_api_base=""
	fi
	step "lua public API selftest" python3 scripts/check_lua_public_api.py --self-test
	if [ -n "$lua_api_base" ]; then
		step "lua public API lint" python3 scripts/check_lua_public_api.py --base "$lua_api_base"
	else
		step "lua public API lint" python3 scripts/check_lua_public_api.py
	fi
fi

case "$(uname -s)" in
	MINGW*|MSYS*|CYGWIN*|Windows_NT)
		spec_platform="windows"
		mare_bin_name="mare.exe"
		;;
	Darwin)
		spec_platform="macos"
		mare_bin_name="mare"
		;;
	*)
		spec_platform="linux"
		mare_bin_name="mare"
		;;
esac

step "acceptance selftests" python3 scripts/test_accept.py
step "test home selftests" python3 scripts/test_nextest_test_home.py
step "third-party notices" sh -c 'python3 scripts/check_notices.py --self-test && python3 scripts/check_notices.py'
if [ "$is_windows" -eq 1 ]; then
	na "generated options" "Linux/macOS only"
else
	step "generated options" python3 scripts/gen_options.py --check
fi
step "clippy -D warnings" cargo clippy --workspace --all-targets -- -D warnings
# On its own line: contracts, mare rpc schema, the runtime catalog, LuaLS
# service types and primitive types agree; schema snapshots (protocol and contract) are current
step "API parity" cargo nextest run --workspace --no-fail-fast -E 'test(/api_parity::/) | test(contract_schema_snapshot) | test(schema_snapshot) | test(protocol_roots_are_reserved_or_owned_by_a_builtin_contract)'
if [ "$sandbox_degraded" -eq 1 ]; then
	unset MARE_TEST_EXPECT_BACKEND
	sandbox_line="degraded — no expected backend, skips allowed (not a complete acceptance)"
else
	case "$spec_platform" in
		linux) MARE_TEST_EXPECT_BACKEND=bwrap ;;
		macos) MARE_TEST_EXPECT_BACKEND=seatbelt ;;
		windows) MARE_TEST_EXPECT_BACKEND=windows-acl ;;
	esac
	export MARE_TEST_EXPECT_BACKEND
	sandbox_line="expected $MARE_TEST_EXPECT_BACKEND"
fi
# Invoked through step below.
# shellcheck disable=SC2329
check_pending_snapshots() {
	pending=$(find . -path './target' -prune -o -path './.git' -prune -o -name '*.snap.new' -print 2>/dev/null || true)
	if [ -n "$pending" ]; then
		echo "accept: pending snapshot files (*.snap.new) left in tree:" >&2
		printf '%s\n' "$pending" >&2
		return 1
	fi
}

INSTA_UPDATE=no
MARE_UPDATE_SNAPSHOTS=no
export INSTA_UPDATE MARE_UPDATE_SNAPSHOTS
step "nextest" cargo nextest run --workspace --no-fail-fast --profile gate
if should_run "nextest"; then
	nextest_line=$(grep -E 'Summary \[' "$work/last" | tail -n 1 | sed 's/^ *//')
fi
step "pending snapshots" check_pending_snapshots
step "doctests" python3 scripts/nextest_test_home.py --doc
if should_run "doctests"; then
	# read now: every later step overwrites $work/last
	doc_line=$(sum_results)
fi
# generated reference pages are current, made with this tree's own binary.
# Cargo reports where it put the binary (CARGO_BUILD_TARGET adds a triple directory), so a guess
# never picks up a stale `mare` elsewhere in a shared target directory; no binary fails the step.
# Under --steps, "generated docs current" needs "build mare" selected too (otherwise it fails).
step "build mare" sh -c 'cargo build -p mare-cli --bin mare --message-format=json >"$1"' - "$work/build.json"
mare_bin=$(grep '"reason":"compiler-artifact"' "$work/build.json" 2>/dev/null | grep '"name":"mare"' |
	grep '"kind":\["bin"\]' | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\\/g' | tail -n 1)
if [ -n "$mare_bin" ]; then
	MARE_BIN="$mare_bin" step "gen_docs selftests" python3 -m unittest scripts/test_gen_docs.py scripts/test_docs_keymaps.py
else
	step "gen_docs selftests" python3 -m unittest scripts/test_gen_docs.py scripts/test_docs_keymaps.py
fi
if should_run "generated docs current"; then
	if [ "$is_windows" -eq 1 ] && ! have lua; then
		na "generated docs current" "Linux/macOS only (no lua)"
	elif [ -n "$mare_bin" ] && [ -x "$mare_bin" ]; then
		step "generated docs current" python3 scripts/gen_docs.py --mare "$mare_bin" --check
	else
		missing "generated docs current" "cargo reported no mare executable"
	fi
fi
step "cargo deny check" cargo deny check

# Invoked through step below.
# shellcheck disable=SC2329
run_release_tests_compile() {
	cargo check --release --workspace --all-targets || return 1
	echo "Test modules excluded under not(debug_assertions) (compiled only in debug):"
	python3 scripts/check_seams.py --list-excluded-test-modules
}

# Invoked through step below.
# shellcheck disable=SC2329
run_release_seams_inert() {
	cargo build --release --locked -p mare-cli --bin mare || return 1
	python3 scripts/check_seams.py --verify-release "$target_dir/release/$mare_bin_name"
}

cleanup_faux_target() {
	[ -n "${faux_target:-}" ] || return 0
	if ! rm -rf "$faux_target"; then
		echo "accept: failed to remove faux target dir: $faux_target" >&2
		return 1
	fi
	rm -f "$work/faux_target"
	faux_target=""
	return 0
}

# Invoked through step below.
# shellcheck disable=SC2329
run_release_faux_absent() {
	python3 scripts/check_seams.py --verify-faux-absent "$target_dir/release/$mare_bin_name" || return 1
	# Register the deterministic path atomically before creating the directory so
	# cleanup_accept removes it even if SIGINT/SIGTERM lands right after mkdir.
	# Never register or delete a path that already existed before mkdir.
	faux_target="$target_dir/faux-target.${work##*/mare-accept.}"
	if [ -e "$faux_target" ] || [ -L "$faux_target" ]; then
		echo "accept: faux target path already exists: $faux_target" >&2
		faux_target=""
		return 1
	fi
	if ! { printf '%s\n' "$faux_target" >"$work/faux_target.tmp" && mv "$work/faux_target.tmp" "$work/faux_target"; }; then
		rm -f "$work/faux_target.tmp"
		faux_target=""
		return 1
	fi
	if ! mkdir "$faux_target"; then
		rm -f "$work/faux_target"
		faux_target=""
		return 1
	fi
	step_rc=0
	if ! CARGO_TARGET_DIR="$faux_target" cargo build --release --locked -p mare-cli --bin mare --features faux; then
		step_rc=1
	elif ! python3 scripts/check_seams.py --verify-faux-present "$faux_target/release/$mare_bin_name"; then
		step_rc=1
	elif python3 scripts/check_seams.py --verify-faux-absent "$faux_target/release/$mare_bin_name" >/dev/null 2>&1; then
		echo "accept: --verify-faux-absent unexpectedly passed on faux-enabled binary" >&2
		step_rc=1
	fi
	cleanup_faux_target || step_rc=1
	return "$step_rc"
}

# Invoked through step below.
# shellcheck disable=SC2329
run_debug_seams_active() {
	cargo build --locked -p mare-cli --bin mare || return 1
	python3 scripts/check_seams.py --verify-debug "$target_dir/debug/$mare_bin_name"
}

if should_run "cordis vendor check"; then
	if [ "$no_git" -eq 1 ] && [ "$allow_no_git" -eq 1 ] && [ -z "${CORDIS_LUA_REPO:-}" ] && [ ! -e "../cordis-lua/.git" ] && [ ! -e "$HOME/ghq/github.com/signalridge/cordis-lua/.git" ]; then
		partial=1
		na "cordis vendor check" "no-git mode (--allow-no-git)"
	else
		step "cordis vendor check" bash scripts/vendor-cordis.sh --check
	fi
fi
if should_run "cordis suite"; then
	if [ "$no_git" -eq 1 ] && [ "$allow_no_git" -eq 1 ] && [ -z "${CORDIS_LUA_REPO:-}" ] && [ ! -e "../cordis-lua/.git" ] && [ ! -e "$HOME/ghq/github.com/signalridge/cordis-lua/.git" ]; then
		partial=1
		na "cordis suite" "no-git mode (--allow-no-git)"
	elif [ -n "$mare_bin" ] && [ -x "$mare_bin" ]; then
		step "cordis suite" bash scripts/cordis-suite.sh "$mare_bin"
	else
		missing "cordis suite" "cargo reported no mare executable"
	fi
fi

if [ "$accept_mode" = "release" ]; then
	step "release tests compile" run_release_tests_compile
	step "release seams inert" run_release_seams_inert
	step "release faux absent" run_release_faux_absent
	step "debug seams active" run_debug_seams_active
fi

step "temporary-file cleanup" check_tmp_leaks

if [ -n "$steps_filter" ] && [ ! -s "$table" ]; then
	echo "accept: --steps matched no known steps: $steps_filter" >&2
	exit 2
fi

total=$(($(date +%s) - t_all))
if [ "$failed" -ne 0 ]; then
	verdict=FAIL
elif [ -n "$steps_filter" ] || [ "$partial" -ne 0 ]; then
	verdict=PARTIAL
else
	verdict=PASS
fi
{
	echo "<!-- accept-section-begin -->"
	echo "## $platform @ $host ($accept_mode): $verdict"
	echo
	if [ -n "$tree_sha" ]; then
		echo "- git: \`$sha\` (tree \`$tree_sha\`)$dirty"
	else
		echo "- git: \`$sha\`$dirty"
	fi
	echo "- scope: this platform only ($platform; does not cover the other release platforms)"
	echo "- started: $started, total ${total}s"
	[ -n "$tools_line" ] && echo "- tools: $tools_line"
	echo "- tests: ${nextest_line:-n/a}"
	echo "- sandbox: $sandbox_line"
	echo "- doctests: ${doc_line:-n/a}"
	echo
	echo "| step | result | duration |"
	echo "|---|---|---|"
	cat "$table"
	echo
	echo "<!-- accept-section-end -->"
} >"$work/section"

cat "$work/section" >>"$report"
echo
cat "$work/section"
echo "appended to $report"
if [ "$failed" -ne 0 ]; then
	exit "$failed"
fi
if [ "$accept_mode" = "release" ] && [ "$verdict" = "PARTIAL" ]; then
	echo "accept: release mode requires PASS (refusing PARTIAL verdict)" >&2
	exit 1
fi
exit 0
