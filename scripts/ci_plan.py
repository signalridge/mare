#!/usr/bin/env python3
"""Plan PR CI jobs from changed files and evaluate the `PR checks` summary gate.

Stdlib-only Python. Design follows signalridge/mare#106:
  - Rust package scope is the reverse transitive closure from `cargo metadata --locked`
    across normal, dev, and build dependency edges.
  - An explicit path table chooses platforms, narrowed Cargo/nextest targets, and
    non-Cargo consumers. One file may select multiple consumers.
  - Fail wide: any unmatched file, unresolvable diff, or planner failure selects the
    full matrix.
  - `--gate` evaluates the single required `PR checks` summary job: it fails if the
    planner failed or if any expected job did not succeed (including skipped/cancelled),
    while allowing unexpected jobs to be skipped.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import pathlib
import re
import subprocess
import sys
import unittest
from dataclasses import dataclass, field
from typing import Any

ALL_PLATFORMS = ("linux", "macos", "windows")
ALL_PR_JOBS = (
    "yamllint",
    "actionlint",
    "scripts",
    "deny",
    "lua",
    "test-linux",
    "test-macos",
    "test-windows",
    "installer-ps1",
    "docs-site",
    "docs-gen",
    "trivy",
    "nix",
    "release-dry-run",
)

# `mare-cli --test all` suites known to compile and run on specific OSes.
UNIX_ONLY_CLI_SUITES = frozenset({"terminal", "net_proxy_linux", "deny_read", "sandbox"})
WINDOWS_ONLY_CLI_SUITES = frozenset({"win_sandbox", "win_process"})

# Cross-platform TUI and Lua suites explicitly runnable via `mare-cli --test all` on macOS/Windows.
TUI_CLI_SUITES = ("tui", "tui_followups", "tui_pty")
LUA_CLI_SUITES = ("lua_specs", "lua_regressions", "lua_runtime_transitions")
APPROVAL_CLI_SUITES = ("frontends", "subagents", "permissions", "tui")
SANDBOX_UNIX_SUITES = ("sandbox", "deny_read", "net_proxy_linux", "permissions")
SANDBOX_MACOS_SUITES = ("sandbox", "deny_read", "permissions")

GENERATED_DOC_PATTERNS = (
    "docs/*/reference/cli.md",
    "docs/*/reference/slash-commands.md",
    "docs/*/reference/rpc.md",
    "docs/*/reference/events.md",
    "docs/*/configure/options.md",
    "docs/*/configure/keymaps.md",
    "docs/*/extend/lua-api.md",
)

CLI_HELP_SOURCES = frozenset({
    "crates/mare-cli/src/cli.rs",
    "crates/mare-cli/src/commands.rs",
    "crates/mare-cli/src/sandbox_cmd.rs",
    "crates/mare-cli/src/plugins_cmd.rs",
    "crates/mare-cli/src/login_cmd.rs",
    "crates/mare-cli/src/session_cmd.rs",
    "crates/mare-cli/src/test_cmd.rs",
    "crates/mare-cli/src/doctor.rs",
    "crates/mare-cli/src/main.rs",
})

RELEASE_DRY_RUN_PATHS = frozenset({
    ".github/ci-tools.json",
    "about.toml",
    "deny.toml",
    "scripts/check_notices.py",
    "rust-toolchain.toml",
    ".python-version",
    ".github/workflows/release.yaml",
    ".github/workflows/release-dry-run.yaml",
    ".github/workflows/release-please.yaml",
    "release-please-config.json",
    ".release-please-manifest.json",
    "Dockerfile.release",
    "Cargo.toml",
    "Cargo.lock",
    "scripts/release_manifests.py",
    "scripts/check_release_gate.py",
    "scripts/check_release_policy.py",
    "scripts/verify_release_archive.py",
})

LINUX_ONLY_OSSANDBOX_FILES = frozenset({
    "crates/mare-binding/src/ossandbox/bwrap.rs",
    "crates/mare-binding/src/ossandbox/bwrap_lifecycle.rs",
    "crates/mare-binding/src/ossandbox/netns.rs",
    "crates/mare-binding/src/ossandbox/netns_inner.rs",
    "crates/mare-binding/src/ossandbox/landlock.rs",
    "crates/mare-binding/src/ossandbox/mount_inspection.rs",
    "crates/mare-binding/src/ossandbox/prexec.rs",
    "crates/mare-binding/src/ossandbox/read_mask.rs",
})

WINDOWS_ONLY_BINDING_FILES = frozenset({
    "crates/mare-binding/src/ossandbox/msys.rs",
    "crates/mare-binding/src/ossandbox/win_tests.rs",
    "crates/mare-binding/src/platform/job_win.rs",
    "crates/mare-binding/src/platform/term_win.rs",
    "crates/mare-binding/src/platform/windows_tests.rs",
})

UNIX_ONLY_BINDING_FILES = frozenset({
    "crates/mare-binding/src/platform/term_unix.rs",
    "crates/mare-binding/src/platform/unix_tests.rs",
})

SCRIPT_SELF_TESTS: dict[str, tuple[str, ...]] = {
    "scripts/ci_plan.py": ("python3 scripts/ci_plan.py --self-test",),
    "scripts/check_release_gate.py": ("python3 scripts/check_release_gate.py --self-test",),
    "scripts/release_manifests.py": ("python3 scripts/release_manifests.py --self-test",),
    "scripts/check_release_policy.py": (
        "python3 scripts/check_release_policy.py --self-test",
        "python3 scripts/check_release_policy.py",
    ),
    "scripts/verify_release_archive.py": ("python3 scripts/verify_release_archive.py --self-test",),
    "scripts/check_notices.py": ("python3 scripts/check_notices.py --self-test",),
    "scripts/nextest_test_home.py": ("python3 scripts/test_nextest_test_home.py",),
    "scripts/nextest_test_home.cmd": ("python3 scripts/test_nextest_test_home.py",),
    "scripts/test_nextest_test_home.py": ("python3 scripts/test_nextest_test_home.py",),
    "scripts/gen_docs.py": ("python3 -m unittest scripts/test_gen_docs.py",),
    "scripts/test_gen_docs.py": ("python3 -m unittest scripts/test_gen_docs.py",),
    "scripts/docs_event_catalog.py": ("python3 -m unittest scripts/test_gen_docs.py",),
    "scripts/plugin_options.py": ("python3 -m unittest scripts/test_gen_docs.py",),
    "scripts/docs_keymaps.py": ("python3 -m unittest scripts/test_docs_keymaps.py",),
    "scripts/test_docs_keymaps.py": ("python3 -m unittest scripts/test_docs_keymaps.py",),
    "scripts/accept.sh": ("python3 -m unittest scripts/test_accept.py",),
    "scripts/test_accept.py": ("python3 -m unittest scripts/test_accept.py",),
    "scripts/check_lua_public_api.py": ("python3 scripts/check_lua_public_api.py --self-test",),
}

DEFAULT_POLICY_COMMANDS = (
    "python3 scripts/ci_plan.py --self-test",
    "python3 scripts/check_release_gate.py --self-test",
    "python3 scripts/release_manifests.py --self-test",
    "python3 scripts/check_release_policy.py --self-test",
    "python3 scripts/check_release_policy.py",
    "python3 scripts/verify_release_archive.py --self-test",
)

CARGO_DEP_SECTION_RE = re.compile(
    r"^\s*\[(?:workspace\.)?(?:(?:dev|build)-)?dependencies(?:\.|\])|"
    r"^\s*\[target\.[^\]]+\.(?:(?:dev|build)-)?dependencies(?:\.|\])|"
    r"^\s*\[(?:patch|replace)(?:\.|\])"
)
WORKFLOW_USES_RE = re.compile(r"^[+-]\s*(?:-\s*)?uses:\s*([^\s#]+)")


@dataclass(frozen=True)
class WorkspaceGraph:
    packages: tuple[str, ...]
    dir_to_pkg: dict[str, str]
    reverse_deps: dict[str, frozenset[str]]
    lib_packages: frozenset[str]


def load_workspace_graph(metadata: dict[str, Any], root: pathlib.Path | None = None) -> WorkspaceGraph:
    ws_root = pathlib.Path(metadata["workspace_root"]).resolve() if root is None else root.resolve()
    ws_packages = metadata.get("packages", [])
    pkg_names = sorted({p["name"] for p in ws_packages})
    pkg_set = set(pkg_names)

    dir_to_pkg: dict[str, str] = {}
    rev: dict[str, set[str]] = {name: set() for name in pkg_names}
    lib_pkgs: set[str] = set()

    for pkg in ws_packages:
        name = pkg["name"]
        manifest = pathlib.Path(pkg["manifest_path"]).resolve()
        try:
            rel_dir = manifest.parent.relative_to(ws_root).as_posix()
        except ValueError:
            rel_dir = manifest.parent.as_posix()
        dir_to_pkg[rel_dir] = name

        for target in pkg.get("targets", []):
            kinds = set(target.get("kind", []))
            if "lib" in kinds or "proc-macro" in kinds:
                lib_pkgs.add(name)

        # Include normal (None), dev, and build dependency edges.
        for dep in pkg.get("dependencies", []):
            dep_name = dep.get("name")
            if dep_name in pkg_set and dep_name != name:
                rev[dep_name].add(name)

    return WorkspaceGraph(
        packages=tuple(pkg_names),
        dir_to_pkg=dir_to_pkg,
        reverse_deps={k: frozenset(sorted(v)) for k, v in rev.items()},
        lib_packages=frozenset(lib_pkgs),
    )


def reverse_transitive_closure(graph: WorkspaceGraph, seeds: set[str]) -> list[str]:
    visited: set[str] = set()
    stack = [s for s in seeds if s in graph.reverse_deps]
    while stack:
        cur = stack.pop()
        if cur in visited:
            continue
        visited.add(cur)
        for dependent in graph.reverse_deps.get(cur, ()):
            if dependent not in visited:
                stack.append(dependent)
    return sorted(visited)


def package_for_path(graph: WorkspaceGraph, path: str) -> str | None:
    best_match: tuple[int, str] | None = None
    for rel_dir, pkg in graph.dir_to_pkg.items():
        if path == rel_dir or path.startswith(rel_dir + "/"):
            if best_match is None or len(rel_dir) > best_match[0]:
                best_match = (len(rel_dir), pkg)
    return best_match[1] if best_match is not None else None


def _matches_any(path: str, patterns: tuple[str, ...]) -> bool:
    return any(fnmatch.fnmatch(path, pat) for pat in patterns)


def _split_diff_by_file(diff_text: str) -> dict[str, list[str]]:
    per_file: dict[str, list[str]] = {}
    current: str | None = None
    for line in diff_text.splitlines():
        if line.startswith("diff --git "):
            parts = line.split()
            if len(parts) >= 4 and parts[3].startswith("b/"):
                current = parts[3][2:]
                per_file.setdefault(current, [])
            else:
                current = None
        elif current is not None:
            per_file[current].append(line)
    return per_file


def diff_touches_cargo_deps(path: str, diff_by_file: dict[str, list[str]] | None) -> bool:
    """Return True if a Cargo.toml diff touches dependency sections (or diff is unavailable)."""
    if diff_by_file is None:
        return True
    lines = diff_by_file.get(path)
    if not lines:
        return True
    in_dep_section = False
    for line in lines:
        if line.startswith("@@"):
            # Hunk header often carries section context after `@@ ... @@ `
            parts = line.split("@@")
            ctx = parts[2] if len(parts) >= 3 else ""
            in_dep_section = bool(CARGO_DEP_SECTION_RE.search(ctx))
            continue
        if line.startswith(("+++", "---")):
            continue
        body = line[1:] if line and line[0] in "+- " else line
        stripped = body.lstrip()
        if stripped.startswith("["):
            in_dep_section = bool(CARGO_DEP_SECTION_RE.match(body))
            if line and line[0] in "+-" and in_dep_section:
                return True
            continue
        if line and line[0] in "+-" and in_dep_section and stripped and not stripped.startswith("#"):
            return True
    return False


def diff_touches_action_pins(path: str, diff_by_file: dict[str, list[str]] | None) -> bool:
    """Return True if a workflow/action diff touches external `uses:` pins (or diff unavailable)."""
    if diff_by_file is None:
        return True
    lines = diff_by_file.get(path)
    if not lines:
        return True
    for line in lines:
        if line.startswith(("+++", "---")):
            continue
        m = WORKFLOW_USES_RE.match(line)
        if m and not m.group(1).startswith("./"):
            return True
    return False


def _suite_expr(suites: set[str]) -> str:
    ordered = sorted(suites)
    if not ordered:
        return ""
    parts = [f"test(/^suite::{name}::/)" for name in ordered]
    return " | ".join(parts)


@dataclass
class _PlanState:
    full_matrix: bool = False
    reason: str = "file-based PR plan"
    seed_packages: set[str] = field(default_factory=set)
    all_packages: bool = False
    platforms: set[str] = field(default_factory=set)
    broad_tests: bool = False
    cli_suites: set[str] = field(default_factory=set)
    cli_lib_tests: bool = False
    need_mare_bin: bool = False
    run_doc_tests: bool = False
    windows_sandbox: bool = False
    installer_ps1: bool = False
    yaml: bool = False
    workflows: bool = False
    scripts: bool = False
    script_commands: list[str] = field(default_factory=list)
    deny: bool = False
    lua: bool = False
    docs_site: bool = False
    docs_gen: bool = False
    trivy: bool = False
    nix: bool = False
    release_dry_run: bool = False

    def add_script_cmd(self, cmd: str) -> None:
        if cmd not in self.script_commands:
            self.script_commands.append(cmd)


def _full_matrix_plan(graph: WorkspaceGraph, reason: str) -> dict[str, Any]:
    pkgs = list(graph.packages)
    pkg_args = " ".join(f"-p {p}" for p in pkgs)
    lib_pkgs = [p for p in pkgs if p in graph.lib_packages]
    lib_pkg_args = " ".join(f"-p {p}" for p in lib_pkgs)
    macos_cli_filter = _suite_expr(set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES))
    windows_cli_filter = _suite_expr(set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES) | {"win_process"})
    return _finalize_plan(
        full_matrix=True,
        reason=reason,
        packages=pkgs,
        pkg_args=pkg_args,
        macos_lib_pkg_args=lib_pkg_args,
        windows_lib_pkg_args=lib_pkg_args,
        linux=True,
        macos=True,
        windows=True,
        linux_filter="",
        macos_cli_filter=macos_cli_filter,
        windows_cli_filter=windows_cli_filter,
        build_mare_bin_linux=True,
        build_mare_bin_macos=True,
        build_mare_bin_windows=True,
        run_doc_tests_linux=True,
        run_rust_lint_on="linux",
        windows_sandbox=True,
        installer_ps1=True,
        yaml=True,
        workflows=True,
        scripts=True,
        script_commands=[
            *DEFAULT_POLICY_COMMANDS,
            "python3 scripts/check_notices.py --self-test",
            "python3 scripts/test_nextest_test_home.py",
            "python3 -m unittest scripts/test_gen_docs.py",
            "python3 -m unittest scripts/test_docs_keymaps.py",
            "sh -n install.sh",
        ],
        deny=True,
        lua=True,
        docs_site=True,
        docs_gen=True,
        trivy=True,
        nix=True,
        release_dry_run=True,
        cli_suites=sorted(set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES) | {"win_process", "win_sandbox"}),
    )


def _finalize_plan(
    *,
    full_matrix: bool,
    reason: str,
    packages: list[str],
    pkg_args: str,
    macos_lib_pkg_args: str,
    windows_lib_pkg_args: str,
    linux: bool,
    macos: bool,
    windows: bool,
    linux_filter: str,
    macos_cli_filter: str,
    windows_cli_filter: str,
    build_mare_bin_linux: bool,
    build_mare_bin_macos: bool,
    build_mare_bin_windows: bool,
    run_doc_tests_linux: bool,
    run_rust_lint_on: str,
    windows_sandbox: bool,
    installer_ps1: bool,
    yaml: bool,
    workflows: bool,
    scripts: bool,
    script_commands: list[str],
    deny: bool,
    lua: bool,
    docs_site: bool,
    docs_gen: bool,
    trivy: bool,
    nix: bool,
    release_dry_run: bool,
    cli_suites: list[str],
) -> dict[str, Any]:
    job_flags = {
        "yamllint": yaml,
        "actionlint": workflows,
        "scripts": scripts,
        "deny": deny,
        "lua": lua,
        "test-linux": linux,
        "test-macos": macos,
        "test-windows": windows,
        "installer-ps1": installer_ps1,
        "docs-site": docs_site,
        "docs-gen": docs_gen,
        "trivy": trivy,
        "nix": nix,
        "release-dry-run": release_dry_run,
    }
    expected_jobs = [job for job in ALL_PR_JOBS if job_flags[job]]
    return {
        "full_matrix": full_matrix,
        "reason": reason,
        "packages": packages,
        "pkg_args": pkg_args,
        "macos_lib_pkg_args": macos_lib_pkg_args,
        "windows_lib_pkg_args": windows_lib_pkg_args,
        "linux": linux,
        "macos": macos,
        "windows": windows,
        "linux_filter": linux_filter,
        "macos_cli_filter": macos_cli_filter,
        "windows_cli_filter": windows_cli_filter,
        "build_mare_bin_linux": build_mare_bin_linux,
        "build_mare_bin_macos": build_mare_bin_macos,
        "build_mare_bin_windows": build_mare_bin_windows,
        "run_doc_tests_linux": run_doc_tests_linux,
        "run_rust_lint_on": run_rust_lint_on,
        "windows_sandbox": windows_sandbox,
        "installer_ps1": installer_ps1,
        "yaml": yaml,
        "workflows": workflows,
        "scripts": scripts,
        "script_commands": script_commands,
        "deny": deny,
        "lua": lua,
        "docs_site": docs_site,
        "docs_gen": docs_gen,
        "trivy": trivy,
        "nix": nix,
        "release_dry_run": release_dry_run,
        "cli_suites": cli_suites,
        "expected_jobs": expected_jobs,
    }


def build_plan(
    files: list[str] | None,
    graph: WorkspaceGraph,
    *,
    diff_text: str | None = None,
    event_name: str = "pull_request",
    pr_head_ref: str = "",
) -> dict[str, Any]:
    if event_name == "workflow_dispatch":
        return _full_matrix_plan(graph, "workflow_dispatch runs full matrix")
    if event_name == "pull_request" and pr_head_ref == "release-please--branches--main":
        return _full_matrix_plan(graph, "release-please PR runs full matrix")
    if not files:
        return _full_matrix_plan(graph, "unable to resolve changed files; running full matrix")

    diff_by_file = _split_diff_by_file(diff_text) if diff_text is not None else None
    st = _PlanState()

    for raw_path in files:
        path = raw_path.strip()
        while path.startswith("./"):
            path = path[2:]
        if not path:
            continue
        matched = False

        # 1. Explicit full-matrix sentinels.
        if path in (".release-please-manifest.json",):
            return _full_matrix_plan(graph, f"{path} requires full matrix")

        # 2. Global Rust/toolchain/nextest configuration.
        if path in ("rust-toolchain.toml",) or path.startswith((".cargo/", ".github/actions/setup-rust/")):
            st.all_packages = True
            st.platforms.update(ALL_PLATFORMS)
            st.broad_tests = True
            st.cli_lib_tests = True
            st.need_mare_bin = True
            st.run_doc_tests = True
            st.windows_sandbox = True
            st.docs_site = True
            st.docs_gen = True
            st.nix = True
            st.release_dry_run = True
            st.trivy = True
            matched = True

        if path in (
            ".config/nextest.toml",
            "scripts/nextest_test_home.py",
            "scripts/nextest_test_home.cmd",
            "scripts/test_nextest_test_home.py",
        ):
            st.all_packages = True
            st.platforms.update(ALL_PLATFORMS)
            st.broad_tests = True
            st.cli_lib_tests = True
            st.need_mare_bin = True
            st.run_doc_tests = True
            st.windows_sandbox = True
            st.scripts = True
            st.add_script_cmd("python3 scripts/test_nextest_test_home.py")
            matched = True

        if path in ("Cargo.toml", "Cargo.lock"):
            st.all_packages = True
            st.platforms.update(ALL_PLATFORMS)
            st.broad_tests = True
            st.cli_lib_tests = True
            st.need_mare_bin = True
            st.run_doc_tests = True
            st.windows_sandbox = True
            st.deny = True
            st.nix = True
            st.release_dry_run = True
            if path == "Cargo.lock" or diff_touches_cargo_deps(path, diff_by_file):
                st.trivy = True
            matched = True

        if path in ("rustfmt.toml", "clippy.toml"):
            st.all_packages = True
            st.platforms.add("linux")
            st.broad_tests = True
            matched = True

        # 3. YAML / Workflows / Actions.
        if path.endswith((".yml", ".yaml")) or path in (".yamllint.yaml", ".python-version"):
            st.yaml = True
            matched = True

        if path.startswith((".github/workflows/", ".github/actions/")):
            st.workflows = True
            st.scripts = True
            for cmd in DEFAULT_POLICY_COMMANDS:
                st.add_script_cmd(cmd)
            if diff_touches_action_pins(path, diff_by_file):
                st.trivy = True
            if path == ".github/workflows/nix.yaml":
                st.nix = True
            if path in (
                ".github/workflows/release.yaml",
                ".github/workflows/release-dry-run.yaml",
                ".github/workflows/release-please.yaml",
            ) or path.startswith(".github/actions/"):
                st.release_dry_run = True
            if path == ".github/workflows/docs.yml":
                st.docs_site = True
                st.docs_gen = True
            if path == ".github/workflows/security.yaml":
                st.trivy = True
            matched = True

        # 4. Dependency, licensing, Docker, Nix, CI metadata, and installers.
        if path in ("deny.toml", "about.toml", "LICENSE", "THIRD_PARTY_NOTICES.md"):
            st.deny = True
            matched = True
        if path in RELEASE_DRY_RUN_PATHS:
            st.release_dry_run = True
            matched = True
        if path in ("Dockerfile", "Dockerfile.release", ".dockerignore"):
            st.trivy = True
            if path == "Dockerfile.release":
                st.scripts = True
                st.add_script_cmd("python3 scripts/check_release_policy.py --self-test")
                st.add_script_cmd("python3 scripts/check_release_policy.py")
            matched = True
        if path in ("flake.nix", "flake.lock"):
            st.nix = True
            if path == "flake.lock":
                st.trivy = True
            matched = True
        if path == ".github/ci-tools.json":
            st.lua = True
            st.docs_site = True
            st.docs_gen = True
            st.scripts = True
            st.add_script_cmd("python3 scripts/check_release_policy.py --self-test")
            st.add_script_cmd("python3 scripts/check_release_policy.py")
            matched = True
        if path in (".github/dependabot.yml", "release-please-config.json"):
            st.scripts = True
            st.add_script_cmd("python3 scripts/check_release_policy.py --self-test")
            st.add_script_cmd("python3 scripts/check_release_policy.py")
            if path == ".github/dependabot.yml":
                st.trivy = True
            matched = True
        if path in (".stylua.toml", ".luarc.json"):
            st.lua = True
            matched = True
        if path in ("install.sh", "docs-site/public/install.sh"):
            st.scripts = True
            st.add_script_cmd("sh -n install.sh")
            st.add_script_cmd("python3 scripts/check_release_policy.py --self-test")
            st.add_script_cmd("python3 scripts/check_release_policy.py")
            if path.startswith("docs-site/"):
                st.docs_site = True
            matched = True
        if path in ("install.ps1", "docs-site/public/install.ps1", "scripts/test_install_ps1.ps1"):
            st.installer_ps1 = True
            if path.startswith("docs-site/"):
                st.docs_site = True
            matched = True

        # 5. Scripts.
        if path.startswith("scripts/"):
            matched = True
            if path == "scripts/windows_sandbox_ci.ps1":
                st.seed_packages.add("mare-cli")
                st.platforms.add("windows")
                st.windows_sandbox = True
                st.need_mare_bin = True
                st.cli_suites.add("win_sandbox")
                st.scripts = True
                st.add_script_cmd("python3 scripts/ci_plan.py --self-test")
            elif path == "scripts/test_install_ps1.ps1":
                pass  # handled above by installer_ps1
            elif path in SCRIPT_SELF_TESTS:
                st.scripts = True
                for cmd in SCRIPT_SELF_TESTS[path]:
                    st.add_script_cmd(cmd)
                if path in (
                    "scripts/gen_docs.py",
                    "scripts/docs_event_catalog.py",
                    "scripts/docs_keymaps.py",
                    "scripts/plugin_options.py",
                    "scripts/test_gen_docs.py",
                    "scripts/test_docs_keymaps.py",
                ):
                    st.docs_site = True
                    st.docs_gen = True
                if path == "scripts/check_lua_public_api.py":
                    st.lua = True
            elif fnmatch.fnmatch(path, "scripts/lua_api_*.py"):
                st.lua = True
                st.scripts = True
                st.add_script_cmd("python3 scripts/check_lua_public_api.py --self-test")
            elif path.startswith("scripts/cordis-suite/") or path in ("scripts/cordis-suite.sh", "scripts/vendor-cordis.sh"):
                st.seed_packages.add("mare-kernel")
                st.platforms.add("linux")
                st.lua = True
                st.cli_suites.add("lua_specs")
                st.scripts = True
                st.add_script_cmd(f"sh -n {path}" if path.endswith(".sh") else "sh -n scripts/cordis-suite.sh")
            elif path.endswith(".py"):
                st.scripts = True
                st.add_script_cmd(f"python3 -m py_compile {path}")
            elif path.endswith(".sh"):
                st.scripts = True
                st.add_script_cmd(f"sh -n {path}")
            else:
                return _full_matrix_plan(graph, f"unmatched script file {path!r}; running full matrix")

        # 6. Docs & non-code repo files.
        if path.startswith("docs/") or path.startswith("docs-site/") or path == ".nvmrc":
            st.docs_site = True
            if _matches_any(path, GENERATED_DOC_PATTERNS):
                st.docs_gen = True
            if path in ("docs-site/package.json", "docs-site/package-lock.json"):
                st.trivy = True
            matched = True
        if (
            fnmatch.fnmatch(path, "README*.md")
            or path in (
                "CHANGELOG.md",
                "SECURITY.md",
                "CONTRIBUTING.md",
                ".gitignore",
                ".gitattributes",
                ".github/PULL_REQUEST_TEMPLATE.md",
                ".github/pull_request_template.md",
                "justfile",
            )
            or path.startswith(("evals/", "examples/", ".github/ISSUE_TEMPLATE/"))
            or path.lower().endswith((
                ".png",
                ".svg",
                ".gif",
                ".webp",
                ".jpg",
                ".jpeg",
                ".ico",
                ".mp4",
                ".cast",
            ))
        ):
            st.docs_site = True
            if path == "CONTRIBUTING.md":
                st.scripts = True
                st.add_script_cmd("python3 scripts/check_release_policy.py --self-test")
                st.add_script_cmd("python3 scripts/check_release_policy.py")
            matched = True

        # 7. Rust workspace crates.
        pkg = package_for_path(graph, path)
        if pkg is not None:
            matched = True
            st.seed_packages.add(pkg)

            # Crate Cargo.toml changes affect deny and potentially Trivy.
            if path.endswith("/Cargo.toml"):
                st.deny = True
                st.broad_tests = True
                st.platforms.add("linux")
                if diff_touches_cargo_deps(path, diff_by_file):
                    st.trivy = True

            # Generated docs inputs inside crates/.
            if (
                path in CLI_HELP_SOURCES
                or path == "crates/mare-config/preset/config_api.lua"
                or path.startswith("crates/mare-plugin/preset/types/")
                or path.startswith("crates/mare-plugin/preset/plugins/")
                or path.startswith("crates/mare-protocol/src/")
                or path in (
                    "crates/mare-tui/src/editor/keymap.rs",
                    "crates/mare-shared/src/keymap_contexts.rs",
                )
            ):
                st.docs_site = True
                st.docs_gen = True

            # Lua public-API audit trigger (matches ci.yml lua coverage).
            if (
                path.endswith(".lua")
                or path.startswith(("crates/mare-plugin/", "crates/mare-kernel/"))
                or (
                    path.startswith("crates/mare-binding/")
                    and not path.startswith("crates/mare-binding/src/winsandbox/")
                    and path not in LINUX_ONLY_OSSANDBOX_FILES
                    and path not in WINDOWS_ONLY_BINDING_FILES
                )
            ):
                st.lua = True

            # ---- Explicit platform & consumer rules per crate/path ----
            if pkg == "mare-tui":
                # TTY, input, and TUI rendering run on all three platforms (no sandbox).
                st.platforms.update(ALL_PLATFORMS)
                st.cli_suites.update(TUI_CLI_SUITES)
                st.need_mare_bin = True

            elif pkg == "mare-protocol":
                # Protocol changes stay broad across all platforms.
                st.platforms.update(ALL_PLATFORMS)
                st.broad_tests = True
                st.cli_lib_tests = True
                st.need_mare_bin = True
                st.run_doc_tests = True
                st.windows_sandbox = True

            elif pkg == "mare-shared":
                if path.startswith("crates/mare-shared/src/style/") or path == "crates/mare-shared/src/ui.rs":
                    st.platforms.update(ALL_PLATFORMS)
                    st.cli_suites.update(TUI_CLI_SUITES)
                    st.need_mare_bin = True
                elif path == "crates/mare-shared/src/private_win.rs":
                    st.platforms.add("windows")
                    st.windows_sandbox = True
                    st.need_mare_bin = True
                    st.cli_suites.add("win_sandbox")
                elif path == "crates/mare-shared/src/version.rs":
                    st.platforms.add("linux")
                    st.cli_suites.add("firstrun")
                    st.need_mare_bin = True
                else:
                    # Core mare-shared stays broad.
                    st.platforms.update(ALL_PLATFORMS)
                    st.broad_tests = True
                    st.cli_lib_tests = True
                    st.need_mare_bin = True
                    st.run_doc_tests = True

            elif pkg == "mare-binding":
                if path in LINUX_ONLY_OSSANDBOX_FILES or path == "crates/mare-binding/src/net_unix.rs":
                    st.platforms.add("linux")
                    st.cli_suites.update(SANDBOX_UNIX_SUITES)
                    st.need_mare_bin = True
                elif path == "crates/mare-binding/src/ossandbox/seatbelt.rs":
                    st.platforms.update({"linux", "macos"})
                    st.cli_suites.update(SANDBOX_MACOS_SUITES)
                    st.need_mare_bin = True
                elif path.startswith("crates/mare-binding/src/winsandbox/") or path in WINDOWS_ONLY_BINDING_FILES:
                    st.platforms.add("windows")
                    st.windows_sandbox = True
                    st.need_mare_bin = True
                    st.cli_suites.update({"win_sandbox", "win_process"})
                elif path in UNIX_ONLY_BINDING_FILES:
                    st.platforms.update({"linux", "macos"})
                    st.cli_suites.update({"tui_pty", "terminal", "process"})
                    st.need_mare_bin = True
                elif path.startswith(("crates/mare-binding/src/ossandbox/", "crates/mare-binding/src/sandbox/")) or path in (
                    "crates/mare-binding/src/sandbox.rs",
                    "crates/mare-binding/src/netproxy.rs",
                    "crates/mare-binding/src/netproxy_http.rs",
                ):
                    st.platforms.update(ALL_PLATFORMS)
                    st.windows_sandbox = True
                    st.need_mare_bin = True
                    st.cli_suites.update(SANDBOX_UNIX_SUITES)
                    st.cli_suites.add("win_sandbox")
                elif path.startswith((
                    "crates/mare-binding/src/pty/",
                    "crates/mare-binding/src/platform/",
                    "crates/mare-binding/src/proc/",
                    "crates/mare-binding/src/fs/",
                )) or path in (
                    "crates/mare-binding/src/pty.rs",
                    "crates/mare-binding/src/ptybridge.rs",
                    "crates/mare-binding/src/pgroups.rs",
                    "crates/mare-binding/src/shell.rs",
                    "crates/mare-binding/src/shellparse.rs",
                    "crates/mare-binding/src/clipboard.rs",
                    "crates/mare-binding/src/filelock.rs",
                ):
                    st.platforms.update(ALL_PLATFORMS)
                    st.need_mare_bin = True
                    st.cli_suites.update({"tui_pty", "terminal", "process", "tools", "win_process"})
                elif path == "crates/mare-binding/src/approval.rs":
                    st.platforms.add("linux")
                    st.cli_suites.update(APPROVAL_CLI_SUITES)
                    st.cli_lib_tests = True
                elif path.startswith("crates/mare-binding/src/ui/"):
                    st.platforms.update(ALL_PLATFORMS)
                    st.cli_suites.update(TUI_CLI_SUITES)
                    st.need_mare_bin = True
                else:
                    st.platforms.add("linux")
                    st.broad_tests = True
                    st.need_mare_bin = True

            elif pkg == "mare-plugin":
                if path.startswith("crates/mare-plugin/preset/tests/"):
                    st.platforms.add("linux")
                    st.cli_suites.add("lua_specs")
                elif path.startswith("crates/mare-plugin/preset/plugins/"):
                    st.platforms.add("linux")
                    st.cli_suites.update({"lua_specs", "lua_regressions", "lua_runtime_transitions", "plugins"})
                    st.need_mare_bin = True
                    if "permissions" in path:
                        st.cli_suites.update({"permissions", "sandbox"})
                    if "agent-approvals" in path or "subagent" in path or "/agents/" in path:
                        st.cli_suites.update(APPROVAL_CLI_SUITES)
                    if "/ui/" in path or path.endswith("/ui.lua") or "statusline" in path:
                        st.cli_suites.update(TUI_CLI_SUITES)
                        st.cli_suites.add("status")
                elif path.startswith("crates/mare-plugin/preset/types/"):
                    st.platforms.add("linux")
                    st.cli_suites.update({"lua_specs", "api_parity", "plugins"})
                else:
                    st.platforms.add("linux")
                    st.broad_tests = True
                    st.need_mare_bin = True

            elif pkg == "mare-kernel":
                if path.startswith("crates/mare-kernel/lua-specs/"):
                    st.platforms.add("linux")
                    st.cli_suites.add("lua_specs")
                else:
                    st.platforms.add("linux")
                    st.broad_tests = True
                    st.need_mare_bin = True

            elif pkg == "mare-host":
                if path.startswith("crates/mare-host/src/approvals/"):
                    st.platforms.add("linux")
                    st.cli_suites.update(APPROVAL_CLI_SUITES)
                    st.cli_lib_tests = True
                else:
                    st.platforms.add("linux")
                    st.broad_tests = True
                    st.need_mare_bin = True

            elif pkg == "mare-cli":
                if path.startswith("crates/mare-cli/src/app/") or path in (
                    "crates/mare-cli/src/terminal.rs",
                    "crates/mare-cli/src/signals.rs",
                ):
                    st.platforms.update(ALL_PLATFORMS)
                    st.cli_suites.update(TUI_CLI_SUITES)
                    st.cli_lib_tests = True
                    st.need_mare_bin = True
                elif path == "crates/mare-cli/src/sandbox_cmd.rs":
                    st.platforms.update(ALL_PLATFORMS)
                    st.windows_sandbox = True
                    st.need_mare_bin = True
                    st.cli_suites.update(SANDBOX_UNIX_SUITES)
                    st.cli_suites.add("win_sandbox")
                elif path in ("crates/mare-cli/src/cli.rs", "crates/mare-cli/src/commands.rs"):
                    st.platforms.add("linux")
                    st.cli_suites.update({"cli", "commands", "cli_session"})
                    st.cli_lib_tests = True
                    st.need_mare_bin = True
                elif path.startswith("crates/mare-cli/tests/suite/"):
                    sub = path[len("crates/mare-cli/tests/suite/") :]
                    suite_mod = sub.split("/", 1)[0]
                    if suite_mod.endswith(".rs"):
                        suite_mod = suite_mod[:-3]
                    if suite_mod == "mod":
                        st.platforms.add("linux")
                        st.broad_tests = True
                        st.need_mare_bin = True
                    elif suite_mod in ("tui", "tui_followups", "tui_pty"):
                        st.platforms.update(ALL_PLATFORMS)
                        st.cli_suites.update(TUI_CLI_SUITES)
                        st.need_mare_bin = True
                    elif suite_mod == "win_sandbox":
                        st.platforms.add("windows")
                        st.windows_sandbox = True
                        st.cli_suites.add("win_sandbox")
                        st.need_mare_bin = True
                    elif suite_mod == "win_process":
                        st.platforms.add("windows")
                        st.cli_suites.add("win_process")
                        st.need_mare_bin = True
                    elif suite_mod == "net_proxy_linux":
                        st.platforms.add("linux")
                        st.cli_suites.add("net_proxy_linux")
                        st.need_mare_bin = True
                    elif suite_mod == "terminal":
                        st.platforms.update({"linux", "macos"})
                        st.cli_suites.add("terminal")
                        st.need_mare_bin = True
                    elif suite_mod in ("sandbox", "sandbox_expect", "deny_read", "permissions"):
                        st.platforms.update(ALL_PLATFORMS)
                        st.windows_sandbox = True
                        st.cli_suites.update(SANDBOX_UNIX_SUITES)
                        st.cli_suites.add("win_sandbox")
                        st.need_mare_bin = True
                    elif suite_mod == "process":
                        st.platforms.update(ALL_PLATFORMS)
                        st.cli_suites.add("process")
                        st.need_mare_bin = True
                    else:
                        st.platforms.add("linux")
                        st.cli_suites.add(suite_mod)
                        st.need_mare_bin = True
                else:
                    st.platforms.add("linux")
                    st.broad_tests = True
                    st.cli_lib_tests = True
                    st.need_mare_bin = True

            else:
                # Platform-neutral leaf crates: mare-edit, mare-config, mare-store,
                # mare-macro, mare-plugin-manager, etc.
                st.platforms.add("linux")
                if path == "crates/mare-config/preset/config_api.lua":
                    st.cli_suites.add("config")
                else:
                    st.broad_tests = True
                    st.need_mare_bin = True

        # 8. Fail wide on any unmatched file.
        if not matched:
            return _full_matrix_plan(graph, f"unmatched path {path!r}; running full matrix")

    # Compute Rust package scope via reverse transitive closure.
    if st.all_packages:
        packages = list(graph.packages)
    elif st.seed_packages:
        packages = reverse_transitive_closure(graph, st.seed_packages)
    else:
        packages = []

    if packages and not st.platforms:
        st.platforms.add("linux")

    pkg_args = " ".join(f"-p {p}" for p in packages)

    linux = "linux" in st.platforms
    macos = "macos" in st.platforms
    windows = "windows" in st.platforms

    # Narrow Linux nextest filter when all changes mapped to specific suites.
    linux_filter = ""
    if linux and packages and not st.broad_tests:
        linux_suites = st.cli_suites - WINDOWS_ONLY_CLI_SUITES
        non_cli_pkgs = [p for p in packages if p != "mare-cli"]
        terms: list[str] = [f"package({p})" for p in non_cli_pkgs]
        if "mare-cli" in packages:
            if st.cli_lib_tests:
                terms.append("(package(mare-cli) & kind(lib))")
            if linux_suites:
                terms.append(f"(package(mare-cli) & binary(all) & ({_suite_expr(linux_suites)}))")
        if terms:
            linux_filter = " | ".join(terms)

    # On macOS and Windows: run `--lib` on selected library packages and select TUI/Lua/platform
    # `mare-cli --test all` suites explicitly.
    non_cli_lib_pkgs = [p for p in packages if p in graph.lib_packages and p != "mare-cli"]
    platform_lib_pkgs = list(non_cli_lib_pkgs)
    if "mare-cli" in packages and (st.broad_tests or st.cli_lib_tests):
        platform_lib_pkgs.append("mare-cli")
    platform_lib_pkg_args = " ".join(f"-p {p}" for p in sorted(platform_lib_pkgs))

    macos_cli_filter = ""
    if macos and "mare-cli" in packages:
        if st.broad_tests:
            macos_suites = set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES)
        else:
            macos_allowed = set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES) | set(SANDBOX_MACOS_SUITES) | {"terminal", "process"}
            macos_suites = st.cli_suites & macos_allowed
        macos_cli_filter = _suite_expr(macos_suites)

    windows_cli_filter = ""
    if windows and "mare-cli" in packages:
        if st.broad_tests:
            win_suites = set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES) | {"win_process"}
        else:
            win_allowed = set(TUI_CLI_SUITES) | set(LUA_CLI_SUITES) | {"permissions", "process", "win_process"}
            win_suites = st.cli_suites & win_allowed
        windows_cli_filter = _suite_expr(win_suites)

    # Run rustfmt + scoped clippy on one selected platform runner (Linux preferred) so a narrow PR
    # does not spawn an extra lint runner.
    if linux and packages:
        run_rust_lint_on = "linux"
    elif windows and packages:
        run_rust_lint_on = "windows"
    elif macos and packages:
        run_rust_lint_on = "macos"
    else:
        run_rust_lint_on = ""

    build_mare_bin_linux = linux and "mare-cli" in packages and (st.broad_tests or st.need_mare_bin)
    build_mare_bin_macos = macos and bool(macos_cli_filter) and (st.broad_tests or st.need_mare_bin)
    build_mare_bin_windows = windows and (
        st.windows_sandbox or (bool(windows_cli_filter) and (st.broad_tests or st.need_mare_bin))
    )

    return _finalize_plan(
        full_matrix=False,
        reason=st.reason,
        packages=packages,
        pkg_args=pkg_args,
        macos_lib_pkg_args=platform_lib_pkg_args if macos else "",
        windows_lib_pkg_args=platform_lib_pkg_args if windows else "",
        linux=linux,
        macos=macos,
        windows=windows,
        linux_filter=linux_filter,
        macos_cli_filter=macos_cli_filter,
        windows_cli_filter=windows_cli_filter,
        build_mare_bin_linux=build_mare_bin_linux,
        build_mare_bin_macos=build_mare_bin_macos,
        build_mare_bin_windows=build_mare_bin_windows,
        run_doc_tests_linux=linux and (st.run_doc_tests or st.broad_tests),
        run_rust_lint_on=run_rust_lint_on,
        windows_sandbox=windows and st.windows_sandbox,
        installer_ps1=st.installer_ps1,
        yaml=st.yaml,
        workflows=st.workflows,
        scripts=st.scripts,
        script_commands=st.script_commands,
        deny=st.deny,
        lua=st.lua,
        docs_site=st.docs_site,
        docs_gen=st.docs_gen,
        trivy=st.trivy,
        nix=st.nix,
        release_dry_run=st.release_dry_run,
        cli_suites=sorted(st.cli_suites),
    )


def evaluate_pr_checks(
    plan_result: str,
    expected_jobs_raw: str | list[str],
    needs_map: dict[str, dict[str, Any]],
) -> tuple[bool, list[str]]:
    """Evaluate the `PR checks` summary gate."""
    errors: list[str] = []

    if plan_result != "success":
        errors.append(f"planner job 'changes' did not succeed (result={plan_result!r})")

    expected_jobs: list[str]
    if isinstance(expected_jobs_raw, list):
        expected_jobs = [str(j) for j in expected_jobs_raw]
    else:
        try:
            parsed = json.loads(expected_jobs_raw) if expected_jobs_raw else []
        except json.JSONDecodeError as exc:
            errors.append(f"invalid expected_jobs JSON from planner: {exc}")
            parsed = []
        if not isinstance(parsed, list):
            errors.append(f"expected_jobs must be a JSON array, got {type(parsed).__name__}")
            expected_jobs = []
        else:
            expected_jobs = [str(j) for j in parsed]

    expected_set = set(expected_jobs)

    for job in expected_jobs:
        info = needs_map.get(job)
        if info is None:
            errors.append(f"expected job {job!r} is missing from PR checks dependencies")
            continue
        result = info.get("result")
        if result != "success":
            errors.append(f"expected job {job!r} did not succeed (result={result!r})")

    for job, info in sorted(needs_map.items()):
        if job == "changes" or job in expected_set:
            continue
        result = info.get("result")
        if result not in ("skipped", "success"):
            errors.append(f"unexpected job {job!r} ended with result={result!r}")

    if errors:
        return False, errors
    summary = ", ".join(expected_jobs) if expected_jobs else "(none)"
    return True, [f"PR checks passed: all expected jobs succeeded ({summary})"]


def _run_git(*args: str, cwd: pathlib.Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=False)


def _resolve_git_diff(
    root: pathlib.Path,
    *,
    event_name: str,
    pr_base_sha: str,
    base_arg: str | None,
    head_arg: str,
) -> tuple[list[str] | None, str | None]:
    base = base_arg
    if not base:
        if event_name == "pull_request":
            if _run_git("rev-parse", "--verify", "HEAD^2", cwd=root).returncode == 0:
                base = "HEAD^1"
            elif pr_base_sha:
                base = pr_base_sha
        elif _run_git("rev-parse", "--verify", "HEAD~1", cwd=root).returncode == 0:
            base = "HEAD~1"
    if not base:
        return None, None

    if _run_git("cat-file", "-e", f"{base}^{{commit}}", cwd=root).returncode != 0:
        if _run_git("fetch", "--no-tags", "--depth=1", "origin", base, cwd=root).returncode != 0:
            return None, None

    names = _run_git("diff", "--name-only", "--no-renames", "-z", base, head_arg, "--", cwd=root)
    if names.returncode != 0:
        return None, None
    files = [p for p in names.stdout.split("\0") if p]

    diff_res = _run_git("diff", "--no-renames", "--unified=0", base, head_arg, "--", cwd=root)
    diff_text = diff_res.stdout if diff_res.returncode == 0 else None
    return files, diff_text


def _load_cargo_metadata(root: pathlib.Path, metadata_json: str | None) -> dict[str, Any]:
    if metadata_json:
        return json.loads(pathlib.Path(metadata_json).read_text(encoding="utf-8"))
    res = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    if res.returncode != 0:
        raise RuntimeError(f"cargo metadata --locked failed (rc={res.returncode}): {res.stderr.strip()}")
    return json.loads(res.stdout)


def _write_github_output(path: str, plan: dict[str, Any]) -> None:
    def fmt(val: Any) -> str:
        if isinstance(val, bool):
            return "true" if val else "false"
        if isinstance(val, (list, dict)):
            return json.dumps(val, separators=(",", ":"))
        return str(val)

    output_keys = (
        "full_matrix",
        "reason",
        "pkg_args",
        "macos_lib_pkg_args",
        "windows_lib_pkg_args",
        "linux",
        "macos",
        "windows",
        "linux_filter",
        "macos_cli_filter",
        "windows_cli_filter",
        "build_mare_bin_linux",
        "build_mare_bin_macos",
        "build_mare_bin_windows",
        "run_doc_tests_linux",
        "run_rust_lint_on",
        "windows_sandbox",
        "installer_ps1",
        "yaml",
        "workflows",
        "scripts",
        "script_commands",
        "deny",
        "lua",
        "docs_site",
        "docs_gen",
        "trivy",
        "nix",
        "release_dry_run",
        "expected_jobs",
    )
    with open(path, "a", encoding="utf-8") as fh:
        for key in output_keys:
            val = fmt(plan[key])
            if "\n" in val:
                delim = "MARE_CI_PLAN_EOF"
                fh.write(f"{key}<<{delim}\n{val}\n{delim}\n")
            else:
                fh.write(f"{key}={val}\n")


def _synthetic_workspace_metadata() -> dict[str, Any]:
    """Minimal workspace metadata mirroring mare's crate graph for offline unit tests."""
    root = "/repo"

    def pkg(name: str, deps: list[tuple[str, str | None]], lib: bool = True, tests: list[str] = ()) -> dict[str, Any]:
        targets = []
        if lib:
            targets.append({"name": name.replace("-", "_"), "kind": ["lib"]})
        for t in tests:
            targets.append({"name": t, "kind": ["test"]})
        return {
            "name": name,
            "manifest_path": f"{root}/crates/{name}/Cargo.toml",
            "dependencies": [{"name": d, "kind": k} for d, k in deps],
            "targets": targets,
        }

    return {
        "workspace_root": root,
        "packages": [
            pkg("mare-shared", []),
            pkg("mare-edit", []),
            pkg("mare-macro", []),
            pkg("mare-config", [("mare-shared", None)]),
            pkg("mare-protocol", [("mare-shared", None)]),
            pkg("mare-plugin-manager", [("mare-shared", None)], tests=["source_change", "sources"]),
            pkg("mare-tui", [("mare-edit", None), ("mare-shared", None)]),
            pkg("mare-store", [("mare-protocol", None), ("mare-shared", None), ("mare-shared", "dev")]),
            pkg("mare-kernel", [("mare-protocol", None), ("mare-shared", None), ("mare-shared", "dev")]),
            pkg(
                "mare-binding",
                [
                    ("mare-config", None),
                    ("mare-edit", None),
                    ("mare-protocol", None),
                    ("mare-shared", None),
                    ("mare-store", None),
                    ("mare-shared", "dev"),
                ],
            ),
            pkg(
                "mare-plugin",
                [
                    ("mare-binding", None),
                    ("mare-config", None),
                    ("mare-kernel", None),
                    ("mare-macro", None),
                    ("mare-plugin-manager", None),
                    ("mare-shared", None),
                    ("mare-kernel", "build"),
                    ("mare-protocol", "build"),
                ],
            ),
            pkg(
                "mare-host",
                [
                    ("mare-binding", None),
                    ("mare-config", None),
                    ("mare-kernel", None),
                    ("mare-plugin", None),
                    ("mare-plugin-manager", None),
                    ("mare-protocol", None),
                    ("mare-shared", None),
                    ("mare-store", None),
                ],
            ),
            pkg(
                "mare-cli",
                [
                    ("mare-binding", None),
                    ("mare-config", None),
                    ("mare-host", None),
                    ("mare-kernel", None),
                    ("mare-plugin", None),
                    ("mare-plugin-manager", None),
                    ("mare-protocol", None),
                    ("mare-shared", None),
                    ("mare-store", None),
                    ("mare-tui", None),
                    ("mare-edit", "dev"),
                    ("mare-kernel", "dev"),
                    ("mare-shared", "dev"),
                ],
                tests=["all"],
            ),
        ],
    }


