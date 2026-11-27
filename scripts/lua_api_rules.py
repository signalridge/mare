"""Boundary import, public-event, and preset private-API audit rules for Lua plugins."""

from __future__ import annotations

import re
import subprocess
from collections import Counter
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from lua_api_runner import ROOT, ast_grep_matches, isolated_temp_dir

CTX_RECEIVER = re.compile(r"(?:ctx|caller|scope|owner|sub|self\.ctx|D\.ctx)")

BOUNDARY_IDENTS = (
    "require",
    "rawget",
    "rawset",
    "load",
    "loadstring",
    "_G",
    "_ENV",
    "package",
)


def _unwrap_parens(expr: str) -> str:
    expr = expr.strip()
    while expr.startswith("(") and expr.endswith(")"):
        depth = 0
        wrapped = True
        for index, ch in enumerate(expr):
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0 and index < len(expr) - 1:
                    wrapped = False
                    break
        if not wrapped or depth != 0:
            break
        expr = expr[1:-1].strip()
    return expr


def public_modules(types_dir: Path) -> set[str]:
    return {m[1] for p in types_dir.rglob("*.lua") for m in re.finditer(r"(?m)^---@meta[ \t]+([\w.-]+)", p.read_text(encoding="utf-8")) if m[1] != "_"}


