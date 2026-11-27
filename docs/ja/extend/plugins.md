# プラグイン

mare のプラグインとは何かを説明します。プラグインの読み込み元、プラグイン定義のフィールド、ライフサイクル、組み込みプラグインのオンとオフ、失敗がどう封じ込められるかを扱います。

## プラグインとは

プラグインは、テーブルを 1 つ返す Lua ファイルです。テーブルがプラグインを記述し、その `apply` 関数が実際の処理を行います。

```lua
-- ~/.config/mare/plugins/hello.lua
return {
  name = "hello",
  inject = { "commands" },
  config_schema = { greeting = { type = "string", default = "hello", desc = "what to say" } },
  apply = function(ctx, cfg)
    ctx.commands:register("hello", {
      description = "Say hello",
      run = function() ui.notify(cfg.greeting) end,
    })
  end,
}
```

ファイルの最上位では、ホワイトリスト内の純粋な標準ライブラリ (`string`、`table`、`math`、`pairs` など) を使ったローカル定数や補助計算を記述でき、最終的に定義テーブルを返します。ホワイトリスト外のグローバル (`mare` や `require` など) の読み取りやグローバル変数への書き込みはエラーとなり、該当する行が示されます。実行時の処理はすべて `apply` に書いてください。[プラグインを書く](writing-a-plugin.md) では、完全なプラグインを順を追って作ります。

## プラグインの読み込み元

mare は、次の順序でプラグインのソースを集めます。後のソースほど優先順位が高くなります。

| 順序 | 由来 | 場所 |
|---|---|---|
| 1 | `builtin` | リリースビルドでは mare にコンパイルされて組み込まれます (デバッグビルドはディスクの `crates/mare-plugin/preset/plugins/` から読み込みます)。読み取り専用のコピーがランタイムディレクトリにあります (`mare runtime`)。 |
| 2 | `package` | インストール済みの [パッケージ](../configure/packages.md) それぞれの `plugins/`。`only` リストが適用されます。 |
| 3 | `user` | `~/.config/mare/plugins/` |
| 4 | `project` | `<project>/.mare/plugins/`。信頼済みのプロジェクトのみ。 |

ディレクトリ内では、`name.lua` と `name/init.lua` がプラグインです。名前が `_` または `.` で始まるエントリは無視されます。

2 つのプラグインが黙って同じ名前を持つことはできません。より優先度の高い由来層にある定義は、`override = true` を指定すれば下位層の同名プラグインを置き換えられます (未指定の場合は拒否され、`override = true` の追加か改名を案内します)。一方、**同じ由来層**に同名定義が 2 つある場合は両方とも拒否され、下位層の元の定義が残ります。同一層での重複は `override = true` を付けても解消できません。前のものをオフにしても回避できません (`enabled = false` は名前全体をオフにします)。`/plugins` は置き換えられたプラグインを `overrides built-in <name>` と表示します。プラグイン定義全体は `<name>.lua` または `<name>/init.lua` (プライベートモジュールが必要なら `lua/` も同梱) に置いて `override = true` を設定できます。単一のモジュールファイルだけを置いてもプラグイン全体は置き換わりません (一時的な差し替えには `package.preload` を使用します)。

`--clean` は組み込みプラグインだけを読み込みます。

## モジュールと `require`

組み込み・サードパーティを問わず、すべてのプラグインが所有者ごとにキャッシュを分離した同一のオープン `require` ローダーを使います。

