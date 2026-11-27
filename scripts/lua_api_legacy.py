"""Legacy Lua API removal checks (ast-grep scans across Lua and Rust Lua strings)."""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from lua_api_runner import ROOT, ast_grep_matches

# Legacy removal: the transition layer and the old Ctx are gone; plugins (and the plugins the
# tests write) use the cordis ctx. Names only the old Ctx had, called on a ctx-like receiver; the
# old single-name `inject("x")`; the `cordis = true` switch; a numeric `priority` on `:on`.
LEGACY_CTX_NAMES = [
    "on_dispose", "with_service", "emit_local", "emit_checked", "emit_checked_local", "bail_local",
    "waterfall_local", "waterfall_checked", "waterfall_checked_local", "policy", "policy_with",
    "rewrite", "on_local", "declare_local", "has_listeners", "has_local_listeners", "policy_epoch",
    "service", "provides", "permitted", "validate", "log", "reload", "reload_barrier", "plugins",
    "problems", "is_builtin", "profile", "profile_mark", "slow_callbacks", "callback_timings",
    "call_service",
]

LEGACY_RECEIVER = r"(?:\bctx|\bcaller|\bscope|\bowner|\bc|\bsub|self\.ctx|D\.ctx)"


LEGACY_PATTERNS = [
    (
        re.compile(
            LEGACY_RECEIVER
            + r"\s*:\s*("
            + "|".join(sorted(LEGACY_CTX_NAMES, key=len, reverse=True))
            + r")\s*\("
        ),
        "the old Ctx's `{0}` (the cordis ctx has none)",
    ),
    (re.compile(LEGACY_RECEIVER + r"\s*:\s*inject\s*\(\s*[\"']"), "the old single-name `inject(\"x\")`"),
    (re.compile(r"\bcordis\s*=\s*true\b"), "the removed `cordis = true` switch"),
]


LEGACY_DOC_MEMBER = re.compile(
    r"`(" + LEGACY_RECEIVER + r"\s*:\s*(?:" + "|".join(sorted(LEGACY_CTX_NAMES, key=len, reverse=True)) + r"))`"
)


LEGACY_SCAN = [
    ("crates/mare-cli/tests", ".rs"),
    # the docs show plugin code too (a Lua block, or a name in the prose): the same names are gone
    ("docs", ".md"),
]


ON_PRIORITY_FIELD_RULE = {
    "any": [
        {"pattern": {"context": "{ priority = $VALUE }", "selector": "field"}},
        {"pattern": {"context": '{ ["priority"] = $VALUE }', "selector": "field"}},
        {"pattern": {"context": "{ ['priority'] = $VALUE }", "selector": "field"}},
    ]
}


ON_PRIORITY_RULE = {
    "id": "legacy-on-priority",
    "language": "Lua",
    "rule": {
        "any": [
            {"pattern": "$CTX:on($EVENT, $HANDLER, $OPTS)"},
            {"pattern": "$CTX:on($EVENT, $HANDLER, $OPTS, $$$EXTRA)"},
        ]
    },
    "constraints": {
        "OPTS": {
            "any": [
                {"kind": "table_constructor", "has": ON_PRIORITY_FIELD_RULE},
                {
                    "kind": "parenthesized_expression",
                    "has": {
                        "kind": "table_constructor",
                        "has": ON_PRIORITY_FIELD_RULE,
                        "stopBy": {"not": {"kind": "parenthesized_expression"}},
                    },
                },
            ]
        }
    },
}


RUST_LUA_STRING_RULE = {
    "id": "legacy-lua-string",
    "language": "Rust",
    "rule": {
        "all": [
            {"any": [{"kind": "raw_string_literal"}, {"kind": "string_literal"}]},
            {"regex": r"(?s):\s*on\s*\("},
            {"regex": r"\bpriority\b"},
        ]
    },
}


