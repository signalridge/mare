#!/usr/bin/env bash
# Vendor cordis-lua into crates/mare-kernel/lua/cordis/.
#
#   scripts/vendor-cordis.sh <ref>     copy lua/cordis/ of cordis-lua at <ref>, its LICENSE and
#                                      NOTICE, and record <ref> and its commit in VENDOR
#   scripts/vendor-cordis.sh --check   verify the copy against the recorded ref and commit
#
# <ref> is a tag (v0.2.0), branch or commit of the cordis-lua repository at $CORDIS_LUA_REPO
# (default: ../cordis-lua next to this repository, else the ghq location). The copy is
# byte-for-byte; lua/cordis/host/ (the reference scheduler for cordis-lua's own tests) is not
# vendored: inside mare the host is mare's own.
#
# --check needs that repository, so it is a separate provenance step (`just vendor-check`), not
# part of the default tests: the vendored files are in this repository and run without it.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
lua_dir="$repo_root/crates/mare-kernel/lua"
dest="$lua_dir/cordis"
vendor_file="$lua_dir/cordis/VENDOR"

src="${CORDIS_LUA_REPO:-}"
if [[ -z "$src" ]]; then
	for candidate in "$repo_root/../cordis-lua" "$(ghq root 2>/dev/null || echo /nonexistent)/github.com/signalridge/cordis-lua" "${HOME:-/nonexistent}/ghq/github.com/signalridge/cordis-lua"; do
		if [[ -e "$candidate/.git" ]]; then
			src="$candidate"
			break
		fi
	done
fi
[[ -n "$src" && -e "$src/.git" ]] || {
	echo "cordis-lua repository not found; set CORDIS_LUA_REPO" >&2
	exit 2
}

# <commit> <outdir>: what the vendored directory must contain for <commit>
export_commit() {
	local commit="$1" out="$2"
	local extra_notice="" extra_upstream=""
	if git -C "$src" cat-file -e "$commit:NOTICE" 2>/dev/null; then
		extra_notice="NOTICE"
	fi
	if git -C "$src" cat-file -e "$commit:UPSTREAM" 2>/dev/null; then
		extra_upstream="UPSTREAM"
	fi
	mkdir -p "$out"
	git -C "$src" archive "$commit" lua/cordis LICENSE ${extra_notice:+"$extra_notice"} ${extra_upstream:+"$extra_upstream"} | tar -x -C "$out"
	rm -rf "$out/lua/cordis/host"
	mv "$out/LICENSE" "$out/lua/cordis/LICENSE"
	if [[ -n "$extra_notice" && -f "$out/$extra_notice" ]]; then
		mv "$out/$extra_notice" "$out/lua/cordis/$extra_notice"
	fi
	if [[ -n "$extra_upstream" && -f "$out/$extra_upstream" ]]; then
		mv "$out/$extra_upstream" "$out/lua/cordis/$extra_upstream"
	fi
}

vendor_text() { # <ref> <commit>
	printf 'repo https://github.com/signalridge/cordis-lua\nref %s\ncommit %s\n' "$1" "$2"
}

tmp_root="${MARE_TMPDIR:-${TMPDIR:-${TEMP:-${TMP:-/tmp}}}}"
tmp="$(mktemp -d "$tmp_root/mare-vendor-cordis.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT INT TERM

if [[ "${1:-}" == "--check" ]]; then
	[[ -f "$vendor_file" ]] || {
		echo "missing $vendor_file" >&2
		exit 1
	}
	ref="$(sed -n 's/^ref //p' "$vendor_file")"
	commit="$(sed -n 's/^commit //p' "$vendor_file")"
	[[ -n "$ref" && -n "$commit" ]] || {
		echo "$vendor_file lacks ref or commit" >&2
		exit 1
	}
	# the recorded ref must still name the recorded commit (a moved tag or an edited ref fails)
	resolved="$(git -C "$src" rev-parse --verify --quiet "$ref^{commit}" || true)"
	if [[ "$resolved" != "$commit" ]]; then
		resolved="$(git -C "$src" rev-parse --verify --quiet "refs/remotes/origin/$ref^{commit}" || true)"
	fi
	if [[ "$resolved" != "$commit" ]]; then
		head_commit="$(git -C "$src" rev-parse --verify --quiet HEAD^{commit} || true)"
		if [[ "$head_commit" == "$commit" ]] && {
			[[ "$ref" == "$commit" ]] || git -C "$src" tag --points-at HEAD | grep -qxF "$ref" || [[ "$(git -C "$src" rev-parse --abbrev-ref HEAD 2>/dev/null || true)" == "$ref" ]]
		}; then
			resolved="$commit"
		fi
	fi
	if [[ "$resolved" != "$commit" ]]; then
		echo "VENDOR ref $ref resolves to ${resolved:-nothing}, not the recorded commit $commit" >&2
		exit 1
	fi
	export_commit "$commit" "$tmp"
	vendor_text "$ref" "$commit" >"$tmp/lua/cordis/VENDOR"
	if diff -r "$tmp/lua/cordis" "$dest" >/dev/null; then
		echo "crates/mare-kernel/lua/cordis matches cordis-lua $ref ($commit)"
	else
		echo "crates/mare-kernel/lua/cordis differs from cordis-lua $ref ($commit):" >&2
		diff -r "$tmp/lua/cordis" "$dest" >&2 || true
		exit 1
	fi
	exit 0
fi

ref="${1:?usage: vendor-cordis.sh <ref> | --check}"
commit="$(git -C "$src" rev-parse --verify "$ref^{commit}")"
export_commit "$commit" "$tmp"
vendor_text "$ref" "$commit" >"$tmp/lua/cordis/VENDOR"
rm -rf "$dest"
mkdir -p "$lua_dir"
cp -R "$tmp/lua/cordis" "$dest"
echo "vendored cordis-lua $ref ($commit) into crates/mare-kernel/lua/cordis"