1. **解決順**: `package.loaded[name]`（グローバル上書きとコアモジュール）→ `package.preload[name]` → 呼び出し元プラグイン自身の `lua/` → コアモジュール（`cordis*`、`mare_kernel*`、`mare_lib*`、`mare.test`）→ 他プラグイン（モジュールルート名で一意に解決）。
2. **コードはオープン、状態はサービス経由**: プラグイン間 `require` はステートレスなヘルパー共有に使い、実行時の状態や機能は Cordis のサービスとイベント（`inject`/`provide`/`ctx:get`/`ctx.bus`）を通します。プラグイン間 `require` は定義ファイルのトップレベルではなく `apply` 内で呼び出してください（全プラグインの `lua/` ルート索引とホットリロード用の依存追跡が確実に機能します）。モジュールは**提供元**の環境で実行されるため、クロスプラグイン `require` されたモジュールが書いたグローバル変数は呼び出し元ではなく提供元の環境に留まります。
3. **主な挙動**:
   - 同名モジュールの分離: 別プラグインの `lua/util.lua` はそれぞれの所有者パーティションにキャッシュされ、衝突しません。
   - 曖昧なルートのエラー: 複数プラグインが同じモジュールルートを提供している場合、他プラグインからの `require` は全提供元を列挙してエラーになります（自身の `lua/` は常に優先されます）。
   - `mare_lib*` は `core` 所有: VM 全体で単一インスタンスを共有し、いずれかのプラグインの再読み込みで破棄されません。
   - 依存側の連動再読み込み: 提供元を再読み込みすると提供元のパーティションだけがクリアされ、提供元の再構築後にそのモジュールを `require` していたプラグインも自動で再読み込みされます。
   - 上書きはプラグイン全体の置換 (`override = true`) または `package.preload` のみサポートされ、公開モジュールは `types/` の `---@meta <name>` で宣言されます。

## 定義テーブル

| フィールド | 型 | 意味 |
|---|---|---|
| `name` | 文字列 | プラグイン名。既定はファイル名。 |
| `inject` | 文字列のリスト | このプラグインが必要とするサービス。すべてに提供元ができるまで、起動せずに待ちます。 |
| `provide` | 文字列のリスト | 提供するサービス (`mare.Service`。[サービスとイベント](services-and-events.md) を参照)。有効な 2 つのプラグインが同じものを宣言することはできません。読み込み順で後のものが拒否され、エラーは両方のファイルを示します。 |
| `contracts` | テーブル | 新たに定義するサービスのメソッド一覧。[サービスとイベント](services-and-events.md) を参照。 |
| `config_schema` | テーブル | プラグインのオプションを 1 項目ずつ宣言します: `{ type, default, desc, choices?, min?, max?, fields?, items?, values?, variants?, on_invalid?, apply? }`。`apply` が `cfg` として受け取る既定値と、すべての入口 (init.lua、`--set`、`MARE_*`、実行時の `mare.plugin` 書き込み) で行うチェックはここから決まります。無効な値は報告され、既定値が残ります。`type = "union", variants = {…}` は、いずれか 1 つの形にちょうど一致する値を受け付けます。`on_invalid = "fail"` は、既定値に戻す代わりにプラグインを起動させません (承認ポリシーのような制限的な設定向け)。実行時の書き込みは、まず `option/apply` で所有プラグインに渡され (そこでのエラー、たとえば保存の失敗で書き込みは失敗し、何も変わりません)、反映後に `option/changed` で通知されます。ユーザーは `mare.plugin(name, {...})` でオプションを設定します。 |
| `config_api` | テーブル | `init.lua` で `mare.<name>` になる関数。後述。 |
| `apply` | 関数 | `apply(ctx, cfg)`。起動時と再読み込みのたびに実行されます。yield できます。 |
| `required` | 真偽値 | 起動に失敗すると、完全な `/reload` が成功するまで、すべてのモデルリクエストが拒否されます。 |
| `override` | 真偽値 | 前のソースにある同名のプラグインを置き換えられます。 |
| `tags` | 文字列のリスト | プラグインの種類。実行せずに読み取られます。`"provider"` はモデルプロバイダーを示します。 |
| `wires` | 文字列のリスト | `llm` サービスに登録するワイヤーフォーマット。 |
| `providers` | 文字列のリスト | `llm` サービスに登録する LLM プロバイダー名 (無効時でも状態表示と帰属判定のために読み取られます)。 |

`cfg` は、`config_schema` の既定値と、ユーザーが `plugins.<name>` に設定した内容をマージしたコピーで、テーブルは再帰的にマージされます。`mare.plugin("hello", { greeting = "hi" })` は `cfg.greeting` を変更します。未知のキーや誤った型には「もしかして」のヒント付きの警告が出るため、すべてのオプションを `config_schema` で宣言してください。

