#!/usr/bin/env python3
"""Verify THIRD_PARTY_NOTICES.md, upstream source headers, Rust dependency notices, and release license packaging."""

from __future__ import annotations

import argparse, hashlib, json, os, re, subprocess, sys, tempfile, tomllib, unittest
from contextlib import contextmanager
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

PORTED_FILES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("crates/mare-binding/src/outdecode.rs", ("MIT", "Apache-2.0")),
    ("crates/mare-binding/src/sse/mod.rs", ("MIT", "Apache-2.0")),
    ("crates/mare-binding/src/sse/parser.rs", ("MIT", "Apache-2.0")),
    ("crates/mare-binding/src/sse/parser/scan.rs", ("MIT", "Apache-2.0")),
    ("crates/mare-binding/src/sse/parser/data.rs", ("MIT", "Apache-2.0")),
    ("crates/mare-binding/src/sse/parser/data_tests.rs", ("MIT", "Apache-2.0")),
    ("crates/mare-binding/src/sse/tests.rs", ("MIT", "Apache-2.0", "eventsource-parser")),
    ("crates/mare-shared/src/redact.rs", ("MIT", "Apache-2.0")),
)

PORTED_MARKER_RE = re.compile(r"\b(?:ported from|translated from|adapted from)\b", re.IGNORECASE)

SYNTAX_ASSETS_DIR = "crates/mare-tui/assets/syntaxes"
SYNTAX_PACKDUMP = "crates/mare-tui/assets/syntaxes.packdump"
EMBEDDED_THEMES: tuple[tuple[str, str], ...] = (
    ("crates/mare-tui/assets/theme.themedump", "base16-ocean.dark"),
    ("crates/mare-tui/assets/theme-light.themedump", "InspiredGitHub"),
)

CODEX_FILES: tuple[str, ...] = (
    "crates/mare-tui/src/reflow.rs",
    "crates/mare-tui/src/stream.rs",
    "crates/mare-tui/src/winprobe.rs",
    "crates/mare-tui/src/paste.rs",
    "crates/mare-shared/src/style/wrap.rs",
    "crates/mare-binding/src/winsandbox/default_win.rs",
    "crates/mare-binding/src/winsandbox/desktop_win.rs",
    "crates/mare-binding/src/winsandbox/enhanced_win.rs",
    "crates/mare-binding/src/winsandbox/enhanced_ipc_win.rs",
    "crates/mare-binding/src/winsandbox/ipc_io_win.rs",
    "crates/mare-binding/src/winsandbox/runner_ipc.rs",
    "crates/mare-cli/src/headless_output.rs",
)

REQUIRED_NOTICE_MARKERS: tuple[str, ...] = (
    "Copyright (c) 2024 4t145",
    "eventsource-parser",
    "Copyright (c) 2026 Espen Hovlandsdal",
    "OpenAI Codex",
    "Copyright 2025 OpenAI",
    "Ratatui",
    "Permission to copy, use, modify, sell and distribute this software is granted.",
    "Permission is hereby granted, free of charge",
    "Apache License",
    "Version 2.0, January 2004",
    "1. Definitions.",
    "9. Accepting Warranty or Additional Liability.",
    "END OF TERMS AND CONDITIONS",
)

CORDIS_LICENSE = "crates/mare-kernel/lua/cordis/LICENSE"
# Generated at release time from Cargo.lock by the pinned cargo-about; shipped in every channel.
RUST_NOTICES = "licenses/RUST-THIRD-PARTY-NOTICES.txt"
ABOUT_CONFIG = "about.toml"
CI_TOOLS = ".github/ci-tools.json"


