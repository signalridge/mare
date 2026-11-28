# 外観

mare の見た目を変更します。名前付きスタイル、テーマとフレーバー、UI コンポーネントとツールレンダラーの上書き、ステータスライン、枠線などの外観オプションを扱います。

## スタイル

スタイルは、`MareAccent` のような名前と、前景色、背景色、テキスト属性からなる仕様です。`mare.ui.style` で設定し、引数 1 つで呼び出すと読み取れます。

```lua
mare.ui.style("MareAccent", { fg = "#7aa2f7", bold = true })
mare.ui.style("MareDim", "#5c6370")          -- a string is a foreground color
local spec = mare.ui.style("MareAccent")             -- read it back
```

仕様では、`fg`、`bg` (テーマのスロット名または `#rrggbb`) と、真偽値の `bold`、`dim`、`italic`、`underline`、`strike`、`reverse` を指定できます。明示的に設定したスタイルは常にテーマより優先され、テーマが変わると再適用されます。

`theme` オプションは、複数のスタイルをまとめて同じように設定します。文字列の値は前景色です。

```lua
mare.setup {
  theme = {
    MareToolName = { fg = "#7aa2f7", bold = true },
    MareDim = "#5c6370",
  },
}
```

主なスタイル名:

| 名前 | 用途 |
|---|---|
| `MareText`、`MareMuted`、`MareDim`、`MareSubtle` | 本文と、その控えめな段階。 |
| `MareAccent`、`MareTitle`、`MareHeading` | 強調、タイトル、Markdown の見出し。 |
| `MareError`、`MareWarning`、`MareSuccess`、`MareInfo` | ステータスの色。 |
| `MareUserMessage`、`MareUserPrompt`、`MareReasoning` | あなたのメッセージとモデルの思考。 |
| `MareToolName`、`MareToolArgs`、`MareToolOk`、`MareToolError`、`MareToolPending`、`MareToolResult` | ツール呼び出しの行。 |
| `MareDiffAdd`、`MareDiffDelete`、`MareDiffAddWord`、`MareDiffDeleteWord` | 差分。 |
| `MareBorder`、`MareBorderActive`、`MareSelection` | パネルと選択中の行。 |
| `MareModeReadOnly`、`MareModeAuto`、`MareModeFull`、`MareModeCustom` | ステータスラインの権限カプセル。 |

`mare.ui.styles()` はすべての名前を返します。TUI では `/lua mare.ui.styles()` で一覧を出力できます。

## テーマとフレーバー

フレーバーは、暗いまたは明るいターミナル向けの、スタイル一式です。次のオプションで選びます。

| オプション | 既定値 | 意味 |
|---|---|---|
| `flavor` | `auto` | `auto`、`dark`、`light`、またはインストール済みテーマの名前。 |
| `flavor_dark` | `dark` | 背景が暗い場合や不明な場合に `auto` が選ぶフレーバー。 |
| `flavor_light` | `light` | 背景が明るい場合に `auto` が選ぶフレーバー。 |

`auto` では、mare は最初のフレームの後にターミナルへ背景色を問い合わせ、切り替えます。`/theme` はライブプレビュー付きのピッカーを開き、`/theme <name>` は直接選択します。どちらも選択を `state.json` に保存しますが、`init.lua` で設定した `flavor` は次回の起動でも優先されます。

```lua
mare.setup {
  flavor = "auto",
  flavor_dark = "night",     -- a theme you installed
  flavor_light = "light",
}
```

### テーマを書く

`<name>.lua` ファイル (シンタックスハイライトだけなら `.tmTheme` ドキュメント) を `~/.config/mare/themes/` に置きます。パッケージも、`themes/` ディレクトリに同じファイルを同梱できます。ファイルはテーブルを返します。最も単純な形は、暗いまたは明るいベースのパレットを変更するものです。

```lua
-- ~/.config/mare/themes/night.lua
return {
  base = "dark",                       -- "dark" or "light"
  palette = {
    accent = "#7aa2f7",
    accent2 = "#ff9e64",
    selection = "#2a2f45",
  },
}
```

パレットのキーは、`accent`、`accent2`、`text`、`muted`、`dim`、`subtle`、`border`、`selection`、`code_bg`、`green`、`yellow`、`red`、`blue`、`add_bg`、`del_bg`、`add_word`、`del_word`、`on_color` で、それぞれ `#rrggbb` で指定します。mare は名前付きスタイルをパレットから導出します。スタイルを直接制御するには、`palette` の代わりに `styles = { MareAccent = { fg = "#7aa2f7" } }` を返します。対になるテーマでは `variants = { dark = {...}, light = {...} }` を指定します。両方が必須で、その場合 `auto` はターミナルに従います。

ファイルを使わない場合は、`theme-default` プラグインのオプションでフレーバーを定義します。

```lua
mare.plugin("theme-default", {
  flavors = { night = { MareAccent = { fg = "#7aa2f7" } } },
  blend_background = false,   -- do not mix the palette with the terminal's background
})
```

