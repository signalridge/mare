#!/usr/bin/env python3
"""Verify mare release workflow, automation, and pinning policies. Stdlib-only."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import stat
import subprocess
import sys
import tempfile
import tomllib
import unittest

SHA_REF_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+@[0-9a-f]{40}$")
USES_RE = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)", re.MULTILINE)
SETUP_RUST_ACTION = ".github/actions/setup-rust/action.yml"
TOOLCHAIN_ACTION_RE = re.compile(r"uses:\s*dtolnay/rust-toolchain@")


def extract_job_block(text: str, job_key: str) -> str:
    lines = text.splitlines()
    marker = f"  {job_key}:"
    start = None
    for idx, line in enumerate(lines):
        if line == marker:
            start = idx
            break
    if start is None:
        return ""
    end = len(lines)
    for idx in range(start + 1, len(lines)):
        line = lines[idx]
        if line and not line.startswith("    ") and not line.lstrip().startswith("#"):
            end = idx
            break
    return "\n".join(lines[start:end])


def extract_step_run(job_block: str, step_name: str) -> str:
    lines = job_block.splitlines()
    needle = f"name: {step_name}"
    step_idx = None
    for idx, line in enumerate(lines):
        if needle in line:
            step_idx = idx
            break
    if step_idx is None:
        return ""
    run_idx = None
    run_indent = 0
    for idx in range(step_idx + 1, len(lines)):
        line = lines[idx]
        if re.match(r"^\s*-\s+(?:name|uses|id|if|run):", line):
            break
        m = re.match(r"^(\s*)run:\s*\|(.*)$", line)
        if m:
            run_idx = idx
            run_indent = len(m.group(1))
            break
    if run_idx is None:
        return ""
    body: list[str] = []
    block_indent: int | None = None
    for line in lines[run_idx + 1 :]:
        if not line.strip():
            body.append("")
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent <= run_indent:
            break
        if block_indent is None:
            block_indent = indent
        body.append(line[block_indent:] if len(line) >= block_indent else line.lstrip(" "))
    return "\n".join(body).strip() + "\n"


def check_all_actions_pinned(root: pathlib.Path) -> list[str]:
    errors: list[str] = []
    wf_dir = root / ".github" / "workflows"
    if not wf_dir.is_dir():
        return ["missing .github/workflows directory"]
    actions_dir = root / ".github" / "actions"
    local_actions = sorted(actions_dir.glob("*/action.y*ml")) if actions_dir.is_dir() else []
    for wf in sorted(wf_dir.glob("*.y*ml")) + local_actions:
        text = wf.read_text(encoding="utf-8")
        for m in USES_RE.finditer(text):
            ref = m.group(1)
            if ref.startswith("./"):
                continue
            if not SHA_REF_RE.match(ref):
                errors.append(f"{wf.relative_to(root)}: unpinned action reference {ref!r}")
    return errors


def check_release_please_config(root: pathlib.Path) -> list[str]:
    errors: list[str] = []
    cfg_path = root / "release-please-config.json"
    if not cfg_path.is_file():
        return ["missing release-please-config.json"]
    cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
    pkg = cfg.get("packages", {}).get(".", {})
    if pkg.get("draft") is not True:
        errors.append("release-please-config.json: packages['.'].draft must be true")
    if pkg.get("force-tag-creation") is not True:
        errors.append("release-please-config.json: packages['.']['force-tag-creation'] must be true")
    extra_paths = {e.get("path") for e in pkg.get("extra-files", []) if isinstance(e, dict)}
    for req in ("Cargo.toml", "Cargo.lock"):
        if req not in extra_paths:
            errors.append(f"release-please-config.json: extra-files missing {req}")
    return errors


def check_release_workflow(root: pathlib.Path) -> list[str]:
    errors: list[str] = []
    rel_path = root / ".github" / "workflows" / "release.yaml"
    if not rel_path.is_file():
        return ["missing .github/workflows/release.yaml"]
    text = rel_path.read_text(encoding="utf-8")
    forbidden_skip = "skip_" + "ci_gate"
    if forbidden_skip in text:
        errors.append(f"release.yaml must not contain {forbidden_skip}")

    val_block = extract_job_block(text, "validate-tag")
    if not val_block:
        errors.append("release.yaml missing validate-tag job")
    else:
        if "secrets." in val_block:
            errors.append("release.yaml validate-tag must not reference secrets")
        if "cargo metadata --locked" not in val_block:
            errors.append("release.yaml validate-tag must verify Cargo.lock with cargo metadata --locked")

    gate_block = extract_job_block(text, "gate")
    if not gate_block:
        errors.append("release.yaml missing gate job")
    else:
        if "needs: [validate-tag]" not in gate_block:
            errors.append("release.yaml gate job must need [validate-tag]")
        if "actions: read" not in gate_block:
            errors.append("release.yaml gate job must request actions: read permission")
        if "--wait-minutes" not in gate_block:
            errors.append("release.yaml gate job must pass --wait-minutes to check_release_gate.py")

    build_block = extract_job_block(text, "build")
    if "needs: [validate-tag, gate, rust-notices]" not in build_block:
        errors.append("release.yaml build job must need [validate-tag, gate, rust-notices]")
    if "scripts/verify_release_archive.py" not in build_block:
        errors.append("release.yaml build job must verify archives via scripts/verify_release_archive.py")
    for target in ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
        if target not in build_block:
            errors.append(f"release.yaml build job must target {target}")
    if "unknown-linux-gnu" in build_block:
        errors.append("release.yaml build job must use *-unknown-linux-musl, not *-unknown-linux-gnu")
    if "musl-tools" not in build_block:
        errors.append("release.yaml build job must install musl-tools for Linux musl builds")
    if "AWS_LC_SYS_NO_JITTER_ENTROPY=1" not in build_block:
        errors.append("release.yaml build job must disable aws-lc jitter entropy on musl")
    if "readelf -l" not in build_block or "INTERP" not in build_block:
        errors.append("release.yaml build job must verify Linux musl binaries have no PT_INTERP")

    release_block = extract_job_block(text, "release")
    if not release_block:
        errors.append("release.yaml missing release job")
    else:
        if "./.github/actions/setup-nfpm" not in release_block:
            errors.append("release.yaml release job must install nfpm via ./.github/actions/setup-nfpm")
        if "nfpm/releases/download" in release_block:
            errors.append("release.yaml must not curl nfpm directly without ./.github/actions/setup-nfpm")
        if "scripts/release_manifests.py" not in release_block:
            errors.append("release.yaml release job must use scripts/release_manifests.py")
        if 'cask "mare" do' in release_block or "pkgbase = mare-bin" in release_block:
            errors.append("release.yaml must not inline manifest templates; keep them in scripts/release_manifests.py")
        if "include-hidden-files: true" not in release_block:
            errors.append("release.yaml package-manifests upload must set include-hidden-files: true")
        if "append_body: true" not in release_block:
            errors.append("release.yaml softprops release step must set append_body: true")
        if "draft: false" not in release_block:
            errors.append("release.yaml softprops release step must publish draft release with draft: false")
        if "steps.metadata.outputs.docker_tags" not in release_block:
            errors.append("release.yaml container build must use conditional steps.metadata.outputs.docker_tags")
        if "${{ env.REGISTRY }}/${{ env.IMAGE_NAME }}:latest" in release_block:
            errors.append("release.yaml must not unconditionally push GHCR :latest")
        if "unknown-linux-gnu" in release_block:
            errors.append("release.yaml release job must package *-unknown-linux-musl, not *-unknown-linux-gnu")

    verify_binary = extract_job_block(text, "verify-binary")
    if not verify_binary:
        errors.append("release.yaml missing verify-binary job")
    else:
        for image in ("ubuntu:20.04", "debian:11", "registry.access.redhat.com/ubi8/ubi"):
            if image not in verify_binary:
                errors.append(f"release.yaml verify-binary must verify Linux binary inside {image}")

    verify_aur = extract_job_block(text, "verify-aur")
    if not verify_aur:
        errors.append("release.yaml missing verify-aur job")
    else:
        if "needs: [release, publish-aur]" not in verify_aur:
            errors.append("release.yaml verify-aur must need [release, publish-aur]")
        if "makepkg" not in verify_aur:
            errors.append("release.yaml verify-aur must test package with makepkg")

    verify_scoop = extract_job_block(text, "verify-scoop")
    if "get.scoop.sh" in verify_scoop:
        errors.append("release.yaml verify-scoop must pin Scoop engine commit instead of get.scoop.sh")
    if "ScoopInstaller/Scoop.git" not in verify_scoop:
        errors.append("release.yaml verify-scoop must clone pinned ScoopInstaller/Scoop.git commit")

    verify_container = extract_job_block(text, "verify-container")
    if "aquasecurity/trivy-action@" not in verify_container:
        errors.append("release.yaml verify-container must scan image with Trivy")
    if "security-events: write" not in verify_container:
        errors.append("release.yaml verify-container must request security-events: write for Trivy SARIF")

    return errors


def check_companion_workflows_and_files(root: pathlib.Path) -> list[str]:
    errors: list[str] = []

    rp_wf = root / ".github" / "workflows" / "release-please.yaml"
    if not rp_wf.is_file():
        errors.append("missing .github/workflows/release-please.yaml")
    else:
        rp_text = rp_wf.read_text(encoding="utf-8")
        if "token: ${{ secrets.GH_PAT }}" not in rp_text:
            errors.append("release-please.yaml must pass GH_PAT token to release-please-action")

    docs_wf = root / ".github" / "workflows" / "docs.yml"
    if not docs_wf.is_file():
        errors.append("missing .github/workflows/docs.yml")
    else:
        deploy_block = extract_job_block(docs_wf.read_text(encoding="utf-8"), "deploy")
        expected_if = "if: github.event_name != 'pull_request' && github.ref == 'refs/heads/main'"
        if expected_if not in deploy_block:
            errors.append(f"docs.yml deploy job must guard deployment with {expected_if!r}")

    sec_wf = root / ".github" / "workflows" / "security.yaml"
    if not sec_wf.is_file():
        errors.append("missing .github/workflows/security.yaml")
    else:
        sec_text = sec_wf.read_text(encoding="utf-8")
        for needle, label in (
            ("github/codeql-action/init@", "CodeQL init"),
            ("languages: rust", "CodeQL Rust"),
            ("aquasecurity/trivy-action@", "Trivy fs scan"),
            ("scan-type: fs", "Trivy scan-type: fs"),
            ("anchore/sbom-action/download-syft@", "SBOM syft"),
            ("github.event_name == 'push' && github.ref == 'refs/heads/main'", "main-only SBOM"),
        ):
            if needle not in sec_text:
                errors.append(f"security.yaml missing {label} ({needle!r})")

    dry_wf = root / ".github" / "workflows" / "release-dry-run.yaml"
    if not dry_wf.is_file():
        errors.append("missing .github/workflows/release-dry-run.yaml")
    else:
        dry_text = dry_wf.read_text(encoding="utf-8")
        for needle in (
            "scripts/release_manifests.py --self-test",
            "scripts/verify_release_archive.py",
            "cargo build --release --locked",
            "./.github/actions/setup-nfpm",
            "windows-latest",
            "Compress-Archive",
        ):
            if needle not in dry_text:
                errors.append(f"release-dry-run.yaml missing {needle!r}")
        if "nfpm/releases/download" in dry_text:
            errors.append("release-dry-run.yaml must install nfpm via ./.github/actions/setup-nfpm")

    ci_wf = root / ".github" / "workflows" / "ci.yml"
    if not ci_wf.is_file():
        errors.append("missing .github/workflows/ci.yml")

    nfpm_action = root / ".github" / "actions" / "setup-nfpm" / "action.yml"
    if not nfpm_action.is_file():
        errors.append("missing .github/actions/setup-nfpm/action.yml")
    else:
        nfpm_text = nfpm_action.read_text(encoding="utf-8")
        for needle in (
            ".nfpm.version",
            ".nfpm.sha256",
            "sha256sum -c",
            "cosign verify-blob",
            "goreleaser/nfpm/.github/workflows/release.yml@refs/tags/v",
        ):
            if needle not in nfpm_text:
                errors.append(f".github/actions/setup-nfpm/action.yml missing {needle!r}")

    dep_path = root / ".github" / "dependabot.yml"
    if not dep_path.is_file():
        errors.append("missing .github/dependabot.yml")
    else:
        dep_text = dep_path.read_text(encoding="utf-8")
        for eco in ("cargo", "github-actions", "docker"):
            if f"package-ecosystem: {eco}" not in dep_text:
                errors.append(f"dependabot.yml missing package-ecosystem: {eco}")

    for wf_name in ("flake-lock-update.yaml", "npm-dependency-update.yaml", "ci-deps-update.yaml"):
        p = root / ".github" / "workflows" / wf_name
        if not p.is_file():
            errors.append(f"missing .github/workflows/{wf_name}")
            continue
        t = p.read_text(encoding="utf-8")
        if "token: ${{ secrets.GH_PAT }}" not in t:
            errors.append(f"{wf_name}: create-pull-request must use token: ${{{{ secrets.GH_PAT }}}}")
        if wf_name == "npm-dependency-update.yaml" and "docs-site" not in t:
            errors.append("npm-dependency-update.yaml must target docs-site")
        if wf_name == "ci-deps-update.yaml":
            for needle in (
                "schedule:",
                "workflow_dispatch:",
                ".github/ci-tools.json",
                "archlinux:base-devel@sha256:",
                "peter-evans/create-pull-request@",
            ):
                if needle not in t:
                    errors.append(f"ci-deps-update.yaml missing {needle!r}")

    errors.extend(check_dockerfile_release(root))

    contrib = root / "CONTRIBUTING.md"
    if not contrib.is_file():
        errors.append("missing CONTRIBUTING.md")
    else:
        ct = contrib.read_text(encoding="utf-8")
        for heading in ("## Commits and pull requests", "## Release automation"):
            if heading not in ct:
                errors.append(f"CONTRIBUTING.md missing section {heading!r}")

    if not (root / "scripts" / "release_manifests.py").is_file():
        errors.append("missing scripts/release_manifests.py")

    install_sh = root / "install.sh"
    site_install_sh = root / "docs-site" / "public" / "install.sh"
    if not install_sh.is_file():
        errors.append("missing install.sh")
    else:
        inst_text = install_sh.read_text(encoding="utf-8")
        if "unknown-linux-musl" not in inst_text:
            errors.append("install.sh must support unknown-linux-musl archives on Linux")
        if site_install_sh.is_file() and site_install_sh.read_text(encoding="utf-8") != inst_text:
            errors.append("docs-site/public/install.sh must match install.sh")

    return errors


def check_dockerfile_release(root: pathlib.Path) -> list[str]:
    df = root / "Dockerfile.release"
    if not df.is_file():
        return ["missing Dockerfile.release"]

    errors: list[str] = []
    stages: set[str] = set()
    from_count = 0
    for line in df.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^\s*FROM\s+(.+?)\s*$", line, re.IGNORECASE)
        if not m:
            continue
        from_count += 1
        tokens = m.group(1).split()
        while tokens and tokens[0].startswith("--"):
            tokens.pop(0)
        if not tokens:
            errors.append(f"Dockerfile.release has malformed FROM line: {line.strip()!r}")
            continue
        image = tokens[0]
        stage = tokens[2].lower() if len(tokens) >= 3 and tokens[1].lower() == "as" else None
        if image.lower() != "scratch" and image.lower() not in stages:
            if not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", image):
                errors.append(
                    f"Dockerfile.release base image must be pinned by @sha256 digest: {image!r}"
                )
        if stage:
            stages.add(stage)

    if from_count == 0:
        errors.append("Dockerfile.release must declare a base image pinned by @sha256 digest")
    return errors


def check_ci_version_files(root: pathlib.Path) -> list[str]:
    errors: list[str] = []

    py_ver = root / ".python-version"
    if not py_ver.is_file():
        errors.append("missing .python-version")
    elif not re.fullmatch(r"\d+\.\d+(?:\.\d+)?", py_ver.read_text(encoding="utf-8").strip()):
        errors.append(f".python-version has invalid version: {py_ver.read_text(encoding='utf-8').strip()!r}")

    nvmrc = root / ".nvmrc"
    if not nvmrc.is_file():
        errors.append("missing .nvmrc")
    elif not re.fullmatch(r"\d+(?:\.\d+\.\d+)?", nvmrc.read_text(encoding="utf-8").strip()):
        errors.append(f".nvmrc has invalid version: {nvmrc.read_text(encoding='utf-8').strip()!r}")

    tools_path = root / ".github" / "ci-tools.json"
    if not tools_path.is_file():
        errors.append("missing .github/ci-tools.json")
    else:
        tools = json.loads(tools_path.read_text(encoding="utf-8"))
        for key in ("stylua", "luals", "ast_grep", "cargo_about"):
            val = str(tools.get(key, ""))
            if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", val):
                errors.append(f".github/ci-tools.json: invalid semver for {key!r}: {val!r}")
        nfpm_entry = tools.get("nfpm")
        if not isinstance(nfpm_entry, dict):
            errors.append(".github/ci-tools.json: 'nfpm' must be an object with 'version' and 'sha256'")
        else:
            nfpm_ver = str(nfpm_entry.get("version", ""))
            nfpm_sha = str(nfpm_entry.get("sha256", ""))
            if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", nfpm_ver):
                errors.append(f".github/ci-tools.json: invalid semver for 'nfpm.version': {nfpm_ver!r}")
            if not re.fullmatch(r"[0-9a-f]{64}", nfpm_sha):
                errors.append(f".github/ci-tools.json: invalid sha256 for 'nfpm.sha256': {nfpm_sha!r}")
        lua_val = str(tools.get("lua", ""))
        if not re.fullmatch(r"\d+\.\d+\.\d+", lua_val):
            errors.append(f".github/ci-tools.json: lua must pin exact X.Y.Z version, got {lua_val!r}")
        lua_sha = str(tools.get("lua_sha256", ""))
        if not re.fullmatch(r"[0-9a-f]{64}", lua_sha):
            errors.append(f".github/ci-tools.json: invalid sha256 for 'lua_sha256': {lua_sha!r}")

    wf_dir = root / ".github" / "workflows"
    forbidden_patterns = (
        (r"(?m)^\s*python-version\s*:", "use python-version-file: .python-version"),
        (r"(?m)^\s*node-version\s*:", "use node-version-file: .nvmrc"),
        (r'(?m)^\s*(?:stylua_ver|ast_grep_ver|ast_grep_version|luals_version|nfpm_ver)\s*=\s*["\']?\d', "read from .github/ci-tools.json"),
        (r'(?m)^\s*ver\s*=\s*["\']\d+\.\d+\.\d+["\']', "read from .github/ci-tools.json"),
        (r'(?m)^\s*version:\s*\d+\.\d+\.\d+', "read stylua version from .github/ci-tools.json"),
        (r'(?m)^\s*luaVersion:\s*["\']\d+\.\d+', "read lua version from .github/ci-tools.json"),
    )
    if wf_dir.is_dir():
        for wf in sorted(wf_dir.glob("*.y*ml")):
            text = wf.read_text(encoding="utf-8")
            rel = wf.relative_to(root).as_posix()
            for pat, hint in forbidden_patterns:
                if re.search(pat, text):
                    errors.append(f"{rel}: hardcoded tool/runtime version ({hint})")
    return errors


def check_rust_toolchain_source(root: pathlib.Path) -> list[str]:
    """rust-toolchain.toml is the only place the Rust version is written down."""
    errors: list[str] = []
    tc_path = root / "rust-toolchain.toml"
    if not tc_path.is_file():
        errors.append("missing rust-toolchain.toml")
    else:
        try:
            channel = tomllib.loads(tc_path.read_text(encoding="utf-8")).get("toolchain", {}).get("channel", "")
        except tomllib.TOMLDecodeError as exc:
            channel = ""
            errors.append(f"rust-toolchain.toml: invalid TOML: {exc}")
        if not re.fullmatch(r"\d+\.\d+\.\d+", str(channel)):
            errors.append(f"rust-toolchain.toml: [toolchain].channel must pin an exact X.Y.Z release, got {channel!r}")

    action = root / SETUP_RUST_ACTION
    if not action.is_file():
        errors.append(f"missing {SETUP_RUST_ACTION}")
    else:
        text = action.read_text(encoding="utf-8")
        if not TOOLCHAIN_ACTION_RE.search(text):
            errors.append(f"{SETUP_RUST_ACTION} must install the toolchain with dtolnay/rust-toolchain")
        for needle in ("rust-toolchain.toml", "toolchain: ${{ steps.pin.outputs.toolchain }}"):
            if needle not in text:
                errors.append(f"{SETUP_RUST_ACTION} must read the toolchain from rust-toolchain.toml ({needle!r})")

    wf_dir = root / ".github" / "workflows"
    if wf_dir.is_dir():
        for wf in sorted(wf_dir.glob("*.y*ml")):
            text = wf.read_text(encoding="utf-8")
            rel = wf.relative_to(root).as_posix()
            if TOOLCHAIN_ACTION_RE.search(text):
                errors.append(f"{rel}: install Rust via ./.github/actions/setup-rust, not dtolnay/rust-toolchain directly")
            if re.search(r"(?m)^\s*toolchain:\s*\S", text):
                errors.append(f"{rel}: hardcoded Rust toolchain (pin it in rust-toolchain.toml)")

    deps = root / ".github" / "workflows" / "ci-deps-update.yaml"
    if deps.is_file():
        deps_text = deps.read_text(encoding="utf-8")
        for needle in ("rust-toolchain.toml", "channel-rust-stable.toml"):
            if needle not in deps_text:
                errors.append(f"ci-deps-update.yaml must refresh rust-toolchain.toml from the stable channel ({needle!r})")

    dep_path = root / ".github" / "dependabot.yml"
    if dep_path.is_file() and "/.github/actions/*" not in dep_path.read_text(encoding="utf-8"):
        errors.append("dependabot.yml: github-actions updates must also cover /.github/actions/*")
    return errors


PWSH_HEREDOC_RE = re.compile(r"<<-?\s*(?:'[A-Za-z_]\w*'|\"[A-Za-z_]\w*\"|[A-Za-z_]\w*)")


def check_pwsh_heredocs(root: pathlib.Path) -> list[str]:
    """Forbid POSIX/bash heredocs inside `shell: pwsh` workflow steps."""
    errors: list[str] = []
    wf_dir = root / ".github" / "workflows"
    actions_dir = root / ".github" / "actions"
    files = sorted(wf_dir.glob("*.y*ml")) if wf_dir.is_dir() else []
    if actions_dir.is_dir():
        files += sorted(actions_dir.glob("*/action.y*ml"))
    for wf in files:
        lines = wf.read_text(encoding="utf-8").splitlines()
        rel = wf.relative_to(root).as_posix()
        in_pwsh_step = False
        in_run_block = False
        run_indent = 0
        for lineno, line in enumerate(lines, 1):
            if in_run_block:
                if line.strip():
                    indent = len(line) - len(line.lstrip(" "))
                    if indent <= run_indent:
                        in_run_block = False
                    else:
                        if PWSH_HEREDOC_RE.search(line):
                            errors.append(f"{rel}:{lineno}: bash heredoc inside `shell: pwsh` step: {line.strip()!r}")
                        continue
                else:
                    continue
            if re.match(r"^\s*-\s+(?:name|uses|id|if|run|shell):", line):
                in_pwsh_step = False
            if re.match(r"^\s*(?:-\s+)?shell:\s*pwsh\b", line):
                in_pwsh_step = True
            m = re.match(r"^(\s*)(?:-\s+)?run:\s*[|>][-+]?\s*$", line)
            if in_pwsh_step and m:
                in_run_block = True
                run_indent = len(m.group(1))
    return errors


def check_repo(root: pathlib.Path) -> list[str]:
    errors: list[str] = []
    errors.extend(check_all_actions_pinned(root))
    errors.extend(check_pwsh_heredocs(root))
    errors.extend(check_release_please_config(root))
    errors.extend(check_release_workflow(root))
    errors.extend(check_companion_workflows_and_files(root))
    errors.extend(check_ci_version_files(root))
    errors.extend(check_rust_toolchain_source(root))
    return errors


class ReleasePolicySelfTests(unittest.TestCase):
    def test_pwsh_heredoc_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            wf = root / ".github" / "workflows"
            wf.mkdir(parents=True)
            (wf / "bad.yaml").write_text(
                "jobs:\n  win:\n    steps:\n      - name: Bad\n        shell: pwsh\n"
                "        run: |\n          python - <<'PY'\n          print(1)\n          PY\n",
                encoding="utf-8",
            )
            errs = check_pwsh_heredocs(root)
            self.assertEqual(len(errs), 1, errs)
            self.assertIn("bash heredoc inside `shell: pwsh`", errs[0])
    def _repo_root(self) -> pathlib.Path:
        return pathlib.Path(__file__).resolve().parent.parent

    def test_release_please_reconcile_shell_contract(self):
        if os.name == "nt":
            self.skipTest("shell contract runs on Unix")
        rp_text = (self._repo_root() / ".github" / "workflows" / "release-please.yaml").read_text(encoding="utf-8")
        job = extract_job_block(rp_text, "release-please")
        script = extract_step_run(job, "Reconcile manifest, tag, and GitHub Release")
        self.assertTrue(script.strip(), "could not extract reconcile script")

        cases = [
            ("published_release", "0", "0", '{"tag_name":"v1.2.3","draft":false,"prerelease":false}', "[]", False),
            ("draft_release_fallback", "0", "1", "", '[{"tag_name":"v1.2.3","draft":true,"prerelease":false}]', False),
            ("missing_tag", "1", "0", '{"tag_name":"v1.2.3","draft":false,"prerelease":false}', "[]", True),
            ("missing_release", "0", "1", "", "[]", True),
            ("prerelease_rejected", "0", "0", '{"tag_name":"v1.2.3","draft":false,"prerelease":true}', "[]", True),
            ("tag_mismatch_rejected", "0", "0", '{"tag_name":"v9.9.9","draft":false,"prerelease":false}', "[]", True),
        ]
        for name, git_rc, tag_api_rc, tag_json, list_json, want_err in cases:
            with self.subTest(case=name), tempfile.TemporaryDirectory() as tmp:
                d = pathlib.Path(tmp)
                (d / ".release-please-manifest.json").write_text('{".":"1.2.3"}\n', encoding="utf-8")
                bindir = d / "bin"
                bindir.mkdir()
                git_bin = bindir / "git"
                git_bin.write_text('#!/bin/sh\nexit "${FAKE_GIT_RC}"\n', encoding="utf-8")
                git_bin.chmod(stat.S_IRWXU)
                gh_bin = bindir / "gh"
                gh_bin.write_text(
                    '#!/bin/sh\n'
                    'case "$*" in\n'
                    '  *releases/tags/*)\n'
                    '    if [ "${FAKE_TAG_API_RC}" != "0" ]; then exit "${FAKE_TAG_API_RC}"; fi\n'
                    '    printf "%s\\n" "$FAKE_TAG_JSON"\n'
                    '    ;;\n'
                    '  */releases*)\n'
                    '    printf "%s\\n" "$FAKE_LIST_JSON"\n'
                    '    ;;\n'
                    '  *) exit 64 ;;\n'
                    'esac\n',
                    encoding="utf-8",
                )
                gh_bin.chmod(stat.S_IRWXU)
                env = dict(
                    os.environ,
                    PATH=str(bindir) + os.pathsep + os.environ.get("PATH", ""),
                    GITHUB_REPOSITORY="signalridge/mare",
                    FAKE_GIT_RC=git_rc,
                    FAKE_TAG_API_RC=tag_api_rc,
                    FAKE_TAG_JSON=tag_json,
                    FAKE_LIST_JSON=list_json,
                )
                res = subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", script],
                    cwd=d,
                    env=env,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if want_err:
                    self.assertNotEqual(res.returncode, 0, f"{name}: expected failure, got 0: {res.stdout}{res.stderr}")
                else:
                    self.assertEqual(res.returncode, 0, f"{name}: expected success, got {res.returncode}: {res.stdout}{res.stderr}")

    def _toolchain_fixture(self, root: pathlib.Path, channel: str = "1.100.0") -> None:
        (root / "rust-toolchain.toml").write_text(
            f'[toolchain]\nchannel = "{channel}"\ncomponents = ["rustfmt", "clippy"]\n', encoding="utf-8"
        )
        action = root / SETUP_RUST_ACTION
        action.parent.mkdir(parents=True)
        action.write_text(
            "# reads rust-toolchain.toml\n"
            "    - uses: dtolnay/rust-toolchain@0123456789012345678901234567890123456789\n"
            "      with:\n        toolchain: ${{ steps.pin.outputs.toolchain }}\n",
            encoding="utf-8",
        )
        wf = root / ".github" / "workflows"
        wf.mkdir(parents=True)
        (wf / "ci.yml").write_text("      - uses: ./.github/actions/setup-rust\n", encoding="utf-8")
        (wf / "ci-deps-update.yaml").write_text(
            "# channel-rust-stable.toml -> rust-toolchain.toml\n", encoding="utf-8"
        )
        (root / ".github" / "dependabot.yml").write_text('directories: ["/", "/.github/actions/*"]\n', encoding="utf-8")

    def test_rust_toolchain_single_source_fixture_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._toolchain_fixture(root)
            self.assertEqual(check_rust_toolchain_source(root), [])

    def test_rust_toolchain_rejects_floating_channel(self):
        for channel in ("stable", "1.100", "nightly-2026-11-01"):
            with self.subTest(channel=channel), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                self._toolchain_fixture(root, channel)
                self.assertTrue(any("exact X.Y.Z" in e for e in check_rust_toolchain_source(root)))

    def test_rust_toolchain_rejects_direct_toolchain_action_in_workflow(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._toolchain_fixture(root)
            (root / ".github" / "workflows" / "docs.yml").write_text(
                "      - uses: dtolnay/rust-toolchain@0123456789012345678901234567890123456789 # stable\n"
                "        with:\n          toolchain: stable\n",
                encoding="utf-8",
            )
            errs = check_rust_toolchain_source(root)
            self.assertTrue(any("docs.yml" in e and "setup-rust" in e for e in errs), errs)
            self.assertTrue(any("docs.yml" in e and "hardcoded Rust toolchain" in e for e in errs), errs)

    def test_rust_toolchain_requires_deps_update_coverage(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._toolchain_fixture(root)
            (root / ".github" / "workflows" / "ci-deps-update.yaml").write_text("tools only\n", encoding="utf-8")
            self.assertTrue(any("ci-deps-update.yaml" in e for e in check_rust_toolchain_source(root)))

    def test_setup_rust_action_reads_pinned_toolchain(self):
        if os.name == "nt":
            self.skipTest("shell contract runs on Unix")
        text = (self._repo_root() / SETUP_RUST_ACTION).read_text(encoding="utf-8")
        script = extract_step_run(text, "Read rust-toolchain.toml")
        self.assertTrue(script.strip(), "could not extract the rust-toolchain.toml reader")
        cases = (
            ('[toolchain]\nchannel = "1.100.0"\nprofile = "minimal"\ncomponents = ["rustfmt", "clippy"]\n',
             "toolchain=1.100.0\ncomponents=rustfmt,clippy\n"),
            ('[toolchain]\r\nchannel = "1.99.1"\r\n', "toolchain=1.99.1\ncomponents=\n"),
            ('[toolchain]\nchannel = "stable"\n', None),
        )
        for body, want in cases:
            with self.subTest(body=body), tempfile.TemporaryDirectory() as tmp:
                d = pathlib.Path(tmp)
                (d / "rust-toolchain.toml").write_bytes(body.encode("utf-8"))
                out = d / "github_output"
                out.write_text("", encoding="utf-8")
                res = subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", script],
                    cwd=d,
                    env=dict(os.environ, GITHUB_OUTPUT=str(out)),
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if want is None:
                    self.assertNotEqual(res.returncode, 0, "a floating channel must be rejected")
                else:
                    self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
                    self.assertEqual(out.read_text(encoding="utf-8"), want)

    def test_validate_tag_semver_contract(self):
        if os.name == "nt":
            self.skipTest("shell contract runs on Unix")
        rel_text = (self._repo_root() / ".github" / "workflows" / "release.yaml").read_text(encoding="utf-8")
        job = extract_job_block(rel_text, "validate-tag")
        script = extract_step_run(job, "Validate release tag syntax")
        self.assertTrue(script.strip(), "could not extract validate-tag syntax script")

        for tag, want_ok in (
            ("v1.0.0", True),
            ("v2.3.4-rc.1", True),
            ("1.0.0", False),
            ("v01.0.0", False),
            ("v1.0.0+build.1", False),
            ("v1.0.0\nfoo", False),
        ):
            with self.subTest(tag=tag), tempfile.TemporaryDirectory() as tmp:
                out = pathlib.Path(tmp) / "github_output"
                out.write_text("", encoding="utf-8")
                env = dict(os.environ, INPUT_TAG=tag, REF_NAME="", GITHUB_OUTPUT=str(out))
                res = subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", script],
                    env=env,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if want_ok:
                    self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
                    self.assertIn(f"tag_name={tag}", out.read_text(encoding="utf-8"))
                else:
                    self.assertNotEqual(res.returncode, 0, f"tag {tag!r} should be rejected")

    def test_dockerfile_release_requires_digest_not_specific_version(self):
        digest = "a" * 64
        for body in (
            f"FROM ubuntu:24.04@sha256:{digest}\n",
            f"FROM ubuntu:26.04@sha256:{digest}\n",
            f"FROM --platform=$BUILDPLATFORM ubuntu:26.04@sha256:{digest} AS runtime\n",
        ):
            with self.subTest(valid=body.strip()), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                (root / "Dockerfile.release").write_text(body, encoding="utf-8")
                self.assertEqual(check_dockerfile_release(root), [])

        for body in (
            "FROM ubuntu:24.04\n",
            "FROM ubuntu:26.04\n",
            "FROM ubuntu:latest\n",
            "FROM ubuntu:26.04@sha256:deadbeef\n",
            f"FROM ubuntu:26.04@sha256:{'A' * 64}\n",
            "RUN echo no-from\n",
        ):
            with self.subTest(invalid=body.strip()), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                (root / "Dockerfile.release").write_text(body, encoding="utf-8")
                errs = check_dockerfile_release(root)
                self.assertTrue(errs, f"expected failure for {body!r}")

    def test_install_sh_selects_linux_musl_targets(self):
        if os.name == "nt":
            self.skipTest("shell contract runs on Unix")
        install_sh = (self._repo_root() / "install.sh").read_text(encoding="utf-8")
        cases = (
            ("x86_64", True, "x86_64-unknown-linux-musl"),
            ("aarch64", True, "aarch64-unknown-linux-musl"),
            ("x86_64", False, "x86_64-unknown-linux-gnu"),
            ("aarch64", False, "aarch64-unknown-linux-gnu"),
        )
        for arch, has_musl, expected_target in cases:
            with self.subTest(arch=arch, has_musl=has_musl), tempfile.TemporaryDirectory() as tmp:
                d = pathlib.Path(tmp)
                bindir = d / "bin"
                bindir.mkdir()
                uname_bin = bindir / "uname"
                uname_bin.write_text(
                    '#!/bin/sh\nif [ "$1" = "-s" ]; then echo Linux; else echo ' + arch + "; fi\n",
                    encoding="utf-8",
                )
                uname_bin.chmod(stat.S_IRWXU)
                curl_bin = bindir / "curl"
                curl_bin.write_text(
                    '#!/bin/sh\n'
                    'out=""\n'
                    'url=""\n'
                    'while [ "$#" -gt 0 ]; do\n'
                    '  case "$1" in\n'
                    '    -o) out="$2"; shift 2 ;;\n'
                    '    https://*) url="$1"; shift ;;\n'
                    '    *) shift ;;\n'
                    '  esac\n'
                    'done\n'
                    'case "$url" in\n'
                    '  */checksums.txt)\n'
                    '    printf "%s\\n" "$FAKE_CHECKSUMS" > "$out"\n'
                    '    exit 0\n'
                    '    ;;\n'
                    '  *)\n'
                    '    echo "CURL_URL:$url" >&2\n'
                    '    exit 99\n'
                    '    ;;\n'
                    'esac\n',
                    encoding="utf-8",
                )
                curl_bin.chmod(stat.S_IRWXU)
                checksums = (
                    f"{'a' * 64}  mare-v1.0.0-{arch}-unknown-linux-musl.tar.gz\n"
                    if has_musl
                    else f"{'a' * 64}  mare-v1.0.0-{arch}-unknown-linux-gnu.tar.gz\n"
                )
                script_path = d / "install.sh"
                script_path.write_text(install_sh, encoding="utf-8")
                res = subprocess.run(
                    ["sh", str(script_path)],
                    env=dict(
                        os.environ,
                        PATH=str(bindir) + os.pathsep + os.environ.get("PATH", ""),
                        MARE_VERSION="v1.0.0",
                        FAKE_CHECKSUMS=checksums,
                    ),
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(res.returncode, 99, res.stdout + res.stderr)
                self.assertIn(f"mare-v1.0.0-{expected_target}.tar.gz", res.stderr)

    def test_release_workflow_catches_linux_gnu_or_missing_musl_checks(self):
        rel_text = (self._repo_root() / ".github" / "workflows" / "release.yaml").read_text(encoding="utf-8")
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            wf_dir = root / ".github" / "workflows"
            wf_dir.mkdir(parents=True)

            (wf_dir / "release.yaml").write_text(rel_text.replace("unknown-linux-musl", "unknown-linux-gnu"), encoding="utf-8")
            errs = check_release_workflow(root)
            self.assertTrue(any("unknown-linux-musl" in e for e in errs), errs)

            (wf_dir / "release.yaml").write_text(rel_text.replace("ubuntu:20.04", "ubuntu:24.04"), encoding="utf-8")
            errs = check_release_workflow(root)
            self.assertTrue(any("ubuntu:20.04" in e for e in errs), errs)

            (wf_dir / "release.yaml").write_text(
                rel_text.replace("registry.access.redhat.com/ubi8/ubi", "ubuntu:24.04"),
                encoding="utf-8",
            )
            errs = check_release_workflow(root)
            self.assertTrue(any("ubi8" in e for e in errs), errs)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="Run release policy unit tests")
    args = parser.parse_args()

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReleasePolicySelfTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

    root = pathlib.Path(__file__).resolve().parent.parent
    errs = check_repo(root)
    if errs:
        for e in errs:
            print(f"release policy error: {e}", file=sys.stderr)
        return 1
    print("check_release_policy.py: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
