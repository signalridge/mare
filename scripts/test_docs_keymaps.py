"""Tests for generated keymap documentation tables, markers, and OS independence."""

import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import docs_keymaps  # noqa: E402
import gen_docs  # noqa: E402

REL = "docs/en/configure/keymaps.md"
HEADER = "# Keymaps\n\nIntro sentinel.\n\n## Default bindings\n\n"
FOOTER = "\n\nTail sentinel.\n"


class KeymapsDocTests(unittest.TestCase):
    def test_generated_keymaps_table_preserves_prose_and_detects_staleness(self):
        keymaps = [
            {"context": "editor", "key": "enter", "action": "editor.submit", "desc": "send message"},
            {"context": "editor", "key": "ctrl+r", "action": "review.viewer", "desc": "review changes"},
            {"context": "editor", "key": "left", "action": "agents", "when": ["editor_empty"], "desc": "sub-agents"},
            {"context": "editor", "key": "f9", "action": "steer", "when": ["working"], "desc": "steer"},
            {"context": "editor", "key": "f9", "action": "complete", "when": ["!working"], "desc": "complete"},
            {"context": "transcript", "key": "esc", "action": "agents.back", "desc": "back to the agents list"},
            {"context": "transcript", "key": "esc", "action": "view.back", "desc": "close panel"},
        ]
        updated = gen_docs.update_keymaps_doc(REL, keymaps, "en")
        self.assertIn("## Bind a key", updated)
        self.assertIn("## Default bindings", updated)
        self.assertIn("| `editor` | `Enter` | `editor.submit` |  | send message |", updated)
        self.assertIn("| `editor` | `Ctrl+R` | `review.viewer` |  | review changes |", updated)
        self.assertIn("| `editor` | `←` | `agents` | `editor_empty` | sub-agents |", updated)
        self.assertIn("| `editor` | `F9` | `steer` | `working` | steer |", updated)
        self.assertIn("| `editor` | `F9` | `complete` | `!working` | complete |", updated)
        self.assertIn("| `transcript` | `Esc` | `agents.back` |  | back to the agents list |", updated)
        self.assertIn("| `transcript` | `Esc` | `view.back` |  | close panel |", updated)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            doc_path = root / REL
            doc_path.parent.mkdir(parents=True)
            doc_path.write_text(updated, encoding="utf-8")
            with patch.object(gen_docs, "ROOT", root):
                self.assertEqual(gen_docs.stale_pages({REL: updated}), [])
                drifted = gen_docs.update_keymaps_doc(
                    REL,
                    keymaps + [{"context": "editor", "key": "alt+p", "action": "model.picker", "desc": "select model"}],
                    "en",
                )
                self.assertEqual(gen_docs.stale_pages({REL: drifted}), [REL])

    def test_markers_must_be_single_standalone_and_in_order(self):
        b, e = docs_keymaps.KEYMAPS_BEGIN, docs_keymaps.KEYMAPS_END
        bad_docs = [
            f"{HEADER}no markers{FOOTER}",
            f"{HEADER}{b}\n{b}\n{e}{FOOTER}",
            f"{HEADER}{b}\n{e}\n{e}{FOOTER}",
            f"{HEADER}{b}\ninline {e}\nprose\n{e}{FOOTER}",
            f"{HEADER}{e}\n{b}{FOOTER}",
            f"{HEADER}inline {b}\n{e}{FOOTER}",
        ]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / REL).parent.mkdir(parents=True)
            for content in bad_docs:
                (root / REL).write_text(content, encoding="utf-8")
                with self.assertRaises(ValueError):
                    docs_keymaps.update_keymaps_doc(root, REL, [], gen_docs.STRINGS["keymaps.cols"]["en"], gen_docs.cell)

            good = f"{HEADER}{b}\nstale\n{e}{FOOTER}"
            (root / REL).write_text(good, encoding="utf-8")
            out = docs_keymaps.update_keymaps_doc(
                root, REL, [{"context": "editor", "key": "enter", "action": "editor.submit", "desc": "send message"}],
                gen_docs.STRINGS["keymaps.cols"]["en"], gen_docs.cell,
            )
            self.assertIn("Intro sentinel.", out)
            self.assertIn("Tail sentinel.", out)
            self.assertNotIn("stale", out)

    def test_os_independent_table_matches_across_unix_and_windows_hosts(self):
        if shutil.which("lua") is None:
            self.skipTest("lua interpreter not available on PATH")
        actions = {"paste-image": "paste an image", "follow-up": "queue a follow-up"}
        unix_list = [{"context": "editor", "key": "ctrl+v", "action": "paste-image", "desc": "paste an image"}]
        win_list = [
            {"context": "editor", "key": "alt+v", "action": "paste-image", "desc": "paste an image"},
            {"context": "editor", "key": "ctrl+q", "action": "follow-up", "desc": "queue a follow-up"},
        ]
        from_unix = docs_keymaps.normalize_platform_keymaps(unix_list, actions, gen_docs.PRESET, "lua")
        from_win = docs_keymaps.normalize_platform_keymaps(win_list, actions, gen_docs.PRESET, "lua")
        cols = gen_docs.STRINGS["keymaps.cols"]["en"]
        table_unix = docs_keymaps.gen_keymaps_table(from_unix, cols, gen_docs.cell)
        table_win = docs_keymaps.gen_keymaps_table(from_win, cols, gen_docs.cell)
        self.assertEqual(table_unix, table_win)
        self.assertIn("| `editor` | `Alt+V` | `paste-image` |  | paste an image (Windows) |", table_unix)
        self.assertIn("| `editor` | `Ctrl+Q` | `follow-up` |  | queue a follow-up (Windows) |", table_unix)
        self.assertIn("| `editor` | `Ctrl+V` | `paste-image` |  | paste an image (Unix / macOS) |", table_unix)


if __name__ == "__main__":
    unittest.main()
