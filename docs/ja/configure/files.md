# 設定ファイル

mare は Lua で設定します。init.lua の場所、設定のレイヤーの組み合わせ方、設定のモジュール分割、再読み込み、エラー時の動作を説明します。

## 設定の置き場所

| パス | 用途 |
|---|---|
| `~/.config/mare/init.lua` | あなたの設定。`mare init` が作成します。 |
| `~/.config/mare/lua/` | `init.lua` から `require` で読み込むモジュール。 |
| `~/.config/mare/plugins/` | あなた自身のプラグイン。[プラグイン](../extend/plugins.md) を参照。 |
| `<project>/.mare/init.lua` | プロジェクトリソースルート (`.mare/` または `.mcp.json` を持つ最寄りのディレクトリ、git ルートまで) のプロジェクト設定。信頼済みのプロジェクトでのみ実行されます。 |
| `<project>/.mare/lua/`、`<project>/.mare/plugins/` | プロジェクトのモジュールとプラグイン。 |
| `~/.local/state/mare/state.json` | 実行時に行った選択 (`/model`、`/effort`、`/theme`、`/config` での編集)。手で編集しないでください。 |
| `~/.config/mare/mare-lock.json` | パッケージのロックファイル。[パッケージ](packages.md) を参照。 |

`$XDG_CONFIG_HOME` と `$XDG_STATE_HOME` が設定されている場合は、それぞれ `~/.config` と `~/.local/state` の代わりに使われます。`MARE_HOME=<dir>` を設定すると、すべてが `<dir>/config`、`<dir>/state`、`<dir>/data`、`<dir>/cache` の下に移動します。使い捨ての環境に便利です。

プロジェクトが信頼済みになるのは、信頼の確認で承認したとき、そのプロジェクトで `mare trust` を実行したとき、または `--trust` を渡したときです。`mare trust --revoke` は最初の 2 つを取り消します。`--no-trust` は、1 回の実行に限りそのプロジェクトを無視します。

## レイヤーの順序

すべてのオプションは 6 つのレイヤーから設定できます。後のレイヤーが前のレイヤーより優先されます。

| # | レイヤー | 例 |
|---|---|---|
| 1 | 既定値 | [オプション](options.md) に記載された値。 |
| 2 | `state.json` | `/model`、`/effort`、`/theme` で保存されたもの。 |
| 3 | ユーザーの `init.lua` | `mare.setup { model = "..." }` |
| 4 | プロジェクトの `.mare/init.lua` | 同じ API。信頼済みのプロジェクトのみ。 |
| 5 | 環境変数 | `MARE_BORDER=single` |
| 6 | コマンドライン | `--set border=single`、`-m`、`-e` |

実際には、次の 2 点が重要です。

- `state.json` が埋めるのは、`init.lua` が設定していない最上位のオプションだけです。`init.lua` が `models = {...}` を設定していれば、保存済みの `models` は全体が無視されます。
- 環境変数の値はオプション全体を置き換えます。`--set` の値はオプションにマージされるため、`--set plugins.tools.edit_mode=replace` は 1 つのキーだけを変更し、残りを保持します。

## 最初の init.lua

`mare init` は、コメント付きの出発点となるファイルと `.luarc.json` を書き出します。既存の `init.lua` を上書きすることはありません。

```sh
mare init
```

```lua
-- ~/.config/mare/init.lua
mare.setup {
  model = "openai/gpt-6-luna",
  border = "single",
}

mare.plugin("tools", { bash_timeout_ms = 120000 })
```

`mare.setup` はコアのオプションを受け取ります (`effort` は `--effort` や `MARE_EFFORT` による起動単位の上書き専用であり、`init.lua` のグローバル `effort` は無視されます)。テーブルは設定済みの内容と再帰的にマージされ、リストと単純な値は置き換えます。すべてのコアオプションとその型、既定値は [オプション](options.md) のページに一覧があります。

## プラグインのオプション

`mare.plugin(name, opts)` は、1 つのプラグインの既定値を上書きします。`mare.setup` と同じマージ規則を使います。

```lua
mare.plugin("statusline", { cwd_width = 32, changed_files = false })
mare.plugin("update-check", { enabled = false })
```

`enabled = false` はプラグインを無効にします。未知のプラグイン名やオプションキーは、「もしかして」のヒント付きの警告になります。同じ値はコマンドラインでも指定できます。

```sh
mare --set plugins.tools.edit_mode=replace
```

## 実行時のオプションの読み書き

`mare.o` はオプションを読み取り (なければ既定値を返します)、書き込みもします。`init.lua` での書き込みは `mare.setup` と同じですが、テーブルがマージされず置き換えられる点が異なります。起動後、たとえば `/lua` から書き込むと、即座に反映され、`option/changed` イベントが発行されます。

```lua
mare.o.border = "double"
mare.o.plugins.url_format = "https://git.example.com/%s.git"   -- dotted write into a table option
local current = mare.o.model                          -- read; falls back to the default
```

## モジュールと require

`~/.config/mare/lua/` が存在する場合、`init.lua` 内の `require` はそこから読み込みます。名前 `a.b` は `lua/a/b.lua` または `lua/a/b/init.lua` に解決されます。モジュール名には英字、数字、`_`、`-` を使えます。モジュールには、`init.lua` と同じ `mare` API が見えます。

```lua
-- ~/.config/mare/init.lua
require("keys")
require("looks")
```

