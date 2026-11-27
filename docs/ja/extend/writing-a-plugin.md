# プラグインを書く

小さな mare プラグインを順を追って作ります。ツール、スラッシュコマンド、イベントリスナー、設定オプション、ステータスラインのセグメントを作り、テストして公開します。

定義のフィールドについては、先に [プラグイン](plugins.md) を読んでください。

## プラグインは必要か

コマンドを実行するツールには、プラグインは不要です。`init.lua` で宣言します。

```lua
mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
  readonly = true,
}
```

`command` 内の各 `{name}` は、モデルの引数で埋められます。`readonly = true` がない場合、mare はそのコマンドに副作用があるものとして扱います。Lua のロジック、イベント、コマンド、インターフェースの部品が必要な場合に、プラグインを使います。

## ステップ 1: 骨組み

`~/.config/mare/plugins/tally.lua` を作成します。

```lua
return {
  name = "tally",
  inject = { "tools", "commands", "ui" },
  apply = function(ctx, cfg)
    ctx.commands:register("tally", {
      description = "Show how many tool calls the current turn has made",
      run = function()
        ui.notify("tally is loaded")
      end,
    })
  end,
}
```

mare を起動するか、実行中のセッションで `/reload` を実行して、`/tally` と入力します。通知が表示されます。`/plugins` には、由来が `user` の `tally` が一覧表示されます。

`inject` は、プラグインが使うサービスを指定します。それぞれが `ctx` のフィールドになります。`ctx.commands`、`ctx.tools`、`ctx.ui` です。`ctx.` を付けない `ui` は、ノードや通知を作るためのグローバルな名前空間です。

## ステップ 2: ツール

`ctx.tools:register(name, def)` でツールを登録します。次の例は、モデルが渡したテキストの行数、単語数、文字数を数えます。

```lua
ctx.tools:register("text_stats", {
  snippet = "Count lines and words in a piece of text",
  description = "Count the lines, words and characters of the given text.",
  parameters = {
    type = "object",
    properties = { text = { type = "string", description = "The text to measure" } },
    required = { "text" },
  },
  side_effect = "read",
  summary = function(args)
    return ("TextStats(%d chars)"):format(#(args.text or ""))
  end,
  execute = function(args)
    local text = args.text or ""
    local _, lines = text:gsub("\n", "\n")
    local words = 0
    for _ in text:gmatch("%S+") do words = words + 1 end
    return { content = ("lines: %d\nwords: %d\nchars: %d"):format(lines + 1, words, #text) }
  end,
})
```

このツールは、組み込みのツールと同じパイプラインを通ります。`tool/rewrite`、引数のチェック、`tool/before` の投票、承認、そして `execute` の順です。プラグインが破棄されると、ツールは取り下げられます。

ツール定義のフィールド:

| フィールド | 意味 |
|---|---|
| `description`、`parameters` | モデルに見える内容。`parameters` は JSON Schema オブジェクトです。どちらも `(model, mode)` の関数にできます。 |
| `execute(args, tctx)` | 呼び出しを実行します。yield できます。結果のテーブルまたは文字列を返します。 |
| `snippet`、`guidelines` | システムプロンプト向けの 1 行の説明と、追加のルール。 |
| `summary(args)` | トランスクリプトに表示されるヘッダー。例: `Read(src/main.rs)`。 |
| `render(args, result, expanded)` | 結果のためのカスタムノード。 |
| `readonly` | 副作用がありません。連続する読み取り専用の呼び出しは並列に実行されます。 |
| `side_effect` | `"read"`、`"write"` (ローカルファイルのみ)、`"external"`。`readonly` の場合の既定は `"read"`、それ以外は `"external"`。 |
| `idempotent` | 2 回実行しても安全です。冪等でない呼び出しが途中でクラッシュした場合、結果不明として再開され、再実行されることはありません。 |
| `unconfined` | この外部プロセスで実行されるため、ローカルのサンドボックスでは制限できません。 |
| `enabled(model, edit_mode)` | `false` を返すと、そのモデルに対してツールを隠します。 |
| `timeout_ms` | 呼び出しの制限時間。 |

