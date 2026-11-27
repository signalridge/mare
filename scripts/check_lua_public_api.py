#!/usr/bin/env python3
"""Audit each builtin against public Lua declarations, without implementation inference.

This is an audit, not a compatibility baseline: every reported diagnostic fails.
Each plugin sees only its own modules and the public type library. In particular,
the implementation of mare_lib and other services cannot make private fields appear
public. LuaLS supplies the parser, alias resolution and type checker.
"""

from __future__ import annotations

import argparse
import re
import shutil
from pathlib import Path
from urllib.parse import unquote, urlparse

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from lua_api_legacy import legacy_api_files, legacy_api_violations, rust_lua_string
from lua_api_require_state import cross_plugin_state_warnings
from lua_api_rules import (
    baseline_growth_errors,
    boot_prims_violations,
    builtin_apply_no_yield_violations,
    cordis_underscore_files,
    cordis_underscore_violations,
    module_violations,
    preset_private_api_violations,
    public_event_violations,
    public_events,
    read_git_baseline_text,
    third_party_require_warnings,
)
from lua_api_runner import (
    BOUNDARY_DIAGNOSTICS,
    PRESET,
    ROOT,
    ast_grep_matches,
    diagnose,
    install_signal_cleanup,
    isolated_temp_dir,
    workspace_at,
)
from lua_api_selftest import self_test
from lua_api_selftest_rules import _init_temp_git

__all__ = [
    "PRESET",
    "ROOT",
    "_init_temp_git",
    "ast_grep_matches",
    "audit",
    "audit_specs",
    "baseline_growth_errors",
    "main",
    "read_git_baseline_text",
    "rust_lua_string",
    "self_test",
]

def audit(name: str, base_dir: Path | None = None) -> int:
    entry = PRESET / "plugins" / f"{name}.lua"
    if not entry.is_file():
        raise ValueError(f"unknown builtin: {name}")
    with isolated_temp_dir("mare-public-api-", base_dir=base_dir) as directory:
        workspace = Path(directory) / "workspace"
        workspace_at(workspace)
        # Contextual typing is essential: the runtime loader is deliberately absent from
        # this workspace, so an unannotated returned definition otherwise leaves apply(ctx)
        # untyped and silently permits every cross-service method.
        source = entry.read_text()
        definition = re.search(r"(?m)^return \{", source)
        if definition is None:
            raise ValueError(f"cannot establish the public PluginDef type for {entry}")
        annotation_line = source[: definition.start()].count("\n") + 1
        source = (
            source[: definition.start()]
            + "---@type mare.PluginDef\n"
            + source[definition.start() :]
        )
        (workspace / entry.name).write_text(source)
        modules = PRESET / "plugins/lua" / name
        if modules.is_dir():
            shutil.copytree(modules, workspace / "lua" / name)
        single_module = PRESET / "plugins/lua" / f"{name}.lua"
        if single_module.is_file():
            (workspace / "lua").mkdir(exist_ok=True)
            shutil.copy2(single_module, workspace / "lua" / f"{name}.lua")
        imports = module_violations(workspace, name)
        reports = diagnose(workspace)
        count = len(imports)
        files = [entry]
        if modules.is_dir():
            files += sorted(modules.rglob("*.lua"))
        if single_module.is_file():
            files.append(single_module)
        for error in public_event_violations(files, public_events()):
            print(error)
            count += 1
        for error in builtin_apply_no_yield_violations([entry]):
            print(error)
            count += 1
        for error in imports:
            relative = error["file"]
            line = error["line"] - int(
                relative == Path(entry.name) and error["line"] > annotation_line
            )
            source = PRESET / "plugins" / relative
            print(
                f"{source.relative_to(ROOT)}:{line}:{error['column']}: public-module: {error['message']}"
            )
        for uri, diagnostics in sorted(reports.items()):
            relative = Path(unquote(urlparse(uri).path)).relative_to(workspace)
            if relative.parts[0] == "types":
                # Types are also checked; report against their actual owning source.
                source = PRESET / relative
            else:
                source = PRESET / "plugins" / relative
            for diagnostic in diagnostics:
                if diagnostic["code"] not in BOUNDARY_DIAGNOSTICS:
                    continue
                start = diagnostic["range"]["start"]
                line = start["line"] + 1
                if relative == Path(entry.name) and line > annotation_line:
                    line -= 1
                message = diagnostic["message"].replace("\n", " ")
                print(
                    f"{source.relative_to(ROOT)}:{line}:{start['character'] + 1}: "
                    f"{diagnostic['code']}: {message}"
                )
                count += 1
        return count

