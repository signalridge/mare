# キーバインド

mare.keymap でキーを割り当て、削除します。キー表記、コンテキストと条件、keys の省略記法、Lua アクション、既定のバインドを扱います。

## キーを割り当てる

`mare.keymap.set(context, key, action, opts)` は、`context` の中の `key` をアクションに割り当てます。アクションは、組み込みアクションの名前、Lua アクションの名前、または Lua 関数です。

```lua
-- ~/.config/mare/init.lua
mare.keymap.set("editor", "<C-o>", "transcript", { desc = "open the transcript" })

mare.keymap.set("editor", "<M-n>", function()
  ui.notify("hello from a key")
end, { desc = "say hello" })
```

`opts` では次を指定できます。

| フィールド | 意味 |
|---|---|
| `desc` | `/hotkeys` と `?` のヘルプに表示されるテキスト。 |
| `unique` | `true` のとき、そのコンテキストでキーがすでに割り当てられていればエラーにします。無効化（`false`）では無視されます。 |
| `when` | 述語、または述語のリスト。すべてが成り立つ間だけ、バインドがキーを受け取ります。無効化（`false`）には条件を付けられません。 |

`set` はハンドルを返します。`handle:remove()` または `mare.keymap.remove(handle)` で、そのバインドを取り消せます。

複数のバインドを 1 回のホスト更新でアトミックに登録するには、`mare.keymap.set_many(specs, opts?)` を使います。各要素は `{ context, key_or_keys, action, opts? }` または `{ context?, key?, keys?, action, desc?, unique?, priority?, when? }` の形式で、`opts` はバッチ全体に適用する既定値を渡します。すべての項目が検証されてからまとめて反映され、返されたハンドルは `set` と同じ形で `handle:remove()` または `mare.keymap.remove(handle)` により一括解除できます。

```lua
local batch = mare.keymap.set_many({
  { "editor", "<F7>", "redraw" },
  { "editor", "<F8>", "suspend", { when = "editor_empty" } },
  { "text", { "<M-b>", "<M-Left>" }, "text.word_left" },
})
```

バインドは読み込み順に関係なく 4 つのレイヤー (`project > user > plugin > default`) に積み重なります。組み込みの既定キーが `default`、サードパーティプラグインが `plugin`、`~/.config/mare/init.lua` が `user`、信頼済み `.mare/init.lua` が `project` です。同じレイヤー内では、無効化マスク (`false`) が `priority` に関係なくすべての通常バインドに優先します (たとえば `priority = 0` のマスクは登録順によらず同層の `priority = 10` のアクションに勝ちます)。通常バインド同士は `priority` (既定 `0`)、次いで登録順で並びます。削除すると下のバインドが戻り、一番下にはエディター自身の処理があります。

## キー表記

次の表記はすべて同じキーを表し、`<C-t>`、`C-t`、`Ctrl+T`、`ctrl+t` は `ctrl+t` に正規化されます。

| 書き方 | 意味 |
|---|---|
| `<C-t>`、`ctrl+t`、`C-t` | Ctrl+T |
| `<M-CR>`、`alt+enter` | Alt+Enter |
| `<S-Tab>`、`shift+tab` | Shift+Tab |
| `<Esc>`、`<Space>`、`<Up>`、`<F5>` | Escape、Space、Up、F5 |

修飾キー: `ctrl` (`c`、`control`)、`alt` (`m`、`meta`、`opt`、`option`)、`shift` (`s`)、`super` (`cmd`、`command`、`win`)。名前付きのキー: `esc`、`enter` (`return`、`cr`)、`tab`、`backspace` (`bs`)、`delete` (`del`)、`insert`、`up`、`down`、`left`、`right`、`home`、`end`、`pageup` (`pgup`)、`pagedown` (`pgdn`)、`space`、`f1` から `f12`。1 文字はそのまま、その文字を表します。

キーシーケンスは、1 つの文字列に複数のキーを並べたものです。`"<C-x>g"`、`"<C-x> g"`、`"ctrl+x g"` は同じです。最初のキーを押すと、mare は続きの候補を表示します。シーケンスは `editor`、`transcript` とそのサブコンテキストで使えます。