def check_repo(root: Path) -> list[str]:
    errors: list[str] = []
    notices_path = root / "THIRD_PARTY_NOTICES.md"
    if not notices_path.is_file():
        return ["missing THIRD_PARTY_NOTICES.md"]
    notices = notices_path.read_text(encoding="utf-8")

    for rel, header_tokens in PORTED_FILES:
        src = root / rel
        if not src.is_file():
            errors.append(f"missing ported source file: {rel}")
        else:
            head = "\n".join(src.read_text(encoding="utf-8").splitlines()[:25])
            for tok in header_tokens:
                if tok not in head:
                    errors.append(f"{rel} header missing license token: {tok}")
        if rel not in notices:
            errors.append(f"THIRD_PARTY_NOTICES.md missing ported file: {rel}")

    for rel in CODEX_FILES:
        src = root / rel
        if not src.is_file():
            errors.append(f"missing Codex-derived source file: {rel}")
        else:
            text = src.read_text(encoding="utf-8")
            if "Copyright 2025 OpenAI" not in text:
                errors.append(f"{rel} missing 'Copyright 2025 OpenAI' notice")
        if rel not in notices:
            errors.append(f"THIRD_PARTY_NOTICES.md missing Codex-derived file: {rel}")

    crates_dir = root / "crates"
    if crates_dir.is_dir():
        for rs in sorted(crates_dir.rglob("*.rs")):
            rel = rs.relative_to(root).as_posix()
            text = rs.read_text(encoding="utf-8", errors="replace")
            if "Copyright 2025 OpenAI" in text and rel not in notices:
                errors.append(f"THIRD_PARTY_NOTICES.md missing OpenAI-copyright file: {rel}")
            parts = rs.relative_to(crates_dir).parts
            if len(parts) >= 2 and parts[1] == "src" and PORTED_MARKER_RE.search(text) and rel not in notices:
                errors.append(f"THIRD_PARTY_NOTICES.md missing ported/adapted source file: {rel}")

    syntax_dir = root / SYNTAX_ASSETS_DIR
    if not syntax_dir.is_dir():
        errors.append(f"missing syntax assets directory: {SYNTAX_ASSETS_DIR}")
    else:
        syntax_files = sorted(syntax_dir.glob("*.sublime-syntax"))
        if not syntax_files:
            errors.append(f"no .sublime-syntax files found in {SYNTAX_ASSETS_DIR}")
        for sf in syntax_files:
            rel = sf.relative_to(root).as_posix()
            if rel not in notices:
                errors.append(f"THIRD_PARTY_NOTICES.md missing embedded syntax file: {rel}")

    if not (root / SYNTAX_PACKDUMP).is_file():
        errors.append(f"missing compiled syntax packdump: {SYNTAX_PACKDUMP}")
    elif SYNTAX_PACKDUMP not in notices:
        errors.append(f"THIRD_PARTY_NOTICES.md missing syntax packdump: {SYNTAX_PACKDUMP}")
    for asset_rel, theme_name in EMBEDDED_THEMES:
        if not (root / asset_rel).is_file():
            errors.append(f"missing embedded theme asset: {asset_rel}")
        for needle, what in ((asset_rel, "asset"), (theme_name, "name")):
            if needle not in notices:
                errors.append(f"THIRD_PARTY_NOTICES.md missing embedded theme {what}: {needle}")
    errors.extend(f"THIRD_PARTY_NOTICES.md missing required license/notice text: {m}" for m in REQUIRED_NOTICE_MARKERS if m not in notices)
    cordis_lic = root / CORDIS_LICENSE
    if not cordis_lic.is_file() or not cordis_lic.read_text(encoding="utf-8").strip():
        errors.append(f"missing or empty {CORDIS_LICENSE}")
    release_yaml = root / ".github/workflows/release.yaml"
    if not release_yaml.is_file():
        errors.append("missing .github/workflows/release.yaml")
    else:
        rel_text = release_yaml.read_text(encoding="utf-8")
        for needle, what in (
            ("THIRD_PARTY_NOTICES.md", "package THIRD_PARTY_NOTICES.md"),
            ("licenses/cordis-LICENSE", "package licenses/cordis-LICENSE"),
            ("check_notices.py --generate-rust-notices", f"generate Rust dependency notices ('check_notices.py --generate-rust-notices')"),
            ("check_notices.py --rust-notices", f"verify Rust dependency notices are fresh ('check_notices.py --rust-notices')"),
            ("./.github/actions/setup-cargo-about", f"install the pinned cargo-about ('./.github/actions/setup-cargo-about')"),
        ):
            if needle not in rel_text:
                errors.append(f".github/workflows/release.yaml does not {what}")
        if rel_text.count(RUST_NOTICES) < 6:
            errors.append(f".github/workflows/release.yaml does not package {RUST_NOTICES} into every archive, package, and image")
    dry_run = root / ".github/workflows/release-dry-run.yaml"
    if dry_run.is_file():
        dry_text = dry_run.read_text(encoding="utf-8")
        errors.extend(f".github/workflows/release-dry-run.yaml missing {n!r}" for n in ("check_notices.py --generate-rust-notices", "check_notices.py --rust-notices", RUST_NOTICES) if n not in dry_text)
    manifests = root / "scripts/release_manifests.py"
    if manifests.is_file() and manifests.read_text(encoding="utf-8").count(RUST_NOTICES) < 2:
        errors.append(f"scripts/release_manifests.py does not install {RUST_NOTICES} in the AUR and NUR packages")
    errors.extend(check_about_config(root))
    dockerfile = root / "Dockerfile.release"
    if not dockerfile.is_file():
        errors.append("missing Dockerfile.release")
    else:
        df_text = dockerfile.read_text(encoding="utf-8")
        errors.extend(f"Dockerfile.release does not copy {n}" for n in ("THIRD_PARTY_NOTICES.md", "cordis-LICENSE", RUST_NOTICES) if n not in df_text)
    return errors


