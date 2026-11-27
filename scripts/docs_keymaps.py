"""Keymap table generation and OS-independent platform annotations for gen_docs.py."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Callable

KEYMAPS_BEGIN = "<!-- BEGIN GENERATED KEYMAPS (scripts/gen_docs.py) -->"
KEYMAPS_END = "<!-- END GENERATED KEYMAPS -->"
KEYMAP_CONTEXT_ORDER = (
    "global", "editor", "text", "input", "transcript", "select", "select.vim", "confirm", "help", "info",
)
KEY_TOKEN_NAMES = {
    "ctrl": "Ctrl",
    "alt": "Alt",
    "shift": "Shift",
    "super": "Super",
    "esc": "Esc",
    "enter": "Enter",
    "tab": "Tab",
    "space": "Space",
    "backspace": "Backspace",
    "pageup": "PageUp",
    "pagedown": "PageDown",
    "home": "Home",
    "end": "End",
    "left": "←",
    "right": "→",
    "up": "↑",
    "down": "↓",
}
_MOD_ORDER = {"c": (0, "ctrl"), "m": (1, "alt"), "s": (2, "shift"), "d": (3, "super")}
_KEY_ALIASES = {"cr": "enter", "bs": "backspace", "del": "delete", "spc": "space", "lt": "<"}
_LUA_PROBE = """
local function probe(os_name)
  local function record_one(ctx, key, action, opts)
    local contexts = type(ctx) == "table" and ctx or { ctx }
    local w = opts and opts.when
    local ws = type(w) == "table" and table.concat(w, ",") or (type(w) == "string" and w or "")
    for _, c in ipairs(contexts) do
      print(os_name .. "\\t" .. c .. "\\t" .. key .. "\\t" .. tostring(action) .. "\\t" .. ws)
    end
    return { remove = function() end }
  end
  _G.mare = {
    os = os_name,
    keymap = {
      set = record_one,
      set_many = function(specs, batch_opts)
        for _, spec in ipairs(specs) do
          local ctx = spec.context ~= nil and spec.context or spec[1]
          local key_spec = spec.keys ~= nil and spec.keys or (spec.key ~= nil and spec.key or spec[2])
          local action = spec.action ~= nil and spec.action or spec[3]
          local opts = spec[4] or batch_opts
          if spec.when ~= nil then
            opts = { when = spec.when }
          end
          local keys = type(key_spec) == "table" and key_spec or { key_spec }
          for _, k in ipairs(keys) do
            record_one(ctx, k, action, opts)
          end
        end
        return { remove = function() end }
      end,
    },
  }
  assert(loadfile(assert(os.getenv("MARE_KEYMAP_DEFAULT_PATH"))))().apply({ effect = function() end })