`tctx` には、`cancel` (ブロッキング呼び出しに渡すトークン)、進捗用の `update(partial)`、`cwd`、`tool_call_id` が入ります。結果のテーブルには `content` (文字列、またはコンテンツブロックのリスト) があり、任意で `is_error`、`details`、`display` (ノード)、`no_truncate`、`max_line_chars` を指定できます。

## ステップ 3: イベントリスナー

`item/completed` でツール呼び出しを数え、各ターンの開始時にリセットします。

```lua
local calls = 0

ctx:on("turn/started", function() calls = 0 end)
ctx:on("item/completed", function(data)
  if data.item and data.item.type == "tool_call" then
    calls = calls + 1
  end
end)
```

リスナーは、プラグインが破棄されると削除されます。`/tally` コマンドを、この数を報告するように変更します。

```lua
run = function()
  ui.notify(("%d tool call(s) in this turn"):format(calls))
end,
```

イベント名と、戻り値の効果については、[サービスとイベント](services-and-events.md) を参照してください。

## ステップ 4: 設定オプション

各オプションを `config_schema` で宣言します。既定値とユーザーの設定が `cfg` として渡されます。型が違う値や範囲外の値は報告され、既定値が残ります。

```lua
config_schema = {
  warn_after = { type = "integer", default = 25, min = 1, desc = "notify when one turn makes more tool calls than this" },
  show_in_status = { type = "boolean", default = true, desc = "show the count in the status line" },
},
```

リスナー内で使います。

```lua
ctx:on("item/completed", function(data)
  if data.item and data.item.type == "tool_call" then
    calls = calls + 1
    if calls == cfg.warn_after + 1 then
      ui.notify(("tally: more than %d tool calls in this turn"):format(cfg.warn_after), "warning")
    end
  end
end)
```

ユーザーは `init.lua` で変更します。宣言していないキーには、提案付きの警告が出ます。

```lua
mare.plugin("tally", { warn_after = 10 })
```

`/reload config` は、オプションが変わったプラグインを再読み込みします。

## ステップ 5: ステータスラインのセグメント

`ctx.ui:status(id, def)` はセグメントを追加します。`render` はノードを返すか、隠すには `nil` を返します。内容が変わったら `ctx.ui:invalidate(id)` を呼び出します。mare は要求されたときにだけ描画します。

```lua
if cfg.show_in_status then
  ctx.ui:status("tally", {
    align = "right",
    order = 20,
    render = function()
      if calls == 0 then return nil end
      return ui.Line({ ui.Span(("calls: %d"):format(calls)):hl("MareDim") })
    end,
  })
end
```

`calls` を変更するたびに、`ctx.ui:invalidate("tally")` を追加してください。`ctx.ui:widget(id, { slot = "above-editor", render = function(width, max_height) ... end })` は、代わりに入力の上または下にブロックを描画します。

上記のスニペットはすべて `apply` の中に順番に入れ、先頭に `local calls = 0` を置きます。`inject` と `config_schema` は、返すテーブルの中で `apply` の隣に置きます。結果は約 55 行の 1 つのファイルになります。

## テストする

- **再読み込み。** ファイルを編集し、このプラグインには `/reload tally`、すべてには `/reload` を実行します。読み込みに失敗したプラグインは、以前のバージョンのまま動き続けます。
- **状態の確認。** `/plugins` は、プラグインが有効かどうかを示し、失敗した場合はトレースバック付きでエラーを表示します。失敗はステータスラインにも `plugin tally failed · /reload` と表示されます。
- **Lua コンソール。** `/lua` は、実行中のセッションで Lua を実行します。たとえば `/lua mare.plugins.status("tally")` や `/lua mare.o.border` です。コードを指定しない場合は、複数行を受け付けるプロンプトが開きます。TUI にのみ存在し、RPC 経由では使えません。
- **ログ。** `ctx.logger:warn("...")` は、`~/.local/state/mare/log/` のログに書き込みます。`/debug` は最近の行を表示します。
- **TUI なしで。** ヘッドレスセッションでプラグインを読み込み、stderr を読みます。読み込めない定義は `mare: warning: plugin ...` と報告されます。

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","client":{"name":"t"},"capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}}' | mare --rpc >/dev/null
```

空の設定に対してテストするには、先頭に `MARE_HOME=$(mktemp -d)` を付けます。

### `mare test` で自動テストを実行する

`mare test` は指定したパス (省略時は現在のディレクトリ。`.git`、`target`、`node_modules`、`snapshots` は除外) から `*_spec.lua` ファイルを探索し、組み込みの `mare.test` ハーネス (`require("mare.test")`) で実行します。

`t.host()` を使うと、本番と同じ Cordis ライフサイクルツリー・loader・イベントバス・コントラクト検証を備えた隔離テストホストを起動できます (必要に応じて `builtins` を読み込んだり、`host:provide(name, impl, opts?)` でコントラクト検証済みのモックサービスを差し込めます)。

`plugins/tally.lua` と並べて `tests/tally_spec.lua` を作成します。

```lua
local t = require("mare.test")

