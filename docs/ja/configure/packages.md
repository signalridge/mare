# パッケージ

mare.plugins.add で git からプラグイン、テーマ、スキルをインストールします。宣言とバージョンの固定、ロックファイル、mare plugin コマンド、起動時の動作を扱います。

## パッケージを宣言する

`mare.plugins.add` はリストを受け取ります。各エントリは文字列またはテーブルです。

```lua
-- ~/.config/mare/init.lua
mare.plugins.add {
  "someone/mare-git-tools",                              -- newest semver tag, else the default branch
  { "someone/mare-extras", version = "^1.2" },           -- a semver range over the tags
  { "someone/monorepo:mare", only = { "tidy" } },        -- a subdirectory; load only some plugins
  { "git@example.com:team/internal-tools.git", branch = "main" },
  { dir = "~/code/my-plugin" },                          -- a local directory, never installed
  { import = "packages" },                               -- more declarations from lua/packages.lua
}
```

短い名前 `owner/repo` は、`plugins.url_format` オプション (既定は `https://github.com/%s.git`) で展開されます。完全なアドレス (URL、`ssh` アドレス、パス) も使えます。使われるトランスポートは `https`、`ssh`、`file` だけで、git がパスワードを尋ねることはありません。プレーンな `http://` と `git://` には `plugins = { allow_insecure = true }` が必要です。

テーブルエントリのフィールド:

| フィールド | 意味 |
|---|---|
| `[1]` | ソース。`owner/repo`、`owner/repo:subdir`、または git アドレス。 |
| `name` | 表示名。`[1]` がない場合は、これがソースになります。 |
| `only` | パッケージから読み込むプラグインの名前。残りは無視されます。 |
| `subdir` | リポジトリのサブディレクトリ (`owner/repo:subdir` と同じ)。 |
| `version`、`tag`、`branch`、`commit`、`rev` | チェックアウトする対象。後述。 |
| `pin` | `true`: `update` はその位置のままにします。 |
| `submodules` | `false`: git サブモジュールを取得しません。 |
| `dir` | ローカルディレクトリ。そこから読み込まれ、インストールもロックもされません。 |
| `dependencies` | 先に読み込まれるパッケージ (文字列または完全なエントリ)。 |
| `enabled` | `false`、または真偽値を返す関数。無効なパッケージはインストールも読み込みもされず、ロックのエントリは残ります。 |
| `required` | `true`: このパッケージの導入・読み込みに失敗すると、修復して `/reload` するまでモデルリクエストをブロックします (宣言マージだけを制御する `optional` とは別)。 |
| `optional` | 宣言マージ専用: 他の場所でもそのパッケージが宣言されている場合にのみ、このエントリが有効になります。 |
| `opts` | プラグインごとの既定値 `{ pluginname = { ... } }`、またはそれを返す関数。 |
| `opts_extend` | `opts` 内のドット区切りパス。宣言をまたいでリストを置き換えるのではなく連結します。 |
| `init` | `function(pkg)`。パッケージのプラグインが読み込まれる前に実行されます。 |
| `config` | `function(pkg, opts)`。プラグインの読み込み後に実行され、`/reload` でも再度実行されます。 |
| `import` | 追加の宣言を返す、`lua/` 配下のモジュール。 |

複数回宣言されたパッケージはマージされます。単一のフィールドは最後の値が優先され、`opts` は深くマージされ、`dependencies` と `only` は累積され、最後に明示された `enabled` 値が有効になります (後続の宣言で省略した場合は前の値を保持)。`opts` のオプションは `mare.plugin` の下に位置づけられ、`mare.plugin` で設定した値が常に優先されます。

```lua
mare.plugins.add {
  {
    "someone/mare-extras",
    version = "^1.2",
    opts = { tidy = { max_lines = 80 } },
    config = function(pkg, opts)
      mare.log("info", pkg.name .. " ready")
    end,
  },
}
```

### 宣言を複数のファイルに分ける