def legacy_ast_scan(source: str, rule: dict, what: str) -> list[dict]:
    result = subprocess.run(
        ["ast-grep", "scan", "--inline-rules", json.dumps(rule), "--stdin", "--json=compact"],
        input=source,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    return ast_grep_matches(result.returncode, result.stdout, result.stderr, what)


def rust_lua_string(literal: str) -> tuple[str, list[int]]:
    """Decode a Rust string literal and map each decoded byte to its original byte offset."""
    data = literal.encode("utf-8")
    raw = re.match(rb'(?:b|c)?r(#{0,255})"', data)
    if raw:
        start = raw.end()
        end = len(data) - len(raw[1]) - 1
        return data[start:end].decode("utf-8"), list(range(start, end + 1))

    start = 2 if data.startswith((b'b"', b'c"')) else 1
    if data[start - 1 : start] != b'"' or not data.endswith(b'"'):
        raise RuntimeError("unexpected Rust string literal in legacy API scan")
    end = len(data) - 1
    decoded = bytearray()
    offsets = []
    escapes = {
        ord("n"): b"\n",
        ord("r"): b"\r",
        ord("t"): b"\t",
        ord("0"): b"\0",
        ord("\\"): b"\\",
        ord('"'): b'"',
        ord("'"): b"'",
    }
    i = start
    while i < end:
        origin = i
        if data[i] != ord("\\"):
            value = data[i : i + 1]
            i += 1
        else:
            i += 1
            if i >= end:
                raise RuntimeError("incomplete Rust string escape in legacy API scan")
            code = data[i]
            i += 1
            if code in escapes:
                value = escapes[code]
            elif code == ord("x"):
                value = bytes([int(data[i : i + 2], 16)])
                i += 2
            elif code == ord("u") and data[i : i + 1] == b"{":
                close = data.index(b"}", i + 1, end)
                value = chr(int(data[i + 1 : close].replace(b"_", b""), 16)).encode("utf-8")
                i = close + 1
            elif code in (ord("\n"), ord("\r")):
                while i < end and data[i] in b" \t\r\n":
                    i += 1
                continue
            else:
                raise RuntimeError("unsupported Rust string escape in legacy API scan")
        decoded.extend(value)
        offsets.extend([origin] * len(value))
    offsets.append(end)
    return decoded.decode("utf-8"), offsets


def legacy_on_priority_positions(path: Path, source: str) -> list[tuple[int, int]]:
    if not re.search(r":\s*on\s*\(", source) or not re.search(r"\bpriority\b", source):
        return []
    if path.suffix != ".rs":
        return [
            (match["range"]["start"]["line"] + 1, match["range"]["start"]["column"] + 1)
            for match in legacy_ast_scan(source, ON_PRIORITY_RULE, "Lua :on priority parser")
        ]

    positions = []
    original = source.encode("utf-8")
    for literal in legacy_ast_scan(source, RUST_LUA_STRING_RULE, "Rust Lua fixture parser"):
        lua, offsets = rust_lua_string(literal["text"])
        for match in legacy_ast_scan(lua, ON_PRIORITY_RULE, "Lua :on priority parser"):
            start = literal["range"]["byteOffset"]["start"] + offsets[match["range"]["byteOffset"]["start"]]
            before = original[:start]
            positions.append((before.count(b"\n") + 1, len(before.rsplit(b"\n", 1)[-1]) + 1))
    return positions


def legacy_api_violations(files: list[Path], root: Path = ROOT) -> list[str]:
    """Every use of an API the legacy removal took away (it replaces legacy-usage.py's count:
    the count is 0 and must stay 0)."""
    errors = []
    for path in files:
        source = path.read_text()
        found = []
        for number, line in enumerate(source.splitlines(), 1):
            stripped = line.lstrip()
            if stripped.startswith("--") or stripped.startswith("//"):
                continue
            for pattern, what in LEGACY_PATTERNS:
                for match in pattern.finditer(line):
                    found.append((number, match.start() + 1, what.format(*match.groups())))
            if path.suffix == ".md":
                for match in LEGACY_DOC_MEMBER.finditer(line):
                    found.append((number, match.start() + 1, f"the old Ctx member `{match.group(1)}`"))
        found += [
            (line, column, "a numeric `priority` (listeners run in registration order)")
            for line, column in legacy_on_priority_positions(path, source)
        ]
        for line, column, message in sorted(found):
            errors.append(f"{path.relative_to(root)}:{line}:{column}: legacy-api: {message}")
    return errors


def legacy_api_files(root: Path = ROOT) -> list[Path]:
    files = []
    # All in-repository Lua, including evals, examples, fixtures and extracted docs. Build output
    # and Git's own files are not repository sources; os.walk does not follow directory symlinks.
    for directory, subdirs, names in os.walk(root):
        subdirs[:] = [
            name for name in subdirs if name != ".git" and not (Path(directory) == root and name == "target")
        ]
        files += [Path(directory) / name for name in names if name.endswith(".lua")]
    for directory, suffix in LEGACY_SCAN:
        files += sorted((root / directory).rglob(f"*{suffix}"))
    return sorted(files)