end
probe("linux")
probe("windows")
"""


def canon_key(seq: str) -> str:
    def one(tok: str) -> str:
        if not (tok.startswith("<") and tok.endswith(">")):
            return tok
        parts = tok[1:-1].split("-")
        mods: list[tuple[int, str]] = []
        i = 0
        while i < len(parts) - 1 and parts[i].lower() in _MOD_ORDER:
            mods.append(_MOD_ORDER[parts[i].lower()])
            i += 1
        mods.sort()
        base = "-".join(parts[i:]).lower()
        base = _KEY_ALIASES.get(base, base)
        return "+".join([m[1] for m in mods] + [base])

    return " ".join(one(part) for part in seq.split())


def format_key_label(seq: str) -> str:
    def one(k: str) -> str:
        parts = k.split("+")
        last = len(parts) - 1
        out = []
        for i, p in enumerate(parts):
            if p in KEY_TOKEN_NAMES:
                out.append(KEY_TOKEN_NAMES[p])
            elif p.startswith("f") and p[1:].isdigit():
                out.append(p.upper())
            elif i == last and len(p) == 1:
                out.append(p.upper())
            else:
                out.append(p)
        return "+".join(out)

    return " ".join(one(part) for part in seq.split())


def format_when(when: object, cell_fn: Callable[[str], str]) -> str:
    if isinstance(when, str) and when:
        return f"`{cell_fn(when)}`"
    if isinstance(when, list) and when:
        return ", ".join(f"`{cell_fn(str(item))}`" for item in when)
    return ""


def _read_default_os_bindings(preset: Path, lua_cmd: str) -> tuple[dict, dict]:
    script = preset / "plugins/keymap-default.lua"
    res = subprocess.run(
        [lua_cmd, "-e", _LUA_PROBE],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=30,
        env={**os.environ, "MARE_KEYMAP_DEFAULT_PATH": str(script)},
        check=True,
    )
    by_os: dict[str, dict[tuple[str, str, str], list[str]]] = {"linux": {}, "windows": {}}
    for line in res.stdout.splitlines():
        os_name, ctx, key, action, when_csv = line.split("\t", 4)
        when_list = [w for w in when_csv.split(",") if w]
        by_os[os_name][(ctx, canon_key(key), action)] = when_list
    return by_os["linux"], by_os["windows"]


def normalize_platform_keymaps(
    keymaps: list[dict], actions: dict[str, str], preset: Path, lua_cmd: str
) -> list[dict]:
    linux_map, windows_map = _read_default_os_bindings(preset, lua_cmd)
    win_only, unix_only = set(windows_map) - set(linux_map), set(linux_map) - set(windows_map)
    descs = {k.get("action", ""): k.get("desc", "") for k in keymaps if k.get("desc")}
    descs.update(actions)
    out: list[dict] = []
    present: set[tuple[str, str, str]] = set()
    for item in keymaps:
        row = dict(item)
        sig = (row.get("context", ""), row.get("key", ""), row.get("action", ""))
        present.add(sig)
        tag = "Windows" if sig in win_only else ("Unix / macOS" if sig in unix_only else None)
        if tag:
            base = (row.get("desc") or "").strip()
            row["desc"] = f"{base} ({tag})" if base else f"({tag})"
        out.append(row)
    for tag, diff, src in (("Windows", win_only, windows_map), ("Unix / macOS", unix_only, linux_map)):
        for sig in sorted(diff - present):
            ctx, key, action = sig
            base = (descs.get(action) or "").strip()
            out.append({"context": ctx, "key": key, "action": action, "when": src[sig] or None, "desc": f"{base} ({tag})" if base else f"({tag})"})
    return out


def gen_keymaps_table(keymaps: list[dict], cols: tuple[str, ...], cell_fn: Callable[[str], str]) -> str:
    order_idx = {ctx: i for i, ctx in enumerate(KEYMAP_CONTEXT_ORDER)}
    lines = [f"| {cols[0]} | {cols[1]} | {cols[2]} | {cols[3]} | {cols[4]} |", "|---|---|---|---|---|"]
    ordered = sorted(
        keymaps,
        key=lambda item: (order_idx.get(item.get("context", ""), len(order_idx)), item.get("context", ""), item.get("key", ""), item.get("action", "")),
    )
    for item in ordered:
        lines.append(
            f"| `{cell_fn(str(item.get('context') or ''))}` | `{cell_fn(format_key_label(str(item.get('key') or '')))}` "
            f"| `{cell_fn(str(item.get('action') or ''))}` | {format_when(item.get('when'), cell_fn)} | {cell_fn(str(item.get('desc') or ''))} |"
        )
    return "\n".join(lines)


def update_keymaps_doc(root: Path, rel: str, keymaps: list[dict], cols: tuple[str, ...], cell_fn: Callable[[str], str]) -> str:
    text = (root / rel).read_text(encoding="utf-8")
    lines = text.splitlines()
    begin_occ, end_occ = sum(l.count(KEYMAPS_BEGIN) for l in lines), sum(l.count(KEYMAPS_END) for l in lines)
    begins = [i for i, l in enumerate(lines) if l.strip() == KEYMAPS_BEGIN]
    ends = [i for i, l in enumerate(lines) if l.strip() == KEYMAPS_END]
    if begin_occ != 1 or end_occ != 1 or len(begins) != 1 or len(ends) != 1 or ends[0] <= begins[0]:
        raise ValueError(
            f"{rel}: expected exactly one standalone {KEYMAPS_BEGIN!r} before {KEYMAPS_END!r} "
            f"(found {begin_occ}/{len(begins)} begin, {end_occ}/{len(ends)} end)"
        )
    head, tail = "\n".join(lines[: begins[0] + 1]), "\n".join(lines[ends[0] :])
    return f"{head}\n{gen_keymaps_table(keymaps, cols, cell_fn)}\n{tail}" + ("\n" if text.endswith("\n") else "")