`{ import = "packages" }` は、`lua/packages.lua`、`lua/packages/init.lua`、`lua/packages/*.lua`、および `lua/packages/<サブディレクトリ>/init.lua` のすべてを読み込みます。各ファイルは宣言のリストを返します。インポートは最大 16 階層まで入れ子にでき、各モジュールは 1 回だけインポートされ、循環は警告付きでスキップされます。

```lua
-- ~/.config/mare/lua/packages/git.lua
return {
  "someone/mare-git-tools",
  { "someone/mare-extras", enabled = function() return mare.os ~= "windows" end },
}
```

## バージョンを固定する

次のフィールドのうち、最初に指定されたものが対象を決めます。順序は次のとおりです。

| フィールド | 対象 |
|---|---|
| `commit = "<sha>"` | そのコミット。`update` が動かすことはありません。 |
| `tag = "v1.4.0"` | そのタグ。 |
| `branch = "main"` | そのブランチの先頭。 |
| `version = "^1.2"` | semver の範囲内で最も新しいタグ。 |
| `rev = "<ref>"` | 任意の git リビジョン。`rev = "=<sha>"` は厳密な固定です。 |
| なし | 最も新しい semver タグ。なければ既定のブランチ。 |

宣言を変更するまでパッケージをその位置に留めるには、`pin = true` を追加します。ソース定義に `auto_update = true` (既定は `false`) を指定すると、バックグラウンドの定期確認で新しいリビジョンを取得し、次の `/reload` または次回起動時にチェックアウトを切り替えます (実行中セッションのプラグイン読み込み中に切り替えることはありません)。

既定では TUI のバックグラウンド確認はプラグインごと・新しいリビジョンごとに 1 回だけ通知し、`mare update --plugins` の実行を案内します (`MARE_NO_UPDATE_CHECK=1` でバックグラウンド確認を無効化、`MARE_OFFLINE=1` は `mare plugin check`/`update` や起動時のリモート clone もスキップし、導入済みパッケージのみ使用します)。

## ロックファイル

`~/.config/mare/mare-lock.json` (信頼済みプロジェクトのプロジェクトスコープでは `.mare/mare-lock.json`) には、パッケージごとにその URL、コミット、ロック時の制約、追跡しているブランチ、チェックアウトの内容ハッシュが記録されます。dotfiles やリポジトリと一緒にコミットすると、どのマシンでも同じパッケージが得られます。mare は各パッケージの処理が終わるたびに書き込むため、失敗したパッケージがあっても他の記録は失われません。解析できないロックファイルがあると、チェックアウトを変更せずに通常のパッケージコマンドが停止します。まず `mare plugin restore --repair` (または `mare doctor --fix`) で変更のないインストール済みチェックアウトから再構築し、それでも解決しない場合はファイルを修正または削除して `mare plugin update` を実行してください。

`plugins.json` はバージョン 2 の `{ "version": 2, "plugins": [{ "source": "owner/repo", "name": "plugin" }] }` を使います。ユーザーまたはプロジェクトのいずれかが参照する共有チェックアウトは保持されます。

チェックアウトは `~/.local/share/mare/packages/<host>/<path>` にあります。

## コマンド

| コマンド | 動作 |
|---|---|
| `mare plugin sync` | 足りないパッケージを、ロックされたコミットでインストールします。`init.lua` の制約が変わった場合は、再解決します。 |
| `mare plugin update [name]` | パッケージ (または 1 つ) を、制約が許す最も新しいリビジョンに移し、新しいコミットを一覧表示して、ロックを書き換えます。厳密な固定はそのままです。 |
| `mare plugin check [name...] [--json]` | チェックアウトや lock を変更せず、リビジョンを取得して比較します。 |
| `mare plugin restore [name]` | 全ソースまたは指定したソースをロック済みコミットでチェックアウトします。ロックは変更しません。 |
| `mare plugin restore --repair` | 破損または欠落した lock を変更のないインストール済みチェックアウトから再構築します。 |
| `mare plugin list [--json]` | 宣言されたソースを状態とともに一覧表示します。 |
| `mare plugin tree [--json]` | 実行時のプラグインツリーを表示します。 |
| `mare plugin install <name...>` | 短い名前でカタログのプラグインを有効にし、必要なソースをインストールします。 |
| `mare plugin uninstall <name...>` | 短い名前でカタログのプラグインを無効にし、不要なソースを削除します。 |
| `mare plugin clean` | 宣言されなくなったチェックアウトと、古いロックのエントリを削除します。 |

