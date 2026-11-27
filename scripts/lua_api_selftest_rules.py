"""Unit self-tests for scanner entry points, public events, baselines, and legacy rules."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from lua_api_apply_sync import self_test_builtin_apply_no_yield  # noqa: F401
from lua_api_legacy import legacy_api_files, legacy_api_violations
from lua_api_rules import (
    boot_prims_violations,
    module_violations,
    preset_private_api_violations,
    public_event_violations,
    public_events,
    third_party_require_warnings,
)
from lua_api_runner import ROOT, ast_grep_matches, isolated_temp_dir, workspace_at

def _init_temp_git(root: Path, message: str = "base") -> None:
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    subprocess.run(["git", "-C", str(root), "config", "user.email", "test@example.com"], check=True)
    subprocess.run(["git", "-C", str(root), "config", "user.name", "Test"], check=True)
    subprocess.run(["git", "-C", str(root), "add", "."], check=True)
    subprocess.run(["git", "-C", str(root), "commit", "-q", "-m", message], check=True)

def self_test_ast_grep_failures() -> None:
    """A failing scanner is an error, never "no violations"."""
    assert ast_grep_matches(1, "[]\n", "", "probe") == []
    good = '[{"file": "a.lua", "range": {"byteOffset": {"start": 0, "end": 3}}}]'
    assert ast_grep_matches(0, good, "", "probe") == json.loads(good)
    for args, expected in [
        ((8, "", "Error: Cannot parse rule"), "probe failed (exit 8): Error: Cannot parse rule"),
        ((0, "[]", "ERROR: a.lua: invalid utf-8"), "probe reported an error: ERROR: a.lua: invalid utf-8"),
        ((1, "", ""), "probe produced no output (expected a JSON array)"),
        ((0, "  \n", ""), "probe produced no output (expected a JSON array)"),
        ((0, '{"file": "a.lua"}', ""), "probe produced dict, not an array of matches"),
        ((0, "[1, 2]", ""), "probe produced an entry that is not a match: 1"),
        ((0, "[{}]", ""), "probe produced an entry that is not a match: {}"),
        ((0, '[{"file": "a.lua"}]', ""), "probe produced an entry that is not a match"),
        ((0, "[{", ""), "probe produced invalid JSON"),
    ]:
        try:
            ast_grep_matches(*args, "probe")
        except RuntimeError as err:
            assert str(err).startswith(expected), f"{args}: {err}"
        else:
            raise AssertionError(f"{args} was accepted as matches")


def self_test_scanner_entry_points(base_dir: Path | None = None) -> None:
    """Both scanners inside `module_violations` (the import parser and the AST scanner) refuse a
    broken ast-grep: driven through the real entry point with a fake `ast-grep` first on PATH."""
    with isolated_temp_dir("mare-public-api-fake-sg-", base_dir=base_dir) as root:
        workspace = root / "workspace"
        workspace_at(workspace)
        (workspace / "probe.lua").write_text("local x = 1\n")
        bin_dir = root / "bin"
        bin_dir.mkdir()
        fake = bin_dir / "ast-grep"
        # mode per subcommand: `scan` is the AST scanner, `run` the import parser
        fake.write_text(
            "#!/bin/sh\n"
            'mode="$FAKE_SG_SCAN"; [ "$1" = run ] && mode="$FAKE_SG_RUN"\n'
            'case "$mode" in\n'
            "  ok) echo '[]'; exit 1 ;;\n"
            "  error) echo '[]'; echo 'ERROR: probe.lua: cannot parse' >&2; exit 0 ;;\n"
            "  empty) exit 0 ;;\n"
            "  object) echo '{}'; exit 0 ;;\n"
            "  junk) echo '[{}]'; exit 0 ;;\n"
            "esac\n"
        )
        fake.chmod(0o755)
        saved = {k: os.environ.get(k) for k in ("PATH", "FAKE_SG_SCAN", "FAKE_SG_RUN")}
        try:
            os.environ["PATH"] = f"{bin_dir}{os.pathsep}{saved['PATH'] or ''}"
            for scan, run, expected in [
                ("error", "ok", "Lua AST scanner reported an error"),
                ("empty", "ok", "Lua AST scanner produced no output"),
                ("object", "ok", "Lua AST scanner produced dict"),
                ("junk", "ok", "Lua AST scanner produced an entry that is not a match"),
                ("ok", "error", "Lua import parser reported an error"),
                ("ok", "empty", "Lua import parser produced no output"),
                ("ok", "junk", "Lua import parser produced an entry that is not a match"),
            ]:
                os.environ["FAKE_SG_SCAN"], os.environ["FAKE_SG_RUN"] = scan, run
                try:
                    module_violations(workspace, "probe")
                except RuntimeError as err:
                    assert str(err).startswith(expected), f"scan={scan} run={run}: {err}"
                else:
                    raise AssertionError(f"scan={scan} run={run}: a broken ast-grep read as no violations")
        finally:
            for k, v in saved.items():
                if v is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = v


def self_test_public_events(base_dir: Path | None = None) -> None:
    """A bare dispatch of a public event is flagged; through ctx.bus, a comment, or an own
    in-process event, it is not."""
    public = public_events()
    assert "turn/started" in public, "PUBLIC_EVENTS not read"
    with isolated_temp_dir("mare-public-events-test-", base_dir=base_dir) as directory:
        probe = Path(directory) / "probe.lua"
        probe.write_text(
            'ctx:emit("turn/started", {})\n'
            'ctx.bus:emit("turn/started", {})\n'
            'ctx:emit("probe/own", 1)\n'
            '-- ctx:emit("turn/completed")\n'
            'self.ctx:waterfall("turn/completed", v)\n'
            "ctx:emit('turn/started')\n"  # 6: single quotes
            'ctx:bail(\n  "turn/completed",\n  {}\n)\n'  # 7: across lines
            'local n = 1 -- ctx:emit("turn/started", {})\n'  # 11: a trailing comment
            "ctx:emit([[turn/completed]])\n"  # 12: a long string (the call above takes lines 7-10)
            'ctx:get("bus"):emit("turn/started", {})\n'  # 13: the same public bus by name
            "self.ctx:get('bus'):emit('turn/completed', {})\n"  # 14: single quotes
            'ctx:get("other"):emit("turn/started", {})\n'  # 15: still a bare dispatch
        )
        lines = [
            error.split(":")[1]
            for error in public_event_violations([probe], public, root=Path(directory))
        ]
        assert lines == ["1", "5", "6", "7", "12", "15"], lines


def self_test_preset_private_api(base_dir: Path | None = None) -> None:
    with isolated_temp_dir("mare-preset-private-api-test-", base_dir=base_dir) as root:
        plugin = root / "crates/mare-plugin/preset/probe.lua"
        config = root / "crates/mare-config/preset/probe.lua"
        plugin.parent.mkdir(parents=True)
        config.parent.mkdir(parents=True)
        config.write_text('local x = 1\n')
        plugin.write_text(
            'ui._existing()\n'
            'ui._new()\n'
            '-- ui._comment()\n'
            'local s = "ui._string"\n'
        )
        errors = preset_private_api_violations(root)
        assert any("probe.lua:ui._existing" in error for error in errors), errors
        assert any("probe.lua:ui._new" in error for error in errors), errors
        assert not any("_comment" in error or "_string" in error for error in errors), errors
        plugin.write_text('local s = "ui._existing"\n')
        assert preset_private_api_violations(root) == []
        nested = root / "crates/mare-plugin/preset/plugins/lua/sub/probe.lua"
        nested.parent.mkdir(parents=True); nested.write_text('mare._nested()\n')
        assert any("plugins/lua/sub/probe.lua:mare._nested" in e for e in preset_private_api_violations(root))
        nested.unlink()
        plugin.write_text('ui["_existing"]()\n')
        assert any("probe.lua:ui._existing" in e for e in preset_private_api_violations(root))
        plugin.write_text('local x = 1\n'); config.write_text('mare._fresh()\n')
        assert any("crates/mare-config/preset/probe.lua:mare._fresh" in e for e in preset_private_api_violations(root))
        config.write_text('rawget(ui, "_wait_hook"); rawset(mare, "_new", 1)\n')
        errors = preset_private_api_violations(root)
        assert any("probe.lua:ui._wait_hook" in e for e in errors) and any("probe.lua:mare._new" in e for e in errors), errors
        config.write_text('ui["\\095fresh"]()\n')
        assert any("probe.lua:ui._fresh" in e for e in preset_private_api_violations(root))
        config.write_text('local x = 1\n')
        assert preset_private_api_violations(root, base="HEAD") == []


def self_test_boot_prims(base_dir: Path | None = None) -> None:
    """Only crates/mare-kernel/lua/mare_kernel/boot.lua may use prims.capture/spawn_root/run_root."""
    with isolated_temp_dir("mare-boot-prims-test-", base_dir=base_dir) as root:
        boot = root / "crates/mare-kernel/lua/mare_kernel/boot.lua"
        other_kernel = root / "crates/mare-kernel/lua/mare_kernel/other.lua"
        preset_plugin = root / "crates/mare-plugin/preset/plugins/probe.lua"
        preset_types = root / "crates/mare-plugin/preset/types/mare/host/prims.lua"
        config_preset = root / "crates/mare-config/preset/probe.lua"
        example = root / "examples/demo/init.lua"
        for p in (boot, other_kernel, preset_plugin, preset_types, config_preset, example):
            p.parent.mkdir(parents=True, exist_ok=True)
        boot.write_text("local s = prims.capture()\nprims.spawn_root(fn)\nprims.run_root(fn)\n")
        preset_types.write_text("function prims.capture() end\nfunction prims.spawn_root(fn) end\n")
        other_kernel.write_text(
            "local a = prims.capture()\n"
            "local b = prims['spawn_root'](fn)\n"
            "local c = rawget(prims, 'run_root')\n"
            "-- prims.capture() comment is ignored\n"
            "local ok = host.capture()\n"
        )
        preset_plugin.write_text("prims.spawn_root(fn)\n")
        config_preset.write_text("rawset(prims, 'capture', fn)\n")
        example.write_text("prims.run_root(fn)\n")
        errors = boot_prims_violations(root)
        assert not any(e.startswith("crates/mare-kernel/lua/mare_kernel/boot.lua:") for e in errors), errors
        assert not any(e.startswith("crates/mare-plugin/preset/types/") for e in errors), errors
        assert any("other.lua:1:" in e and "prims.capture" in e for e in errors), errors
        assert any("other.lua:2:" in e and "prims.spawn_root" in e for e in errors), errors
        assert any("other.lua:3:" in e and "prims.run_root" in e for e in errors), errors
        assert not any("other.lua:4:" in e or "other.lua:5:" in e for e in errors), errors
        assert any("plugins/probe.lua:1:" in e for e in errors), errors
        assert any("mare-config/preset/probe.lua:1:" in e for e in errors), errors
        assert any("examples/demo/init.lua:1:" in e for e in errors), errors


def self_test_third_party_requires(base_dir: Path | None = None) -> None:
    """Third-party plugins may require own lua/ modules (any root name) and ---@meta modules; others warn only."""
    with isolated_temp_dir("mare-third-party-req-test-", base_dir=base_dir) as root:
        types_dir = root / "types"
        types_dir.mkdir()
        (types_dir / "mare_lib.lua").write_text("---@meta mare_lib\n")
        plugins_dir = root / "plugins"
        (plugins_dir / "tool-spill/lua/spill").mkdir(parents=True)
        (plugins_dir / "tool-spill/lua/spill/retain.lua").write_text("return {}\n")
        (plugins_dir / "tool-spill/init.lua").write_text(
            'local lib = require("mare_lib")\n'
            'local retain = require("spill.retain")\n'
            'local internal = require("mare_kernel.boot")\n'
            'local other = require("other.secret")\n'
            'local dyn = require(mod_name)\n'
        )
        warnings = third_party_require_warnings(plugins_dir, types_dir)
        assert len(warnings) == 3, warnings
        assert all(w.startswith("warning: ") for w in warnings), warnings
        assert any("mare_kernel.boot" in w for w in warnings), warnings
        assert any("other.secret" in w for w in warnings), warnings
        assert any("dynamic" in w for w in warnings), warnings
        assert not any("mare_lib" in w or "spill.retain" in w for w in warnings), warnings


def self_test_legacy_api(base_dir: Path | None = None) -> None:
    with isolated_temp_dir("mare-legacy-api-test-", base_dir=base_dir) as directory:
        probe = Path(directory) / "probe.lua"
        probe.write_text(
            "ctx:on_local('x', f)\n"  # 1: legacy
            "ctx:on('x', f)\n"  # 2: ok
            "local a = ctx:inject('agent')\n"  # 3: legacy
            "ctx:inject({ 'agent' }, function(sub) end)\n"  # 4: ok
            "return { name = 'p', cordis = true }\n"  # 5: legacy
            "ctx:on('x', f, { priority = 5 })\n"  # 6: legacy
            "-- ctx:with_service('x', f)\n"  # 7: ok (comment)
            "ctx.bus:emit_checked('x')\n"  # 8: ok (the bus)
            "sub:service('agent')\n"  # 9: legacy
            "query:register('p', { priority = 5 })\n"  # 10: ok (not :on)
        )
        root = Path(directory)
        lines = [error.split(":")[1] for error in legacy_api_violations([probe], root=root)]
        assert lines == ["1", "3", "5", "6", "9"], lines

        doc_probe = Path(directory) / "probe.md"
        doc_probe.write_text("Use `ctx:service` here.\nUse `ctx:get` instead.\n")
        lines = [error.split(":")[1] for error in legacy_api_violations([doc_probe], root=root)]
        assert lines == ["1"], lines

        priority_probe = Path(directory) / "priority.lua"
        priority_probe.write_text(
            'ctx:on("frontend/follow-up", function() return { error = "refused by test" } end, { priority = 1000 })\n'
            'ctx:on("frontend/follow-up", function()\n'
            '  local a, b = 1, 2\n'
            '  if a then commands:register("x", { priority = b }) end\n'
            '  return { error = "refused by test" }\n'
            'end, {\n'
            '  prepend = true,\n'
            '  priority = 1000,\n'
            '})\n'
            'ctx:on("legal", function()\n'
            '  commands:register("go", { priority = 9 })\n'
            '  keymap:bind("K", { priority = 10 })\n'
            '  return { priority = 3 }\n'
            'end, { prepend = true })\n'
            'ctx.commands:register("go", { priority = 9 })\n'
            'ctx.keymap:bind("K", { priority = 10 })\n'
            '-- ctx:on("comment", f, { priority = 1000 })\n'
            'ctx:on("middle", f, { prepend = true, priority = rank, global = true })\n'
            'ctx:on("nested", f, { config = { priority = 10 } })\n'
            'local text = [=[ctx:on("text", f, { priority = 10 })]=]\n'
            'ctx:on("bracket", f, { ["priority"] = 5 })\n'
            "ctx:on('bracket', f, { ['priority'] = 5 })\n"
            'ctx:on("extra", f, { priority = 5 }, extra)\n'
            'ctx:on("value", f, { other = priority })\n'
            'ctx:on("paren", f, (({ priority = 5 })))\n'
            'ctx:on("safe-paren", f, ({ config = { priority = 5 } }))\n'
            'ctx:on("safe-expression", f, (wrap({ priority = 5 })))\n'
        )
        rust_probe = Path(directory) / "priority.rs"
        rust_probe.write_text(
            'fn fixture() {\n'
            '    let plugin = r#"return { apply = function(ctx)\n'
            '        ctx:on("frontend/follow-up", function() return { error = "refused by test" } end, { priority = 1000 })\n'
            '        ctx:on("multi", function()\n'
            '            return { error = "refused by test" }\n'
            '        end, { priority = 1000 })\n'
            '        ctx.commands:register("ok", { priority = 10 })\n'
            '    end }"#;\n'
            '    let other = r#"ctx:on(\'inline\', function() end, { priority = 10 })"#;\n'
            '    // ctx:on("comment", f, { priority = 1000 })\n'
            '    let normal = "ctx:on(\'normal\', f, { priority = 10 })";\n'
            '    let escaped = "ctx:on(\\"escaped\\", f, { priority = 10 })";\n'
            '}\n'
        )
        lua_lines = [
            error.split(":")[1] for error in legacy_api_violations([priority_probe], root=root)
        ]
        rust_lines = [
            error.split(":")[1] for error in legacy_api_violations([rust_probe], root=root)
        ]
        assert lua_lines == ["1", "2", "18", "21", "22", "23", "25"], lua_lines
        assert rust_lines == ["3", "4", "9", "11", "12"], rust_lines

        # Discovery must include plugins outside the preset, including eval and example plugins.
        probes = [
            Path(directory) / "evals/tasks/probe/environment/plugins/probe/init.lua",
            Path(directory) / "examples/plugins/probe.lua",
            Path(directory) / "examples/target/plugins/probe.lua",
            Path(directory) / "docs/extracted/probe.lua",
            Path(directory) / "crates/mare-cli/tests/fixtures/probe.lua",
        ]
        for path in probes:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("ctx:on_local('x', f)\n")
        discovered = set(legacy_api_files(root=root))
        assert set(probes) <= discovered, set(probes) - discovered
        assert len(legacy_api_violations(probes, root=root)) == len(probes)