NOTICE_FILE_RE = re.compile(r"^(?:notice|copyright)(?:[._-].*)?$", re.IGNORECASE)
STAMP_TOOL_RE = re.compile(r"(?m)^Generated by: cargo-about (\S+) \(about\.toml sha256 ([0-9a-f]{64})\)$")
STAMP_LOCK_RE = re.compile(r"(?m)^Cargo\.lock sha256: ([0-9a-f]{64})$")
RULE = "=" * 80


def _third_party(pkg: dict) -> bool:
    return pkg.get("source") is not None  # workspace members use mare's own LICENSE


def _sha256_text(path: Path) -> str:
    return hashlib.sha256(path.read_bytes().replace(b"\r\n", b"\n")).hexdigest()  # LF-normalized for CRLF runners


def rust_notices_stamp(root: Path) -> dict[str, str]:
    tools = json.loads((root / CI_TOOLS).read_text(encoding="utf-8"))
    return {"cargo_about": str(tools.get("cargo_about", "")), "about_toml": _sha256_text(root / ABOUT_CONFIG), "cargo_lock": _sha256_text(root / "Cargo.lock")}


def render_rust_notices(about: dict, crate_notices: list[tuple[str, str, str, str]], stamp: dict[str, str]) -> str:
    """Render cargo-about JSON into the shipped notices file. Output is deterministic and path-free."""
    sections: list[tuple[str, str, list[str], str]] = []
    for lic in about.get("licenses", []):
        users = sorted(
            {f"{u['crate']['name']} {u['crate']['version']}" for u in lic.get("used_by", []) if _third_party(u["crate"])}
        )
        if users:
            sections.append((str(lic["id"]), str(lic["name"]), users, str(lic["text"]).rstrip()))
    sections.sort(key=lambda s: (s[0], s[2], s[3]))
    crates = sorted({f"{c['package']['name']} {c['package']['version']}" for c in about.get("crates", []) if _third_party(c["package"])})

    out = [
        "mare Rust third-party notices",
        "=============================",
        "",
        "The mare binaries are built from the Rust crates listed below. Each section",
        "gives one license text and the crates distributed under it; the NOTICE and",
        "COPYRIGHT files those crates ship follow at the end.",
        "",
        f"Generated by: cargo-about {stamp['cargo_about']} (about.toml sha256 {stamp['about_toml']})",
        f"Cargo.lock sha256: {stamp['cargo_lock']}",
        f"Crates: {len(crates)}",
        "",
    ]
    for lic_id, name, users, text in sections:
        out += [RULE, f"License: {name} ({lic_id})", "Used by:", *(f"  - {u}" for u in users), "", text, ""]
    for crate, version, fname, text in sorted(crate_notices):
        out += [RULE, f"{fname} of {crate} {version}", "", text.rstrip(), ""]
    return "\n".join(out)