`mare.plugin("theme-default", { dirs = { "~/themes" } })` は、検索されるディレクトリを置き換えます。

## コンポーネントを上書きする

トランスクリプトの各部分は `mare.ui` 内の Lua テーブルで、そのメソッドを置き換えられます。`init.lua` は既定値のインストール後に実行されるため、代入が優先されます。

```lua
-- replace how a tool call header looks
function mare.ui.ToolCall:header(summary, state)
  local mark = state == "ok" and "+" or state == "error" and "x" or "."
  local hl = state == "error" and "MareToolError" or "MareToolOk"
  return ui.Line({ ui.Span(mark .. " "):hl(hl), ui.Span(summary):hl("MareToolName") })
end

-- wrap a component instead of replacing it
local render = mare.ui.UserMessage.render
function mare.ui.UserMessage:render(text)
  return ui.Lines({ render(self, text), ui.Gap() })
end
```

上書きできるコンポーネントは、`UserMessage`、`AssistantMessage` (`render`、`marker`)、`Reasoning`、`TurnEnd`、`Notice`、`ToolCall`、`Status`、`Editor`、`Logo`、`Working`、およびオーバーレイの部品 `KeyHints`、`Tabs`、`Select`、`Confirm` です。コンポーネントは、メッセージの開始時と終了時、および無効化されたときに実行され、ストリームされるトークンごとに実行されることはありません。

`ui.Span`、`ui.Line`、`ui.Lines`、`ui.Gap`、`ui.Paragraph`、`ui.Row`、`ui.Block`、`ui.Markdown` などは、コンポーネントが返すノードを組み立てます。一覧は [Lua API](../extend/lua-api.md) のページにあります。

### ツールレンダラー

`mare.ui.renderer(kind, { compact = fn, expanded = fn })` は、1 つのツールの完了した結果の描画方法を変更します。`kind` は `"tool:<name>"` です。`compact` はスクロールバックで、`expanded` はトランスクリプト (`Ctrl+T`) で使われます。どちらも `(call, result)` を受け取ってノードを返します。この関数は、削除用の関数を返します。

```lua
mare.ui.renderer("tool:bash", {
  compact = function(call, result)
    local command = call.arguments and call.arguments.command or "bash"
    return ui.Line({ ui.Span("$ "):hl("MareAccent"), ui.Span(command) })
  end,
})
```

ユーザーと思考のメッセージも、同じ方法で `message:user` と `message:reasoning` のレンダラーを受け付け、それぞれ `(text)` と `(text, secs)` を受け取ります。

`init.lua` で設定したレンダラーとコンポーネントは、`/reload config` で削除され、設定し直されます。

## ステータスライン

`statusline` プラグインは、入力の下のバーを描画します。権限カプセル、モデルと effort、コンテキストのゲージ、ディレクトリ、ブランチ、プルリクエスト、変更されたファイル、コストです。`/statusline` はセグメントを非表示にするピッカーを開き、選択は `state.json` に保存されます。`init.lua` では次のように設定します。

```lua
mare.plugin("statusline", {
  hidden = { "pr", "cost" },     -- mode, model, effort, context, cwd, branch, pr, changes, cost, quota
  cwd_width = 32,
  gauge_width = 8,
  changed_files = true,
})
```

`layout` は別の配置を選びます。`combined` (既定)、`classic`、`codex`、`kimi` です。`layout_opts` はその個々の要素を上書きし、`looks` は独自の配置を定義します。

```lua
mare.plugin("statusline", {
  layout = "combined",
  layout_opts = { sep = "space", flash = 0, cwd_segments = 3 },
})
```

look はデータです。`left` と `right` はセグメントを列挙し、`drop` はターミナルが狭くなるにつれてセグメントが譲る順序を列挙します (`hints:short` は、削除する前に短縮します)。それ以外のキーは、各要素の形式を選びます。`sep` (`dot` または `space`)、`mode` (`word`、`capsule`、`dim`、`off`)、`model` (`text`、`dim`、`muted`)、`effort` (`reasoning`、`suffix`、`off`)、`jobs` (`bracket`、`icon`)、`context` (`kimi`、`left`、`gauge`)、`context_color`、`hints`、`flash` (変更が表示され続ける秒数)、`cwd_segments` です。

代わりに自分のスクリプトの出力を表示するには、`command` (シェルを介して実行される文字列、または argv のテーブル) を設定します。標準入力で JSON オブジェクト (セッション ID、cwd、幅、モデル、コンテキスト、コスト) を受け取り、その最初の出力行が表示されます。`command_replaces = true` は、組み込みのセグメントを隠します。

```lua
mare.plugin("statusline", { command = "~/bin/statusline.sh", command_timeout_ms = 500 })
```