### 設定関数

`config_api` を使うと、プラグインが設定言語に関数を追加できます。キーはドット区切りのパスで、関数は最初の引数として設定テーブルを受け取ります。

```lua
config_api = {
  ["greeting.set"] = function(C, text) C.greeting = text end,
},
```

プラグインがインストールされた後は、`init.lua` から `mare.greeting.set("hi")` を呼び出せ、`apply` は `mare.config.greeting` を読み取れます。組み込みの `tools` プラグインは、`mare.tool.command` にこれを使っています。

### プロバイダーパック

原則は「ベンダーはデータ、プロトコルはコード、名前は識別子にすぎない」です。サードパーティプラグインは、`stream` や `endpoint` コールバックを書かずに OpenAI 互換 (または Responses 互換) のベンダーを純粋なデータエントリとして登録できます。同じトップレベルテーブルから `providers` を導出すれば宣言のずれも防げます。

```lua
local PRESETS = {
  acme = {
    label = "Acme AI",
    wire = "openai-chat",
    dialect = "deepseek",
    base_url = "https://api.acme.example/v1",
    api_key_env = "ACME_API_KEY",
    key_prefixes = { "acme-" },
    models_dev = false,
    default_model = "acme-v1",
    models = {
      { model_id = "acme-v1", context_window = 128000, max_output = 8192 },
    },
  },
}

local NAMES = {}
for name in pairs(PRESETS) do NAMES[#NAMES + 1] = name end
table.sort(NAMES)

return {
  name = "vendor-acme",
  inject = { "llm" },
  tags = { "provider" },
  providers = NAMES,
  apply = function(ctx)
    for _, name in ipairs(NAMES) do
      local entry = table.copy(PRESETS[name])
      entry.provider = name
      ctx.llm:register_provider(entry)
    end
  end,
}
```

ユーザーは `mare.provider("acme", { ... })` で任意のフィールド (`base_url`、`default_model`、`models`、`wire_api` など) をオーバーレイでき、`vendor-acme` を無効化またはアンロードするとプロバイダーとパラメーター規則がまとめて撤回されます。

## ライフサイクル

![mare プラグインのライフサイクルと override / reload フロー](../../assets/diagrams/plugin-lifecycle.svg)

1. mare はすべての定義を読み取ります。読み取れないファイルは報告され、スキップされます。
2. プラグインは依存関係の順に起動します。プラグインは `inject` のすべてのサービスに提供元ができるまで待つため、提供元が先に起動し、その後に残りがソース順で起動します。提供元がない場合は依存するプラグインが待機し、依存関係が循環するプラグインは `not loaded: dependency cycle` として拒否されます。
3. mare はプラグインごとに `cfg` を組み立て、`apply(ctx, cfg)` を呼び出します。
4. `ctx` を通じて登録されたものはすべて、そのプラグインに属します。リスナー、サービス、タイマー、タスク、ツール、コマンド、キーバインド、ステータスのセグメント、そしてすべての `ctx:effect` です。
5. 再読み込み時や終了時に、mare はプラグインを破棄してこれらすべてを取り下げ、再読み込みの場合は再び `apply` します。そのプラグインが提供するサービスを inject しているプラグインも、一緒に再起動します。

このライフサイクルについて mare が約束することは [ランタイム保証](runtime-guarantees.md) にあり、どれもテストで固定されています。

`ctx` には次のメソッドとフィールドがあります。