def _crate_notice_files(about: dict) -> list[tuple[str, str, str, str]]:
    found: list[tuple[str, str, str, str]] = []
    for entry in about.get("crates", []):
        pkg = entry["package"]
        if not _third_party(pkg):
            continue
        crate_dir = Path(pkg["manifest_path"]).parent
        for f in sorted(crate_dir.iterdir()):
            if f.is_file() and NOTICE_FILE_RE.match(f.name):
                found.append((pkg["name"], pkg["version"], f.name, f.read_text(encoding="utf-8", errors="replace")))
    return found


def generate_rust_notices(root: Path, dest: Path) -> int:
    """Run the pinned cargo-about over Cargo.lock and write the notices file."""
    stamp = rust_notices_stamp(root)
    # Not $CARGO: build wrappers set it for their own children.
    cargo = os.environ.get("MARE_NOTICES_CARGO", "cargo")
    ver = subprocess.run([cargo, "about", "--version"], cwd=root, capture_output=True, text=True, check=False)
    if ver.returncode != 0 or ver.stdout.split()[-1:] != [stamp["cargo_about"]]:
        print(f"notices: need cargo-about {stamp['cargo_about']} (from {CI_TOOLS}), got {ver.stdout.strip() or ver.stderr.strip()!r}", file=sys.stderr)
        return 1
    res = subprocess.run(
        [cargo, "about", "generate", "--workspace", "--all-features", "--locked", "--fail",
         "--config", ABOUT_CONFIG, "--format", "json"],
        cwd=root, capture_output=True, text=True, check=False,
    )
    if res.returncode != 0:
        print(res.stderr, file=sys.stderr)
        return 1
    about = json.loads(res.stdout)
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(render_rust_notices(about, _crate_notice_files(about), stamp), encoding="utf-8", newline="\n")
    print(f"notices: wrote {dest} ({sum(1 for c in about['crates'] if _third_party(c['package']))} crates)")
    return 0


def verify_rust_notices(root: Path, path: Path) -> list[str]:
    """The notices file exists, lists licenses, and was generated from this Cargo.lock and config."""
    if not path.is_file():
        return [f"missing Rust dependency notices: {path}"]
    text = path.read_text(encoding="utf-8")
    want = rust_notices_stamp(root)
    errors: list[str] = []
    tool, lock = STAMP_TOOL_RE.search(text), STAMP_LOCK_RE.search(text)
    if not tool or not lock:
        return [f"{path}: missing generation stamp; regenerate with --generate-rust-notices"]
    if tool.group(1) != want["cargo_about"]:
        errors.append(f"{path}: generated by cargo-about {tool.group(1)}, {CI_TOOLS} pins {want['cargo_about']}")
    if tool.group(2) != want["about_toml"]:
        errors.append(f"{path}: stale, {ABOUT_CONFIG} changed since generation")
    if lock.group(1) != want["cargo_lock"]:
        errors.append(f"{path}: stale, Cargo.lock changed since generation")
    if "\nLicense: " not in text:
        errors.append(f"{path}: lists no license texts")
    return errors