```lua
mare.keymap.set("editor", "<C-x>t", "transcript", { desc = "transcript (chord)" })
```

どのキーが mare に届くかは、そもそもターミナルによって異なります。`mare doctor` は Windows Terminal での Alt+Enter のような既知の競合を説明します。また、[オプション](options.md) の `key_conflicts` と `terminal_caps` オプションで、mare が把握している内容を拡張できます。

## コンテキスト

キーは、フォーカスと状態によって決まる検索チェーンに沿って解決されます。

| コンテキスト | 検索チェーン | 適用される場所 |
|---|---|---|
| `global` | `global` | `editor` と `transcript` が共有する非モーダルのフォールバック。 |
| `text` | `text` | テキスト入力欄 (`editor`、`input`) が共有する行編集コンテキスト。 |
| `editor` | `editor` → `text` → `global` | プロンプトの入力欄。 |
| `editor.completion`、`editor.queue`、`editor.history`、`editor.selection` | サブコンテキスト → `editor` → `text` → `global` | 補完メニュー表示中、キュー選択・編集中、履歴閲覧中、マウス選択中に有効。 |
| `input` | `input` → `text` | `mare.ui.input` の入力ダイアログ (行編集キーは `text` にフォールバック)。 |
| `transcript` | `transcript` → `global` | トランスクリプトのオーバーレイ (`Ctrl+T`) と、サブエージェントのビューのようにプラグインが開くビュー。 |
| `transcript.search` | `transcript.search` → `transcript` → `global` | トランスクリプトで `/` 検索を入力中に有効 (印字可能文字は `transcript` や `global` より先に検索語へ追加されます)。 |
| `select` | `select` | 選択ダイアログ (`mare.ui.select`)。キーシーケンスを利用できます。 |
| `select.vim` | `select.vim` → `select` | `vim = true` で開いた選択ダイアログで検索入力中でないときに有効。 |
| `select.search` | `select.search` → `select` | 選択ダイアログで `/` 検索を入力中に有効 (印字可能文字は `select` より先にフィルターへ追加されます)。 |
| `confirm` | `confirm` | 確認ダイアログ (`mare.ui.confirm`)。 |
| `help` | `help` | キーヘルプのモーダル (`?`)。 |
| `info` | `info` | スクロール可能な情報オーバーレイ (`mare.ui.overlay`)。 |

印字可能文字のルール: チェーンに `text` が含まれ、`Ctrl` / `Alt` を伴わない印字可能文字が押された場合、検索は `text` の後で止まります (`global` のバインドに入力を奪われません)。

`context` はリストでも構いません: `mare.keymap.set({ "editor", "transcript" }, "<F2>", "redraw")`。プラグインは `mare.keymap.context(name, { parent })` でサブコンテキストを宣言できます (`name` は `parent .. "." .. seg`、`seg` は `^[a-z][a-z0-9_-]*$` に一致)。`handle:remove()` でそのコンテキスト、子孫、バインドをすべて削除します。未知のコンテキスト名はエラーです。インストール画面と起動エラー画面はキーマップを経由しません。

## 予約キー

`Ctrl+C` はコアの予約キーで、バインドや解除はできません。最上位のフォーカスに作用します。

| フォーカス | 動作 |
|---|---|
| モーダル | 閉じて理由 `interrupted` を返します。Esc などの通常のキャンセルは `cancelled` です。 |
| 待機中のプレフィックスまたは補完 | プレフィックスを取り消すか、補完メニューを閉じます。 |
| トランスクリプトまたはプラグインビュー | 入力中の検索を取り消し、それ以外ではビューを閉じます。 |
| 入力欄 | 選択範囲のコピー、下書きの消去、作業の中断、または 2 回目の押下で終了します。 |

## 条件

`when` を使うと、1 つのキーに異なる動作をさせられます。述語は固定で、プラグインから追加することはできません。名前の前に `!` を付けると否定になります。

| 述語 | 真になる条件 |
|---|---|
| `editor_empty` | 入力欄にテキストがない。 |
| `completion_open` | 補完メニューが開いている。 |
| `paste_burst` | 高速に連続したキー入力が貼り付けとして扱われている。 |
| `working` | エージェントが実行中。 |