class CiPlanSelfTests(unittest.TestCase):
    def setUp(self) -> None:
        self.graph = load_workspace_graph(_synthetic_workspace_metadata(), root=pathlib.Path("/repo"))

    def test_reverse_transitive_closure_includes_normal_dev_and_build_edges(self) -> None:
        self.assertEqual(reverse_transitive_closure(self.graph, {"mare-tui"}), ["mare-cli", "mare-tui"])
        self.assertEqual(
            reverse_transitive_closure(self.graph, {"mare-protocol"}),
            ["mare-binding", "mare-cli", "mare-host", "mare-kernel", "mare-plugin", "mare-protocol", "mare-store"],
        )
        # Build-dependency edge: mare-kernel (build dep of mare-plugin) reaches mare-plugin -> mare-host -> mare-cli.
        self.assertEqual(
            reverse_transitive_closure(self.graph, {"mare-kernel"}),
            ["mare-cli", "mare-host", "mare-kernel", "mare-plugin"],
        )
        # Dev-dependency edge: mare-edit (dev dep of mare-cli + normal dep of mare-tui/mare-binding).
        self.assertIn("mare-cli", reverse_transitive_closure(self.graph, {"mare-edit"}))

    def test_replay_pr_58_tui_three_platforms_no_sandbox(self) -> None:
        files = [
            "crates/mare-tui/src/live_input.rs",
            "crates/mare-tui/src/tty.rs",
        ]
        plan = build_plan(files, self.graph)
        self.assertFalse(plan["full_matrix"])
        self.assertEqual(plan["packages"], ["mare-cli", "mare-tui"])
        self.assertEqual(plan["pkg_args"], "-p mare-cli -p mare-tui")
        self.assertEqual(plan["expected_jobs"], ["test-linux", "test-macos", "test-windows"])
        self.assertFalse(plan["windows_sandbox"])
        self.assertIn("package(mare-tui)", plan["linux_filter"])
        self.assertIn("suite::tui::", plan["linux_filter"])
        self.assertIn("suite::tui_pty::", plan["macos_cli_filter"])
        self.assertIn("suite::tui_pty::", plan["windows_cli_filter"])
        self.assertEqual(plan["macos_lib_pkg_args"], "-p mare-tui")
        self.assertEqual(plan["windows_lib_pkg_args"], "-p mare-tui")

    def test_live_input_only_runs_tui_three_platforms_and_nothing_else(self) -> None:
        plan = build_plan(["crates/mare-tui/src/live_input.rs"], self.graph)
        self.assertEqual(plan["expected_jobs"], ["test-linux", "test-macos", "test-windows"])
        self.assertEqual(plan["packages"], ["mare-cli", "mare-tui"])
        self.assertFalse(plan["windows_sandbox"])
        self.assertFalse(plan["docs_site"])
        self.assertFalse(plan["docs_gen"])
        self.assertFalse(plan["lua"])
        self.assertFalse(plan["trivy"])
        self.assertFalse(plan["nix"])
        self.assertFalse(plan["release_dry_run"])

    def test_replay_pr_66_lua_public_api_tui_and_approvals(self) -> None:
        files = [
            "crates/mare-binding/src/approval.rs",
            "crates/mare-binding/src/ui/modals.rs",
            "crates/mare-cli/src/app/view/modal.rs",
            "crates/mare-cli/src/app/view/render.rs",
            "crates/mare-cli/src/app/view/transcript.rs",
            "crates/mare-cli/tests/suite/tui.rs",
            "crates/mare-cli/tests/suite/tui/agents.rs",
            "crates/mare-cli/tests/suite/tui/dialogs/approvals_and_select.rs",
            "crates/mare-cli/tests/suite/tui/dialogs/questions_and_child.rs",
            "crates/mare-host/src/approvals/child.rs",
            "crates/mare-plugin/preset/plugins/lua/agent-approvals/dialog.lua",
            "crates/mare-plugin/preset/plugins/lua/agents/list.lua",
            "crates/mare-plugin/preset/plugins/lua/ui/transcript.lua",
            "crates/mare-plugin/preset/plugins/subagent.lua",
            "crates/mare-plugin/preset/plugins/ui.lua",
            "crates/mare-shared/src/style/mod.rs",
            "crates/mare-shared/src/style/wrap.rs",
        ]
        plan = build_plan(files, self.graph)
        self.assertFalse(plan["full_matrix"])
        self.assertTrue(plan["lua"])
        self.assertTrue(plan["linux"])
        self.assertTrue(plan["macos"])
        self.assertTrue(plan["windows"])
        self.assertFalse(plan["windows_sandbox"])
        self.assertFalse(plan["nix"])
        self.assertFalse(plan["trivy"])
        self.assertFalse(plan["release_dry_run"])
        for suite in ("tui", "frontends", "subagents", "permissions", "lua_specs"):
            self.assertIn(suite, plan["cli_suites"])

    def test_replay_pr_80_windows_sandbox_three_platform_and_docs(self) -> None:
        files = [
            "crates/mare-binding/src/netproxy.rs",
            "crates/mare-binding/src/netproxy_http.rs",
            "crates/mare-binding/src/ossandbox/deny_read.rs",
            "crates/mare-binding/src/winsandbox/enhanced.rs",
            "crates/mare-cli/src/cli.rs",
            "crates/mare-cli/src/commands.rs",
            "crates/mare-cli/src/sandbox_cmd.rs",
            "crates/mare-cli/tests/suite/permissions/presets.rs",
            "crates/mare-cli/tests/suite/tui/statusline.rs",
            "crates/mare-plugin/preset/plugins/lua/permissions/presets.lua",
            "crates/mare-plugin/preset/plugins/lua/review/git.lua",
            "crates/mare-plugin/preset/plugins/lua/statusline/git.lua",
            "crates/mare-plugin/preset/plugins/permissions.lua",
            "docs/en/reference/cli.md",
            "docs/en/use/permissions.md",
            "docs/ja/reference/cli.md",
            "docs/ja/use/permissions.md",
            "docs/zh/reference/cli.md",
            "docs/zh/use/permissions.md",
        ]
        plan = build_plan(files, self.graph)
        self.assertFalse(plan["full_matrix"])
        self.assertTrue(plan["linux"])
        self.assertTrue(plan["macos"])
        self.assertTrue(plan["windows"])
        self.assertTrue(plan["windows_sandbox"])
        self.assertTrue(plan["docs_site"])
        self.assertTrue(plan["docs_gen"])
        self.assertTrue(plan["lua"])
        self.assertFalse(plan["nix"])
        self.assertFalse(plan["release_dry_run"])

    def test_docs_only_pr_runs_site_build_only(self) -> None:
        plan = build_plan(["docs/en/use/permissions.md", "docs-site/src/pages/index.astro"], self.graph)
        self.assertFalse(plan["full_matrix"])
        self.assertEqual(plan["expected_jobs"], ["docs-site"])
        self.assertFalse(plan["docs_gen"])
        self.assertFalse(plan["linux"])

    def test_generated_doc_edit_runs_docs_gen_and_site(self) -> None:
        plan = build_plan(["docs/en/reference/cli.md"], self.graph)
        self.assertEqual(plan["expected_jobs"], ["docs-site", "docs-gen"])

    def test_mare_protocol_stays_broad(self) -> None:
        plan = build_plan(["crates/mare-protocol/src/lib.rs"], self.graph)
        self.assertTrue(plan["linux"])
        self.assertTrue(plan["macos"])
        self.assertTrue(plan["windows"])
        self.assertEqual(plan["linux_filter"], "")
        self.assertTrue(plan["docs_gen"])
        self.assertIn("mare-binding", plan["packages"])
        self.assertIn("mare-cli", plan["packages"])

    def test_platform_specific_sandbox_and_lua_specs(self) -> None:
        linux_sbx = build_plan(["crates/mare-binding/src/ossandbox/bwrap.rs"], self.graph)
        self.assertEqual(linux_sbx["expected_jobs"], ["test-linux"])
        self.assertFalse(linux_sbx["windows_sandbox"])

        win_sbx = build_plan(["crates/mare-binding/src/winsandbox/enhanced.rs"], self.graph)
        self.assertEqual(win_sbx["expected_jobs"], ["test-windows"])
        self.assertTrue(win_sbx["windows_sandbox"])
        self.assertEqual(win_sbx["run_rust_lint_on"], "windows")

        lua_spec = build_plan(["crates/mare-plugin/preset/tests/lib/foo_spec.lua"], self.graph)
        self.assertEqual(lua_spec["expected_jobs"], ["lua", "test-linux"])
        self.assertIn("suite::lua_specs::", lua_spec["linux_filter"])

    def test_unmatched_file_fails_wide_to_full_matrix(self) -> None:
        plan = build_plan(["unexpected-root-file.xyz"], self.graph)
        self.assertTrue(plan["full_matrix"])
        self.assertEqual(plan["expected_jobs"], list(ALL_PR_JOBS))

    def test_trivy_triggers_on_cargo_deps_and_action_pins_only(self) -> None:
        dep_diff = (
            "diff --git a/crates/mare-shared/Cargo.toml b/crates/mare-shared/Cargo.toml\n"
            "@@ -10,0 +11 @@ [dependencies]\n"
            "+semver.workspace = true\n"
        )
        plan_dep = build_plan(["crates/mare-shared/Cargo.toml"], self.graph, diff_text=dep_diff)
        self.assertTrue(plan_dep["trivy"])

        meta_diff = (
            "diff --git a/crates/mare-shared/Cargo.toml b/crates/mare-shared/Cargo.toml\n"
            "@@ -2,1 +2,1 @@ [package]\n"
            '-description = "old"\n'
            '+description = "new"\n'
        )
        plan_meta = build_plan(["crates/mare-shared/Cargo.toml"], self.graph, diff_text=meta_diff)
        self.assertFalse(plan_meta["trivy"])

        pin_diff = (
            "diff --git a/.github/workflows/pr-ci.yml b/.github/workflows/pr-ci.yml\n"
            "@@ -20,1 +20,1 @@\n"
            "-      - uses: actions/checkout@0000000000000000000000000000000000000000\n"
            "+      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1\n"
        )
        plan_pin = build_plan([".github/workflows/pr-ci.yml"], self.graph, diff_text=pin_diff)
        self.assertTrue(plan_pin["trivy"])
        self.assertTrue(plan_pin["workflows"])
        self.assertTrue(plan_pin["yaml"])

        non_pin_diff = (
            "diff --git a/.github/workflows/pr-ci.yml b/.github/workflows/pr-ci.yml\n"
            "@@ -20,1 +20,1 @@\n"
            "-    timeout-minutes: 20\n"
            "+    timeout-minutes: 25\n"
        )
        plan_non_pin = build_plan([".github/workflows/pr-ci.yml"], self.graph, diff_text=non_pin_diff)
        self.assertFalse(plan_non_pin["trivy"])

    def test_pr_checks_gate_blocking_and_mergeable_cases(self) -> None:
        base_needs = {job: {"result": "skipped"} for job in ALL_PR_JOBS}

        # 1. Selected Windows job fails -> blocked.
        needs = dict(base_needs)
        needs["test-linux"] = {"result": "success"}
        needs["test-macos"] = {"result": "success"}
        needs["test-windows"] = {"result": "failure"}
        ok, errs = evaluate_pr_checks("success", ["test-linux", "test-macos", "test-windows"], needs)
        self.assertFalse(ok)
        self.assertTrue(any("test-windows" in e for e in errs), errs)

        # 2. Selected job skipped -> blocked.
        needs = dict(base_needs)
        needs["docs-site"] = {"result": "skipped"}
        ok, errs = evaluate_pr_checks("success", ["docs-site"], needs)
        self.assertFalse(ok)
        self.assertTrue(any("docs-site" in e and "skipped" in e for e in errs), errs)

        # 3. Planner fails -> blocked.
        needs = {job: {"result": "success"} for job in ALL_PR_JOBS}
        ok, errs = evaluate_pr_checks("failure", list(ALL_PR_JOBS), needs)
        self.assertFalse(ok)
        self.assertTrue(any("changes" in e for e in errs), errs)

        # 4. Unrelated job skipped -> mergeable.
        needs = dict(base_needs)
        needs["docs-site"] = {"result": "success"}
        ok, msgs = evaluate_pr_checks("success", ["docs-site"], needs)
        self.assertTrue(ok, msgs)

        # 5. Called workflow (nix / release-dry-run / trivy) failed -> blocked.
        needs = dict(base_needs)
        needs["nix"] = {"result": "failure"}
        ok, errs = evaluate_pr_checks("success", ["nix"], needs)
        self.assertFalse(ok)
        self.assertTrue(any("nix" in e for e in errs), errs)

    def test_pr_ci_trivy_disables_codeql_via_explicit_workflow_call_input(self) -> None:
        root = pathlib.Path(__file__).resolve().parent.parent
        security = (root / ".github/workflows/security.yaml").read_text(encoding="utf-8")
        pr_ci = (root / ".github/workflows/pr-ci.yml").read_text(encoding="utf-8")

        # Called reusable workflows inherit the caller's github.event_name (e.g. pull_request),
        # so gating on github.event_name != 'workflow_call' fails to skip CodeQL on PR Trivy calls.
        self.assertNotIn("github.event_name != 'workflow_call'", security)
        self.assertIn("run_codeql:", security)
        self.assertIn("toJSON(inputs.run_codeql) != 'false'", security)
        self.assertRegex(
            pr_ci,
            r"uses:\s*\./\.github/workflows/security\.yaml\s*\n\s*with:\s*\n\s*run_codeql:\s*false",
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="Run internal unit and replay tests")
    parser.add_argument("--gate", action="store_true", help="Evaluate PR checks gate from env vars")
    parser.add_argument("--files", nargs="*", help="Explicit changed file paths (for replay/testing)")
    parser.add_argument("--files-file", help="Read newline-separated changed file paths from a file")
    parser.add_argument("--diff-file", help="Read unified diff text from a file")
    parser.add_argument("--base", help="Git base commit/ref to diff against")
    parser.add_argument("--head", default="HEAD", help="Git head commit/ref (default: HEAD)")
    parser.add_argument("--metadata-json", help="Path to precomputed `cargo metadata` JSON")
    parser.add_argument("--event-name", default=os.environ.get("EVENT_NAME", "pull_request"))
    parser.add_argument("--pr-base-sha", default=os.environ.get("PR_BASE_SHA", ""))
    parser.add_argument("--pr-head-ref", default=os.environ.get("PR_HEAD_REF", ""))
    parser.add_argument("--github-output", help="Append GitHub Actions outputs to this file")
    parser.add_argument("--json", action="store_true", help="Print plan as formatted JSON")
    args = parser.parse_args()

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(CiPlanSelfTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

    if args.gate:
        plan_result = os.environ.get("PLAN_RESULT", "")
        expected_raw = os.environ.get("EXPECTED_JOBS", "")
        needs_raw = os.environ.get("NEEDS_JSON", "{}")
        try:
            needs_map = json.loads(needs_raw)
        except json.JSONDecodeError as exc:
            print(f"PR checks error: invalid NEEDS_JSON: {exc}", file=sys.stderr)
            return 1
        if not isinstance(needs_map, dict):
            print("PR checks error: NEEDS_JSON must be a JSON object", file=sys.stderr)
            return 1
        ok, msgs = evaluate_pr_checks(plan_result, expected_raw, needs_map)
        for msg in msgs:
            print(msg, file=sys.stdout if ok else sys.stderr)
        return 0 if ok else 1

    root = pathlib.Path(__file__).resolve().parent.parent
    metadata = _load_cargo_metadata(root, args.metadata_json)
    graph = load_workspace_graph(metadata, root=root if not args.metadata_json else None)

    files: list[str] | None
    diff_text: str | None = None
    if args.files is not None:
        files = list(args.files)
    elif args.files_file:
        files = [line.strip() for line in pathlib.Path(args.files_file).read_text(encoding="utf-8").splitlines() if line.strip()]
    else:
        files, diff_text = _resolve_git_diff(
            root,
            event_name=args.event_name,
            pr_base_sha=args.pr_base_sha,
            base_arg=args.base,
            head_arg=args.head,
        )

    if args.diff_file:
        diff_text = pathlib.Path(args.diff_file).read_text(encoding="utf-8")

    plan = build_plan(
        files,
        graph,
        diff_text=diff_text,
        event_name=args.event_name,
        pr_head_ref=args.pr_head_ref,
    )

    if args.github_output:
        _write_github_output(args.github_output, plan)

    if args.json or not args.github_output:
        print(json.dumps(plan, indent=2, sort_keys=True))
    else:
        print(f"ci_plan: {plan['reason']} -> expected_jobs={plan['expected_jobs']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
