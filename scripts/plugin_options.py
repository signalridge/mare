#!/usr/bin/env python3
"""Per-plugin config schema extraction, LuaLS/doc rendering, and declared-key checking.

Sources of truth:
  crates/mare-plugin/preset/plugins/*.lua  (each plugin's `config_schema`)

Outputs:
  crates/mare-plugin/preset/types/mare/plugin_options.gen.lua
  Markdown section appended by scripts/gen_docs.py to docs/<lang>/configure/options.md

Usage:
  python3 -B scripts/plugin_options.py --check
  python3 -B scripts/plugin_options.py --self-test
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
PLUGINS_DIR = ROOT / "crates/mare-plugin/preset/plugins"
VALIDATE_LUA = ROOT / "crates/mare-config/preset/config_api_validate.lua"
PLUGIN_LUALS_OUT = ROOT / "crates/mare-plugin/preset/types/mare/plugin_options.gen.lua"

LUA_EXTRACT_PRESETS = r"""
local validate_path = arg[1]
local fn = assert(loadfile(validate_path))
local cfg = {}
local deps = {
  cfg = cfg,
  state = {},
  C = {},
  STRUCT = {},
  luatype = type,
  where = function() return nil end,
  in_config_scope = function() return false end,
  merge = function(a) return a end,
  deepcopy = function(v) return v end,
  validate_ui_config = function() end,
  validate_working = function() return true end,
  clear_sources = function() end,
  closest = function() return nil end,
  set_preset_state = function() end,
}
fn()({}, deps)
local choices = (cfg.plugin_preset_entry and cfg.plugin_preset_entry.choices) or {}
local parts = {}
for i = 1, #choices do
  parts[i] = string.format("%q", tostring(choices[i]))
end
io.write("[" .. table.concat(parts, ",") .. "]\n")
"""


def load_plugin_preset_choices(lua: str = "lua", validate_lua: Path = VALIDATE_LUA) -> list[str]:
    res = subprocess.run(
        [lua, "-", str(validate_lua)],
        input=LUA_EXTRACT_PRESETS,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(res.stdout)

HEADINGS = {
    "en": (
        "## Plugin options",
        'Built-in plugin options declared via `config_schema`, set with `mare.plugin("<name>", { ... })`. '
        "Every plugin also accepts `enabled = false` to disable it.",
    ),
    "zh": (
        "## 插件选项",
        '内置插件通过 `config_schema` 声明的选项，用 `mare.plugin("<name>", { ... })` 设置。'
        "每个插件也支持 `enabled = false` 来禁用。",
    ),
    "ja": (
        "## プラグインオプション",
        '組み込みプラグインが `config_schema` で宣言するオプション。`mare.plugin("<name>", { ... })` で設定します。'
        "各プラグインは `enabled = false` で無効化することもできます。",
    ),
}

COLS = {
    "en": ("Option", "Type", "Default", "Description"),
    "zh": ("选项", "类型", "默认值", "说明"),
    "ja": ("オプション", "型", "既定値", "説明"),
}

LUA_EXTRACT = r"""
local plugins_dir = arg[1]
package.path = plugins_dir .. "/lua/?.lua;" .. plugins_dir .. "/lua/?/init.lua;" .. package.path

local function esc(s)
  return s:gsub("\\", "\\\\"):gsub('"', '\\"'):gsub("\n", "\\n"):gsub("\r", "\\r"):gsub("\t", "\\t")
end

local function is_array(t)
  local n = 0
  for k in pairs(t) do
    if type(k) ~= "number" or math.type(k) ~= "integer" or k < 1 then
      return false
    end
    n = n + 1
  end
  return n > 0 and n == #t
end

