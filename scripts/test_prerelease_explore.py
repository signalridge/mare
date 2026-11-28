#!/usr/bin/env python3
"""Unit tests for scripts/prerelease_explore.py."""

import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

import prerelease_explore as pe  # noqa: E402


class TestPrereleaseExplore(unittest.TestCase):
    def test_simple_screen_handles_cursor_clear_and_alt_screen(self) -> None:
        scr = pe.SimpleScreen(40, 6)
        scr.feed(b"hello world\r\nsecond line")
        self.assertIn("hello world", scr.render())
        self.assertIn("second line", scr.render())

        # Enter alternate screen, draw overlay, then leave alternate screen.
        scr.feed(b"\x1b[?1049hTranscript overlay\x1b[?1049l")
        rendered = scr.render()
        self.assertIn("hello world", rendered)
        self.assertNotIn("Transcript overlay", rendered)

        # Erase display clears prior content.
        scr.feed(b"\x1b[2J\x1b[Hfresh welcome")
        self.assertEqual(scr.render().strip(), "fresh welcome")

    def test_redact_masks_api_key(self) -> None:
        self.assertEqual(pe.redact("Bearer sk-secret-123 failed", "sk-secret-123"), "Bearer [REDACTED] failed")
        self.assertEqual(pe.redact("no secret", ""), "no secret")

    def test_wrapper_stores_key_in_0600_file_and_not_argv(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp) / "home"
            home.mkdir()
            mare_bin = Path(tmp) / "mare"
            mare_bin.write_text("#!/bin/sh\n", encoding="utf-8")

            argv, env = pe.build_wrapper_and_env(
                home, mare_bin, "opencode-go/glm-5.3-flash", "secret-token-xyz", faux=False, extra_args=[]
            )
            self.assertNotIn("secret-token-xyz", " ".join(argv))
            self.assertNotIn("secret-token-xyz", json.dumps(env))
            keyfile = home / "api.key"
            self.assertTrue(keyfile.is_file())
            self.assertEqual(stat.S_IMODE(os.stat(keyfile).st_mode), 0o600)
            self.assertEqual(keyfile.read_text(encoding="utf-8"), "secret-token-xyz")

    def test_write_review_prompt_includes_checklist_and_steps(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            records = [
                pe.StepRecord(1, "startup", "Cold start", "01_startup.txt", True, 0.4, "started"),
                pe.StepRecord(2, "empty_side", "/side on empty", "02_empty_side.txt", False, 1.2, "timeout"),
            ]
            meta = {
                "timestamp_utc": "2026-11-28T00:00:00+00:00",
                "mare_version": "mare 1.0.1-rc",
                "model": "opencode-go/glm-5.3-flash",
                "cols": 110,
                "rows": 30,
            }
            prompt = pe.write_review_prompt(out, meta, records)
            text = prompt.read_text(encoding="utf-8")
            self.assertIn("docs/prerelease-exploratory-checklist.md", text)
            self.assertIn("Claude Code, Codex, and Kimi Code", text)
            self.assertIn("01_startup.txt", text)
            self.assertIn("FAIL", text)


if __name__ == "__main__":
    unittest.main()