def check_about_config(root: Path) -> list[str]:
    """cargo-about accepts exactly the licenses cargo-deny allows, at a pinned cargo-about version."""
    errors: list[str] = []
    about_path, deny_path, tools_path = root / ABOUT_CONFIG, root / "deny.toml", root / CI_TOOLS
    if not about_path.is_file():
        return [f"missing {ABOUT_CONFIG}"]
    about = tomllib.loads(about_path.read_text(encoding="utf-8"))
    if about.get("ignore-dev-dependencies") is not True:
        errors.append(f"{ABOUT_CONFIG}: ignore-dev-dependencies must be true (dev-only crates are not distributed)")
    if deny_path.is_file():
        allow = tomllib.loads(deny_path.read_text(encoding="utf-8")).get("licenses", {}).get("allow", [])
        if sorted(about.get("accepted", [])) != sorted(allow):
            errors.append(f"{ABOUT_CONFIG}: accepted must equal deny.toml [licenses].allow")
    version = json.loads(tools_path.read_text(encoding="utf-8")).get("cargo_about", "") if tools_path.is_file() else ""
    if not (isinstance(version, str) and version.count(".") == 2 and version.replace(".", "").isdigit()):
        errors.append(f"{CI_TOOLS}: cargo_about must pin an exact X.Y.Z version, got {version!r}")
    return errors


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _make_valid_repo(root: Path) -> None:
    for rel, tokens in PORTED_FILES:
        _write(root / rel, f"//! {' '.join(tokens)}\n")
    for rel in CODEX_FILES:
        _write(root / rel, "//! Copyright 2025 OpenAI (Apache-2.0).\n")
    _write(root / CORDIS_LICENSE, "MIT License\n")
    _write(
        root / ".github/workflows/release.yaml",
        "cp LICENSE THIRD_PARTY_NOTICES.md\ncp x licenses/cordis-LICENSE\nuses: ./.github/actions/setup-cargo-about\n"
        f"python3 scripts/check_notices.py --generate-rust-notices x\npython3 scripts/check_notices.py --rust-notices x\n{RUST_NOTICES}\n" * 1
        + f"{RUST_NOTICES}\n" * 5,
    )
    _write(
        root / ".github/workflows/release-dry-run.yaml",
        f"check_notices.py --generate-rust-notices {RUST_NOTICES}\ncheck_notices.py --rust-notices {RUST_NOTICES}\n",
    )
    _write(root / "scripts/release_manifests.py", f"{RUST_NOTICES}\n{RUST_NOTICES}\n")
    _write(root / ABOUT_CONFIG, 'accepted = ["MIT", "Apache-2.0"]\nignore-dev-dependencies = true\n')
    _write(root / "deny.toml", '[licenses]\nallow = ["Apache-2.0", "MIT"]\n')
    _write(root / CI_TOOLS, '{"cargo_about": "0.9.3"}\n')
    _write(root / "Cargo.lock", "version = 4\n")
    _write(
        root / "Dockerfile.release",
        "COPY THIRD_PARTY_NOTICES.md /usr/share/licenses/mare/THIRD_PARTY_NOTICES.md\n"
        f"COPY licenses/cordis-LICENSE /usr/share/licenses/mare/cordis-LICENSE\nCOPY {RUST_NOTICES} /usr/share/licenses/mare/RUST-THIRD-PARTY-NOTICES.txt\n",
    )
    syntax_file = f"{SYNTAX_ASSETS_DIR}/Rust.sublime-syntax"
    _write(root / syntax_file, "%YAML 1.2\n---\nname: Rust\n")
    (root / SYNTAX_PACKDUMP).write_bytes(b"packdump")
    for asset_rel, _ in EMBEDDED_THEMES:
        (root / asset_rel).parent.mkdir(parents=True, exist_ok=True)
        (root / asset_rel).write_bytes(b"themedump")
    lines = [rel for rel, _ in PORTED_FILES] + list(CODEX_FILES) + [syntax_file, SYNTAX_PACKDUMP] + [x for pair in EMBEDDED_THEMES for x in pair] + list(REQUIRED_NOTICE_MARKERS)
    _write(root / "THIRD_PARTY_NOTICES.md", "\n".join(lines) + "\n")


