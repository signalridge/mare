#!/usr/bin/env python3
"""Generate Homebrew, Scoop, AUR, NUR manifests and release outputs. Stdlib-only."""

from __future__ import annotations

import argparse
import base64
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

STABLE_SEMVER_RE = re.compile(r"^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
SLSA_SUFFIXES = (".tar.gz", ".zip", ".deb", ".rpm")
CHANNEL_SECTION_START = "<!-- mare-install-channels:start -->"
CHANNEL_SECTION_END = "<!-- mare-install-channels:end -->"
VALID_CHANNEL_STATUSES = frozenset({"pending", "published", "skipped", "failed"})
CHANNEL_ORDER = ("container", "homebrew", "scoop", "aur", "nur")


def render_release_notes_install_section(
    *,
    tag: str,
    version: str,
    channels: dict[str, str],
) -> str:
    """Render the marker-bounded installation & channel availability section for a GitHub Release."""
    for name in CHANNEL_ORDER:
        status = channels.get(name, "pending")
        if status not in VALID_CHANNEL_STATUSES:
            raise ValueError(f"invalid status {status!r} for channel {name!r}")

    def channel_line(key: str, label: str, published_text: str) -> str:
        status = channels.get(key, "pending")
        if status == "published":
            return f"- {label} (available): {published_text}"
        if status == "pending":
            return (
                f"- {label} (pending): publishing in progress; use the direct archives "
                "or quick install above until channel sync finishes."
            )
        if status == "skipped":
            return f"- {label} (not published): not published for this release."
        return (
            f"- {label} (sync failed): automated publication failed for this release; "
            "use the direct archives or quick install above."
        )

    lines = [
        CHANNEL_SECTION_START,
        "Installation: https://signalridge.github.io/mare/en/get-started/installation/",
        "",
        "### Available now",
        "- Quick install (macOS / Linux): `curl -fsSL https://signalridge.github.io/mare/install.sh | sh`",
        "- Quick install (Windows): `irm https://signalridge.github.io/mare/install.ps1 | iex`",
        f"- Nix Flake: `nix run github:signalridge/mare/{tag}`",
        "- Linux packages (`.deb` / `.rpm`), direct archives, SPDX SBOMs, "
        "Cosign bundle (`checksums.txt.sigstore.json`), and SLSA provenance are attached to this release.",
        "",
        "### Package managers and container image",
        channel_line(
            "container",
            "Container (GHCR)",
            f"`docker pull ghcr.io/signalridge/mare:{version}`",
        ),
        channel_line(
            "homebrew",
            "Homebrew (macOS / Linux)",
            "`brew tap signalridge/tap && brew trust signalridge/tap && brew install signalridge/tap/mare` "
            "(Upgrading from the earlier cask: `brew uninstall --cask mare && brew install signalridge/tap/mare`)",
        ),
        channel_line(
            "scoop",
            "Scoop (Windows)",
            "`scoop bucket add signalridge https://github.com/signalridge/scoop-bucket` "
            "then `scoop install signalridge/mare`",
        ),
        channel_line(
            "nur",
            "Nix (NUR)",
            "`nix profile install github:signalridge/nur-packages#mare`",
        ),
        channel_line(
            "aur",
            "Arch Linux (AUR)",
            "`yay -S mare-bin`",
        ),
        CHANNEL_SECTION_END,
    ]
    return "\n".join(lines) + "\n"


def upsert_release_notes_section(existing_body: str, section: str) -> str:
    """Replace the marker-bounded channel section in-place, preserving surrounding release notes."""
    block = section.strip()
    start_idx = existing_body.find(CHANNEL_SECTION_START)
    end_idx = existing_body.find(CHANNEL_SECTION_END)
    if start_idx != -1 and end_idx != -1 and end_idx >= start_idx:
        before = existing_body[:start_idx].rstrip()
        after = existing_body[end_idx + len(CHANNEL_SECTION_END) :].lstrip()
        parts = [p for p in (before, block, after) if p]
        return "\n\n".join(parts) + "\n"
    trimmed = existing_body.strip()
    if not trimmed:
        return block + "\n"
    return trimmed + "\n\n" + block + "\n"


def parse_checksums(checksums_text: str) -> dict[str, str]:
    sums: dict[str, str] = {}
    for line in checksums_text.splitlines():
        line = line.strip()
        if not line:
            continue
        digest, name = line.split(None, 1)
        sums[name.lstrip("*")] = digest
    return sums


def parse_stable_semver(tag_or_version: str) -> tuple[int, int, int] | None:
    m = STABLE_SEMVER_RE.match(tag_or_version.strip())
    if not m:
        return None
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def is_highest_stable_version(version: str, existing_tags: list[str]) -> bool:
    current = parse_stable_semver(version)
    if current is None:
        return False
    for tag in existing_tags:
        other = parse_stable_semver(tag)
        if other is not None and other > current:
            return False
    return True


def list_git_tags(repo_dir: pathlib.Path) -> list[str]:
    proc = subprocess.run(
        ["git", "tag", "-l", "v*"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        return []
    return [line.strip() for line in proc.stdout.splitlines() if line.strip()]


def generate_manifests(
    *,
    tag: str,
    version: str,
    dist_dir: pathlib.Path,
    manifests_dir: pathlib.Path,
    registry: str = "ghcr.io",
    image_name: str = "signalridge/mare",
    existing_tags: list[str] | None = None,
) -> dict[str, str]:
    sums = parse_checksums((dist_dir / "checksums.txt").read_text(encoding="utf-8"))

    def need_sum(filename: str) -> str:
        if filename not in sums:
            raise ValueError(f"missing {filename} in checksums.txt")
        return sums[filename]

    darwin_amd64 = f"mare-{tag}-x86_64-apple-darwin.tar.gz"
    darwin_arm64 = f"mare-{tag}-aarch64-apple-darwin.tar.gz"
    linux_amd64 = f"mare-{tag}-x86_64-unknown-linux-musl.tar.gz"
    linux_arm64 = f"mare-{tag}-aarch64-unknown-linux-musl.tar.gz"
    win_amd64 = f"mare-{tag}-x86_64-pc-windows-msvc.zip"
    win_arm64 = f"mare-{tag}-aarch64-pc-windows-msvc.zip"

    (manifests_dir / "Formula").mkdir(parents=True, exist_ok=True)
    formula = f"""# typed: false
# frozen_string_literal: true

# Auto-generated by mare release workflow. DO NOT EDIT.
class Mare < Formula
  desc "Hyperextensible agent harness (Rust core, Lua plugins)"
  homepage "https://github.com/signalridge/mare"
  version "{version}"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/signalridge/mare/releases/download/{tag}/{darwin_amd64}"
      sha256 "{need_sum(darwin_amd64)}"
    end
    if Hardware::CPU.arm?
      url "https://github.com/signalridge/mare/releases/download/{tag}/{darwin_arm64}"
      sha256 "{need_sum(darwin_arm64)}"
    end
  end

  on_linux do
    if Hardware::CPU.intel? && Hardware::CPU.is_64_bit?
      url "https://github.com/signalridge/mare/releases/download/{tag}/{linux_amd64}"
      sha256 "{need_sum(linux_amd64)}"
    end
    if Hardware::CPU.arm? && Hardware::CPU.is_64_bit?
      url "https://github.com/signalridge/mare/releases/download/{tag}/{linux_arm64}"
      sha256 "{need_sum(linux_arm64)}"
    end
  end

  def install
    bin.install "mare"
    doc.install "THIRD_PARTY_NOTICES.md", "licenses"
  end

  test do
    system "#{{bin}}/mare", "--version"
  end
end
"""
    (manifests_dir / "Formula" / "mare.rb").write_text(formula, encoding="utf-8")

    scoop = {
        "version": version,
        "description": "Hyperextensible agent harness (Rust core, Lua plugins)",
        "homepage": "https://github.com/signalridge/mare",
        "license": "Apache-2.0",
        "architecture": {
            "64bit": {
                "url": f"https://github.com/signalridge/mare/releases/download/{tag}/{win_amd64}",
                "hash": need_sum(win_amd64),
                "extract_dir": win_amd64[:-4],
                "bin": ["mare.exe"],
            },
            "arm64": {
                "url": f"https://github.com/signalridge/mare/releases/download/{tag}/{win_arm64}",
                "hash": need_sum(win_arm64),
                "extract_dir": win_arm64[:-4],
                "bin": ["mare.exe"],
            },
        },
        "checkver": "github",
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": "https://github.com/signalridge/mare/releases/download/v$version/mare-v$version-x86_64-pc-windows-msvc.zip",
                    "extract_dir": "mare-v$version-x86_64-pc-windows-msvc",
                },
                "arm64": {
                    "url": "https://github.com/signalridge/mare/releases/download/v$version/mare-v$version-aarch64-pc-windows-msvc.zip",
                    "extract_dir": "mare-v$version-aarch64-pc-windows-msvc",
                },
            }
        },
    }
    (manifests_dir / "mare.json").write_text(json.dumps(scoop, indent=2) + "\n", encoding="utf-8")

    (manifests_dir / "aur").mkdir(parents=True, exist_ok=True)
    aur_ver = version.replace("-", "_")
    pkgbuild = f"""# Maintainer: SignalRidge <support@signalridge.com>
pkgname=mare-bin
pkgver={aur_ver}
pkgrel=1
pkgdesc="Hyperextensible AI coding agent harness (Rust core, Lua 5.5 plugins)"
arch=('x86_64' 'aarch64')
url="https://github.com/signalridge/mare"
license=('Apache-2.0')
optdepends=('bubblewrap: Linux sandbox isolation')
provides=('mare')
conflicts=('mare')
source_x86_64=("mare-{aur_ver}-x86_64.tar.gz::https://github.com/signalridge/mare/releases/download/{tag}/{linux_amd64}")
source_aarch64=("mare-{aur_ver}-aarch64.tar.gz::https://github.com/signalridge/mare/releases/download/{tag}/{linux_arm64}")
sha256sums_x86_64=('{need_sum(linux_amd64)}')
sha256sums_aarch64=('{need_sum(linux_arm64)}')

package() {{
  local target_arch="$CARCH"
  local dir="$srcdir/mare-{tag}-$target_arch-unknown-linux-musl"
  install -Dm755 "$dir/mare" "$pkgdir/usr/bin/mare"
  install -Dm644 "$dir/LICENSE" "$pkgdir/usr/share/licenses/mare-bin/LICENSE"
  install -Dm644 "$dir/THIRD_PARTY_NOTICES.md" "$pkgdir/usr/share/licenses/mare-bin/THIRD_PARTY_NOTICES.md"
  install -Dm644 "$dir/licenses/cordis-LICENSE" "$pkgdir/usr/share/licenses/mare-bin/cordis-LICENSE"
  install -Dm644 "$dir/licenses/RUST-THIRD-PARTY-NOTICES.txt" "$pkgdir/usr/share/licenses/mare-bin/RUST-THIRD-PARTY-NOTICES.txt"
  install -Dm644 "$dir/README.md" "$pkgdir/usr/share/doc/mare-bin/README.md"
}}
"""
    (manifests_dir / "aur" / "PKGBUILD").write_text(pkgbuild, encoding="utf-8")

    srcinfo = f"""pkgbase = mare-bin
\tpkgdesc = Hyperextensible AI coding agent harness (Rust core, Lua 5.5 plugins)
\tpkgver = {aur_ver}
\tpkgrel = 1
\turl = https://github.com/signalridge/mare
\tarch = x86_64
\tarch = aarch64
\tlicense = Apache-2.0
\toptdepends = bubblewrap: Linux sandbox isolation
\tprovides = mare
\tconflicts = mare
\tsource_x86_64 = mare-{aur_ver}-x86_64.tar.gz::https://github.com/signalridge/mare/releases/download/{tag}/{linux_amd64}
\tsha256sums_x86_64 = {need_sum(linux_amd64)}
\tsource_aarch64 = mare-{aur_ver}-aarch64.tar.gz::https://github.com/signalridge/mare/releases/download/{tag}/{linux_arm64}
\tsha256sums_aarch64 = {need_sum(linux_arm64)}

pkgname = mare-bin
"""
    (manifests_dir / "aur" / ".SRCINFO").write_text(srcinfo, encoding="utf-8")

    (manifests_dir / "nur").mkdir(parents=True, exist_ok=True)
    nix_sys_expr = "${stdenv.hostPlatform.system}"
    nur_nix = f"""{{
  lib,
  stdenv,
  fetchurl,
  git,
  makeBinaryWrapper,
}}:

let
  version = "{version}";
  sources = {{
    x86_64-linux = {{
      url = "https://github.com/signalridge/mare/releases/download/{tag}/{linux_amd64}";
      sha256 = "{need_sum(linux_amd64)}";
      dir = "{linux_amd64[:-7]}";
    }};
    aarch64-linux = {{
      url = "https://github.com/signalridge/mare/releases/download/{tag}/{linux_arm64}";
      sha256 = "{need_sum(linux_arm64)}";
      dir = "{linux_arm64[:-7]}";
    }};
    x86_64-darwin = {{
      url = "https://github.com/signalridge/mare/releases/download/{tag}/{darwin_amd64}";
      sha256 = "{need_sum(darwin_amd64)}";
      dir = "{darwin_amd64[:-7]}";
    }};
    aarch64-darwin = {{
      url = "https://github.com/signalridge/mare/releases/download/{tag}/{darwin_arm64}";
      sha256 = "{need_sum(darwin_arm64)}";
      dir = "{darwin_arm64[:-7]}";
    }};
  }};
  srcInfo = sources.{nix_sys_expr} or (throw "Unsupported system: {nix_sys_expr}");
in
stdenv.mkDerivation {{
  pname = "mare";
  inherit version;

  src = fetchurl {{
    inherit (srcInfo) url sha256;
  }};

  sourceRoot = srcInfo.dir;
  dontBuild = true;
  dontConfigure = true;

  nativeBuildInputs = [ makeBinaryWrapper ];

  installPhase = ''
    runHook preInstall
    install -Dm755 mare "$out/bin/mare"
    wrapProgram "$out/bin/mare" \\
      --suffix PATH : ${{lib.makeBinPath [ git ]}}
    install -Dm644 LICENSE "$out/share/licenses/mare/LICENSE"
    install -Dm644 THIRD_PARTY_NOTICES.md "$out/share/licenses/mare/THIRD_PARTY_NOTICES.md"
    install -Dm644 licenses/cordis-LICENSE "$out/share/licenses/mare/cordis-LICENSE"
    install -Dm644 licenses/RUST-THIRD-PARTY-NOTICES.txt "$out/share/licenses/mare/RUST-THIRD-PARTY-NOTICES.txt"
    install -Dm644 README.md "$out/share/doc/mare/README.md"
    mkdir -p "$out/share/mare"
    cp -r types "$out/share/mare/types"
    runHook postInstall
  '';

  meta = with lib; {{
    description = "Hyperextensible AI coding agent harness (Rust core, Lua 5.5 plugins)";
    homepage = "https://github.com/signalridge/mare";
    license = licenses.asl20;
    maintainers = [ ];
    platforms = builtins.attrNames sources;
    mainProgram = "mare";
  }};
}}
"""
    (manifests_dir / "nur" / "default.nix").write_text(nur_nix, encoding="utf-8")

    subject_lines = [
        f"{sums[name]}  {name}" for name in sorted(sums) if name.endswith(SLSA_SUFFIXES)
    ]
    hashes_b64 = base64.b64encode(("\n".join(subject_lines) + "\n").encode("utf-8")).decode("ascii")

    binary_matrix = [
        {"os": "ubuntu-latest", "asset": linux_amd64, "command": f"./{linux_amd64[:-7]}/mare"},
        {"os": "macos-latest", "asset": darwin_arm64, "command": f"./{darwin_arm64[:-7]}/mare"},
        {"os": "windows-latest", "asset": win_amd64, "command": f"./{win_amd64[:-4]}/mare.exe"},
    ]

    tags = existing_tags if existing_tags is not None else []
    is_prerelease = "-" in version
    latest_ok = is_highest_stable_version(version, tags)
    image_base = f"{registry}/{image_name}"
    docker_tag_lines = [f"{image_base}:{version}"]
    if latest_ok:
        docker_tag_lines.append(f"{image_base}:latest")

    return {
        "hashes": hashes_b64,
        "deb_asset": f"mare_{version}_linux_amd64.deb",
        "rpm_asset": f"mare_{version}_linux_amd64.rpm",
        "binary_matrix": json.dumps(binary_matrix),
        "is_prerelease": "true" if is_prerelease else "false",
        "make_latest": "true" if latest_ok else "false",
        "docker_tags": "\n".join(docker_tag_lines),
    }