```lua
-- ~/.config/mare/lua/keys.lua
mare.keymap.set("editor", "<M-g>", "transcript", { desc = "open the transcript" })
```

プロジェクトでは、まず `.mare/lua/`、次にユーザーのディレクトリ、最後に標準ライブラリが検索されます。直接または他のモジュールを介して自分自身を require するモジュールはエラーです。設定の評価は毎回、空のモジュールキャッシュから始まるため、`/reload config` はモジュールをもう一度実行します。

## 結果を確認する

`/config` はすべてのオプションを、値とその由来とともに一覧表示します。由来は、`default`、`state.json`、ファイルと行を示すレイヤー、`env MARE_<NAME>`、`cli` のいずれかです。`/config border` は名前で絞り込みます。設定したプラグインのオプションは 2 つ目の一覧に表示され、設定の警告は末尾に表示されます。全体のオーバーレイでは、文字列、数値、真偽値のオプションを Enter で編集できます。新しい値はすぐに適用されて `state.json` に保存されますが、`init.lua` で設定した値は、次回の起動でも優先されます。`/settings` は別名です。

## 再読み込み

| コマンド | 効果 |
|---|---|
| `/reload config` | `init.lua` とプロジェクトの `init.lua` をもう一度実行します。`plugins.<name>` のオプションが変わったプラグイン (およびそれに依存するプラグイン) だけを再読み込みし、変更されたコアオプションを通知します。 |
| `/reload` | 上記のすべてに加え、新しく宣言されたパッケージのインストール、新しいパッケージのプラグインの開始、削除されたパッケージのプラグインの停止、その他すべてのプラグインの再読み込みを行います。 |
| `/reload <plugin>` | 1 つのプラグインを再読み込みします。 |

`--set` の値は再読み込みをまたいで有効なままです。モデルへのリクエストの実行中や、別の再読み込みが進行中の場合、再読み込みは拒否されます。その後でもう一度実行してください。パッケージの変更には完全な `/reload` が必要で、`watchdog_ms` は起動時にのみ読み取られます。

評価に失敗した再読み込みは、古い設定をそのまま残します。次の登録は、試行の前の状態に戻されます。オプションとその由来、`init.lua` のキーバインド、設定が登録した UI コンポーネント、`mare.tool.command` のようなプラグインの設定関数が追加したエントリ、`mare.on` の購読です。元に戻らないのは、あなたの Lua が実行中に行ったそれ以外のことです。書き込まれたファイル、起動されたコマンド、設定されたグローバル変数などです。`init.lua` には副作用を持たせないでください。

## init.lua が失敗したとき

`init.lua` のエラーで mare の起動が止まることはありません。エラーより前の部分は実行され、それ以降は実行されません。

- TUI では、ダイアログにファイル、行、エラーの最初の行が表示されます。エディター (`editor` オプション、なければ `$VISUAL` / `$EDITOR`) でファイルを開く、終了する、続行する、を選べます。続行すると、ファイルを直して `/reload` が成功するまで、モデルへのリクエストはすべてブロックされます。ステータスラインには `config error · /reload` と表示されます。
- `-p`、`--json`、`--rpc` ではダイアログはなく、mare はエラーを出力して終了コード 2 で終了します。

未知のオプション名や型の誤りのように、エラーではない間違いは警告です。`/config` に表示され、ヘッドレスと RPC モードでは stderr に `mare: config warning: ...` として出力されます。`--strict-config` を指定すると、この 2 つのモードは代わりに終了コード 2 で終了します。

```sh
mare -p --strict-config "summarize README.md"
```

設定を一切使わずに起動するには、`--clean` を使います。`init.lua`、プロジェクトの設定、ユーザーとプロジェクトのプラグイン、パッケージ、`state.json` をスキップするため、組み込みプラグインとコマンドラインの値だけが残ります。

## エディターの補完

`mare init` は `~/.config/mare/.luarc.json` も書き出します。これは LuaLS を、`mare` API の Lua 型定義と、インストール済みパッケージが同梱する型ファイルに向けるものです。型定義は、mare のバージョンが変わるとランタイムディレクトリにコピーされます。`mare runtime` がそのパスを表示します (`~/.local/share/mare/runtime`、型定義は `types/`)。組み込みプラグインのソースも読み取り専用でそこにあり、読んだり `~/.config/mare/plugins/` にコピーしたりできます。`.luarc.json` を編集すると、mare はそれを書き換えなくなります。

## 機密値

API キーを `init.lua` に書かないでください。`mare login` は認証情報を `~/.config/mare/auth.json` に保存します。プロバイダーの `api_key` には、環境変数の名前、またはキーを出力するコマンドを指定できます。

```lua
mare.provider("openai", {
  base_url = "https://proxy.example.com/v1",
  api_key = { cmd = { "pass", "show", "proxy/openai" }, refresh_interval_ms = 3600000 },
})
```

`/config` と RPC の `options.get` メソッドは、キーが秘密情報らしく見える値 (`api_key`、`token`、`secret`、`password`、`credential`、`authorization`、`cookie`、`headers`、`query`、`env`) と、クエリ文字列やユーザー名を含む URL を隠します。自分のプラグインのオプションにも同様の印を付けられます。

```lua
mare.config_sensitive("plugins.myplugin.private_value")
```

これが影響するのは、mare が表示する内容だけです。ファイルを暗号化するわけではありません。