class CheckNoticesTests(unittest.TestCase):
    @contextmanager
    def _repo(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _make_valid_repo(root)
            yield root

    def test_valid_repo_passes(self) -> None:
        with self._repo() as root:
            self.assertEqual(check_repo(root), [])

    def test_missing_notices_fails(self) -> None:
        with self._repo() as root:
            (root / "THIRD_PARTY_NOTICES.md").unlink()
            self.assertTrue(any("missing THIRD_PARTY_NOTICES.md" in e for e in check_repo(root)))

    def test_missing_codex_or_apache_text_fails(self) -> None:
        with self._repo() as root:
            p = root / "THIRD_PARTY_NOTICES.md"
            p.write_text(p.read_text(encoding="utf-8").replace(CODEX_FILES[0], "").replace("END OF TERMS AND CONDITIONS", ""), encoding="utf-8")
            errs = check_repo(root)
            self.assertTrue(any(CODEX_FILES[0] in e for e in errs) and any("END OF TERMS AND CONDITIONS" in e for e in errs))

    def test_unlisted_ported_src_file_fails(self) -> None:
        with self._repo() as root:
            _write(root / "crates/mare-binding/src/sse/extra.rs", "// Ported from upstream 1.0 (MIT).\n")
            errs = check_repo(root)
            self.assertTrue(any("crates/mare-binding/src/sse/extra.rs" in e for e in errs), errs)

    def test_missing_embedded_syntax_or_theme_fails(self) -> None:
        with self._repo() as root:
            _write(root / SYNTAX_ASSETS_DIR / "YAML.sublime-syntax", "%YAML 1.2\n---\nname: YAML\n")
            p = root / "THIRD_PARTY_NOTICES.md"
            p.write_text(p.read_text(encoding="utf-8").replace("InspiredGitHub", ""), encoding="utf-8")
            errs = check_repo(root)
            self.assertTrue(any("YAML.sublime-syntax" in e for e in errs) and any("InspiredGitHub" in e for e in errs), errs)

    def test_missing_packaging_entries_fail(self) -> None:
        with self._repo() as root:
            _write(root / "Dockerfile.release", "COPY LICENSE /usr/share/licenses/mare/LICENSE\n")
            errs = check_repo(root)
            self.assertTrue(any("Dockerfile.release" in e and "THIRD_PARTY_NOTICES.md" in e for e in errs))
            self.assertTrue(any("Dockerfile.release" in e and "cordis-LICENSE" in e for e in errs))

    def test_missing_rust_notices_packaging_fails(self) -> None:
        with self._repo() as root:
            _write(root / "Dockerfile.release", "COPY THIRD_PARTY_NOTICES.md x\nCOPY licenses/cordis-LICENSE y\n")
            _write(root / "scripts/release_manifests.py", "PKGBUILD only\n")
            rel = root / ".github/workflows/release.yaml"
            rel.write_text(rel.read_text(encoding="utf-8").replace("--generate-rust-notices", "--skip"), encoding="utf-8")
            errs = check_repo(root)
            self.assertTrue(any("Dockerfile.release" in e and RUST_NOTICES in e for e in errs), errs)
            self.assertTrue(any("release_manifests.py" in e for e in errs) and any("release.yaml" in e and "generate" in e for e in errs), errs)

    def test_about_config_must_match_deny_allow_list(self) -> None:
        with self._repo() as root:
            _write(root / ABOUT_CONFIG, 'accepted = ["MIT", "GPL-3.0"]\nignore-dev-dependencies = true\n')
            _write(root / CI_TOOLS, '{"cargo_about": "latest"}\n')
            errs = check_repo(root)
            self.assertTrue(any("deny.toml" in e for e in errs) and any("cargo_about" in e for e in errs), errs)

    @staticmethod
    def _about_json() -> dict:
        pkg = lambda n, v, s: {"name": n, "version": v, "source": s, "manifest_path": f"/abs/{n}/Cargo.toml"}
        reg = "registry+https://github.com/rust-lang/crates.io-index"
        return {
            "overview": [],
            "crates": [{"package": pkg("zeta", "1.0.0", reg)}, {"package": pkg("alpha", "0.2.0", reg)}, {"package": pkg("mare-cli", "1.0.0", None)}],
            "licenses": [
                {"id": "MIT", "name": "MIT License", "text": "Copyright (c) Zeta\nPermission is hereby granted", "source_path": "/abs/zeta/LICENSE",
                 "used_by": [{"crate": pkg("zeta", "1.0.0", reg)}, {"crate": pkg("alpha", "0.2.0", reg)}]},
                {"id": "BSD-3-Clause", "name": 'BSD 3-Clause "New" or "Revised" License', "text": "Redistribution and use", "used_by": [{"crate": pkg("alpha", "0.2.0", reg)}]},
                {"id": "Apache-2.0", "name": "Apache License 2.0", "text": "own license", "used_by": [{"crate": pkg("mare-cli", "1.0.0", None)}]},
            ],
        }

    def test_render_rust_notices_lists_third_party_crates_only(self) -> None:
        stamp = {"cargo_about": "0.9.3", "about_toml": "a" * 64, "cargo_lock": "b" * 64}
        text = render_rust_notices(self._about_json(), [("alpha", "0.2.0", "NOTICE", "Alpha NOTICE text")], stamp)
        self.assertEqual(text, render_rust_notices(self._about_json(), [("alpha", "0.2.0", "NOTICE", "Alpha NOTICE text")], stamp), "output must be deterministic")
        for s in ("cargo-about 0.9.3", f"Cargo.lock sha256: {'b' * 64}", 'License: BSD 3-Clause "New" or "Revised" License (BSD-3-Clause)', "  - alpha 0.2.0\n  - zeta 1.0.0\n", "Alpha NOTICE text"):
            self.assertIn(s, text)
        for s in ("mare-cli", "own license", "/abs/"):
            self.assertNotIn(s, text)
        self.assertLess(text.index("(BSD-3-Clause)"), text.index("(MIT)"))

    def test_verify_rust_notices_detects_stale_or_missing_file(self) -> None:
        with self._repo() as root:
            out = root / RUST_NOTICES
            self.assertTrue(any("missing" in e for e in verify_rust_notices(root, out)))
            _write(out, render_rust_notices(self._about_json(), [], rust_notices_stamp(root)))
            self.assertEqual(verify_rust_notices(root, out), [])
            lock = root / "Cargo.lock"
            lock.write_bytes(lock.read_bytes().replace(b"\n", b"\r\n"))  # CRLF checkouts hash same as LF
            self.assertEqual(verify_rust_notices(root, out), [])
            _write(lock, 'version = 4\n[[package]]\nname = "new"\n')
            self.assertTrue(any("stale" in e and "Cargo.lock" in e for e in verify_rust_notices(root, out)))
            _write(root / CI_TOOLS, '{"cargo_about": "0.9.4"}\n')
            self.assertTrue(any("cargo-about" in e for e in verify_rust_notices(root, out)))

    def test_verify_rust_notices_rejects_empty_license_list(self) -> None:
        with self._repo() as root:
            out = root / RUST_NOTICES
            _write(out, render_rust_notices({"overview": [], "crates": [], "licenses": []}, [], rust_notices_stamp(root)))
            self.assertTrue(any("no license" in e for e in verify_rust_notices(root, out)))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--generate-rust-notices", metavar="OUT", type=Path, help="generate Rust dependency notices with cargo-about")
    parser.add_argument("--rust-notices", metavar="FILE", type=Path, help="verify a generated Rust dependency notices file is fresh")
    args = parser.parse_args(argv)
    if args.generate_rust_notices:
        return generate_rust_notices(ROOT, args.generate_rust_notices.resolve())
    if args.rust_notices:
        errs = verify_rust_notices(ROOT, args.rust_notices.resolve())
        for e in errs:
            print(f"notices: {e}", file=sys.stderr)
        if not errs:
            print(f"notices: {args.rust_notices} is fresh")
        return 1 if errs else 0
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=1).run(unittest.defaultTestLoader.loadTestsFromTestCase(CheckNoticesTests)).wasSuccessful() else 1
    if errs := check_repo(ROOT):
        for e in errs:
            print(f"notices: {e}", file=sys.stderr)
        return 1
    syntax_count = len(list((ROOT / SYNTAX_ASSETS_DIR).glob("*.sublime-syntax")))
    print(f"notices: ok ({len(PORTED_FILES)} ported + {len(CODEX_FILES)} Codex files, {syntax_count} syntaxes + {len(EMBEDDED_THEMES)} embedded themes, license texts, cordis-LICENSE and Rust dependency notices packaging)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
