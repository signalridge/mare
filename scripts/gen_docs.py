#!/usr/bin/env python3
"""Generate the reference pages of the documentation from the code.

Sources of truth:
  docs/<lang>/reference/cli.md             `mare --help` and every subcommand's `--help`
  docs/<lang>/reference/slash-commands.md  command registrations in the built-in plugins
  docs/<lang>/configure/options.md         the option registry (mare-config/preset/config_api.lua)
  docs/<lang>/extend/lua-api.md            the LuaLS definitions (mare-plugin/preset/types)
  docs/<lang>/reference/rpc.md             `mare rpc schema` (mare-protocol)
  docs/<lang>/reference/events.md          protocol event registry (`protocol_events!` + `mare rpc schema`)

    scripts/gen_docs.py --mare target/debug/mare           # write
    scripts/gen_docs.py --mare target/debug/mare --check   # fail when a page is stale

Descriptions come from the code and stay in English in every language; only the page
titles and the introductory sentences are translated (STRINGS below).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from docs_event_catalog import contract_event_catalog, event_payload_schemas, parse_event_registry
from plugin_options import render_plugin_options_md
from docs_keymaps import (
    KEYMAPS_BEGIN,
    KEYMAPS_END,
    gen_keymaps_table,
    normalize_platform_keymaps,
    update_keymaps_doc as _update_keymaps_doc,
)

ROOT = Path(__file__).resolve().parent.parent
PRESET = ROOT / "crates/mare-plugin/preset"
OPTIONS_LUA = ROOT / "crates/mare-config/preset/config_api.lua"
EVENT_REGISTRY = ROOT / "crates/mare-protocol/src/event_registry.rs"
LANGS = ("en", "zh", "ja")


def default_mare():
    if binary := os.environ.get("MARE_BIN"):
        return str(Path(binary).expanduser().resolve())
    target = Path(os.environ.get("CARGO_TARGET_DIR") or "target")
    if not target.is_absolute():
        target = ROOT / target
    if build_target := os.environ.get("CARGO_BUILD_TARGET"):
        target /= Path(build_target).stem if build_target.endswith(".json") else build_target
    name = "mare.exe" if os.name == "nt" else "mare"
    debug, release = (target / profile / name for profile in ("debug", "release"))
    return str(release if not debug.is_file() and release.is_file() else debug)


STRINGS = {
    "generated": {
        "en": "This page is generated from the code by `scripts/gen_docs.py`. Do not edit it by hand.",
        "zh": "本页由 `scripts/gen_docs.py` 从代码生成，请勿手工修改。说明文字来自代码，保持英文。",
        "ja": "このページは `scripts/gen_docs.py` がコードから生成します。手で編集しないでください。説明文はコード由来のため英語のままです。",
    },
    "cli.title": {"en": "CLI", "zh": "命令行", "ja": "CLI"},
    "cli.intro": {
        "en": "Every `mare` command and flag, as printed by `--help`.",
        "zh": "`mare` 的全部命令和参数，内容与 `--help` 的输出一致。",
        "ja": "`mare` のすべてのコマンドとフラグ。`--help` の出力と同じ内容です。",
    },
    "slash.title": {
        "en": "Slash commands",
        "zh": "斜杠命令",
        "ja": "スラッシュコマンド",
    },
    "slash.intro": {
        "en": "Slash commands registered by the built-in plugins. Packages and your own plugins can add more; `/commands` lists what is available in a running session.",
        "zh": "内置插件注册的斜杠命令。包和你自己的插件可以再加；在会话里用 `/commands` 查看当前可用的全部命令。",
        "ja": "組み込みプラグインが登録するスラッシュコマンド。パッケージや自作プラグインで追加できます。実行中のセッションでは `/commands` で一覧できます。",
    },
    "slash.cols": {
        "en": ("Command", "Description", "Aliases"),
        "zh": ("命令", "说明", "别名"),
        "ja": ("コマンド", "説明", "エイリアス"),
    },
    "options.title": {"en": "Options", "zh": "选项", "ja": "オプション"},
    "options.intro": {
        "en": "Core options set with `mare.setup { ... }` in `init.lua`, `MARE_<NAME>` in the environment, or `--set name=value` on the command line.",
        "zh": "核心选项：在 `init.lua` 里用 `mare.setup { ... }` 设置，也可以用环境变量 `MARE_<NAME>` 或命令行 `--set name=value`。",
        "ja": "コアオプション。`init.lua` の `mare.setup { ... }`、環境変数 `MARE_<NAME>`、コマンドラインの `--set name=value` で設定します。",
    },
    "options.more": {
        "en": 'Plugin options are set with `mare.plugin("name", { ... })`; `/config` shows every value in a running session together with the layer and the file and line it came from.',
        "zh": '插件自己的选项用 `mare.plugin("name", { ... })` 设置；在会话里用 `/config` 可以看到每个值、它来自哪一层以及所在的文件和行号。',
        "ja": 'プラグインのオプションは `mare.plugin("name", { ... })` で設定します。実行中のセッションでは `/config` で各値と、その値を決めたレイヤー・ファイル・行を確認できます。',
    },
    "options.cols": {
        "en": ("Option", "Type", "Default", "Description"),
        "zh": ("选项", "类型", "默认值", "说明"),
        "ja": ("オプション", "型", "既定値", "説明"),
    },
    "options.invocation": {
        "en": "Invocation only",
        "zh": "仅限单次调用",
        "ja": "起動時のみ",
    },
    "lua.title": {"en": "Lua API", "zh": "Lua API", "ja": "Lua API"},
    "lua.intro": {
        "en": "The public `mare.*`, `ui.*`, and `ctx.*` functions, taken from the LuaLS definitions that ship with mare. Run `mare init` to get completion and inline documentation for all of them in your editor.",
        "zh": "公开的 `mare.*`、`ui.*` 与 `ctx.*` 函数，取自随 mare 发布的 LuaLS 类型定义。运行 `mare init` 后，编辑器里可以得到全部函数的补全和内联文档。",
        "ja": "公開されている `mare.*`、`ui.*`、`ctx.*` 関数。mare に同梱される LuaLS 定義から抽出しています。`mare init` を実行すると、エディターで補完とインラインドキュメントが使えます。",
    },
    "lua.ui_alias": {
        "en": "Also reachable as `mare.ui.*` (`mare.ui = ui`).",
        "zh": "也可通过 `mare.ui.*` 访问（`mare.ui = ui`）。",
        "ja": "`mare.ui.*` としてもアクセスできます（`mare.ui = ui`）。",
    },
    "lua.source": {"en": "Defined in", "zh": "定义于", "ja": "定義元"},
    "rpc.title": {"en": "RPC reference", "zh": "RPC 参考", "ja": "RPC リファレンス"},
    "rpc.intro": {
        "en": "Every method, notification and callback of `mare --rpc`, taken from `mare rpc schema`. Field names use `snake_case` across the protocol. How a session runs over RPC, with examples, is in [JSON-RPC](../integrate/rpc.md).",
        "zh": "`mare --rpc` 的全部方法、通知和回调，取自 `mare rpc schema`。协议字段统一使用 `snake_case`。会话如何经 RPC 运行以及示例见 [JSON-RPC](../integrate/rpc.md)。",
        "ja": "`mare --rpc` のすべてのメソッド、通知、コールバック。`mare rpc schema` から抽出しています。プロトコル全体のフィールド名は `snake_case` に統一されています。RPC でのセッションの流れと例は [JSON-RPC](../integrate/rpc.md) を参照してください。",
    },
    "rpc.version": {
        "en": "Protocol version",
        "zh": "协议版本",
        "ja": "プロトコルバージョン",
    },
    "rpc.methods": {
        "en": "Methods (client → mare)",
        "zh": "方法（客户端 → mare）",
        "ja": "メソッド（クライアント → mare）",
    },
    "rpc.notifications": {
        "en": "Notifications (mare → client)",
        "zh": "通知（mare → 客户端）",
        "ja": "通知（mare → クライアント）",
    },
    "rpc.client_notifications": {
        "en": "Notifications (client → mare)",
        "zh": "通知（客户端 → mare）",
        "ja": "通知（クライアント → mare）",
    },
    "rpc.callbacks": {
        "en": "Callbacks (mare → client requests)",
        "zh": "回调（mare → 客户端的请求）",
        "ja": "コールバック（mare → クライアントへのリクエスト）",
    },
    "rpc.errors": {"en": "Error codes", "zh": "错误码", "ja": "エラーコード"},
    "rpc.error_cols": {
        "en": ("`data.code`", "number", "meaning"),
        "zh": ("`data.code`", "编号", "含义"),
        "ja": ("`data.code`", "番号", "意味"),
    },
    "rpc.errors_intro": {
        "en": "An error's `data.code` says what failed and `data.data` carries its additional data. A service's own codes are `<service>.<name>`, declared by its contract (`services.list` shows them); anything else is `internal`.",
        "zh": "错误的 `data.code` 说明出了什么错，`data.data` 是附加数据。服务自己的错误码写作 `<服务>.<名字>`，由服务契约声明（`services.list` 会列出）；其余一律为 `internal`。",
        "ja": "エラーの `data.code` は何が失敗したかを、`data.data` は追加データを表します。サービス独自のコードは `<サービス>.<名前>` で、サービスの契約が宣言します（`services.list` に表示）。それ以外はすべて `internal` です。",
    },
    "rpc.cols": {
        "en": ("Field", "Type", "Required", "Description"),
        "zh": ("字段", "类型", "必填", "说明"),
        "ja": ("フィールド", "型", "必須", "説明"),
    },
    "rpc.result": {"en": "Result", "zh": "结果", "ja": "結果"},
    "rpc.method_errors": {"en": "Errors", "zh": "错误", "ja": "エラー"},
    "rpc.noparams": {
        "en": "No parameters.",
        "zh": "无参数。",
        "ja": "パラメーターなし。",
    },
    "rpc.yes": {"en": "yes", "zh": "是", "ja": "はい"},
    "events.title": {
        "en": "Event reference",
        "zh": "事件参考",
        "ja": "イベントリファレンス",
    },
    "events.intro": {
        "en": "Built-in public protocol and contract events, with their payload schemas. See [JSON event stream](../integrate/json-stream.md) and [Services and events](../extend/services-and-events.md) for usage.",
        "zh": "内置公开协议事件和契约事件及其载荷 schema。用法见 [JSON 事件流](../integrate/json-stream.md) 与 [服务与事件](../extend/services-and-events.md)。",
        "ja": "組み込みの公開プロトコルイベントとコントラクトイベント、およびそのペイロードスキーマです。使い方は [JSON イベントストリーム](../integrate/json-stream.md) と [サービスとイベント](../extend/services-and-events.md) を参照してください。",
    },
    "events.meta": {
        "en": ("Mode", "Delivery", "Ignorable", "Retain", "On error"),
        "zh": ("模式", "投递", "可忽略", "保留快照", "出错时"),
        "ja": ("モード", "配信", "無視可能", "保持", "エラー時"),
    },
    "events.open_payload": {
        "en": "Open JSON payload (provider/extension-defined fields).",
        "zh": "开放 JSON 载荷（字段由 provider/扩展定义）。",
        "ja": "オープンな JSON ペイロード（フィールドはプロバイダー／拡張により定義）。",
    },
    "events.example": {
        "en": "Example",
        "zh": "示例",
        "ja": "例",
    },
    "events.no": {"en": "no", "zh": "否", "ja": "いいえ"},
    "keymaps.cols": {
        "en": ("Context", "Key", "Action", "When", "Description"),
        "zh": ("上下文", "按键", "动作", "条件", "说明"),
        "ja": ("コンテキスト", "キー", "アクション", "条件", "説明"),
    },
}


def cell(text: str) -> str:
    """One table cell: escape pipes everywhere and angle brackets outside code spans."""
    parts = text.replace("\n", " ").strip().split("`")
    for i in range(0, len(parts), 2):
        parts[i] = parts[i].replace("<", "&lt;").replace(">", "&gt;")
    return "`".join(parts).replace("|", "\\|")


def header(lang: str, key: str) -> list[str]:
    return [
        f"# {STRINGS[key + '.title'][lang]}",
        "",
        STRINGS[key + ".intro"][lang],
        "",
        f"> {STRINGS['generated'][lang]}",
        "",
    ]


# --- CLI -----------------------------------------------------------------------------------


def run_help(mare: str, args: list[str]) -> str:
    out = subprocess.run(
        [mare, *args, "--help"],
        capture_output=True,
        text=True,
        check=True,
        env={
            "NO_COLOR": "1",
            "PATH": "/usr/bin:/bin",
            "HOME": "/nonexistent",
            "COLUMNS": "100",
        },
    )
    # clap prints the program's file name; the pages name the command, whatever binary made them
    name = Path(mare).name
    lines = (line.rstrip() for line in out.stdout.strip().splitlines())
    return "\n".join(
        "Usage: mare" + line[len("Usage: " + name) :] if line.startswith("Usage: " + name) else line
        for line in lines
    )


def subcommands(help_text: str) -> list[str]:
    names, inside = [], False
    for line in help_text.splitlines():
        if line.startswith("Commands:"):
            inside = True
            continue
        if inside:
            if not line.startswith("  "):
                break
            name = line.split()[0]
            if name != "help":
                names.append(name)
    return names


def gen_cli(mare: str, lang: str) -> str:
    out = header(lang, "cli")
    top = run_help(mare, [])
    out += ["## mare", "", "```text", top, "```", ""]
    for name in subcommands(top):
        text = run_help(mare, [name])
        out += [f"## mare {name}", "", "```text", text, "```", ""]
        for sub in subcommands(text):
            out += [
                f"### mare {name} {sub}",
                "",
                "```text",
                run_help(mare, [name, sub]),
                "```",
                "",
            ]
    return "\n".join(out).rstrip() + "\n"


# --- slash commands ------------------------------------------------------------------------

# A throwaway plugin asks the running command registry for its list once the agent starts (every
# plugin has started by then; the offline `faux/echo` model answers, so nothing leaves the machine). Aliases are hidden from
# the list, so the alias names found in the source are checked against the registry.
PROBE = """
return {
  name = "zz-docs-probe",
  inject = { "commands" },
  apply = function(ctx)
    ctx:on("turn/started", function()
      local out = { commands = {}, aliases = {}, keymaps = {}, actions = {} }
      for _, d in ipairs(ctx.commands:list().data) do
        out.commands[#out.commands + 1] = { name = d.name, description = d.description, args = d.args }
      end
      for _, name in ipairs(%(candidates)s) do
        local d = ctx.commands:get(name)
        if d and d.alias_of then out.aliases[name] = d.alias_of end
      end
      for _, a in ipairs(mare.keymap.actions()) do
        if a.name and a.desc then out.actions[a.name] = a.desc end
      end
      for _, b in ipairs(mare.keymap.list()) do
        out.keymaps[#out.keymaps + 1] = {
          key = b.key,
          context = b.context,
          action = b.action,
          desc = b.desc,
          when = b.when,
          layer = b.layer,
        }
      end
      mare.fs.write(%(out)s, mare.json.encode(out))
    end)
  end,
}
"""
ALIAS_CALL = re.compile(r'alias\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)')


def registered_commands(mare: str) -> dict:
    candidates = sorted(
        {
            m.group(1)
            for p in (PRESET / "plugins").rglob("*.lua")
            for m in ALIAS_CALL.finditer(p.read_text(encoding="utf-8"))
        }
    )
    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    with tempfile.TemporaryDirectory(prefix="mare-docs-", dir=tmp_root) as tmp:
        t = Path(tmp)
        (t / "config/mare/plugins").mkdir(parents=True)
        (t / "work").mkdir()
        out = t / "commands.json"
        lua_list = "{ " + ", ".join(json.dumps(c) for c in candidates) + " }"
        (t / "config/mare/plugins/zz-docs-probe.lua").write_text(
            PROBE % {"candidates": lua_list, "out": json.dumps(str(out))}
        )
        (t / "config/mare/init.lua").write_text(
            'mare.setup { model = "faux/echo", update_check = false, news = false }\n'
        )
        env = {
            "HOME": str(t),
            "MARE_TMPDIR": str(t),
            "TMPDIR": str(t),
            "TEMP": str(t),
            "TMP": str(t),
            "XDG_CONFIG_HOME": str(t / "config"),
            "XDG_STATE_HOME": str(t / "state"),
            "XDG_DATA_HOME": str(t / "data"),
            "XDG_CACHE_HOME": str(t / "cache"),
            "PATH": "/usr/bin:/bin",
            "NO_COLOR": "1",
        }
        subprocess.run(
            [mare, "-p", "--no-session", "hi"],
            cwd=t / "work",
            env=env,
            capture_output=True,
            check=True,
            timeout=60,
        )
        return json.loads(out.read_text(encoding="utf-8"))


TUI_ONLY_CMD = re.compile(
    r'ctx\.commands:register\(\s*"([^"]+)"\s*,\s*\{(.*?)\}\s*\)',
    re.S,
)
TUI_CMD_FIELD = {
    "description": re.compile(r'\bdescription\s*=\s*"([^"]+)"'),
    "args": re.compile(r'\bargs\s*=\s*"([^"]+)"'),
    "local_only": re.compile(r'\blocal_only\s*=\s*true\b'),
}


def tui_only_commands(existing: set[str]) -> list[dict]:
    """Built-in slash commands whose plugin returns early outside TUI mode (such as `/lua`)."""
    extra: list[dict] = []
    for path in sorted((PRESET / "plugins").rglob("*.lua")):
        text = path.read_text(encoding="utf-8")
        if 'mare.mode ~= "tui"' not in text:
            continue
        for m in TUI_ONLY_CMD.finditer(text):
            name, body = m.group(1), m.group(2)
            if name in existing:
                continue
            dm = TUI_CMD_FIELD["description"].search(body)
            am = TUI_CMD_FIELD["args"].search(body)
            desc = dm.group(1) if dm else ""
            if TUI_CMD_FIELD["local_only"].search(body):
                desc = f"{desc} (TUI local command)".strip()
            else:
                desc = f"{desc} (TUI only)".strip()
            extra.append({"name": name, "description": desc, "args": am.group(1) if am else None})
            existing.add(name)
    return extra


def gen_slash(data: dict, lang: str) -> str:
    aliases: dict[str, list[str]] = {}
    for short, target in data["aliases"].items():
        aliases.setdefault(target, []).append(short)
    commands = list(data["commands"])
    commands.extend(tui_only_commands({d["name"] for d in commands}))
    cols = STRINGS["slash.cols"][lang]
    out = header(lang, "slash")
    out += [f"| {cols[0]} | {cols[1]} | {cols[2]} |", "|---|---|---|"]
    for d in sorted(commands, key=lambda d: d["name"]):
        usage = f"/{d['name']}" + (f" {d['args']}" if d.get("args") else "")
        extra = ", ".join(f"`/{a}`" for a in sorted(aliases.get(d["name"], [])))
        out.append(
            f"| `{usage.replace('|', '\\|')}` | {cell(d.get('description') or '')} | {extra} |"
        )
    return "\n".join(out) + "\n"


# --- options -------------------------------------------------------------------------------

LUA_DUMP = r"""
local mare = {}
local chunk = assert(loadfile(arg[1]))
pcall(chunk, mare, {})
local function show(v)
  if type(v) == "table" then
    local keys = {}
    for k in pairs(v) do keys[#keys + 1] = k end
    table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
    local parts = {}
    for _, k in ipairs(keys) do
      parts[#parts + 1] = (type(k) == "number" and "" or (tostring(k) .. " = ")) .. show(v[k])
    end
    return "{ " .. table.concat(parts, ", ") .. " }"
  elseif type(v) == "string" then
    return string.format("%q", v)
  end
  return tostring(v)
end
local names = {}
for name in pairs(mare.options) do names[#names + 1] = name end
table.sort(names)
for _, name in ipairs(names) do
  local o = mare.options[name]
  local ty = type(o.type) == "table" and table.concat(o.type, " or ") or tostring(o.type)
  local flag = o.invocation_only and "invocation" or ""
  io.write(table.concat({ name, ty, o.default ~= nil and show(o.default) or "", flag, ((o.desc or ""):gsub("[\t\n]", " ")) }, "\t"), "\n")
end
"""


def gen_options(lua: str, lang: str) -> str:
    dump = subprocess.run(
        [lua, "-e", LUA_DUMP.replace("arg[1]", repr(str(OPTIONS_LUA)))],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    cols = STRINGS["options.cols"][lang]
    out = header(lang, "options")
    out += [
        STRINGS["options.more"][lang],
        "",
        f"| {cols[0]} | {cols[1]} | {cols[2]} | {cols[3]} |",
        "|---|---|---|---|",
    ]
    for line in dump.splitlines():
        name, ty, default, flag, desc = line.split("\t")
        if flag == "invocation":
            desc = f"**{STRINGS['options.invocation'][lang]}.** {desc}"
        out.append(
            f"| `{name}` | {cell(ty)} | {('`' + cell(default) + '`') if default else ''} | {cell(desc)} |"
        )
    out += render_plugin_options_md(lua, lang, cell)
    return "\n".join(out) + "\n"


# --- Lua API -------------------------------------------------------------------------------

FUNC = re.compile(r"^function ((?:mare|ui)(?:\.\w+)*[.:]\w+)\((.*?)\) end")
METHOD = re.compile(r"^function (\w+):(\w+)\((.*?)\) end")
CLASS = re.compile(r"^---@class ([\w.]+)")
LOCAL = re.compile(r"^local (\w+) = \{\}")
SERVICE_FIELD = re.compile(r"^---@field ([\w-]+) ([\w.]+)", re.M)
METHOD_FIELD = re.compile(r"^---@field (\w+) fun\(self: ([\w.]+)(?:,\s*(.*?))?\)(?::\s*(.*))?$")


def service_fields(types: Path) -> dict[str, str]:
    """Class -> how a plugin reaches it: `mare.UsageService` -> `ctx.usage`, `mare.Ctx` -> `ctx`
    (from ctx.lua)."""
    ctx = (types / "mare/services/ctx.lua").read_text(encoding="utf-8")
    fields = {cls: f"ctx.{field}" for field, cls in SERVICE_FIELD.findall(ctx)}
    # ctx itself: its methods (`ctx:after`, `ctx:get`, ...) are in-process plugin API too
    fields["mare.Ctx"] = "ctx"
    return fields


def gen_lua_api(lang: str) -> str:
    types = PRESET / "types"
    services = service_fields(types)
    groups: dict[str, list[tuple[str, str, str]]] = {}
    for path in sorted(types.rglob("*.lua")):
        rel = path.relative_to(types).as_posix()
        doc: list[str] = []
        # `---@class C` then `local X = {}`: methods `function X:m(` belong to class C
        # (or a class of this file with the local's name, e.g. `mare.Ctx` ... `local Ctx = {}`
        # with other classes declared in between)
        pending: str | None = None
        locals_: dict[str, str] = {}
        classes: list[str] = []
        for line in path.read_text(encoding="utf-8").splitlines():
            c = CLASS.match(line)
            local = LOCAL.match(line)
            if c:
                pending = c.group(1)
                classes.append(pending)
            elif local:
                name = local.group(1)
                named = next((k for k in classes if k.rsplit(".", 1)[-1] == name), None)
                if named or pending:
                    locals_[name] = named or pending
                pending = None
            elif not line.startswith("---"):
                pending = None
            if line.startswith("---"):
                mf = METHOD_FIELD.match(line)
                if mf and mf.group(2) in services:
                    field_ns = services[mf.group(2)]
                    mname = mf.group(1)
                    raw_params = mf.group(3) or ""
                    # strip LuaLS type annotations from param list (`names: string[], fn: fun(...)` -> `names, fn`)
                    pnames: list[str] = []
                    depth = 0
                    cur = []
                    for ch in raw_params:
                        if ch in "([{<":
                            depth += 1
                            cur.append(ch)
                        elif ch in ")]}>":
                            depth = max(0, depth - 1)
                            cur.append(ch)
                        elif ch == "," and depth == 0:
                            tok = "".join(cur).strip().split(":", 1)[0].strip()
                            if tok:
                                pnames.append(tok)
                            cur = []
                        else:
                            cur.append(ch)
                    if cur:
                        tok = "".join(cur).strip().split(":", 1)[0].strip()
                        if tok:
                            pnames.append(tok)
                    sig = f"{field_ns}:{mname}({', '.join(pnames)})"
                    summary = " ".join(doc).strip()
                    entries = groups.setdefault(field_ns, [])
                    dup = next((i for i, e in enumerate(entries) if e[0] == sig), None)
                    if dup is None:
                        entries.append((sig, summary, rel))
                    elif summary and not entries[dup][1]:
                        entries[dup] = (sig, summary, rel)
                    doc = []
                    continue
                body = line[3:].strip()
                if not body.startswith("@"):
                    doc.append(body)
                continue
            m = FUNC.match(line)
            service = METHOD.match(line)
            field = service and services.get(locals_.get(service.group(1), ""))
            if not m and field:
                # a service method (contract-generated or hand-written), under its ctx field
                name, params = service.group(2), service.group(3)
                if not name.startswith("_"):
                    entries = groups.setdefault(field, [])
                    sig = f"{field}:{name}({params})"
                    summary = " ".join(doc).strip()
                    dup = next((i for i, e in enumerate(entries) if e[0] == sig), None)
                    if dup is None:
                        entries.append((sig, summary, rel))
                    elif summary and not entries[dup][1]:
                        entries[dup] = (sig, summary, rel)
            if m:
                full, params = m.group(1), m.group(2)
                if "._" not in full and not full.split(".")[-1].startswith("_"):
                    namespace = (
                        full.rsplit(".", 1)[0]
                        if "." in full.replace(":", ".")
                        else "mare"
                    )
                    namespace = re.split(r"[:]", namespace)[0]
                    # the whole description: a cut summary drops what comes last (option lists
                    # such as mare.http.serve's close_grace_ms are usually last)
                    summary = " ".join(doc).strip()
                    entries = groups.setdefault(namespace, [])
                    sig = f"{full}({params})"
                    # the same function declared in two type files: keep one row, prefer a description
                    dup = next((i for i, e in enumerate(entries) if e[0] == sig), None)
                    if dup is None:
                        entries.append((sig, summary, rel))
                    elif summary and not entries[dup][1]:
                        entries[dup] = (sig, summary, rel)
            doc = []
    out = header(lang, "lua")
    for namespace in sorted(groups):
        entries = groups[namespace]
        sources = sorted({rel for _, _, rel in entries})
        out += [
            f"## {namespace}",
            "",
            f"{STRINGS['lua.source'][lang]} "
            + ", ".join(f"`types/{s}`" for s in sources)
            + "."
            + (" " + STRINGS["lua.ui_alias"][lang] if namespace == "ui" or namespace.startswith("ui.") else ""),
            "",
        ]
        out += ["| | |", "|---|---|"]
        for sig, summary, _ in entries:
            out.append(f"| `{cell(sig)}` | {cell(summary)} |")
        out.append("")
    return "\n".join(out).rstrip() + "\n"


# --- RPC -----------------------------------------------------------------------------------


def rpc_schema(mare: str) -> dict:
    out = subprocess.run(
        [mare, "--clean", "rpc", "schema"],
        capture_output=True,
        text=True,
        check=True,
        env={"NO_COLOR": "1", "PATH": "/usr/bin:/bin", "HOME": "/nonexistent"},
    )
    return json.loads(out.stdout)


def rpc_resolve(catalog: dict, node: dict) -> dict:
    """Follow `$ref` (a JSON pointer into the whole catalog) until a concrete schema."""
    seen = 0
    while isinstance(node, dict) and "$ref" in node and seen < 20:
        target: object = catalog
        for part in node["$ref"].lstrip("#/").split("/"):
            target = target[part.replace("~1", "/").replace("~0", "~")]
        node = target  # type: ignore[assignment]
        seen += 1
    return node


def rpc_type(node: object, inline_depth: int = 0) -> str:
    """A short type label: referenced type names stay names, unions are joined with |."""
    if node is True or node == {}:
        return "any"
    if not isinstance(node, dict):
        return "any"
    if "$ref" in node:
        return node["$ref"].rsplit("/", 1)[-1]
    if "const" in node:
        return json.dumps(node["const"], ensure_ascii=False)
    if "enum" in node:
        return " | ".join(json.dumps(v, ensure_ascii=False) for v in node["enum"])
    for key in ("anyOf", "oneOf"):
        if key in node:
            labels = []
            for alt in node[key]:
                label = rpc_type(alt, inline_depth)
                if label not in labels:
                    labels.append(label)
            return " | ".join(labels)
    if "allOf" in node and len(node["allOf"]) == 1:
        return rpc_type(node["allOf"][0], inline_depth)
    kind = node.get("type")
    if isinstance(kind, list):
        return " | ".join(
            rpc_type({**node, "type": k}, inline_depth) if k in ("array", "object") else k
            for k in kind
        )
    if kind == "array":
        return rpc_type(node.get("items", True), inline_depth) + "[]"
    if kind == "object":
        if "title" in node:
            return node["title"]
        props = node.get("properties")
        extra = node.get("additionalProperties")
        if isinstance(extra, dict) and not props:
            return "map<string, " + rpc_type(extra, inline_depth + 1) + ">"
        if isinstance(props, dict) and props and inline_depth < 2 and len(props) <= 6:
            req = set(node.get("required") or [])
            parts = [
                f"{k}{'' if k in req else '?'}: {rpc_type(v, inline_depth + 1)}"
                for k, v in props.items()
            ]
            return "{ " + ", ".join(parts) + " }"
        return "object"
    if kind == "integer" and str(node.get("format", "")).startswith("uint"):
        return "integer ≥ 0"
    return kind or "any"


def rpc_fields(catalog: dict, schema: dict, lang: str) -> list[str]:
    root = rpc_resolve(catalog, schema)
    props: dict = {}
    required: set = set()
    for part in [root, *root.get("allOf", [])]:
        part = rpc_resolve(catalog, part)
        props.update(part.get("properties", {}))
        required.update(part.get("required", []))
    if not props:
        return [STRINGS["rpc.noparams"][lang], ""]
    cols = STRINGS["rpc.cols"][lang]
    out = ["| " + " | ".join(cols) + " |", "|---|---|---|---|"]
    yes = STRINGS["rpc.yes"][lang]
    for name, prop in props.items():
        desc = prop.get("description", "") if isinstance(prop, dict) else ""
        if isinstance(prop, dict) and "default" in prop and prop["default"] is not None:
            desc = (
                desc + f" (default `{json.dumps(prop['default'], ensure_ascii=False)}`)"
            ).strip()
        out.append(
            f"| `{name}` | `{cell(rpc_type(prop))}` | {yes if name in required else ''} | {cell(desc)} |"
        )
    out.append("")
    return out


def rpc_entry(catalog: dict, name: str, spec: dict, lang: str, level: str) -> list[str]:
    params = spec.get("params", spec)
    out = [f"{level} `{name}`", ""]
    desc = rpc_resolve(catalog, params).get("description", "")
    if desc:
        out += [desc.strip(), ""]
    out += rpc_fields(catalog, params, lang)
    if "result" in spec:
        result = spec["result"]
        resolved = rpc_resolve(catalog, result) if isinstance(result, dict) else {}
        has_props = isinstance(resolved, dict) and (
            bool(resolved.get("properties"))
            or any(
                isinstance(rpc_resolve(catalog, p), dict) and bool(rpc_resolve(catalog, p).get("properties"))
                for p in resolved.get("allOf", [])
            )
        )
        if has_props:
            label = (
                rpc_type(result)
                if isinstance(result, dict) and "$ref" in result
                else (result.get("title") if isinstance(result, dict) else None)
            )
            if label:
                out += [f"{STRINGS['rpc.result'][lang]}: `{cell(label)}`", ""]
            else:
                out += [f"{STRINGS['rpc.result'][lang]}:", ""]
            out += rpc_fields(catalog, result, lang)
        else:
            label = (
                rpc_type(result)
                if isinstance(result, dict) and "$ref" in result
                else (result.get("title") if isinstance(result, dict) else None) or rpc_type(result)
            )
            out += [f"{STRINGS['rpc.result'][lang]}: `{cell(label)}`", ""]
    if errors := spec.get("errors"):
        out += [f"{STRINGS['rpc.method_errors'][lang]}: " + ", ".join(f"`{e}`" for e in errors), ""]
    return out


# Hand-written pages quote the protocol version in examples; keep them on the current one.
VERSION_QUOTES = re.compile(
    r'("protocolVersion": ?"|"protocol_version": ?"|# mare RPC \(protocol |current |mare RPC (?:protocol )?)(\d+\.\d+)'
)


def sync_versions(version: str, check: bool) -> list[str]:
    paths = [
        ROOT / "docs/image-resources.md",
        ROOT / "docs-site/src/components/home/copy.ts",
    ]
    for lang in LANGS:
        paths += [
            ROOT / f"docs/{lang}/integrate/rpc.md",
            ROOT / f"docs/{lang}/integrate/json-stream.md",
            ROOT / f"docs/{lang}/extend/writing-a-plugin.md",
        ]
    stale = []
    for path in paths:
        if not path.exists():
            continue
        text = path.read_text(encoding="utf-8")
        new = VERSION_QUOTES.sub(lambda m: m.group(1) + version, text)
        if new != text:
            stale.append(path.relative_to(ROOT).as_posix())
            if not check:
                path.write_text(new, encoding="utf-8")
    return stale


def gen_rpc(catalog: dict, lang: str) -> str:
    out = header(lang, "rpc")
    out += [f"{STRINGS['rpc.version'][lang]}: **{catalog['protocol_version']}**", ""]
    out += [f"## {STRINGS['rpc.methods'][lang]}", ""]
    families: dict[str, list[str]] = {}
    for name in sorted(catalog["methods"]):
        families.setdefault(name.split(".", 1)[0], []).append(name)
    for family, names in families.items():
        if names == [family]:
            out += rpc_entry(catalog, family, catalog["methods"][family], lang, "###")
            continue
        out += [f"### {family}", ""]
        for name in names:
            out += rpc_entry(catalog, name, catalog["methods"][name], lang, "####")
    for key, section in (
        ("notifications", "rpc.notifications"),
        ("client_notifications", "rpc.client_notifications"),
        ("callbacks", "rpc.callbacks"),
    ):
        out += [f"## {STRINGS[section][lang]}", ""]
        for name in sorted(catalog.get(key, {})):
            out += rpc_entry(catalog, name, catalog[key][name], lang, "###")
    out += [
        f"## {STRINGS['rpc.errors'][lang]}",
        "",
        STRINGS["rpc.errors_intro"][lang],
        "",
        "| " + " | ".join(STRINGS["rpc.error_cols"][lang]) + " |",
        "|---|---|---|",
    ]
    for name, entry in catalog.get("errors", {}).items():
        out.append(f"| `{name}` | `{entry['number']}` | {cell(entry['meaning'])} |")
    return "\n".join(out).rstrip() + "\n"


def gen_events(catalog: dict, lang: str, contract_events: list[dict] | None = None) -> str:
    """Generate docs/<lang>/reference/events.md from protocol_events! and `mare rpc schema`."""
    entries = parse_event_registry(EVENT_REGISTRY)
    payloads = event_payload_schemas(catalog)
    names = {entry["name"] for entry in entries}
    for event in contract_events or []:
        if event.get("public") and event["name"] not in names:
            entries.append(event)
            payloads[event["name"]] = event.get("data_schema") or {}
    out = header(lang, "events")
    out += [f"{STRINGS['rpc.version'][lang]}: **{catalog['protocol_version']}**", ""]
    families: dict[str, list[dict]] = {}
    for entry in entries:
        families.setdefault(entry["name"].split("/", 1)[0], []).append(entry)
    meta_cols = STRINGS["events.meta"][lang]
    yes = STRINGS["rpc.yes"][lang]
    no = STRINGS["events.no"][lang]
    for family, items in families.items():
        out += [f"## {family}", ""]
        for entry in items:
            name = entry["name"]
            out += [f"### `{name}`", "", entry["doc"], ""]
            out += [
                "| " + " | ".join(meta_cols) + " |",
                "|---|---|---|---|---|",
                f"| `{entry['mode']}` | `{entry['delivery']}` | {yes if entry['ignorable'] else no} | {yes if entry['retain'] else no} | `{entry['on_error']}` |",
                "",
            ]
            schema = payloads.get(name, {})
            resolved = rpc_resolve(catalog, schema) if schema else {}
            props: dict = {}
            for part in [resolved, *resolved.get("allOf", [])]:
                part = rpc_resolve(catalog, part)
                props.update(part.get("properties", {}))
            if props:
                out += rpc_fields(catalog, schema, lang)
            else:
                out += [STRINGS["events.open_payload"][lang], ""]
            if entry.get("example") is not None:
                out += [
                    f"**{STRINGS['events.example'][lang]}**", "", "```json",
                    json.dumps(entry["example"], ensure_ascii=False, indent=2), "```", "",
                ]
    return "\n".join(out).rstrip() + "\n"


def update_keymaps_doc(rel: str, keymaps: list[dict], lang: str) -> str:
    return _update_keymaps_doc(ROOT, rel, keymaps, STRINGS["keymaps.cols"][lang], cell)


def stale_pages(pages: dict[str, str]) -> list[str]:
    """The pages (relative path -> generated content) whose file differs: what --check reports."""
    return [
        rel
        for rel, content in pages.items()
        if not (ROOT / rel).exists()
        or (ROOT / rel).read_text(encoding="utf-8") != content
    ]


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--mare",
        default=default_mare(),
        help="mare binary used for --help output",
    )
    ap.add_argument(
        "--lua",
        default="lua",
        help="Lua 5.4+ interpreter used to read the option registry",
    )
    ap.add_argument(
        "--check", action="store_true", help="do not write; exit 1 when a page differs"
    )
    args = ap.parse_args()
    if os.sep in args.mare or "/" in args.mare or Path(args.mare).exists():
        args.mare = str(Path(args.mare).expanduser().resolve())

    stale: list[str] = []

    commands = registered_commands(args.mare)
    catalog = rpc_schema(args.mare)
    contract_events = contract_event_catalog(args.mare)
    keymaps = normalize_platform_keymaps(
        commands.get("keymaps", []), commands.get("actions", {}), PRESET, args.lua
    )
    for lang in LANGS:
        pages = {
            f"docs/{lang}/reference/cli.md": gen_cli(args.mare, lang),
            f"docs/{lang}/reference/slash-commands.md": gen_slash(commands, lang),
            f"docs/{lang}/configure/options.md": gen_options(args.lua, lang),
            f"docs/{lang}/extend/lua-api.md": gen_lua_api(lang),
            f"docs/{lang}/reference/rpc.md": gen_rpc(catalog, lang),
            f"docs/{lang}/reference/events.md": gen_events(catalog, lang, contract_events),
            f"docs/{lang}/configure/keymaps.md": update_keymaps_doc(
                f"docs/{lang}/configure/keymaps.md", keymaps, lang
            ),
        }
        if args.check:
            stale += stale_pages(pages)
        else:
            for rel, content in pages.items():
                path = ROOT / rel
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
    synced = sync_versions(catalog["protocol_version"], args.check)
    if args.check:
        stale += synced
    if stale:
        print(
            "stale generated docs (run scripts/gen_docs.py):",
            *stale,
            sep="\n  ",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
