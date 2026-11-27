#!/usr/bin/env python3
"""Verify required license/notice files inside a release .tar.gz or .zip archive. Stdlib-only."""

from __future__ import annotations

import argparse
import pathlib
import sys
import tarfile
import tempfile
import unittest
import zipfile

REQUIRED_REL_PATHS = (
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    "licenses/cordis-LICENSE",
    "licenses/RUST-THIRD-PARTY-NOTICES.txt",
)


def _archive_members(archive_path: pathlib.Path) -> set[str]:
    name = archive_path.name.lower()
    if name.endswith((".tar.gz", ".tgz")):
        with tarfile.open(archive_path, "r:gz") as tf:
            return {m.replace("\\", "/").lstrip("./") for m in tf.getnames()}
    if name.endswith(".zip"):
        with zipfile.ZipFile(archive_path, "r") as zf:
            return {m.replace("\\", "/").lstrip("./") for m in zf.namelist()}
    raise ValueError(f"unsupported archive format: {archive_path}")


def verify_archive(archive_path: pathlib.Path, prefix: str) -> list[str]:
    """Return missing required member paths for `archive_path` under top-level `prefix`."""
    if not archive_path.is_file():
        return [f"archive not found: {archive_path}"]
    clean_prefix = prefix.replace("\\", "/").strip("/")
    members = _archive_members(archive_path)
    required = [f"{clean_prefix}/{rel}" for rel in REQUIRED_REL_PATHS]
    return [req for req in required if req not in members]


class VerifyReleaseArchiveTests(unittest.TestCase):
    def test_tar_gz_pass_and_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            prefix = "mare-v1.0.0-x86_64-unknown-linux-musl"
            stage = root / prefix
            (stage / "licenses").mkdir(parents=True)
            for rel in REQUIRED_REL_PATHS:
                (stage / rel).write_text("ok\n", encoding="utf-8")
            good = root / f"{prefix}.tar.gz"
            with tarfile.open(good, "w:gz") as tf:
                tf.add(stage, arcname=prefix)
            self.assertEqual(verify_archive(good, prefix), [])

            (stage / "licenses/RUST-THIRD-PARTY-NOTICES.txt").unlink()
            bad = root / "bad.tar.gz"
            with tarfile.open(bad, "w:gz") as tf:
                tf.add(stage, arcname=prefix)
            missing = verify_archive(bad, prefix)
            self.assertEqual(missing, [f"{prefix}/licenses/RUST-THIRD-PARTY-NOTICES.txt"])

    def test_zip_normalizes_windows_separators_and_catches_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            prefix = "mare-v1.0.0-x86_64-pc-windows-msvc"
            good = root / f"{prefix}.zip"
            with zipfile.ZipFile(good, "w") as zf:
                for rel in REQUIRED_REL_PATHS:
                    zf.writestr(f"{prefix}\\{rel.replace('/', '\\')}", "ok\n")
            self.assertEqual(verify_archive(good, prefix), [])

            bad = root / "bad.zip"
            with zipfile.ZipFile(bad, "w") as zf:
                zf.writestr(f"{prefix}/LICENSE", "ok\n")
            missing = verify_archive(bad, prefix)
            self.assertEqual(len(missing), 3, missing)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="Run built-in unit tests")
    parser.add_argument("archive", nargs="?", type=pathlib.Path, help="Path to .tar.gz or .zip archive")
    parser.add_argument("prefix", nargs="?", help="Expected top-level directory inside the archive")
    args = parser.parse_args(argv)

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(VerifyReleaseArchiveTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

    if args.archive is None or not args.prefix:
        parser.error("specify --self-test or <archive> <prefix>")

    missing = verify_archive(args.archive, args.prefix)
    if missing:
        print(f"archive verification FAILED for {args.archive}: missing {missing}", file=sys.stderr)
        return 1
    print(f"archive verification OK: {args.archive}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