| メンバー | 動作 |
|---|---|
| `ctx.<service>` | `inject` に挙げたサービス。例: `ctx.tools`。 |
| `ctx:get(name)` | 名前でサービスを取得します。提供元がない間は `nil`。なくても動けるサービス向けです。 |
| `ctx:inject({ name, ... }, fn)` | 挙げたサービスすべてに提供元がある間、`fn(ctx)` を実行します。どれかがなくなると `fn` の登録したものは取り下げられ、戻ると `fn` が再び実行されます。 |
| `ctx:on(event, fn, opts)`、`ctx:once(event, fn)` | イベントをリッスンします。`opts.prepend = true` でリスナーを先頭に置きます。取り除く関数を返します。 |
| `ctx.bus:emit`、`:emit_checked`、`:bail`、`:waterfall`、`:waterfall_checked` | イベントを発行します (`inject = { "bus" }`)。[サービスとイベント](services-and-events.md) を参照。 |
| `ctx.policy:voter`、`:rewriter`、`:decide` | 決定点への投票、引数の書き換え、決定の問い合わせ (`inject = { "policy" }`)。 |
| `ctx:plugin(Class)` | サービスを提供します。`Class` は `mare.Service` のサブクラスです。 |
| `ctx:effect(fn)` | `fn` をすぐに実行し、返された関数をプラグインがなくなるときに実行します。 |
| `ctx:spawn(fn)` | バックグラウンドタスクを実行します。破棄時に取り消されます (`inject = { "task" }`)。 |
| `ctx:after(ms, fn)`、`ctx:every(ms, fn)` | タイマー。破棄時に取り消されます (`inject = { "task" }`)。 |
| `ctx.logger:warn(...)`、`ctx.logger:error(...)` | プラグイン名でログに書き込みます。error はそのプラグインの問題としても記録され、`kernel/error` が発行されます。 |
| `ctx.kernel:reload(name)`、`:plugins()`、`:problems()` | 1 つのプラグイン、またはすべてを再読み込みします。プラグインの一覧と、記録された失敗 (`inject = { "kernel" }`)。 |

適用済みのプラグインは、`mare.plugins.status(name)` と `mare.plugins.list()` でも確認できます。`mare.plugins.failures()` は、起動時または直近の完全な再読み込みで失敗したものを返します。

## 組み込みプラグインのオンとオフ

1 つのプラグイン (由来を問わない):

```lua
mare.plugin("update-check", { enabled = false })
```

名前はすべてのソースにまたがる 1 つのスロットです。`enabled = false` は、その名前の組み込みプラグインと、パッケージ・ユーザー・プロジェクトの同名プラグインをすべてオフにします。各レイヤーの値はリーフごとに比較されるため、信頼済みプロジェクトの `false` はあなたの `true` より優先され、`--set plugins.<名前>.enabled=…` はその両方より優先されます。

最小プラグインセットはプリセットです:

```lua
mare.setup { plugins = { preset = "minimal" } }
mare.plugin("jobs", { enabled = true }) -- 必要なものを戻す
```

`minimal` は、`minimal` タグも `provider` タグも持たない組み込みプラグインをすべてオフにします。`minimal` タグ付きのものは `agent`、`agent-approvals`、`approval-review-model`、`approvals`、`baseline`、`commands`、`context`、`conversations`、`fs`、`keymap-default`、`llm`、`permissions`、`session`、`statusline`、`theme-default`、`tools`、`ui`、`webserver` で、さらに `provider` タグ付きの組み込みプロバイダー (`provider-anthropic`、`provider-google`、`provider-openai`、`provider-openai-compat`、`provider-opencode-go`、およびデバッグビルドまたは `faux` 機能有効時の `provider-faux`) も残ります。`override = true` で組み込みを置き換えたプラグインや、パッケージ・ユーザー・プロジェクトのプラグインは影響を受けません。どのレイヤーでも明示した `enabled` はプリセットより優先されます。未知のプリセット名は設定エラーで、報告されたうえでプリセットなしとして扱われます (`--strict-config` では起動を拒否)。`preset` は予約語のため、この名前のプラグインは作れません。

組み込みプラグインを分野別に示します。