t.describe("tally", function()
  t.it("counts lines, words, and chars via text_stats", function()
    local host = t.host({ builtins = { "tools", "commands", "ui" } })
    local plugin = dofile("plugins/tally.lua")
    host:load(plugin, { warn_after = 25, show_in_status = false })
    t.expect.settled(host)
    t.expect.eq(host:state("tally"), "ACTIVE")

    local tool = assert(host:get("tools"):get("text_stats"))
    local res = tool.execute({ text = "alpha beta\ngamma" })
    t.expect.eq(res.content, "lines: 2\nwords: 3\nchars: 16")
    t.expect.snapshot(res.content, "text_stats_sample")

    host:dispose()
    t.expect.eq(host:get("tools"), nil)
  end)
end)
```

`host` は実際のプラグインライフサイクルを動かすため、依存欠如時の `"PENDING"` 状態、`host:provide(...)` によるコントラクト検証、`host:dispose()` 時のクリーンアップ、`host:reload(name, def, cfg)` の失敗ロールバックまで同じフィクスチャで検証できます。明示的に `dispose()` しなかったテストホストも、各 `t.it` ケース終了時に自動で破棄されます。

`tests/tally_spec.lua` にはスナップショット検証が含まれるため、初回は `-u` で基準スナップショットを作成して内容を確認したうえで、プラグインリポジトリのルートから通常のテストを実行します。

```sh
mare test -u tests/tally_spec.lua # 初回は snapshots/*.snap を作成・更新 (コミット前に内容を確認)
mare test                         # すべての *_spec.lua を探索し保存済みスナップショットと照合
mare test tests/tally_spec.lua    # 単一の spec ファイルを実行
mare test -f "text_stats"         # ケース名 (部分一致) で絞り込み
```

`expect.snapshot(value, name?)` は `<spec_dir>/snapshots/<spec_stem>__<name>.snap` にスナップショットを保存します。`-u` (`--update-snapshots`) を付けない場合、不一致があるとケースは失敗し比較用の `.snap.new` が書き出されます。終了コードは `0` (全件成功)、`1` (テスト失敗)、`2` (引数・探索・構文/ロードエラー) です。

mare リポジトリのチェックアウトで組み込みプラグインに取り組む場合、`just run` はデバッグビルドを起動し、起動のたびに `crates/mare-plugin/preset/plugins/*.lua` をディスクから読み込みます。ファイルを編集して `/reload` するだけで、再コンパイルは不要です。

## パッケージとして公開する

パッケージは git リポジトリです。プラグインを `plugins/` に置きます。

```
mare-tally/
  plugins/tally.lua
  mare-plugin.toml        # optional
```

```toml
# mare-plugin.toml
name = "mare-tally"
description = "Count tool calls per turn"
version = "0.1.0"
requires = ">=0.1"     # a mare older than this will not load the package
```

プッシュして、semver タグでリリースにタグを付けます。

```sh
git tag v0.1.0 && git push --tags
```

誰でも、`init.lua` で宣言して `mare plugin sync` を実行すればインストールできます。オプションはプラグイン名の下に指定します。

```lua
mare.plugins.add {
  { "you/mare-tally", opts = { tally = { warn_after = 10 } } },
}
```

1 つのリポジトリに複数のプラグインを含めることができ、ユーザーは `only = { "tally" }` でその一部だけを読み込めます。パッケージは `skills/`、`prompts/`、`themes/`、`types/` (Lua の型定義) を同梱することもできます。[パッケージ](../configure/packages.md) を参照してください。