```lua
-- F6 queues a follow-up, but only while the agent is busy
mare.keymap.set("editor", "<F6>", "follow-up", { when = "working" })

-- "?" opens the key help only on an empty input
mare.keymap.set("editor", "?", "keymap-help", { when = { "editor_empty", "!completion_open" } })
```

## アクション

文字列のアクションは、コアアクション、次に名前付きの Lua アクションの順で検索されます。登録されていないアクションはキーを次のバインドまたはエディターに渡します。コアアクションは対応コンテキストを持ち、対応外のコンテキストへの割り当てはエラーになります。`mare.keymap.actions()` で登録済みアクション一覧を取得できます。

`editor` コンテキストのコアアクション:

| アクション | 動作 |
|---|---|
| `interrupt` | 補完を閉じる、または実行中の作業を停止します。アイドル時に 2 回押すと巻き戻しピッカー。 |
| `quit-if-empty` | 入力が空のとき、終了します。 |
| `quit` | 終了します。 |
| `transcript` | トランスクリプトのオーバーレイを開きます。 |
| `toggle-expand` | ツール出力をコンパクト表示と展開表示で切り替えます。 |
| `steer` | 入力を実行中のターンに送ります。 |
| `follow-up` | 現在のターンの後に実行されるよう、入力をキューに入れます。 |
| `history-search` | 入力履歴を検索します。 |
| `external-editor` | エディターで入力を編集します。 |
| `paste-image` | クリップボードの画像を貼り付けます。 |
| `complete` | `@file` や `/command` を補完します。 |
| `newline` | 改行を挿入します。 |
| `redraw` | 画面を再描画します。 |
| `suspend` | mare を一時停止します (Windows では不可)。 |
| `select-mode` | マウスでライブ領域のテキストを選択します。 |
| `keymap-help` | すべてのバインドを検索できる一覧を開きます。 |
| `editor.submit` | 入力を送信します (直前が `\` なら改行を挿入)。 |

`text` コンテキスト (`editor` と `input` が共有) の行編集アクションは、`text.newline`、`text.left`、`text.right`、`text.word_left`、`text.word_right`、`text.line_start`、`text.line_end`、`text.up`、`text.down`、`text.delete_back`、`text.delete_word_back`、`text.delete_forward`、`text.delete_word_forward`、`text.kill_line_start`、`text.kill_line_end`、`text.yank`、`text.yank_pop`、`text.undo` です。

`input` コンテキストのダイアログアクションは、`input.accept` (入力を確定) と `input.cancel` (`cancelled` で閉じる) です。

`transcript` コンテキストのアクションは、`view.close`、`view.back`、`view.up`、`view.down`、`view.page_up`、`view.page_down`、`view.top`、`view.bottom`、`view.search`、`view.copy`、`view.next_match`、`view.prev_match` です。

モーダルコンテキストのアクションと既定キー (`keymap-default` が登録):
- `select` (`select.vim` は `k`/`j`/`g`/`<S-g>`/`l`/`h` を追加): `select.up` (`<Up>`)、`select.down` (`<Down>`)、`select.page_up` (`<PageUp>`)、`select.page_down` (`<PageDown>`)、`select.top` (`<Home>`)、`select.bottom` (`<End>`)、`select.accept` (`<CR>`)、`select.back` (`<Esc>`)、`select.search` (`/`)、`select.tab_next` (`<Tab>`、`<Right>`、`]`)、`select.tab_prev` (`<S-Tab>`、`<Left>`、`[`)、`select.help` (`?`)、`select.preview_up`/`down`/`page_up`/`page_down`/`top`/`bottom` (`<S-Up>`/`<S-Down>`/`<S-PageUp>`/`<S-PageDown>`/`<S-Home>`/`<S-End>`)、`select.body_up`/`down`/`page_up`/`page_down`/`top`/`bottom` (`<C-Up>`/`<C-Down>`/`<C-PageUp>`/`<C-PageDown>`/`<C-Home>`/`<C-End>`)。
- `confirm`: `confirm.yes` (`y`、`<S-y>`)、`confirm.no` (`n`、`<S-n>`)、`confirm.toggle` (`<Left>`、`<Right>`、`<Tab>`、`<S-Tab>`、`h`、`l`)、`confirm.accept` (`<CR>`)、`confirm.cancel` (`<Esc>`、`q`)、`confirm.scroll_up`/`scroll_down`/`page_up`/`page_down`/`top`/`bottom` (`<Up>`/`k`、`<Down>`/`j`、`<PageUp>`、`<PageDown>`、`<Home>`、`<End>`)。
- `help`: `help.up` (`<Up>`)、`help.down` (`<Down>`)、`help.accept` (`<CR>`)、`help.cancel` (`<Esc>`)。
- `info`: `info.up` (`<Up>`、`k`)、`info.down` (`<Down>`、`j`)、`info.page_up` (`<PageUp>`)、`info.page_down` (`<PageDown>`、`<Space>`)、`info.top` (`<Home>`)、`info.bottom` (`<End>`)。

プラグインはさらにアクションを追加します。組み込みの `copy` プラグインは、既定のキーを持たない `copy.last` を定義します。`agents` プラグインは `agents`、`agents.back`、`agents.prev`、`agents.next` を定義します。

```lua
mare.keymap.set("editor", "<M-c>", "copy.last", { desc = "copy the last response" })
```

### Lua アクション

`mare.keymap.action(name, fn, opts)` は名前付きのアクションを登録します。これにより、ユーザーは名前で再割り当てでき、ヒントもそのキーを見つけられます。`fn(ev)` は `{ key, context }` を受け取ります。最初の yield まで即座に実行されるため、何かを待つ前に判断してください。ちょうど `false` を返すと辞退となり、キーは下にあるバインドと検索チェーンへ、次にエディターへと渡ります。それ以外の結果は、キーを受け取ったことになります。

`opts.keys` ではコンテキストごとに既定キーを指定します。値はキー文字列、`{ key = "<F5>", when = "working" }` のようなレコード、または文字列とレコードのリストです。単一レコードには文字列の `key` が必要で、空のテーブルはキーなしを表します。アクションを解除すると既定バインドも解除されます。

```lua
mare.keymap.action("greet", function(ev)
  if ev.context ~= "editor" then return false end   -- decline: the key falls through
  ui.notify("pressed " .. ev.key)
end, { desc = "show the key that ran this action" })

mare.keymap.set("editor", "<M-d>", "greet")
```

`set` に直接渡した関数も、同じように動作します。

## keys の省略記法

`mare.setup { keys = { ... } }` は、テーブルからエディターのキーを割り当てます。値が `false` または `"none"` の場合、それ以前にプラグインレイヤーで登録されたバインド（`keymap-default` や `commands` など）を無効にします。後から読み込まれるプラグインを無効にするには、`init.lua` で `mare.keymap.set` を使ってください。

```lua
mare.setup {
  keys = {
    ["<C-o>"] = "transcript",
    ["<M-p>"] = false,          -- free Alt+P (the model picker's default key)
  },
}
```

1 つのキーを、異なるアクションで 2 通りに書く (`"<C-o>"` と `"ctrl+o"`) とエラーになり、どちらも割り当てられません。`editor` 以外のコンテキスト、`when`、関数が必要な場合は `mare.keymap.set` を使ってください。

## バインドの削除と確認

| 呼び出し | 動作 |
|---|---|
| `mare.keymap.set(context, key, false)` | 呼び出し元のレイヤー以下とチェーン後続コンテキストにある、このコンテキストのキーのバインドを無効にします。返されたハンドルを削除すると解除されます。 |
| `mare.keymap.del(context, key)` | 呼び出し元のレイヤーにある `context` 内の `key` の最も新しいバインドだけを削除します。下にあったものが戻ります。 |
| `mare.keymap.context(name, { parent })` | `parent` の下にプラグイン用サブコンテキストを宣言します。`handle:remove()` で子孫とバインドごと削除します。 |
| `mare.keymap.remove(handle)` | `set` の呼び出しが返したバインドを削除します。 |
| `mare.keymap.list([opts])` | 到達可能でマスクされていないすべてのバインドを `{ key, context, desc, action, when }` として返します (下の既定表にある 2 つの `Ctrl+R` のように、同じコンテキスト・同じキーで複数行になることがあります)。`list({ context = "editor" })` と `list({ snapshot = true })` を指定できます。 |
| `mare.keymap.hint(action, context)` | `context` がフォーカスのとき `action` を起動できるキーを返します (`project > user > plugin > default`、同層内は `priority` と登録順)。 |
| `mare.keymap.actions()` | 登録済みのコアアクションと Lua アクション (`{ name, desc, contexts, core, essential }`)。 |

```lua
mare.keymap.set("editor", "<C-z>", false) -- disable suspend
```

`/hotkeys` は TUI で同じ一覧を表示します。入力が空のときに `?` を押すと、検索できる一覧が開きます。`/reload config` と `/reload` では、まず設定再評価がステージングした `init.lua` のバインドを確定し、その後にプラグイン再読み込み用のキーマップガードが開かれます。`init.lua` の評価自体が失敗すれば元の設定バインドが保たれますが、後続のプラグイン再読み込みが失敗または中断された場合、プラグイン側のキーマップはガードで扱われる一方で、すでに確定した新しい設定バインドは有効なまま残ることがあります。

必須アクション (`editor.submit`、`input.accept`、`input.cancel`) にキーが割り当てられていない場合 (`keymap-default` 無効化や明示的な解除など)、暗黙に既定キーを復活させず、`/hotkeys` の `No key` と `mare doctor` の警告で報告します。

## 既定のバインド

コアアクションの既定キーは組み込みプラグイン `keymap-default` が登録し、プラグイン独自のアクション (`review.viewer`、`agents`、`model.picker`、`model.cycle`、`effort.down`、`effort.up` など) もそれぞれのプラグインが `default` レイヤーに登録します。`mare.plugin("keymap-default", { enabled = false })` でコア既定キーをすべて無効にできます。他のプラグインが登録したキーは変わらず、予約キー `Ctrl+C` もそのまま使えます。

`init.lua` で `mare.keymap.set` を使って既定キーを変更または解除できます:

```lua
mare.keymap.set("editor", "<M-o>", "model.picker")
mare.keymap.set("editor", "<M-m>", false) -- model.cycle を無効化
mare.keymap.set("editor", "<C-y>", "review.viewer")
mare.keymap.set("editor", "<M-a>", "agents", { when = "editor_empty" })
```

<!-- BEGIN GENERATED KEYMAPS (scripts/gen_docs.py) -->
| コンテキスト | キー | アクション | 条件 | 説明 |
|---|---|---|---|---|
| `editor` | `?` | `keymap-help` | `editor_empty`, `!completion_open`, `!paste_burst` | all key bindings (on an empty input) |
| `editor` | `Alt+,` | `effort.down` |  | lower reasoning effort (the model's levels) |
| `editor` | `Alt+.` | `effort.up` |  | raise reasoning effort (the model's levels) |
| `editor` | `Alt+Enter` | `follow-up` | `working` | queue the input as a follow-up |
| `editor` | `Alt+M` | `model.cycle` |  | next favorite / recent model |
| `editor` | `Alt+P` | `model.picker` |  | select model (keeps the input) |
| `editor` | `Alt+S` | `select-mode` |  | select text in the live area with the mouse, release copies |
| `editor` | `Alt+V` | `paste-image` |  | paste an image from the clipboard (Windows) |
| `editor` | `Ctrl+B` | `background` |  | move the running command to the background |
| `editor` | `Ctrl+D` | `quit-if-empty` |  | exit when the input is empty |
| `editor` | `Ctrl+G` | `external-editor` |  | edit the input in $EDITOR |
| `editor` | `Ctrl+L` | `redraw` |  | redraw the screen |
| `editor` | `Ctrl+O` | `toggle-expand` |  | toggle compact / expanded tool output and thinking |
| `editor` | `Ctrl+Q` | `follow-up` |  | queue the input as a follow-up (Windows) |
| `editor` | `Ctrl+R` | `history-search` |  | search input history |
| `editor` | `Ctrl+R` | `review.viewer` |  | review changes: mare's edits per turn, or git unstaged / staged / last commit |
| `editor` | `Ctrl+S` | `steer` |  | send the input into the running turn |
| `editor` | `Ctrl+T` | `transcript` |  | expanded transcript |
| `editor` | `Ctrl+V` | `paste-image` |  | paste an image from the clipboard (Unix / macOS) |
| `editor` | `Ctrl+Z` | `suspend` |  | suspend mare (resume with fg) |
| `editor` | `Enter` | `editor.submit` |  | send the input |
| `editor` | `Esc` | `interrupt` |  | interrupt the agent · twice when idle: rewind picker (/undo) |
| `editor` | `←` | `agents` | `editor_empty` | sub-agents and other sessions (/agents) |
| `editor` | `Shift+Tab` | `permissions.cycle` | `!completion_open` | cycle permission mode |
| `editor` | `Tab` | `complete` |  | complete @file or /command |
| `text` | `Alt+B` | `text.word_left` |  | one word left |
| `text` | `Alt+Backspace` | `text.delete_word_back` |  | delete the word before the cursor |
| `text` | `Alt+D` | `text.delete_word_forward` |  | delete the word after the cursor |
| `text` | `Alt+delete` | `text.delete_word_forward` |  | delete the word after the cursor |
| `text` | `Alt+Enter` | `text.newline` |  | new line |
| `text` | `Alt+F` | `text.word_right` |  | one word right |
| `text` | `Alt+←` | `text.word_left` |  | one word left |
| `text` | `Alt+→` | `text.word_right` |  | one word right |
| `text` | `Alt+Shift+Enter` | `text.newline` |  | new line |
| `text` | `Alt+Shift+←` | `text.select_word_left` |  | extend selection one word left |
| `text` | `Alt+Shift+→` | `text.select_word_right` |  | extend selection one word right |
| `text` | `Alt+Shift+Z` | `text.redo` |  | redo the last undone edit |
| `text` | `Alt+Y` | `text.yank_pop` |  | cycle older deletions |
| `text` | `Backspace` | `text.delete_back` |  | delete the character before the cursor |
| `text` | `Ctrl+_` | `text.undo` |  | undo the last edit |
| `text` | `Ctrl+A` | `text.line_start` |  | start of line |
| `text` | `Ctrl+B` | `text.left` |  | one character left |
| `text` | `Ctrl+Backspace` | `text.delete_word_back` |  | delete the word before the cursor |
| `text` | `Ctrl+D` | `text.delete_forward` |  | delete the character after the cursor |
| `text` | `Ctrl+delete` | `text.delete_word_forward` |  | delete the word after the cursor |
| `text` | `Ctrl+E` | `text.line_end` |  | end of line |
| `text` | `Ctrl+End` | `text.line_end` |  | end of line |
| `text` | `Ctrl+F` | `text.right` |  | one character right |
| `text` | `Ctrl+H` | `text.delete_back` |  | delete the character before the cursor |
| `text` | `Ctrl+Home` | `text.line_start` |  | start of line |
| `text` | `Ctrl+J` | `text.newline` |  | new line |
| `text` | `Ctrl+K` | `text.kill_line_end` |  | delete to the end of the line |
| `text` | `Ctrl+←` | `text.word_left` |  | one word left |
| `text` | `Ctrl+N` | `text.down` |  | line down |
| `text` | `Ctrl+P` | `text.up` |  | line up |
| `text` | `Ctrl+→` | `text.word_right` |  | one word right |
| `text` | `Ctrl+Shift+←` | `text.select_word_left` |  | extend selection one word left |
| `text` | `Ctrl+Shift+→` | `text.select_word_right` |  | extend selection one word right |
| `text` | `Ctrl+Shift+Z` | `text.redo` |  | redo the last undone edit |
| `text` | `Ctrl+U` | `text.kill_line_start` |  | delete to the start of the line |
| `text` | `Ctrl+W` | `text.delete_word_back` |  | delete the word before the cursor |
| `text` | `Ctrl+Y` | `text.yank` |  | paste the last deletion |
| `text` | `Ctrl+Z` | `text.undo` |  | undo the last edit |
| `text` | `delete` | `text.delete_forward` |  | delete the character after the cursor |
| `text` | `↓` | `text.down` |  | line down |
| `text` | `End` | `text.line_end` |  | end of line |
| `text` | `Home` | `text.line_start` |  | start of line |
| `text` | `←` | `text.left` |  | one character left |
| `text` | `→` | `text.right` |  | one character right |
| `text` | `Shift+Backspace` | `text.delete_back` |  | delete the character before the cursor |
| `text` | `Shift+delete` | `text.delete_forward` |  | delete the character after the cursor |
| `text` | `Shift+↓` | `text.select_down` |  | extend selection one line down |
| `text` | `Shift+End` | `text.select_line_end` |  | extend selection to end of line |
| `text` | `Shift+Enter` | `text.newline` |  | new line |
| `text` | `Shift+Home` | `text.select_line_start` |  | extend selection to start of line |
| `text` | `Shift+←` | `text.select_left` |  | extend selection one character left |
| `text` | `Shift+→` | `text.select_right` |  | extend selection one character right |
| `text` | `Shift+↑` | `text.select_up` |  | extend selection one line up |
| `text` | `↑` | `text.up` |  | line up |
| `input` | `Enter` | `input.accept` |  | confirm input |
| `input` | `Esc` | `input.cancel` |  | cancel input |
| `transcript` | `/` | `view.search` |  | search |
| `transcript` | `Alt+←` | `agents.prev` |  | the previous sub-agent |
| `transcript` | `Alt+→` | `agents.next` |  | the next sub-agent |
| `transcript` | `B` | `view.page_up` |  | scroll up a page |
| `transcript` | `Ctrl+T` | `view.close` |  | close the transcript |
| `transcript` | `↓` | `view.down` |  | scroll down a line |
| `transcript` | `End` | `view.bottom` |  | go to the bottom |
| `transcript` | `Esc` | `agents.back` |  | back to the agents list |
| `transcript` | `Esc` | `view.back` |  | clear the search, else close the transcript |
| `transcript` | `F` | `view.page_down` |  | scroll down a page |
| `transcript` | `G` | `view.top` |  | go to the top |
| `transcript` | `Home` | `view.top` |  | go to the top |
| `transcript` | `J` | `view.down` |  | scroll down a line |
| `transcript` | `K` | `view.up` |  | scroll up a line |
| `transcript` | `N` | `view.next_match` |  | next search match |
| `transcript` | `PageDown` | `view.page_down` |  | scroll down a page |
| `transcript` | `PageUp` | `view.page_up` |  | scroll up a page |
| `transcript` | `Q` | `view.close` |  | close the transcript |
| `transcript` | `Shift+G` | `view.bottom` |  | go to the bottom |
| `transcript` | `Shift+N` | `view.prev_match` |  | previous search match |
| `transcript` | `Space` | `view.page_down` |  | scroll down a page |
| `transcript` | `↑` | `view.up` |  | scroll up a line |
| `transcript` | `Y` | `view.copy` |  | copy the message at the reading position |
| `select` | `/` | `select.search` |  | search (fuzzy) |
| `select` | `?` | `select.help` |  | all keys |
| `select` | `[` | `select.tab_prev` |  | previous tab |
| `select` | `]` | `select.tab_next` |  | next tab |
| `select` | `Ctrl+↓` | `select.body_down` |  | scroll body down |
| `select` | `Ctrl+End` | `select.body_bottom` |  | scroll body to bottom |
| `select` | `Ctrl+Home` | `select.body_top` |  | scroll body to top |
| `select` | `Ctrl+P` | `select.toggle_preview` |  | toggle preview |
| `select` | `Ctrl+PageDown` | `select.body_page_down` |  | scroll body page down |
| `select` | `Ctrl+PageUp` | `select.body_page_up` |  | scroll body page up |
| `select` | `Ctrl+↑` | `select.body_up` |  | scroll body up |
| `select` | `↓` | `select.down` |  | next item |
| `select` | `End` | `select.bottom` |  | last item |
| `select` | `Enter` | `select.accept` |  | choose the selected item |
| `select` | `Esc` | `select.back` |  | clear search / close |
| `select` | `Home` | `select.top` |  | first item |
| `select` | `←` | `select.tab_prev` |  | previous tab |
| `select` | `PageDown` | `select.page_down` |  | page down |
| `select` | `PageUp` | `select.page_up` |  | page up |
| `select` | `→` | `select.tab_next` |  | next tab |
| `select` | `Shift+↓` | `select.preview_down` |  | scroll preview down |
| `select` | `Shift+End` | `select.preview_bottom` |  | scroll preview to bottom |
| `select` | `Shift+Home` | `select.preview_top` |  | scroll preview to top |
| `select` | `Shift+PageDown` | `select.preview_page_down` |  | scroll preview page down |
| `select` | `Shift+PageUp` | `select.preview_page_up` |  | scroll preview page up |
| `select` | `Shift+Tab` | `select.tab_prev` |  | previous tab |
| `select` | `Shift+↑` | `select.preview_up` |  | scroll preview up |
| `select` | `Space` | `select.toggle` |  | toggle selection |
| `select` | `Tab` | `select.tab_next` |  | next tab |
| `select` | `↑` | `select.up` |  | previous item |
| `select.vim` | `G` | `select.top` |  | first item |
| `select.vim` | `H` | `select.tab_prev` |  | previous tab |
| `select.vim` | `J` | `select.down` |  | next item |
| `select.vim` | `K` | `select.up` |  | previous item |
| `select.vim` | `L` | `select.tab_next` |  | next tab |
| `select.vim` | `Shift+G` | `select.bottom` |  | last item |
| `confirm` | `↓` | `confirm.scroll_down` |  | scroll down |
| `confirm` | `End` | `confirm.bottom` |  | scroll to bottom |
| `confirm` | `Enter` | `confirm.accept` |  | confirm choice |
| `confirm` | `Esc` | `confirm.cancel` |  | cancel |
| `confirm` | `H` | `confirm.toggle` |  | switch choice |
| `confirm` | `Home` | `confirm.top` |  | scroll to top |
| `confirm` | `J` | `confirm.scroll_down` |  | scroll down |
| `confirm` | `K` | `confirm.scroll_up` |  | scroll up |
| `confirm` | `L` | `confirm.toggle` |  | switch choice |
| `confirm` | `←` | `confirm.toggle` |  | switch choice |
| `confirm` | `N` | `confirm.no` |  | answer no |
| `confirm` | `PageDown` | `confirm.page_down` |  | scroll page down |
| `confirm` | `PageUp` | `confirm.page_up` |  | scroll page up |
| `confirm` | `Q` | `confirm.cancel` |  | cancel |
| `confirm` | `→` | `confirm.toggle` |  | switch choice |
| `confirm` | `Shift+N` | `confirm.no` |  | answer no |
| `confirm` | `Shift+Tab` | `confirm.toggle` |  | switch choice |
| `confirm` | `Shift+Y` | `confirm.yes` |  | answer yes |
| `confirm` | `Tab` | `confirm.toggle` |  | switch choice |
| `confirm` | `↑` | `confirm.scroll_up` |  | scroll up |
| `confirm` | `Y` | `confirm.yes` |  | answer yes |
| `help` | `↓` | `help.down` |  | next item |
| `help` | `Enter` | `help.accept` |  | run selected key |
| `help` | `Esc` | `help.cancel` |  | close |
| `help` | `↑` | `help.up` |  | previous item |
| `info` | `↓` | `info.down` |  | scroll down |
| `info` | `End` | `info.bottom` |  | scroll to bottom |
| `info` | `Esc` | `info.close` |  | close overlay |
| `info` | `Home` | `info.top` |  | scroll to top |
| `info` | `J` | `info.down` |  | scroll down |
| `info` | `K` | `info.up` |  | scroll up |
| `info` | `PageDown` | `info.page_down` |  | scroll page down |
| `info` | `PageUp` | `info.page_up` |  | scroll page up |
| `info` | `Space` | `info.page_down` |  | scroll page down |
| `info` | `↑` | `info.up` |  | scroll up |
<!-- END GENERATED KEYMAPS -->