バックグラウンド探索セグメント: `changes` (`git status`)、`pr` (`gh pr view`)、および外部 `command` の出力は、起動完了後 (`ui/ready`) にバックグラウンドで非同期に実行され、各ターン後に更新されます。そのため起動時の最初のフレームはサブプロセス探索でブロックされず、結果が到着すると該当セグメントの内容が更新されます (`branch` は最初のフレームで `.git/HEAD` から同期的に読み取られ、各ターン後にバックグラウンドで更新されます)。外部 `command` は `ui/ready` の後にのみ実行されるため、UI のないヘッドレスセッションや RPC セッションでは実行されません。その他のセグメント (`mode`、`hints`、`flags`、`model`、`effort`、`context`、`cwd`、`branch`、`cost`、`quota`、`peer_allow`、`tips`、`jobs`、`workflow`、`mcp`) は最初のフレームから同期的に描画され、準備完了後の状態と一致します。

自分のプラグインからセグメントを追加するには、`ui` サービスを使います: `ctx.ui:status(id, { align = "right", order = 20, render = function() ... end })`。[プラグインを書く](../extend/writing-a-plugin.md) を参照してください。

## その他の外観オプション

| オプション | 値 | 効果 |
|---|---|---|
| `border` | `rounded` (既定)、`single`、`double`、`thick`、`hidden` | パネルとポップアップの枠線。 |
| `icons` | `nerd`、`unicode`、`ascii` | アイコンセット (未設定時は端末から検出)。`nerd` ではコールアウトに加えてフェンスコードブロックの見出しにも Nerd Font の言語アイコンを表示します (`rust`、`python`、`ts`、`go`、`lua`、`bash`、`sql`、`docker`、`yaml` など 70 以上の言語・別名に対応)。 |
| `prompt` | 文字列、既定は `❯` | 入力プロンプトの記号。 |
| `terminal.ambiwidth` | `single`、`double` | 東アジアの曖昧幅文字の幅。Vim と同様。 |
| `spinner` | グリフのリスト、または `{ frames = {...}, still = "•" }` | スピナーのグリフ。 |
| `ui.working` | `{ glyph, label, elapsed, tokens, interrupt_hint, sep, tools, interval_ms, text_throttle_ms, frames, glyphs, verbs }` | 作業中の行のきめ細かな見た目設定 (既定 `{ glyph = "ember", label = "glint", elapsed = "auto", tokens = true, interrupt_hint = true, sep = "dot", tools = "still", text_throttle_ms = 250 }`)。 |
| `ui.tool_call`、`ui.stopping` | `{ spinner = "braille" }` / `{ spinner = true }` | ツール実行中・停止処理中のスピナー (`false` で静止ドット)。 |
| `markdown` | `{ glyphs = {...}, code_line_numbers = false }` | Markdown の描画 (`icons = "nerd"` のときフェンスコードブロック見出しに言語アイコンが付く)。 |
| `logo` | `true`、`false`、`"compact"` | 起動時のロゴ。 |
| `reduce_motion` | 真偽値 | アニメーションの代わりに静的なテキストにします。 |
| `images_protocol` | `kitty`、`iterm2`、`sixel`、`none` | ターミナルの画像プロトコル。未設定の場合は検出されます。 |
| `char_widths` | `{ from, to, width }` または `{ char, width }` のリスト | 表示幅の上書き。 |

```lua
mare.setup {
  border = "single",
  prompt = ">",
  icons = "nerd",
  ui = {
    working = {
      glyph = "ember",
      label = "glint",
      elapsed = "auto",
      tokens = true,
      interrupt_hint = true,
      sep = "dot",
      tools = "still",
      text_throttle_ms = 250,
    },
    tool_call = { spinner = "braille" },
  },
  markdown = { code_line_numbers = true },
  reduce_motion = true,
}
```

`ui.working` の各項目は個別に設定できます: `glyph` (`ember`、`swell`、`braille`、`moon`、`phase`、`wave`、`sail`、`bullet`、`pulse`、`none` または `glyphs` のキー)、`label` (`accent`、`shimmer`、`glint`、`foam`、`dots`)、`elapsed` (`auto`、`seconds`、`off`)、`tokens`、`interrupt_hint`、`sep` (`dot`、`bullet`)、`tools` (`still`、`spin`)、`interval_ms`、`text_throttle_ms`、インライン `frames` / カスタム `glyphs`、および各フェーズ (`reasoning`、`responding`、`tool`、`shell`、`exploring`) のラベルを変える `verbs`。`reduce_motion` のもとでは、すべてが静止します。既定値を含む完全な一覧は [オプション](options.md) のページにあります。

トランスクリプトに影響するプラグインのオプション、たとえば `preview_lines` (コンパクト表示でツール呼び出しごとに表示する出力の行数) や `diff_fold_lines` (コンパクト diff を折りたたむ前に残す行数。既定 `8`、`false` で全文表示) は、`ui` プラグインに属します。

```lua
mare.plugin("ui", { preview_lines = 4, diff_fold_lines = 12 })
```
