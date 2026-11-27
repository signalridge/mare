#!/usr/bin/env python3
"""Warn when a cross-plugin-required Lua module holds mutable module-level state
or registers listeners at require time (`require-decision-final.md` rule 8)."""

from __future__ import annotations

import os
import re
import sys
import tempfile
from pathlib import Path

_REQ_RE = re.compile(r"""\brequire\s*(?:\(\s*)?(["'])([\w.-]+)\1""")
_LOCAL_DECL_RE = re.compile(r"^local\s+([A-Za-z_]\w*)\s*=\s*(.+)$")
_RETURN_IDENT_RE = re.compile(r"^return\s+([A-Za-z_]\w*)\s*$")
_LISTENER_TOP_RE = re.compile(r"(?:^|\s)(?:mare\.on|[\w.]+:on|[\w.]+:once)\s*\(")


def _discover_plugins(plugins_dir: Path) -> tuple[dict[str, list[Path]], dict[str, tuple[str, Path]]]:
    """Return (owner -> lua files, module_name -> (owner, path))."""
    owner_files: dict[str, list[Path]] = {}
    module_index: dict[str, tuple[str, Path]] = {}
    if not plugins_dir.is_dir():
        return owner_files, module_index
    for entry in sorted(plugins_dir.iterdir()):
        if entry.name.startswith(("_", ".")):
            continue
        if entry.is_file() and entry.suffix == ".lua":
            owner_files.setdefault(entry.stem, []).append(entry)
        elif entry.name == "lua" and entry.is_dir():
            for mod_file in sorted(entry.rglob("*.lua")):
                rel = mod_file.relative_to(entry).with_suffix("")
                parts = list(rel.parts)
                if parts and parts[-1] == "init":
                    parts.pop()
                if not parts:
                    continue
                if len(parts) == 1 or parts[0] == "mare_lib":
                    module_index[".".join(parts)] = ("_shared", mod_file)
                else:
                    owner_files.setdefault(parts[0], []).append(mod_file)
                    module_index[".".join(parts)] = (parts[0], mod_file)
        elif entry.is_dir() and (entry / "init.lua").is_file():
            owner = entry.name
            files = [entry / "init.lua"]
            lua_root = entry / "lua"
            if lua_root.is_dir():
                for mod_file in sorted(lua_root.rglob("*.lua")):
                    files.append(mod_file)
                    rel = mod_file.relative_to(lua_root).with_suffix("")
                    parts = list(rel.parts)
                    if parts and parts[-1] == "init":
                        parts.pop()
                    if parts:
                        module_index[".".join(parts)] = (owner, mod_file)
            owner_files[owner] = files
    return owner_files, module_index


def _analyze_module(mod_name: str, path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    exported: str | None = None
    for line in reversed(lines):
        m = _RETURN_IDENT_RE.match(line.strip())
        if m:
            exported = m.group(1)
            break

    top_locals: dict[str, int] = {}
    warnings: list[str] = []
    for lineno, raw in enumerate(lines, 1):
        code = raw.split("--", 1)[0]
        if not code.strip():
            continue
        if not raw[0].isspace():
            if _LISTENER_TOP_RE.search(code):
                warnings.append(f"warning: {mod_name} ({path}:{lineno}): registers a listener at require time")
            m = _LOCAL_DECL_RE.match(code.strip())
            if m and m.group(1) != exported and not m.group(2).strip().startswith("function"):
                rhs = m.group(2).strip()
                if '__mode = "k"' not in rhs and "__mode = 'k'" not in rhs:
                    top_locals[m.group(1)] = lineno

    for name in sorted(top_locals):
        decl_line = top_locals[name]
        mut_re = re.compile(
            rf"(?:\b{re.escape(name)}\s*(?:\[[^\]]+\]|\.\w+)?\s*=(?!=)"
            rf"|\btable\.(?:insert|remove)\s*\(\s*{re.escape(name)}\b)"
        )
        for lineno, raw in enumerate(lines, 1):
            if lineno > decl_line and mut_re.search(raw.split("--", 1)[0]):
                warnings.append(f"warning: {mod_name} ({path}:{lineno}): mutates module-level state `{name}`")
                break
    return warnings

def cross_plugin_state_warnings(plugins_dir: Path) -> list[str]:
    owner_files, module_index = _discover_plugins(plugins_dir)
    cross_targets: set[str] = set()
    for caller, files in owner_files.items():
        for file in files:
            for match in _REQ_RE.finditer(file.read_text(encoding="utf-8")):
                provider = module_index.get(match.group(2))
                if provider and provider[0] != caller:
                    cross_targets.add(match.group(2))
    return [w for mod in sorted(cross_targets) for w in _analyze_module(mod, module_index[mod][1])]

def _self_test() -> None:
    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    with tempfile.TemporaryDirectory(dir=tmp_root) as tmp:
        root = Path(tmp)
        (root / "alpha/lua/alpha").mkdir(parents=True)
        (root / "beta").mkdir(parents=True)
        (root / "alpha/init.lua").write_text('return { name = "alpha", apply = function() end }\n')
        (root / "alpha/lua/alpha/bad.lua").write_text("local cache = {}\nlocal M = {}\nfunction M.set(k, v) cache[k] = v end\nreturn M\n")
        (root / "alpha/lua/alpha/ok.lua").write_text('local TAG = "x"\nlocal M = {}\nfunction M.f() return TAG end\nreturn M\n')
        (root / "beta/init.lua").write_text('return { name = "beta", apply = function() require("alpha.bad"); require("alpha.ok") end }\n')
        found = cross_plugin_state_warnings(root)
        assert any("alpha.bad" in w for w in found), found
        assert not any("alpha.ok" in w for w in found), found

def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        _self_test()
        print("lua_api_require_state: self-test passed")
        return 0
    target = Path(argv[0]) if argv else Path("crates/mare-plugin/preset/plugins")
    for w in cross_plugin_state_warnings(target):
        print(w)
    return 0

if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