def module_violations(workspace: Path, owner: str) -> list[dict]:
    """Check imports and dynamic/raw loader bypasses with ast-grep's Lua parser."""
    public = public_modules(workspace / "types")
    sources = [str(path.relative_to(workspace)) for path in workspace.glob("*.lua")]
    sources += [
        str(path.relative_to(workspace)) for path in (workspace / "lua").rglob("*.lua")
    ]
    if not sources:
        return []

    def parsed(pattern: str, selector: str | None = None) -> list[dict]:
        command = [
            "ast-grep",
            "run",
            "--lang",
            "lua",
            "--pattern",
            pattern,
            "--json=compact",
        ]
        if selector:
            command += ["--selector", selector]
        result = subprocess.run(
            [*command, *sources],
            cwd=workspace,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        return ast_grep_matches(result.returncode, result.stdout, result.stderr, "Lua import parser")

    def scanned(rules_yaml: str) -> list[dict]:
        result = subprocess.run(
            ["ast-grep", "scan", "--inline-rules", rules_yaml, "--json=compact", *sources],
            cwd=workspace,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        return ast_grep_matches(result.returncode, result.stdout, result.stderr, "Lua AST scanner")

    blocks_by_file: dict[str, list[tuple[int, int]]] = {}
    for match in scanned("id: block\nlanguage: lua\nrule:\n  kind: block"):
        offset = match["range"]["byteOffset"]
        blocks_by_file.setdefault(match["file"], []).append(
            (offset["start"], offset["end"])
        )

    local_load_scopes: dict[str, list[tuple[int, int]]] = {}
    for pattern in (
        "local function load($$$ARGS) $$$BODY end",
        "local load = function($$$ARGS) $$$BODY end",
    ):
        for match in parsed(pattern):
            rel = match["file"]
            offset = match["range"]["byteOffset"]
            decl_start, decl_end = offset["start"], offset["end"]
            enclosing = [
                (b_start, b_end)
                for b_start, b_end in blocks_by_file.get(rel, ())
                if b_start <= decl_start and decl_end <= b_end
            ]
            if enclosing:
                _, scope_end = min(enclosing, key=lambda span: span[1] - span[0])
            else:
                scope_end = (workspace / rel).stat().st_size
            local_load_scopes.setdefault(rel, []).append((decl_start, scope_end))

    calls = parsed("require $ARG")
    direct = {(match["file"], match["range"]["byteOffset"]["start"]) for match in calls}
    errors: list[dict] = []
    reported_positions: set[tuple[str, int, int]] = set()

    def add_error(rel: str, start: dict, message: str) -> None:
        pos = (rel, start["line"] + 1, start["column"] + 1)
        if pos in reported_positions:
            return
        reported_positions.add(pos)
        errors.append(
            {
                "file": Path(rel),
                "line": pos[1],
                "column": pos[2],
                "message": message,
            }
        )

    for match in parsed("$TABLE[$KEY]", "bracket_index_expression"):
        table = _unwrap_parens(match["metaVariables"]["single"].get("TABLE", {}).get("text", ""))
        if table in {"_ENV", "_G"}:
            add_error(
                match["file"],
                match["range"]["start"],
                "dynamic global/loader access via _G/_ENV is forbidden",
            )
        elif table == "package":
            add_error(
                match["file"],
                match["range"]["start"],
                "dynamic loader access via package is forbidden",
            )

    # Plugin Lua never reads the root context:
    # a plugin that needs to see every scope registers with `{ global = true }`. Only ctx-like
    # receivers are checked; kernel Lua (crates/mare-kernel/lua) is outside this audit.
    for pattern, selector in [("$TABLE.root", "dot_index_expression"), ("$TABLE[$KEY]", "bracket_index_expression")]:
        for match in parsed(pattern, selector):
            variables = match["metaVariables"]["single"]
            table = _unwrap_parens(variables.get("TABLE", {}).get("text", ""))
            if selector == "bracket_index_expression" and variables.get("KEY", {}).get("text") not in {'"root"', "'root'"}:
                continue
            if CTX_RECEIVER.fullmatch(table):
                add_error(
                    match["file"],
                    match["range"]["start"],
                    "plugin code must not read the root context (ctx.root); observe every scope with { global = true }",
                )

    for name in BOUNDARY_IDENTS:
        fields: set[tuple[str, int]] = set()
        for pattern, selector, at_end in [
            (f"$TABLE.{name}", "dot_index_expression", True),
            (f"$TABLE:{name}()", "method_index_expression", True),
            (f"{{{name} = $VALUE}}", "field", False),
        ]:
            for match in parsed(pattern, selector):
                table = match["metaVariables"]["single"].get("TABLE", {}).get("text", "")
                if _unwrap_parens(table) in {"_ENV", "_G", "package"}:
                    continue
                offset = match["range"]["byteOffset"]
                fields.add(
                    (
                        match["file"],
                        offset["end"] - len(name) if at_end else offset["start"],
                    )
                )
        for match in parsed(name, "identifier"):
            rel = match["file"]
            byte_start = match["range"]["byteOffset"]["start"]
            if (rel, byte_start) in fields:
                continue
            if name == "require":
                if (rel, byte_start) in direct:
                    continue
                add_error(
                    rel,
                    match["range"]["start"],
                    "require must be a direct checked call; storing/passing the loader cannot be resolved",
                )
            elif name in {"rawget", "rawset"}:
                add_error(
                    rel,
                    match["range"]["start"],
                    "rawget/rawset bypasses public API type and metatable checks",
                )
            elif name in {"load", "loadstring"}:
                if name == "load" and (
                    owner == "lua-console"
                    or any(
                        s <= byte_start <= e for s, e in local_load_scopes.get(rel, ())
                    )
                ):
                    continue
                add_error(
                    rel,
                    match["range"]["start"],
                    "dynamic code loading via load/loadstring is forbidden",
                )
            elif name in {"_G", "_ENV"}:
                if owner == "lua-console" and name == "_ENV":
                    continue
                add_error(
                    rel,
                    match["range"]["start"],
                    "dynamic global/loader access via _G/_ENV is forbidden",
                )
            elif name == "package":
                add_error(
                    rel,
                    match["range"]["start"],
                    "dynamic loader access via package is forbidden",
                )

    for match in calls:
        argument = _unwrap_parens(match["metaVariables"]["single"]["ARG"]["text"])
        literal = re.fullmatch(r"(['\"])([\w.-]+)\1", argument)
        long_literal = re.fullmatch(r"\[(=*)\[([\w.-]+)\]\1\]", argument)
        module = literal[2] if literal else long_literal[2] if long_literal else None
        owned = module is not None and (
            module == owner or module.startswith(owner + ".")
        )
        exists = False
        if owned:
            path = workspace / "lua" / module.replace(".", "/")
            exists = path.with_suffix(".lua").is_file() or (path / "init.lua").is_file()
        if module in public or (owned and exists):
            continue
        add_error(
            match["file"],
            match["range"]["start"],
            f"undeclared import {module!r}" if module else "unresolved dynamic import",
        )
    return errors


PROTOCOL_EVENT_REGISTRY = ROOT / "crates/mare-protocol/src/event_registry.rs"


# a bare cordis dispatch (a call through `ctx.bus` is the entry)
BARE_DISPATCH_MODES = {"emit", "emit_checked", "serial", "parallel", "bail", "waterfall", "waterfall_checked"}


def public_events() -> set[str]:
    """The public event list (mare-protocol protocol_events!): the one source of publicity."""
    text = PROTOCOL_EVENT_REGISTRY.read_text()
    start = text.find("protocol_events! {")
    if start == -1:
        raise ValueError(f"cannot find protocol_events! in {PROTOCOL_EVENT_REGISTRY}")
    return set(re.findall(r'^\s*"([^"]+)"\s*=>\s*[A-Za-z0-9_]+\(', text[start:], re.M))


def string_literal(text: str) -> str | None:
    """The value of a Lua short or long string literal, else None (an expression)."""
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "'\"":
        return text[1:-1]
    long = re.fullmatch(r"\[(=*)\[(.*)\]\1\]", text, re.DOTALL)
    return long[2] if long else None


def public_event_violations(
    files: list[Path], public: set[str], root: Path = ROOT
) -> list[str]:
    """A plugin emits a public mare event through `ctx.bus` (the Rust entry:
    taps, blocked turns, the record), never with a bare cordis dispatch.
    Method calls are read with ast-grep (comments, quote styles and line breaks are the parser's)."""
    if not files:
        return []
    errors: list[tuple[tuple[str, int, int], str]] = []
    seen: set[tuple[str, int, int]] = set()
    for pattern in ("$R:$M($E, $$$A)", "$R:$M($E)"):
        result = subprocess.run(
            [
                "ast-grep",
                "run",
                "--lang",
                "lua",
                "--pattern",
                pattern,
                "--json=compact",
                *map(str, files),
            ],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        for match in ast_grep_matches(
            result.returncode, result.stdout, result.stderr, "public-event scanner"
        ):
            single = match["metaVariables"]["single"]
            receiver, mode = single["R"]["text"], single["M"]["text"]
            event = string_literal(single["E"]["text"])
            start = match["range"]["start"]
            key = (match["file"], start["line"], start["column"])
            if key in seen or mode not in BARE_DISPATCH_MODES or event is None:
                continue
            seen.add(key)
            is_bus = receiver.split(".")[-1] == "bus" or re.search(
                r''':get\s*\(\s*(["'])bus\1\s*\)\s*$''', receiver
            )
            if not is_bus and event in public:
                path = Path(match["file"])
                path = path if path.is_absolute() else root / path
                position = (str(path.relative_to(root)), start["line"], start["column"])
                errors.append(
                    (
                        position,
                        (
                            f"{position[0]}:{start['line'] + 1}:{start['column'] + 1}: public-event: "
                            f"`{event}` is a public event: emit it through ctx.bus:{mode}, "
                            f"not a bare cordis dispatch (it would miss the taps)"
                        ),
                    )
                )
    return [message for _, message in sorted(errors)]


def preset_private_api_counts(root: Path) -> Counter[str]:
    files = sorted(
        path.relative_to(root)
        for directory in ("crates/mare-plugin/preset", "crates/mare-config/preset")
        for path in (root / directory).rglob("*.lua")
    )
    if not files:
        return Counter()
    counts: Counter[str] = Counter()
    for pattern, selector in (
        ("$TABLE.$FIELD", "dot_index_expression"),
        ("$TABLE:$FIELD()", "method_index_expression"),
        ("$TABLE[$KEY]", "bracket_index_expression"),
        ("rawget($TABLE, $KEY)", None),
        ("rawset($TABLE, $KEY, $VALUE)", None),
    ):
        command = ["ast-grep", "run", "--lang", "lua", "--pattern", pattern, "--json=compact"]
        if selector:
            command += ["--selector", selector]
        result = subprocess.run(
            [*command, *map(str, files)],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        matches = ast_grep_matches(result.returncode, result.stdout, result.stderr, "preset private API scanner")
        for match in matches:
            variables = match["metaVariables"]["single"]
            table = _unwrap_parens(variables.get("TABLE", {}).get("text", ""))
            if table not in {"ui", "mare"}:
                continue
            if "KEY" in variables:
                key = preset_lua_string(variables["KEY"]["text"])
            else:
                key = variables.get("FIELD", {}).get("text", "")
            if key and key.startswith("_"):
                counts[f"{Path(match['file']).as_posix()}:{table}.{key}"] += 1
    return counts


def preset_lua_string(text: str) -> str | None:
    value = string_literal(text)
    if value is None or text.startswith("["):
        return value

    def unescape(match: re.Match[str]) -> str:
        escape = match[1]
        if escape.isdecimal():
            return chr(int(escape))
        if escape.startswith("x"):
            return chr(int(escape[1:], 16))
        if escape.startswith("u{"):
            return chr(int(escape[2:-1], 16))
        if escape.startswith("z"):
            return ""
        return {"a": "\a", "b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t", "v": "\v"}.get(escape, escape)

    return re.sub(r"\\(\d{1,3}|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|z\s*|\r?\n|.)", unescape, value)


def read_git_baseline_text(root: Path, rel_path: Path, base_ref: str) -> str:
    """Resolve `base_ref` in `root` and return `rel_path`'s content at that commit."""
    rev = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--verify", f"{base_ref}^{{commit}}"],
        capture_output=True,
        text=True,
        check=False,
    )
    if rev.returncode != 0 or not rev.stdout.strip():
        detail = rev.stderr.strip() or "not a valid commit"
        raise ValueError(f"invalid --base git ref {base_ref!r}: {detail}")
    base_sha = rev.stdout.strip()
    posix_rel = rel_path.as_posix()
    show = subprocess.run(
        ["git", "-C", str(root), "show", f"{base_sha}:{posix_rel}"],
        capture_output=True,
        text=True,
        check=False,
    )
    if show.returncode != 0:
        detail = show.stderr.strip() or f"missing `{posix_rel}`"
        raise ValueError(f"cannot read {posix_rel} at --base {base_ref} ({base_sha[:8]}): {detail}")
    return show.stdout


def baseline_growth_errors(
    current: dict[str, int] | Counter[str],
    base: dict[str, int] | Counter[str],
    base_ref: str,
    label: str,
) -> list[str]:
    """Fail when total baseline entries or any individual entry grew relative to `base_ref`."""
    errors: list[str] = []
    cur_total = sum(current.values())
    base_total = sum(base.values())
    if cur_total > base_total:
        errors.append(
            f"{label} baseline grew vs --base {base_ref}: total {cur_total} > base {base_total}"
        )
    for key in sorted(current.keys()):
        cur_n = current[key]
        base_n = base.get(key, 0)
        if cur_n > base_n:
            errors.append(
                f"{label} baseline entry grew vs --base {base_ref} for {key}: current {cur_n}, base {base_n}"
            )
    return errors


def preset_private_api_violations(root: Path, base: str | None = None) -> list[str]:
    del base  # Zero-tolerance: no allowlist or baseline remains.
    actual = preset_private_api_counts(root)
    return [
        f"forbidden preset private API {key}: found {actual[key]}"
        for key in sorted(actual.keys())
    ]


_BOOT_PRIMS_ALLOWED = "crates/mare-kernel/lua/mare_kernel/boot.lua"
_BOOT_ONLY_PRIMS = frozenset({"capture", "spawn_root", "run_root"})


def _lua_source_files(root: Path, *, include_specs: bool = False) -> list[Path]:
    dirs = [
        "crates/mare-kernel/lua/mare_kernel",
        "crates/mare-plugin/preset",
        "crates/mare-config/preset",
        "examples",
    ]
    if include_specs:
        dirs.append("crates/mare-kernel/lua-specs")
    types_root = root / "crates/mare-plugin/preset/types"
    files: list[Path] = []
    for rel_dir in dirs:
        base = root / rel_dir
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.lua")):
            if path.is_relative_to(types_root):
                continue
            files.append(path)
    return files


def boot_prims_violations(root: Path = ROOT) -> list[str]:
    """Only `crates/mare-kernel/lua/mare_kernel/boot.lua` may use `prims.{capture,spawn_root,run_root}`."""
    files = [
        path
        for path in _lua_source_files(root, include_specs=False)
        if path.relative_to(root).as_posix() != _BOOT_PRIMS_ALLOWED
    ]
    if not files:
        return []
    errors: list[tuple[tuple[str, int, int], str]] = []
    seen: set[tuple[str, int, int, str]] = set()
    for pattern, selector in (
        ("$TABLE.$FIELD", "dot_index_expression"),
        ("$TABLE:$FIELD()", "method_index_expression"),
        ("$TABLE[$KEY]", "bracket_index_expression"),
        ("rawget($TABLE, $KEY)", None),
        ("rawset($TABLE, $KEY, $VALUE)", None),
    ):
        command = ["ast-grep", "run", "--lang", "lua", "--pattern", pattern, "--json=compact"]
        if selector:
            command += ["--selector", selector]
        result = subprocess.run(
            [*command, *map(str, files)],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        for match in ast_grep_matches(result.returncode, result.stdout, result.stderr, "boot prims scanner"):
            variables = match["metaVariables"]["single"]
            table = _unwrap_parens(variables.get("TABLE", {}).get("text", ""))
            tail = table.rsplit(".", 1)[-1].rsplit(":", 1)[-1]
            if tail != "prims":
                continue
            key = (
                preset_lua_string(variables["KEY"]["text"])
                if "KEY" in variables
                else variables.get("FIELD", {}).get("text", "")
            )
            if key not in _BOOT_ONLY_PRIMS:
                continue
            path = Path(match["file"])
            path = path if path.is_absolute() else root / path
            rel_path = path.relative_to(root).as_posix()
            start = match["range"]["start"]
            dedup = (rel_path, start["line"], start["column"], key)
            if dedup in seen:
                continue
            seen.add(dedup)
            errors.append(
                (
                    (rel_path, start["line"], start["column"]),
                    f"{rel_path}:{start['line'] + 1}:{start['column'] + 1}: boot-prims: "
                    f"only `{_BOOT_PRIMS_ALLOWED}` may access `prims.{key}`",
                )
            )
    return [message for _, message in sorted(errors)]


def third_party_require_warnings(plugins_dir: Path, types_dir: Path | None = None) -> list[str]:
    """Warn (never fail) when a third-party plugin requires non-owned, non-`---@meta` modules."""
    if not plugins_dir.is_dir():
        return []
    public = public_modules(types_dir if types_dir is not None else ROOT / "crates/mare-plugin/preset/types")
    warnings: list[str] = []
    for entry in sorted(plugins_dir.iterdir()):
        if entry.name.startswith(("_", ".")):
            continue
        owned_modules: set[str] = set()
        files: list[Path] = []
        if entry.is_file() and entry.suffix == ".lua":
            files = [entry]
        elif entry.is_dir() and (entry / "init.lua").is_file():
            files = [entry / "init.lua"]
            lua_root = entry / "lua"
            if lua_root.is_dir():
                for mod_file in sorted(lua_root.rglob("*.lua")):
                    files.append(mod_file)
                    parts = list(mod_file.relative_to(lua_root).with_suffix("").parts)
                    if parts and parts[-1] == "init":
                        parts.pop()
                    if parts:
                        owned_modules.add(".".join(parts))
        else:
            continue
        result = subprocess.run(
            ["ast-grep", "run", "--lang", "lua", "--pattern", "require $ARG", "--json=compact", *map(str, files)],
            cwd=plugins_dir,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        for match in ast_grep_matches(result.returncode, result.stdout, result.stderr, "third-party require scanner"):
            arg = _unwrap_parens(match["metaVariables"]["single"]["ARG"]["text"])
            lit = re.fullmatch(r"(['\"])([\w.-]+)\1", arg)
            long_lit = re.fullmatch(r"\[(=*)\[([\w.-]+)\]\1\]", arg)
            module = lit[2] if lit else long_lit[2] if long_lit else None
            if module in public or module in owned_modules:
                continue
            start = match["range"]["start"]
            rel = Path(match["file"])
            rel = rel.relative_to(plugins_dir) if rel.is_absolute() else rel
            detail = f"undeclared module {module!r}" if module else "dynamic require"
            warnings.append(
                f"warning: {rel.as_posix()}:{start['line'] + 1}:{start['column'] + 1}: "
                f"third-party plugin `{entry.stem if entry.is_file() else entry.name}` requires {detail}"
            )
    return sorted(warnings)


_CORDIS_METHOD_DEF_RE = re.compile(r"function\s+[A-Za-z0-9_.]+\s*[.:]\s*(_[A-Za-z][A-Za-z0-9_]*)\s*\(")
_CORDIS_SELF_FIELD_RE = re.compile(r"\bself\.(_[A-Za-z][A-Za-z0-9_]*)")
# Removed private methods promoted to public APIs in cordis-lua v1.0.0; keep forbidden.
_CORDIS_REMOVED_PRIVATES = frozenset({"_getImpl", "_resolve"})
# Fiber-internal `_remove` is caught on fiber receivers via `_is_cordis_fiber_receiver`;
# non-fiber tables (e.g. tools/registry.lua `def._remove`) legitimately use `_remove`.
_CORDIS_FIBER_ONLY_FIELDS = frozenset({"_remove"})

_FIBER_RECEIVER_RE = re.compile(r"(?:^|[._])fiber$", re.IGNORECASE)


def cordis_private_members(root: Path = ROOT) -> tuple[frozenset[str], frozenset[str]]:
    """Compute Cordis single-underscore private fields and methods from the vendored library."""
    cordis_dir = root / "crates/mare-kernel/lua/cordis"
    if not cordis_dir.is_dir():
        cordis_dir = ROOT / "crates/mare-kernel/lua/cordis"
    methods: set[str] = set(_CORDIS_REMOVED_PRIVATES)
    fields: set[str] = set(_CORDIS_REMOVED_PRIVATES)
    for path in sorted(cordis_dir.rglob("*.lua")):
        text = path.read_text(encoding="utf-8")
        found_methods = set(_CORDIS_METHOD_DEF_RE.findall(text))
        methods.update(found_methods)
        fields.update(found_methods)
        fields.update(_CORDIS_SELF_FIELD_RE.findall(text))
    return frozenset(fields - _CORDIS_FIBER_ONLY_FIELDS), frozenset(methods)


def cordis_underscore_files(root: Path = ROOT) -> list[Path]:
    """Return all Lua source files outside `crates/mare-kernel/lua/cordis/**`."""
    return _lua_source_files(root, include_specs=True)


def _is_cordis_fiber_receiver(table: str) -> bool:
    tail = table.strip().rsplit(".", 1)[-1].rsplit(":", 1)[-1]
    return bool(_FIBER_RECEIVER_RE.search(table.strip())) or tail.lower().endswith("fiber")


def cordis_underscore_violations(
    files: list[Path],
    root: Path = ROOT,
) -> list[str]:
    """Forbid all Cordis underscore private members computed from the vendored library
    and any `fiber._*` access outside `crates/mare-kernel/lua/cordis/**`.
    """
    if not files:
        return []
    cordis_fields, cordis_methods = cordis_private_members(root)
    errors: list[tuple[tuple[str, int, int], str]] = []
    seen: set[tuple[str, int, int, str]] = set()
    for pattern, selector in (
        ("$TABLE.$FIELD", "dot_index_expression"),
        ("$TABLE:$FIELD()", "method_index_expression"),
        ("$TABLE[$KEY]", "bracket_index_expression"),
        ("rawget($TABLE, $KEY)", None),
        ("rawset($TABLE, $KEY, $VALUE)", None),
    ):
        command = ["ast-grep", "run", "--lang", "lua", "--pattern", pattern, "--json=compact"]
        if selector:
            command += ["--selector", selector]
        result = subprocess.run(
            [*command, *map(str, files)],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        matches = ast_grep_matches(
            result.returncode, result.stdout, result.stderr, "cordis underscore scanner"
        )
        for match in matches:
            variables = match["metaVariables"]["single"]
            table = _unwrap_parens(variables.get("TABLE", {}).get("text", ""))
            if not table:
                continue
            if "KEY" in variables:
                key = preset_lua_string(variables["KEY"]["text"])
            else:
                key = variables.get("FIELD", {}).get("text", "")
            if not key or not key.startswith("_") or key.startswith("__"):
                continue

            is_fiber_receiver = _is_cordis_fiber_receiver(table)
            private_pool = cordis_methods if selector == "method_index_expression" else cordis_fields
            if not ((key in private_pool) or is_fiber_receiver):
                continue

            path = Path(match["file"])
            path = path if path.is_absolute() else root / path
            rel_path = path.relative_to(root).as_posix()
            access = f"{table}.{key}"
            start = match["range"]["start"]
            dedup_key = (rel_path, start["line"], start["column"], access)
            if dedup_key in seen:
                continue
            seen.add(dedup_key)

            position = (rel_path, start["line"], start["column"])
            errors.append(
                (
                    position,
                    (
                        f"{rel_path}:{start['line'] + 1}:{start['column'] + 1}: cordis-underscore: "
                        f"forbidden Cordis internal access `{access}`"
                    ),
                )
            )
    return [message for _, message in sorted(errors)]


def self_test_cordis_underscore(base_dir: Path | None = None) -> None:
    """Verify Cordis underscore detection computed from the vendored library with zero allowlist."""
    with isolated_temp_dir("mare-cordis-underscore-test-", base_dir=base_dir) as directory:
        root = Path(directory)
        preset_probe = root / "crates/mare-plugin/preset/plugins/probe.lua"
        kernel_boot = root / "crates/mare-kernel/lua/mare_kernel/boot.lua"
        kernel_loader = root / "crates/mare-kernel/lua/mare_kernel/plugins/loader.lua"
        kernel_other = root / "crates/mare-kernel/lua/mare_kernel/other.lua"
        config_probe = root / "crates/mare-config/preset/probe.lua"
        example_probe = root / "examples/demo/init.lua"
        cordis_self = root / "crates/mare-kernel/lua/cordis/fiber.lua"
        for path in (preset_probe, kernel_boot, kernel_loader, kernel_other, config_probe, example_probe, cordis_self):
            path.parent.mkdir(parents=True, exist_ok=True)

        cordis_self.write_text(
            "self._internal = {}\n"
            "self._error = nil\n"
            "self._hooks = {}\n"
            "self._disposables = {}\n"
            "self._store = {}\n"
            "self._remove = nil\n"
            "function Future:_catch() end\n"
        )
        preset_probe.write_text(
            "local a = root.registry._internal\n"  # 1: violation
            "local b = fiber._error\n"  # 2: violation
            "events:_resolve('emit')\n"  # 3: violation
            "local c = events._hooks\n"  # 4: violation
            "ctx._disposables = {}\n"  # 5: violation
            "local d = loaderFiber._custom\n"  # 6: violation (fiber._*)
            "local e = rawget(fiber, '_secret')\n"  # 7: violation (fiber._*)
            "rawset(fiber, '_injected', true)\n"  # 8: violation (fiber._*)
            "local ok1 = local_obj._custom\n"  # 9: ok (non-fiber, non-Cordis)
            "local bad_store = fiber._store\n"  # 10: violation
            "local ok3 = rawget(other, '_other')\n"  # 11: ok
            "local ok4 = def._remove\n"  # 12: ok (non-fiber _remove)
            "local bad_rem = fiber._remove\n"  # 13: violation (fiber._remove)
        )
        # Zero allowlist: kernel files must also fail on every Cordis private member.
        kernel_boot.write_text(
            "for _, rt in root.registry._internal:pairs() do end\n"  # 1: violation
            "local e1 = fiber._error\n"  # 2: violation
            "local e2 = loaderFiber._error\n"  # 3: violation
            "reflect:_getImpl('svc', false)\n"  # 4: violation
            "fut:_catch(fn)\n"  # 5: violation
        )
        kernel_loader.write_text(
            "if fiber._store == nil then end\n"  # 1: violation
            "local ok = self._refused\n"  # 2: ok (loader's own field)
        )
        kernel_other.write_text("local e1 = fiber._error\nlocal e2 = loaderFiber._store\n")
        config_probe.write_text("local cfg_bad = fiber._error\n")
        example_probe.write_text("local ex_bad = events._hooks\n")

        discovered = cordis_underscore_files(root)
        assert cordis_self not in discovered, "cordis/ implementation must not be scanned"
        violations = cordis_underscore_violations(discovered, root=root)
        actual = [f"{err.split(':')[0]}:{err.split(':')[1]}" for err in violations]
        expected = [
            "crates/mare-config/preset/probe.lua:1",
            *(f"crates/mare-kernel/lua/mare_kernel/boot.lua:{i}" for i in (1, 2, 3, 4, 5)),
            *(f"crates/mare-kernel/lua/mare_kernel/other.lua:{i}" for i in (1, 2)),
            "crates/mare-kernel/lua/mare_kernel/plugins/loader.lua:1",
            *(f"crates/mare-plugin/preset/plugins/probe.lua:{i}" for i in (1, 2, 3, 4, 5, 6, 7, 8, 10, 13)),
            "examples/demo/init.lua:1",
        ]
        assert actual == expected, f"expected {expected}, got {actual} ({violations})"


from lua_api_apply_sync import builtin_apply_no_yield_violations  # noqa: E402, F401