local function encode(v, depth)
  depth = depth or 0
  if depth > 16 then return "null" end
  local tv = type(v)
  if tv == "nil" then return "null"
  elseif tv == "boolean" then return v and "true" or "false"
  elseif tv == "number" then
    if v ~= v or v == math.huge or v == -math.huge then return "null" end
    return tostring(v)
  elseif tv == "string" then
    return '"' .. esc(v) .. '"'
  elseif tv == "table" then
    if is_array(v) then
      local parts = {}
      for i = 1, #v do parts[i] = encode(v[i], depth + 1) end
      return "[" .. table.concat(parts, ",") .. "]"
    end
    local keys = {}
    for k in pairs(v) do
      if type(k) == "string" then keys[#keys + 1] = k end
    end
    table.sort(keys)
    local parts = {}
    for _, k in ipairs(keys) do
      if type(v[k]) ~= "function" then
        parts[#parts + 1] = '"' .. esc(k) .. '":' .. encode(v[k], depth + 1)
      end
    end
    return "{" .. table.concat(parts, ",") .. "}"
  end
  return "null"
end

local function make_stub_mare()
  local stub
  stub = setmetatable({}, {
    __index = function() return stub end,
    __call = function() return stub end,
  })
  return {
    os = "linux",
    arch = "x86_64",
    mode = "host",
    json = { null = {} },
    ui = stub,
  }
end

local out = {}
for i = 2, #arg do
  local path = arg[i]
  local stem = path:match("([^/\\]+)%.lua$")
  local env = setmetatable({ mare = make_stub_mare() }, { __index = _G })
  local chunk = assert(loadfile(path, "t", env))
  local ok, def = pcall(chunk)
  if ok and type(def) == "table" and type(def.config_schema) == "table" then
    local name = type(def.name) == "string" and def.name or stem
    out[#out + 1] = {
      name = name,
      file = stem .. ".lua",
      schema = def.config_schema,
    }
  end
end
table.sort(out, function(a, b) return a.name < b.name end)
io.write(encode(out), "\n")
"""


def load_plugin_schemas(lua: str = "lua", plugins_dir: Path = PLUGINS_DIR) -> list[dict[str, Any]]:
    files = sorted(str(p) for p in plugins_dir.glob("*.lua"))
    res = subprocess.run(
        [lua, "-", str(plugins_dir), *files],
        input=LUA_EXTRACT,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(res.stdout)


def _pascal_name(name: str) -> str:
    return "".join(part.capitalize() for part in re.split(r"[^A-Za-z0-9]+", name) if part)


def _schema_to_luals(entry: dict[str, Any]) -> str:
    ty = entry.get("type")
    if isinstance(ty, list):
        base = "|".join(str(t) for t in ty)
    elif ty == "enum":
        choices = entry.get("choices") or []
        parts = [json.dumps(c) for c in choices]
        if entry.get("choices_from"):
            parts.append("string")
        base = "|".join(parts) if parts else "string"
    elif ty == "string":
        choices = entry.get("choices") or []
        if choices:
            parts = [json.dumps(c) for c in choices]
            if entry.get("choices_from"):
                parts.append("string")
            base = "|".join(parts)
        else:
            base = "string"
    elif ty in ("integer", "number", "boolean"):
        base = ty
    elif ty == "list":
        items = entry.get("items")
        if isinstance(items, dict):
            inner = _schema_to_luals(items)
            base = f"({inner})[]" if "|" in inner else f"{inner}[]"
        else:
            base = "any[]"
    elif ty == "map":
        values = entry.get("values")
        val_t = _schema_to_luals(values) if isinstance(values, dict) else "any"
        base = f"table<string, {val_t}>"
    elif ty == "object":
        fields = entry.get("fields")
        if isinstance(fields, dict) and fields:
            parts = [f"{k}?: {_schema_to_luals(fields[k])}" for k in sorted(fields)]
            base = "{ " + ", ".join(parts) + " }"
        else:
            base = "table"
    elif ty == "union":
        variants = entry.get("variants") or []
        parts = [_schema_to_luals(v) for v in variants if isinstance(v, dict)]
        base = "|".join(parts) if parts else "any"
    else:
        base = "any"
    if entry.get("nullable") and not base.endswith("|nil"):
        base = f"{base}|nil"
    return base


def _clean_desc(desc: str | None) -> str:
    return re.sub(r"[\t\r\n]+", " ", (desc or "")).strip()


def render_plugin_luals(
    plugins: list[dict[str, Any]],
    preset_choices: list[str] | None = None,
    lua: str = "lua",
) -> str:
    if preset_choices is None:
        preset_choices = load_plugin_preset_choices(lua)
    lines = [
        "---@meta _",
        "-- @generated by scripts/gen_options.py from crates/mare-plugin/preset/plugins/*.lua.",
        "-- Do not edit by hand.",
        "",
    ]
    non_empty = [p for p in plugins if p.get("schema")]
    for plugin in non_empty:
        name = plugin["name"]
        cls = f"mare.PluginOpts.{_pascal_name(name)}"
        lines.append(f"--- Options for built-in plugin `{name}` (`mare.plugin(\"{name}\", {{ ... }})`).")
        lines.append(f"---@class {cls}")
        lines.append("---@field enabled? boolean Enable or disable this plugin.")
        schema = plugin["schema"]
        for key in sorted(schema):
            entry = schema[key]
            if not isinstance(entry, dict):
                continue
            luals_t = _schema_to_luals(entry)
            desc = _clean_desc(entry.get("desc"))
            env_var = entry.get("env")
            if env_var:
                suffix = f"(env: {env_var})"
                desc = f"{desc} {suffix}".strip()
            lines.append(f"---@field {key}? {luals_t} {desc}" if desc else f"---@field {key}? {luals_t}")
        lines.append("")

    preset_union = "|".join(json.dumps(c) for c in preset_choices) if preset_choices else "string"
    lines.append("--- Map of built-in plugin names to their typed option tables.")
    lines.append("---@class mare.PluginConfigMap")
    lines.append(f"---@field preset? {preset_union} Built-in plugin preset.")
    for plugin in non_empty:
        name = plugin["name"]
        cls = f"mare.PluginOpts.{_pascal_name(name)}"
        key = name if re.match(r"^[A-Za-z_][A-Za-z0-9_]*$", name) else f'["{name}"]'
        lines.append(f"---@field {key}? {cls}")
    lines.append("")
    return "\n".join(lines)


def _fmt_default(val: Any) -> str:
    if val is None:
        return ""
    if isinstance(val, bool):
        return "true" if val else "false"
    if isinstance(val, (int, float)):
        return str(val)
    if isinstance(val, str):
        return json.dumps(val, ensure_ascii=False)
    if isinstance(val, list):
        return "{ " + ", ".join(_fmt_default(x) for x in val) + " }" if val else "{}"
    if isinstance(val, dict):
        if not val:
            return "{}"
        parts = [f"{k} = {_fmt_default(val[k])}" for k in sorted(val)]
        return "{ " + ", ".join(parts) + " }"
    return str(val)


def _fmt_doc_type(entry: dict[str, Any]) -> str:
    ty = entry.get("type")
    if isinstance(ty, list):
        base = " or ".join(str(t) for t in ty)
    elif ty == "enum" and entry.get("choices"):
        parts = [str(c) for c in entry["choices"]]
        if entry.get("choices_from"):
            parts.append(f"<{entry['choices_from']}>")
        base = "|".join(parts)
    elif ty == "string" and entry.get("choices"):
        parts = [str(c) for c in entry["choices"]]
        if entry.get("choices_from"):
            parts.append(f"<{entry['choices_from']}>")
        base = "|".join(parts)
    elif ty == "union" and entry.get("variants"):
        base = " or ".join(_fmt_doc_type(v) for v in entry["variants"] if isinstance(v, dict))
    else:
        base = str(ty or "any")
    if entry.get("nullable"):
        base += "|null"
    return base


def _flatten_schema(schema: dict[str, Any], prefix: str = "") -> list[tuple[str, dict[str, Any]]]:
    rows: list[tuple[str, dict[str, Any]]] = []
    for key in sorted(schema):
        entry = schema[key]
        if not isinstance(entry, dict):
            continue
        path = f"{prefix}.{key}" if prefix else key
        fields = entry.get("fields")
        if entry.get("type") == "object" and isinstance(fields, dict) and fields:
            rows.extend(_flatten_schema(fields, path))
        else:
            rows.append((path, entry))
    return rows


def render_plugin_options_md(lua: str, lang: str, cell_fn) -> list[str]:
    plugins = [p for p in load_plugin_schemas(lua) if p.get("schema")]
    heading, intro = HEADINGS[lang]
    cols = COLS[lang]
    out = ["", heading, "", intro]
    for plugin in plugins:
        rows = _flatten_schema(plugin["schema"])
        if not rows:
            continue
        out += [
            "",
            f"### `{plugin['name']}`",
            "",
            f"| {cols[0]} | {cols[1]} | {cols[2]} | {cols[3]} |",
            "|---|---|---|---|",
        ]
        for path, entry in rows:
            ty = _fmt_doc_type(entry)
            default = _fmt_default(entry.get("default"))
            desc = _clean_desc(entry.get("desc"))
            if env_var := entry.get("env"):
                env_note = f"**Env:** `{env_var}`."
                desc = f"{env_note} {desc}".strip()
            default_cell = f"`{cell_fn(default)}`" if default else ""
            out.append(f"| `{path}` | {cell_fn(ty)} | {default_cell} | {cell_fn(desc)} |")
    return out


# --- Static check: every `cfg.<key>` read in built-in plugins is declared in `config_schema` ---

def _tokenize_lua(src: str) -> list[tuple[str, str, int]]:
    """Minimal Lua 5.4 tokenizer returning (kind, text, line) tokens, skipping comments."""
    tokens: list[tuple[str, str, int]] = []
    i, n, line = 0, len(src), 1
    while i < n:
        ch = src[i]
        if ch in " \t\r":
            i += 1
            continue
        if ch == "\n":
            line += 1
            i += 1
            continue
        # comments
        if ch == "-" and i + 1 < n and src[i + 1] == "-":
            i += 2
            if i < n and src[i] == "[":
                m = re.match(r"\[(=*)\[", src[i:])
                if m:
                    end_pat = "]" + m.group(1) + "]"
                    end = src.find(end_pat, i + len(m.group(0)))
                    chunk = src[i:] if end == -1 else src[i : end + len(end_pat)]
                    line += chunk.count("\n")
                    i = n if end == -1 else end + len(end_pat)
                    continue
            nl = src.find("\n", i)
            i = n if nl == -1 else nl
            continue
        # long strings [[...]]
        if ch == "[":
            m = re.match(r"\[(=*)\[", src[i:])
            if m:
                start_line = line
                end_pat = "]" + m.group(1) + "]"
                end = src.find(end_pat, i + len(m.group(0)))
                raw = src[i + len(m.group(0)) :] if end == -1 else src[i + len(m.group(0)) : end]
                line += raw.count("\n")
                tokens.append(("str", raw, start_line))
                i = n if end == -1 else end + len(end_pat)
                continue
        # quoted strings
        if ch in ("'", '"'):
            quote, start_line = ch, line
            i += 1
            buf = []
            while i < n and src[i] != quote:
                if src[i] == "\\" and i + 1 < n:
                    if src[i + 1] == "\n":
                        line += 1
                    buf.append(src[i + 1])
                    i += 2
                else:
                    if src[i] == "\n":
                        line += 1
                    buf.append(src[i])
                    i += 1
            i = min(n, i + 1)
            tokens.append(("str", "".join(buf), start_line))
            continue
        # identifiers / keywords
        if ch.isalpha() or ch == "_":
            j = i + 1
            while j < n and (src[j].isalnum() or src[j] == "_"):
                j += 1
            tokens.append(("ident", src[i:j], line))
            i = j
            continue
        # numbers
        if ch.isdigit():
            j = i + 1
            while j < n and (src[j].isalnum() or src[j] in "._"):
                j += 1
            tokens.append(("num", src[i:j], line))
            i = j
            continue
        # punctuation
        tokens.append(("sym", ch, line))
        i += 1
    return tokens


def _parse_params(tokens: list[tuple[str, str, int]], open_paren: int) -> tuple[list[str], int]:
    params: list[str] = []
    j = open_paren + 1
    n = len(tokens)
    while j < n and tokens[j][1] != ")":
        if tokens[j][0] == "ident":
            params.append(tokens[j][1])
        j += 1
    return params, j


def _read_field_after(tokens: list[tuple[str, str, int]], idx: int) -> tuple[str, int] | None:
    """If `tokens[idx]` is followed by `.key` or `["key"]`, return `(key, end_idx)`."""
    n = len(tokens)
    if idx + 2 < n and tokens[idx + 1][1] == "." and tokens[idx + 2][0] == "ident":
        return tokens[idx + 2][1], idx + 2
    if (
        idx + 3 < n
        and tokens[idx + 1][1] == "["
        and tokens[idx + 2][0] == "str"
        and tokens[idx + 3][1] == "]"
    ):
        return tokens[idx + 2][1], idx + 3
    return None


def _scan_scope_cfg_reads(
    tokens: list[tuple[str, str, int]],
    start: int,
    initial_cfg: set[str],
    initial_deps: set[str],
    stop_on_root_end: bool,
    bind_module_cfg_param: bool = False,
) -> list[tuple[str, int]]:
    """Lexical scanner tracking plugin `cfg` variables and `D.cfg` aliases across Lua blocks."""
    n = len(tokens)
    reads: list[tuple[str, int]] = []
    # Each frame is [cfg_vars: set[str], deps_vars: set[str], expects_do: bool].
    stack: list[list[Any]] = [[set(initial_cfg), set(initial_deps), False]]
    i = start
    while i < n and stack:
        kind, text, line = tokens[i]
        cur_cfg, cur_deps, _ = stack[-1]

        if kind == "ident" and text in ("if", "repeat"):
            stack.append([set(cur_cfg), set(cur_deps), False])
            i += 1
            continue
        if kind == "ident" and text in ("for", "while"):
            next_cfg, next_deps = set(cur_cfg), set(cur_deps)
            if text == "for":
                j = i + 1
                while j < n and tokens[j][1] not in ("=", "in", "do"):
                    if tokens[j][0] == "ident":
                        next_cfg.discard(tokens[j][1])
                        next_deps.discard(tokens[j][1])
                    j += 1
            stack.append([next_cfg, next_deps, True])
            i += 1
            continue
        if kind == "ident" and text == "do":
            if stack[-1][2]:
                stack[-1][2] = False
            else:
                stack.append([set(cur_cfg), set(cur_deps), False])
            i += 1
            continue
        if kind == "ident" and text == "function":
            next_cfg, next_deps = set(cur_cfg), set(cur_deps)
            is_mod_fn = False
            prev_tok = tokens[i - 1][1] if i > 0 else ""
            j = i + 1
            while j < n and tokens[j][1] != "(":
                if tokens[j][1] in (".", ":"):
                    is_mod_fn = True
                j += 1
            if j < n and tokens[j][1] == "(":
                params, close_j = _parse_params(tokens, j)
                for p in params:
                    next_cfg.discard(p)
                    next_deps.discard(p)
                if bind_module_cfg_param and len(stack) == 1 and prev_tok == "return":
                    if "cfg" in params:
                        next_cfg.add("cfg")
                    elif len(params) == 1 and params[0] in ("D", "deps"):
                        next_deps.add(params[0])
                elif bind_module_cfg_param and is_mod_fn and "cfg" in params and not cur_cfg and not cur_deps:
                    next_cfg.add("cfg")
                stack.append([next_cfg, next_deps, False])
                i = close_j + 1
                continue
            stack.append([next_cfg, next_deps, False])
            i += 1
            continue
        if kind == "ident" and text in ("end", "until"):
            if len(stack) > 1 or stop_on_root_end:
                stack.pop()
            i += 1
            continue
        if kind == "ident" and text == "local":
            if i + 1 < n and tokens[i + 1][1] != "function":
                lhs: list[str] = []
                j = i + 1
                while j < n and tokens[j][0] == "ident":
                    lhs.append(tokens[j][1])
                    j += 1
                    if j < n and tokens[j][1] == ",":
                        j += 1
                    else:
                        break
                # Parse RHS expressions when `=` follows so `local ctx, cfg = D.ctx, D.cfg` binds `cfg`,
                # while `local cfg = D.cfg.sensitive` shadows `cfg` (and records `sensitive` below).
                aliased_cfg: set[str] = set()
                if j < n and tokens[j][1] == "=" and cur_deps:
                    expr_idx = 0
                    k = j + 1
                    paren = 0
                    brace = 0
                    expr_start = k
                    while k < n and expr_idx < len(lhs):
                        tk, tt, _ = tokens[k]
                        if tt in ("(", "["):
                            paren += 1
                        elif tt in (")", "]"):
                            paren = max(0, paren - 1)
                        elif tt == "{":
                            brace += 1
                        elif tt == "}":
                            brace = max(0, brace - 1)
                        at_sep = paren == 0 and brace == 0 and tt == ","
                        at_stmt_end = paren == 0 and brace == 0 and (
                            tt == ";"
                            or (
                                k > expr_start
                                and tk == "ident"
                                and tt in ("local", "return", "if", "for", "while", "do", "end", "function")
                            )
                        )
                        if at_sep or at_stmt_end or k == n - 1:
                            end_k = k if (at_sep or at_stmt_end) else k + 1
                            seg = tokens[expr_start:end_k]
                            if (
                                len(seg) == 3
                                and seg[0][0] == "ident"
                                and seg[0][1] in cur_deps
                                and seg[1][1] == "."
                                and seg[2][1] == "cfg"
                            ):
                                aliased_cfg.add(lhs[expr_idx])
                            expr_idx += 1
                            if at_stmt_end:
                                break
                            expr_start = k + 1
                        k += 1
                for name in lhs:
                    cur_cfg.discard(name)
                    cur_deps.discard(name)
                cur_cfg.update(aliased_cfg)
            i += 1
            continue

        if kind == "ident":
            prev = tokens[i - 1][1] if i > 0 else ""
            if prev not in (".", ":"):
                if text in cur_cfg:
                    hit = _read_field_after(tokens, i)
                    if hit:
                        reads.append((hit[0], line))
                elif text in cur_deps:
                    first = _read_field_after(tokens, i)
                    if first and first[0] == "cfg":
                        second = _read_field_after(tokens, first[1])
                        if second:
                            reads.append((second[0], line))
        i += 1
    return reads


def find_cfg_reads(src: str) -> tuple[str | None, list[tuple[str, int]]]:
    """Find the plugin `apply(ctx, <cfg>)` parameter name and top-level `<cfg>.<key>` reads."""
    tokens = _tokenize_lua(src)
    n = len(tokens)
    cfg_param: str | None = None
    body_start = -1
    for i in range(n - 6):
        if (
            tokens[i][1] == "apply"
            and tokens[i + 1][1] == "="
            and tokens[i + 2][1] == "function"
            and tokens[i + 3][1] == "("
        ):
            params, close_j = _parse_params(tokens, i + 3)
            if len(params) >= 2:
                cfg_param = params[1]
                body_start = close_j + 1
            break
    if not cfg_param or body_start < 0:
        return None, []
    return cfg_param, _scan_scope_cfg_reads(tokens, body_start, {cfg_param}, set(), stop_on_root_end=True)


def find_module_cfg_reads(src: str) -> list[tuple[str, int]]:
    """Find top-level plugin `cfg.<key>` reads inside a dual-layer module under `plugins/lua/<plugin>/`."""
    tokens = _tokenize_lua(src)
    return _scan_scope_cfg_reads(
        tokens,
        0,
        set(),
        set(),
        stop_on_root_end=False,
        bind_module_cfg_param=True,
    )


def check_declared_keys(root: Path = ROOT, lua: str = "lua") -> list[str]:
    """Verify every `cfg.<key>` read in built-in plugins and `plugins/lua/**` is declared in `config_schema`."""
    plugins_dir = root / "crates/mare-plugin/preset/plugins"
    schemas = load_plugin_schemas(lua, plugins_dir)
    schemas_by_file = {p["file"]: p for p in schemas}
    schemas_by_name = {p["name"]: p for p in schemas}
    errors: list[str] = []

    def report_undeclared(path: Path, plugin_name: str, declared: set[str], reads: list[tuple[str, int]]) -> None:
        rel = path.relative_to(root).as_posix()
        seen: set[tuple[str, int]] = set()
        for key, line in reads:
            if key not in declared and (key, line) not in seen:
                seen.add((key, line))
                errors.append(f"{rel}:{line}: plugin `{plugin_name}` reads undeclared config key `{key}`")

    for path in sorted(plugins_dir.glob("*.lua")):
        src = path.read_text(encoding="utf-8")
        _, reads = find_cfg_reads(src)
        if not reads:
            continue
        info = schemas_by_file.get(path.name)
        declared = set((info or {}).get("schema", {}).keys()) | {"enabled"}
        plugin_name = (info or {}).get("name", path.stem)
        report_undeclared(path, plugin_name, declared, reads)

    lua_dir = plugins_dir / "lua"
    if lua_dir.is_dir():
        for subdir in sorted(p for p in lua_dir.iterdir() if p.is_dir()):
            owner_file = plugins_dir / f"{subdir.name}.lua"
            if not owner_file.exists() and subdir.name not in schemas_by_name:
                continue
            info = schemas_by_name.get(subdir.name) or schemas_by_file.get(owner_file.name)
            declared = set((info or {}).get("schema", {}).keys()) | {"enabled"}
            plugin_name = (info or {}).get("name", subdir.name)
            for mod_path in sorted(subdir.rglob("*.lua")):
                reads = find_module_cfg_reads(mod_path.read_text(encoding="utf-8"))
                if reads:
                    report_undeclared(mod_path, plugin_name, declared, reads)
    return errors


def _run_self_test() -> None:
    sample_ok = """
    return {
      name = "demo",
      config_schema = { foo = { type = "string", default = "a" } },
      apply = function(ctx, cfg)
        local x = cfg.foo
        local y = cfg["enabled"]
        -- cfg.commented_out should be ignored
        local s = "cfg.in_string"
        local function helper(cfg)
          return cfg.shadowed_param
        end
        if true then
          local cfg = { bar = 1 }
          local z = cfg.bar
        end
        return x, y, s, helper
      end,
    }
    """
    _, reads = find_cfg_reads(sample_ok)
    assert [k for k, _ in reads] == ["foo", "enabled"], f"unexpected reads: {reads}"

    sample_bad = """
    return {
      name = "demo",
      apply = function(ctx, cfg)
        return cfg.declared, cfg.undeclared_typo
      end,
    }
    """
    _, bad_reads = find_cfg_reads(sample_bad)
    assert [k for k, _ in bad_reads] == ["declared", "undeclared_typo"], f"unexpected bad_reads: {bad_reads}"

    # Standalone `do ... end` must NOT prematurely pop the `apply` scope.
    sample_do_block = """
    return {
      name = "demo",
      apply = function(ctx, cfg)
        do
          local tmp = 1
        end
        return cfg.after_do
      end,
    }
    """
    _, do_reads = find_cfg_reads(sample_do_block)
    assert [k for k, _ in do_reads] == ["after_do"], f"standalone do..end dropped later cfg reads: {do_reads}"

    # Dual-layer plugin submodule (`plugins/lua/<name>/*.lua`) reads via D.cfg / local cfg = D.cfg.
    sample_module = """
    return function(D)
      local ctx, cfg = D.ctx, D.cfg
      local a = cfg.declared_key
      local b = D.cfg.direct_key
      do
        local sub = D.cfg.sub_obj or {}
        local ignored = sub.nested_field
      end
      return a, b, cfg.undeclared_mod_key
    end
    """
    mod_reads = find_module_cfg_reads(sample_module)
    assert [k for k, _ in mod_reads] == [
        "declared_key",
        "direct_key",
        "sub_obj",
        "undeclared_mod_key",
    ], f"unexpected module reads: {mod_reads}"

    # Sub-object alias `local cfg = D.cfg.sensitive or {}` must record `sensitive` and NOT flag inner fields.
    sample_sub_alias = """
    return function(D)
      local cfg = D.cfg.sensitive or {}
      return cfg.disable, cfg.patterns
    end
    """
    sub_reads = find_module_cfg_reads(sample_sub_alias)
    assert [k for k, _ in sub_reads] == ["sensitive"], f"unexpected sub_alias reads: {sub_reads}"

    # Rendered PluginConfigMap.preset must match runtime PLUGIN_PRESETS (no nonexistent "standard").
    rendered = render_plugin_luals(load_plugin_schemas())
    assert '"standard"' not in rendered, "PluginConfigMap.preset must not emit invalid preset literal 'standard'"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--lua", default="lua", help="Lua 5.4+ interpreter")
    ap.add_argument("--check", action="store_true", help="verify generated LuaLS file and declared keys")
    ap.add_argument("--self-test", action="store_true", help="run built-in self-tests")
    args = ap.parse_args()

    if args.self_test:
        _run_self_test()

    plugins = load_plugin_schemas(args.lua)
    content = render_plugin_luals(plugins, lua=args.lua)
    failed = False
    if args.check:
        if not PLUGIN_LUALS_OUT.exists() or PLUGIN_LUALS_OUT.read_text(encoding="utf-8") != content:
            print(
                f"stale generated file: {PLUGIN_LUALS_OUT.relative_to(ROOT).as_posix()} "
                "(run python3 scripts/gen_options.py)",
                file=sys.stderr,
            )
            failed = True
    elif not args.self_test:
        PLUGIN_LUALS_OUT.write_text(content, encoding="utf-8")

    if args.check or not args.self_test:
        errs = check_declared_keys(ROOT, args.lua)
        if errs:
            print("undeclared plugin config key reads:", *errs, sep="\n  ", file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