| 分野 | プラグイン |
|---|---|
| コア | `agent`、`baseline`、`cells`、`compact`、`context`、`conversations`、`fs`、`jobs`、`llm`、`mcp`、`message`、`proc`、`query`、`query-disk`、`session`、`spill`、`subagent`、`terminal`、`tools`、`tools-apply-patch`、`web`、`web-fetch-http`、`webserver` |
| プロバイダー | `provider-anthropic`、`provider-google`、`provider-openai`、`provider-openai-compat`、`provider-opencode-go`、`provider-faux` (デバッグビルド、または `faux` フィーチャーを有効にしたリリースビルドのみ) |
| 安全性 | `permissions`、`approvals`、`approval-review-model`、`agent-approvals`、`repeat-guard` |
| インターフェース | `ui`、`statusline`、`theme-default`、`keymap-default`、`commands`、`copy`、`images`、`file-ref`、`shell-mode`、`agents`、`review`、`changes`、`notify`、`usage`、`status` |
| 管理 | `plugins`、`profile`、`lua-console`、`update-check`、`onboarding`、`user-config` |

## 確認と再読み込み

`/plugins` は、すべてのプラグインを一覧表示します。有効なものには塗りつぶしたドットが付き、由来と上書きが示され、その下に記録されたすべての問題がトレースバック付きで表示されます。`mare doctor` には、二重に定義されたプラグイン名も含まれます。

| コマンド | 動作 |
|---|---|
| `/reload` | 完全なホスト再読み込み: `init.lua` を再実行し、新しいパッケージをインストールし、新しいプラグインを開始し、削除されたものをアンロードし、残りを再読み込みして `reload/completed` を発行します。 |
| `/reload config` | 設定の再読み込み: `init.lua` を再実行し、オプション差分を比較して有効・無効状態を同期し、保留中の項目を再試行します。 |
| `/reload <name>` | 名前指定の再読み込み: 対象プラグインの定義をその場で置き換えます (新しい定義の読み込みに失敗した場合は古い定義を維持)。 |

モデルへのリクエストは、組み立てからストリーミングの終了まで、リースを保持するため、その最中に再読み込みでプラグインを置き換えることはできません。リクエストがアクティブな間は拒否されるので、その後でもう一度実行してください。新しい定義の読み込みに失敗したプラグインは、古い定義のまま動き続けます。

デバッグビルドでは、組み込みプラグインのソースは起動のたびに `crates/mare-plugin/preset/plugins/` から読み込まれるため、編集して `/reload` を実行するだけで済みます。リリースビルドでは、`MARE_DEV_PLUGINS=<dir>` を設定すると、`<dir>/<name>.lua` が存在する場合に、mare は各組み込みプラグインをそのファイルから読み込みます。

## プラグインが失敗したとき

失敗は、それを起こしたプラグインの中に留まります。

| 失敗の内容 | 何が起こるか |
|---|---|
| 定義を読み込めない (構文エラー、最上位での禁止されたアクセス) | プラグインはスキップされます。mare は `requests continue without it` と警告し、TUI のステータスラインに `plugin <name> failed · /reload` と表示されます。 |
| `apply` がエラーを発生させる | プラグインは起動しません。エラーは記録され、他のプラグインは通常どおり起動します。 |
| `inject` のサービスに提供元がない | プラグインは待ち、提供元が現れると起動します。`/reload` は何を待っているかを報告します。 |
| リスナー、タスク、タイマーがエラーを発生させる | エラーは通知として、また `kernel/error` イベントとして報告されます。プラグインは動き続けます。 |
| 投票者がエラーを発生させる、または時間切れになる | `deny` として扱われます。 |
| Lua が長時間 yield せずに実行される | ウォッチドッグがコールバックを中止します。 |

プラグインが基幹サービス (`agent`、`llm`、`tools`、`permissions`、`approvals`、`session`) のいずれかを提供している場合、`required = true` を宣言している場合、または必須パッケージ (`required = true`) 由来の場合、その失敗はすべてのモデルリクエストをブロックし、理由が表示されます。完全な `/reload` が成功すると、ブロックは解除されます。任意のプラグインや任意パッケージの失敗は警告にとどまり、リクエストは続行できます。

ウォッチドッグの上限は `watchdog_ms` で、既定は 10000 です。`init.lua` の実行前に読み取られるため、コマンドラインか環境変数で設定してください: `mare --set watchdog_ms=20000` または `MARE_WATCHDOG_MS=20000`。暴走したコードを `pcall` で囲んでも、中止は握りつぶされません。