def write_github_outputs(path: pathlib.Path, outputs: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8") as fh:
        for key, value in outputs.items():
            if "\n" in value:
                delim = f"EOF_{key.upper()}"
                fh.write(f"{key}<<{delim}\n{value}\n{delim}\n")
            else:
                fh.write(f"{key}={value}\n")


class ReleaseManifestsSelfTests(unittest.TestCase):
    def _sample_checksums(self, tag: str, ver: str) -> str:
        files = [
            f"mare-{tag}-x86_64-apple-darwin.tar.gz",
            f"mare-{tag}-aarch64-apple-darwin.tar.gz",
            f"mare-{tag}-x86_64-unknown-linux-musl.tar.gz",
            f"mare-{tag}-aarch64-unknown-linux-musl.tar.gz",
            f"mare-{tag}-x86_64-pc-windows-msvc.zip",
            f"mare-{tag}-aarch64-pc-windows-msvc.zip",
            f"mare_{ver}_linux_amd64.deb",
            f"mare_{ver}_linux_arm64.deb",
            f"mare_{ver}_linux_amd64.rpm",
            f"mare_{ver}_linux_arm64.rpm",
        ]
        return "\n".join(f"{i:064x}  {name}" for i, name in enumerate(files, 1)) + "\n"

    def test_generates_all_manifests_and_includes_deb_rpm_in_slsa_subjects(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            dist = root / "dist"
            manifests = root / "manifests"
            dist.mkdir()
            (dist / "checksums.txt").write_text(self._sample_checksums("v1.2.3", "1.2.3"), encoding="utf-8")

            outputs = generate_manifests(
                tag="v1.2.3",
                version="1.2.3",
                dist_dir=dist,
                manifests_dir=manifests,
                existing_tags=["v1.0.0", "v1.2.2", "v1.2.3"],
            )
            formula_path = manifests / "Formula" / "mare.rb"
            self.assertTrue(formula_path.is_file())
            self.assertFalse((manifests / "Casks").exists())
            formula_text = formula_path.read_text(encoding="utf-8")
            self.assertIn("class Mare < Formula", formula_text)
            self.assertIn('version "1.2.3"', formula_text)
            self.assertIn('license "Apache-2.0"', formula_text)
            self.assertIn("on_macos do", formula_text)
            self.assertIn("on_linux do", formula_text)
            self.assertIn("if Hardware::CPU.intel?", formula_text)
            self.assertIn("if Hardware::CPU.arm?", formula_text)
            self.assertIn("mare-v1.2.3-aarch64-apple-darwin.tar.gz", formula_text)
            self.assertIn("mare-v1.2.3-x86_64-apple-darwin.tar.gz", formula_text)
            self.assertIn("mare-v1.2.3-x86_64-unknown-linux-musl.tar.gz", formula_text)
            self.assertIn("mare-v1.2.3-aarch64-unknown-linux-musl.tar.gz", formula_text)
            self.assertNotIn("unknown-linux-gnu", formula_text)
            self.assertIn('bin.install "mare"', formula_text)
            self.assertIn('doc.install "THIRD_PARTY_NOTICES.md", "licenses"', formula_text)
            self.assertIn('system "#{bin}/mare", "--version"', formula_text)
            self.assertNotIn("postflight", formula_text)
            self.assertNotIn("xattr", formula_text)
            self.assertNotIn('cask "mare"', formula_text)
            self.assertTrue((manifests / "mare.json").is_file())
            self.assertTrue((manifests / "aur" / "PKGBUILD").is_file())
            self.assertTrue((manifests / "aur" / ".SRCINFO").is_file())
            self.assertTrue((manifests / "nur" / "default.nix").is_file())
            pkgbuild_text = (manifests / "aur" / "PKGBUILD").read_text(encoding="utf-8")
            self.assertIn("THIRD_PARTY_NOTICES.md", pkgbuild_text)
            self.assertIn("licenses/cordis-LICENSE", pkgbuild_text)
            self.assertIn("licenses/RUST-THIRD-PARTY-NOTICES.txt", pkgbuild_text)
            self.assertIn("unknown-linux-musl", pkgbuild_text)
            self.assertNotIn("unknown-linux-gnu", pkgbuild_text)
            self.assertNotIn("gcc-libs", pkgbuild_text)
            srcinfo_text = (manifests / "aur" / ".SRCINFO").read_text(encoding="utf-8")
            self.assertIn("unknown-linux-musl", srcinfo_text)
            self.assertNotIn("unknown-linux-gnu", srcinfo_text)
            self.assertNotIn("gcc-libs", srcinfo_text)
            nur_text = (manifests / "nur" / "default.nix").read_text(encoding="utf-8")
            self.assertIn("THIRD_PARTY_NOTICES.md", nur_text)
            self.assertIn("licenses/cordis-LICENSE", nur_text)
            self.assertIn("licenses/RUST-THIRD-PARTY-NOTICES.txt", nur_text)
            self.assertIn("unknown-linux-musl", nur_text)
            self.assertNotIn("unknown-linux-gnu", nur_text)
            self.assertNotIn("autoPatchelfHook", nur_text)
            self.assertIn("makeBinaryWrapper", nur_text)
            self.assertIn("--suffix PATH : ${lib.makeBinPath [ git ]}", nur_text)

            decoded = base64.b64decode(outputs["hashes"]).decode("utf-8")
            self.assertIn("mare_1.2.3_linux_amd64.deb", decoded)
            self.assertIn("mare_1.2.3_linux_arm64.deb", decoded)
            self.assertIn("mare_1.2.3_linux_amd64.rpm", decoded)
            self.assertIn("mare_1.2.3_linux_arm64.rpm", decoded)
            self.assertIn("mare-v1.2.3-x86_64-unknown-linux-musl.tar.gz", decoded)
            self.assertNotIn("unknown-linux-gnu", decoded)
            self.assertIn("x86_64-unknown-linux-musl", outputs["binary_matrix"])
            self.assertEqual(outputs["is_prerelease"], "false")
            self.assertEqual(outputs["make_latest"], "true")
            self.assertIn("ghcr.io/signalridge/mare:1.2.3", outputs["docker_tags"])
            self.assertIn("ghcr.io/signalridge/mare:latest", outputs["docker_tags"])

    def test_prerelease_and_older_tag_never_write_latest(self):
        self.assertFalse(is_highest_stable_version("1.3.0-rc.1", ["v1.2.3"]))
        self.assertFalse(is_highest_stable_version("1.0.0", ["v1.0.0", "v1.0.2"]))
        self.assertTrue(is_highest_stable_version("1.0.3", ["v1.0.0", "v1.0.2", "v2.0.0-rc.1"]))
        self.assertTrue(is_highest_stable_version("2.0.0", ["v1.10.0", "v2.0.0"]))

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            dist = root / "dist"
            manifests = root / "manifests"
            dist.mkdir()
            (dist / "checksums.txt").write_text(self._sample_checksums("v1.0.0-rc.1", "1.0.0-rc.1"), encoding="utf-8")
            outputs = generate_manifests(
                tag="v1.0.0-rc.1",
                version="1.0.0-rc.1",
                dist_dir=dist,
                manifests_dir=manifests,
                existing_tags=["v0.9.0"],
            )
            self.assertEqual(outputs["is_prerelease"], "true")
            self.assertEqual(outputs["make_latest"], "false")
            self.assertNotIn(":latest", outputs["docker_tags"])
            self.assertIn("pkgver=1.0.0_rc.1", (manifests / "aur" / "PKGBUILD").read_text(encoding="utf-8"))
            self.assertIn("pkgver = 1.0.0_rc.1", (manifests / "aur" / ".SRCINFO").read_text(encoding="utf-8"))

    def test_missing_checksum_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            dist = root / "dist"
            dist.mkdir()
            (dist / "checksums.txt").write_text("0" * 64 + "  only-one.tar.gz\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                generate_manifests(tag="v1.0.0", version="1.0.0", dist_dir=dist, manifests_dir=root / "m")

    def test_release_notes_initial_marks_downstream_channels_pending(self):
        initial = render_release_notes_install_section(
            tag="v1.2.3",
            version="1.2.3",
            channels={c: "pending" for c in CHANNEL_ORDER},
        )
        self.assertIn(CHANNEL_SECTION_START, initial)
        self.assertIn(CHANNEL_SECTION_END, initial)
        self.assertIn("curl -fsSL https://signalridge.github.io/mare/install.sh | sh", initial)
        self.assertIn("nix run github:signalridge/mare/v1.2.3", initial)
        self.assertIn("Homebrew (macOS / Linux) (pending):", initial)
        self.assertIn("Scoop (Windows) (pending):", initial)
        self.assertIn("Arch Linux (AUR) (pending):", initial)
        self.assertIn("Nix (NUR) (pending):", initial)
        self.assertIn("Container (GHCR) (pending):", initial)
        self.assertNotIn("brew install signalridge/tap/mare", initial)
        self.assertNotIn("scoop install signalridge/mare", initial)
        self.assertNotIn("yay -S mare-bin", initial)
        self.assertNotIn("docker pull ghcr.io/signalridge/mare:1.2.3", initial)

    def test_release_notes_update_reflects_channel_results_and_preserves_changelog(self):
        initial = render_release_notes_install_section(
            tag="v1.2.3",
            version="1.2.3",
            channels={c: "pending" for c in CHANNEL_ORDER},
        )
        release_body = "## What's Changed\n- Fix thing (#42)\n\n" + initial
        updated_section = render_release_notes_install_section(
            tag="v1.2.3",
            version="1.2.3",
            channels={
                "container": "published",
                "homebrew": "published",
                "scoop": "failed",
                "aur": "skipped",
                "nur": "published",
            },
        )
        updated = upsert_release_notes_section(release_body, updated_section)
        self.assertIn("## What's Changed\n- Fix thing (#42)", updated)
        self.assertIn("Container (GHCR) (available): `docker pull ghcr.io/signalridge/mare:1.2.3`", updated)
        self.assertIn("Homebrew (macOS / Linux) (available):", updated)
        self.assertIn("brew install signalridge/tap/mare", updated)
        self.assertIn("Nix (NUR) (available):", updated)
        self.assertIn("Scoop (Windows) (sync failed):", updated)
        self.assertNotIn("scoop install signalridge/mare", updated)
        self.assertIn("Arch Linux (AUR) (not published):", updated)
        self.assertNotIn("yay -S mare-bin", updated)
        # Idempotent on repeated updates.
        self.assertEqual(upsert_release_notes_section(updated, updated_section), updated)


def _parse_channel_statuses(items: list[str]) -> dict[str, str]:
    statuses: dict[str, str] = {}
    for item in items:
        if "=" not in item:
            raise ValueError(f"expected channel=status, got {item!r}")
        key, val = item.split("=", 1)
        key = key.strip()
        val = val.strip()
        if key not in CHANNEL_ORDER:
            raise ValueError(f"unknown channel {key!r}")
        if val not in VALID_CHANNEL_STATUSES:
            raise ValueError(f"invalid status {val!r} for channel {key!r}")
        statuses[key] = val
    return statuses


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", help="Release tag, e.g. v1.0.0")
    parser.add_argument("--version", help="Release version, e.g. 1.0.0")
    parser.add_argument("--dist-dir", type=pathlib.Path, default=pathlib.Path("dist"))
    parser.add_argument("--manifests-dir", type=pathlib.Path, default=pathlib.Path("manifests"))
    parser.add_argument("--registry", default=os.environ.get("REGISTRY", "ghcr.io"))
    parser.add_argument("--image-name", default=os.environ.get("IMAGE_NAME", "signalridge/mare"))
    parser.add_argument("--github-output", type=pathlib.Path)
    parser.add_argument("--write-initial-notes", type=pathlib.Path, help="Write initial release notes section")
    parser.add_argument("--update-release-body", type=pathlib.Path, help="Existing release body file to update")
    parser.add_argument("--output-body", type=pathlib.Path, help="Destination file for updated release body")
    parser.add_argument(
        "--channel-status",
        action="append",
        default=[],
        help="Channel publication status as name=pending|published|skipped|failed",
    )
    parser.add_argument("--self-test", action="store_true", help="Run internal unit tests")
    args = parser.parse_args()

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReleaseManifestsSelfTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

    tag = args.tag or os.environ.get("TAG_NAME", "")
    version = args.version or os.environ.get("VERSION", "")
    if not tag or not version:
        print("--tag and --version (or TAG_NAME and VERSION env vars) are required", file=sys.stderr)
        return 2

    if args.update_release_body is not None:
        if args.output_body is None:
            print("--output-body is required with --update-release-body", file=sys.stderr)
            return 2
        statuses = _parse_channel_statuses(args.channel_status)
        existing = args.update_release_body.read_text(encoding="utf-8")
        section = render_release_notes_install_section(tag=tag, version=version, channels=statuses)
        args.output_body.write_text(upsert_release_notes_section(existing, section), encoding="utf-8")
        return 0

    existing_tags = list_git_tags(pathlib.Path.cwd())
    outputs = generate_manifests(
        tag=tag,
        version=version,
        dist_dir=args.dist_dir,
        manifests_dir=args.manifests_dir,
        registry=args.registry,
        image_name=args.image_name,
        existing_tags=existing_tags,
    )
    if args.write_initial_notes is not None:
        args.write_initial_notes.parent.mkdir(parents=True, exist_ok=True)
        initial = render_release_notes_install_section(
            tag=tag,
            version=version,
            channels={c: "pending" for c in CHANNEL_ORDER},
        )
        args.write_initial_notes.write_text(initial, encoding="utf-8")
    out_path = args.github_output or (pathlib.Path(os.environ["GITHUB_OUTPUT"]) if "GITHUB_OUTPUT" in os.environ else None)
    if out_path is not None:
        write_github_outputs(out_path, outputs)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