```sh
mare plugin sync
mare plugin update someone/mare-extras
mare plugin list
```

取得は並列で実行され (`plugins.concurrency`、既定は 8)、各 git 操作にはタイムアウトがあり (`plugins.timeout`、既定は 120 秒)、Ctrl-C で取り消せます。ローカルに編集があるチェックアウトには印が付き、`clean` が削除することはありません。`mare plugin` コマンドは `--trust`、`--no-trust`、または保存済みの信頼判断に従って設定を読み込みます。信頼済みのプロジェクトでは `.mare/init.lua` を実行し、プロジェクトスコープでは `.mare/mare-lock.json` も使われます。ユーザー設定だけを対象にしたい場合は `--no-trust` を付けてください。

TUI 内では、`/plugins` が、すべてまたは 1 つのパッケージに対する同じ操作に加え、ログと差分のビュー、カタログ化されたパッケージのオンとオフを切り替える Discover タブ (`~/.config/mare/plugins.json` に保存) を備えたパネルを開きます。各行の状態も報告されます。インストール済み、ロックより遅れている、または進んでいる、ローカルの変更、欠落、無効、ローカルディレクトリ、または未読み込みとその理由です。`plugins` のオプションは [オプション](options.md) にあります。

初めて対話モードで開くと、mare は公式 プラグインカタログの取得と任意のプラグインの選択を提案します。最初から選択されている項目はありません。スキップして後から `/plugins` → Discover で選ぶこともできます。`mare.plugin("onboarding", { plugins = false })` で案内を無効にできます。`plugins.json` がない場合は、`state.json` の `onboarding.plugins` を削除すると再び案内されます。

```lua
mare.setup {
  plugins = { url_format = "https://git.example.com/%s.git", timeout = 60, concurrency = 4 },
}
```

## 起動と再読み込み

起動時、プラグインを読み込む前に、mare は足りない宣言済みのパッケージをロックされたコミットでインストールし、進行状況を TUI に表示します。その後、各パッケージの `init` フックを実行し、インストール済みの各パッケージのプラグインを依存関係の順に読み込み、最後に各パッケージの `config` フックを実行します。

必須パッケージとそのフック、コアまたは必須プラグインが失敗すると、修復して再読み込みするまでリクエストは拒否されます。任意のパッケージやプラグインの失敗は警告され、他のプラグインとセッションは動作を続けます。provider の失敗はそのモデルを使うリクエストに影響します。

パッケージの変更 (同期、アップグレード、`init.lua` の編集) は `/reload` で反映されます。新しいパッケージはインストールされて開始され、削除されたものはアンロードされ、`config` フックが再度実行されます。mare はディスク上で `init.lua` や `plugins.json` が変更されたことにも気づき、`/reload` を実行するよう知らせます。

`--clean` は、パッケージを完全にスキップします。

## パッケージの内容

| ディレクトリ | 読み込み元 |
|---|---|
| `plugins/` | プラグインローダー: `name.lua` または `name/init.lua` ファイル。[プラグイン](../extend/plugins.md) を参照。 |
| `skills/`、`prompts/` | コンテキスト: スキルとプロンプトテンプレート。 |
| `themes/` | テーマファイル。[外観](ui.md) を参照。 |
| `types/` | LuaLS の定義。エディター向けにミラーされます。 |

ルートの任意の `mare-plugin.toml` では、`requires = ">=0.4"` (より新しい mare を必要とするパッケージは読み込まれません)、`dependencies`、`name`、`description`、`version` を設定できます。自作のパッケージを公開するには、これらのディレクトリを持つリポジトリをプッシュし、`v1.0.0` のような semver タグでリリースにタグを付けます。レイアウトは [プラグインを書く](../extend/writing-a-plugin.md) に示されています。