def audit_specs(base_dir: Path | None = None) -> int:
    tests_dir = PRESET / "tests"
    if not tests_dir.is_dir():
        return 0
    specs = sorted(tests_dir.rglob("*_spec.lua"))
    if not specs:
        return 0
    with isolated_temp_dir("mare-public-api-specs-", base_dir=base_dir) as directory:
        workspace = Path(directory) / "workspace"
        workspace.mkdir(parents=True)
        shutil.copytree(PRESET / "types", workspace / "types")
        name_map: dict[str, Path] = {}
        for spec in specs:
            rel = spec.relative_to(tests_dir)
            flat = "__".join(rel.parts)
            (workspace / flat).write_text(spec.read_text())
            name_map[flat] = spec
        imports = module_violations(workspace, "__specs__")
        reports = diagnose(workspace)
        count = len(imports)
        for error in imports:
            source = name_map.get(str(error["file"]), PRESET / "tests" / error["file"])
            print(
                f"{source.relative_to(ROOT)}:{error['line']}:{error['column']}: "
                f"public-module: {error['message']}"
            )
        for uri, diagnostics in sorted(reports.items()):
            relative = Path(unquote(urlparse(uri).path)).relative_to(workspace)
            if relative.parts[0] == "types":
                source = PRESET / relative
            else:
                source = name_map.get(relative.as_posix(), PRESET / "tests" / relative)
            for diagnostic in diagnostics:
                if diagnostic["code"] not in BOUNDARY_DIAGNOSTICS:
                    continue
                start = diagnostic["range"]["start"]
                line = start["line"] + 1
                message = diagnostic["message"].replace("\n", " ")
                print(
                    f"{source.relative_to(ROOT)}:{line}:{start['character'] + 1}: "
                    f"{diagnostic['code']}: {message}"
                )
                count += 1
        return count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plugin", action="append", help="audit only this builtin (repeatable)")
    parser.add_argument("--third-party-dir", action="append", help="warn-only audit for a third-party plugins directory")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--base", help="accepted for compatibility (preset private API lint has no baseline)")
    args = parser.parse_args()
    if not shutil.which("lua-language-server"):
        parser.error("lua-language-server is required")
    if not shutil.which("ast-grep"):
        parser.error("ast-grep with Lua support is required")
    install_signal_cleanup()
    with isolated_temp_dir("mare-public-api-") as run_dir:
        if args.self_test:
            self_test(base_dir=run_dir)
            return 0
        if args.third_party_dir:
            for raw_dir in args.third_party_dir:
                target = Path(raw_dir)
                for warning in (
                    *third_party_require_warnings(target, PRESET / "types"),
                    *cross_plugin_state_warnings(target),
                ):
                    print(warning)
            return 0
        names = args.plugin or sorted(path.stem for path in (PRESET / "plugins").glob("*.lua"))
        count = sum(audit(name, base_dir=run_dir) for name in names)
        if not args.plugin:
            count += audit_specs(base_dir=run_dir)
            for error in (
                *legacy_api_violations(legacy_api_files()),
                *boot_prims_violations(ROOT),
                *cordis_underscore_violations(cordis_underscore_files(ROOT), root=ROOT),
                *preset_private_api_violations(ROOT, base=args.base),
            ):
                print(error)
                count += 1
            for warning in cross_plugin_state_warnings(PRESET / "plugins"):
                print(warning)
        elif args.base is not None:
            for error in preset_private_api_violations(ROOT, base=args.base):
                print(error)
                count += 1
        print(f"public Lua API: {count} diagnostics across {len(names)} builtin(s) and preset/tests")
        return int(count != 0)


if __name__ == "__main__":
    raise SystemExit(main())
