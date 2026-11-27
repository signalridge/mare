"""Self-test of scripts/gen_docs.py's Lua API page: service methods are documented under their
ctx field, and a changed or added method makes --check report the page stale."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gen_docs  # noqa: E402

PAGE = "docs/en/extend/lua-api.md"
USAGE = "mare/services/usage.gen.lua"
DOC = "--- Latest subscription quota per provider (the llm/rate-limits event)."

class LuaApiPageTests(unittest.TestCase):
    def setUp(self):
        self.preset = gen_docs.PRESET
        self.tmp = tempfile.TemporaryDirectory()
        shutil.copytree(self.preset / "types", Path(self.tmp.name) / "types")
        gen_docs.PRESET = Path(self.tmp.name)

    def tearDown(self):
        gen_docs.PRESET = self.preset
        self.tmp.cleanup()

    def edit(self, old, new):
        path = gen_docs.PRESET / "types" / USAGE
        text = path.read_text(encoding="utf-8")
        self.assertEqual(text.count(old), 1, old)
        path.write_text(text.replace(old, new), encoding="utf-8")

    def page(self):
        return gen_docs.gen_lua_api("en")
    def test_generated_service_methods_are_documented_and_current(self):
        page = self.page()
        self.assertIn(
            "| `ctx.usage:rate_limits()` | Latest subscription quota per provider (the llm/rate-limits event). |",
            page,
        )
        self.assertIn("## ctx.theme", page)
        self.assertIn("| `ctx:get(name)` |", page)
        self.assertEqual(gen_docs.stale_pages({PAGE: page}), [])

    def test_a_changed_method_doc_makes_the_page_stale(self):
        self.edit(DOC, "--- Quota per provider, changed.")
        page = self.page()
        self.assertIn("| `ctx.usage:rate_limits()` | Quota per provider, changed. |", page)
        self.assertEqual(gen_docs.stale_pages({PAGE: page}), [PAGE])

    def test_an_added_method_makes_the_page_stale(self):
        self.edit("function M:rate_limits() end", "function M:rate_limits() end\n\n--- New.\nfunction M:added(x) end")
        page = self.page()
        self.assertIn("| `ctx.usage:added(x)` | New. |", page)
        self.assertEqual(gen_docs.stale_pages({PAGE: page}), [PAGE])

class EventReferencePageTests(unittest.TestCase):
    def setUp(self):
        self.registry = gen_docs.EVENT_REGISTRY
        self.tmp = tempfile.TemporaryDirectory()
        copy = Path(self.tmp.name) / "event_registry.rs"
        copy.write_text(self.registry.read_text(encoding="utf-8"), encoding="utf-8")
        gen_docs.EVENT_REGISTRY = copy
        self.catalog = json.loads(
            (Path(__file__).resolve().parent.parent / "crates/mare-protocol/schema.json").read_text(encoding="utf-8")
        )
        self.events = gen_docs.contract_event_catalog(gen_docs.default_mare())
    def tearDown(self):
        gen_docs.EVENT_REGISTRY = self.registry
        self.tmp.cleanup()

    def test_generated_events_page_documents_registry_and_detects_staleness(self):
        self.assertIn("turn/started", gen_docs.event_payload_schemas(self.catalog))
        page = gen_docs.gen_events(self.catalog, "en", self.events)
        self.assertIn("| `revision` |", page.split("### `approval/changed`", 1)[1].split("###", 1)[0])
        self.assertIn("### `turn/started`", page)
        self.assertIn("`emit`", page)
        self.assertIn("`required`", page)
        self.assertIn('"turn_id": "t-1"', page)
        events_page = "docs/en/reference/events.md"
        if (gen_docs.ROOT / events_page).exists():
            self.assertEqual(gen_docs.stale_pages({events_page: page}), [])
            stale_copy = page.replace("Agent turn started.", "Agent turn started (edited).")
            self.assertEqual(gen_docs.stale_pages({events_page: stale_copy}), [events_page])

        src = gen_docs.EVENT_REGISTRY.read_text(encoding="utf-8")
        gen_docs.EVENT_REGISTRY.write_text(
            src.replace('doc: "Agent turn started.",', 'doc: "Agent turn started (changed).",', 1),
            encoding="utf-8",
        )
        updated = gen_docs.gen_events(self.catalog, "en", self.events)
        self.assertIn("Agent turn started (changed).", updated)
        if (gen_docs.ROOT / events_page).exists():
            self.assertEqual(gen_docs.stale_pages({events_page: updated}), [events_page])

    def test_generated_rpc_page_lists_baseline_read_errors(self):
        page = gen_docs.gen_rpc(self.catalog, "en")
        baseline_section = page.split("#### `baseline.read`", 1)[1].split("### ", 1)[0]
        self.assertIn("Errors: `baseline.stale`, `baseline.unknown_part`, `baseline.item_too_large`", baseline_section)
        errors_section = page.split("## Error codes", 1)[1]
        for code in ("baseline.stale", "baseline.unknown_part", "baseline.item_too_large"):
            self.assertIn(f"| `{code}` | `-32000` |", errors_section)

class EventCatalogBinaryTests(unittest.TestCase):
    def test_standalone_scripts_find_siblings_with_isolated_python(self):
        for name in ("gen_docs.py", "test_nextest_test_home.py"):
            with self.subTest(script=name):
                cmd = [sys.executable, "-I", "-S", str(Path(__file__).with_name(name)), "--help"]
                result = subprocess.run(cmd, capture_output=True, text=True, timeout=30, check=False)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_catalog_uses_the_cargo_target_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            if os.name != "nt":
                (real := root / "real").mkdir()
                (root := root / "link").symlink_to(real)
            custom, binary = root / "custom-target", "mare.exe" if os.name == "nt" else "mare"
            release = root / "target" / "release-only" / "release" / binary
            release.parent.mkdir(parents=True)
            release.touch()
            cases = (
                ({}, root / "target" / "debug" / binary),
                ({"CARGO_TARGET_DIR": str(custom)}, custom / "debug" / binary),
                ({"CARGO_TARGET_DIR": "relative-target"}, root / "relative-target" / "debug" / binary),
                ({"CARGO_TARGET_DIR": str(custom), "CARGO_BUILD_TARGET": "example-triple"}, custom / "example-triple" / "debug" / binary),
                ({"CARGO_TARGET_DIR": str(custom), "CARGO_BUILD_TARGET": "spec/custom.json"}, custom / "custom" / "debug" / binary),
                ({"CARGO_BUILD_TARGET": "release-only"}, release),
                ({"MARE_BIN": str(release)}, release.resolve()),
            )
            for env, expected in cases:
                with (self.subTest(env=env), patch.dict(os.environ, env, clear=True), patch.object(gen_docs, "ROOT", root),
                      patch.object(gen_docs, "contract_event_catalog", return_value=[]) as catalog):
                    fixture = EventReferencePageTests("test_generated_events_page_documents_registry_and_detects_staleness")
                    fixture.setUp()
                    try:
                        catalog.assert_called_once_with(str(expected))
                    finally:
                        fixture.tearDown()

    def test_docs_helpers_honor_mare_tmpdir_and_pass_tmpdir_to_child(self):
        import docs_event_catalog
        with tempfile.TemporaryDirectory() as outer:
            mare_tmp = Path(outer) / "mare-tmp"
            mare_tmp.mkdir()
            clean_env = {k: v for k, v in os.environ.items() if k not in ("TMPDIR", "TEMP", "TMP", "MARE_TMPDIR")}
            clean_env["MARE_TMPDIR"] = str(mare_tmp)
            seen_events, seen_cmds = {}, {}
            def fake_rpc_exchange(binary, requests, *, env, cwd):
                seen_events.update(cwd=Path(cwd).resolve(), env=dict(env))
                return [{"id": 2, "result": {"data": []}}]
            def fake_run(cmd, *, cwd, env, capture_output, check, timeout):
                seen_cmds.update(cwd=Path(cwd).resolve(), env=dict(env))
                (Path(cwd).parent / "commands.json").write_text('{"commands":[],"aliases":{}}', encoding="utf-8")
                return subprocess.CompletedProcess(cmd, 0, "", "")
            with patch.dict(os.environ, clean_env, clear=True):
                with patch.object(docs_event_catalog, "rpc_exchange", side_effect=fake_rpc_exchange):
                    self.assertEqual(docs_event_catalog.contract_event_catalog("/bin/true"), [])
                with patch.object(gen_docs.subprocess, "run", side_effect=fake_run):
                    self.assertEqual(gen_docs.registered_commands("/bin/true"), {"commands": [], "aliases": {}})
            for seen in (seen_events, seen_cmds):
                self.assertTrue(seen["cwd"].is_relative_to(mare_tmp.resolve()), seen["cwd"])
                self.assertEqual(seen["env"].get("TMPDIR"), seen["env"].get("HOME"))

if __name__ == "__main__":
    unittest.main()
