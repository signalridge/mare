# モデルとプロバイダー

プロバイダーへのサインイン、モデルと推論の強度の選択、モデルのメタデータの上書き、カスタムプロバイダーの追加の方法を扱います。

モデルは `provider/id` の形で指定します。例: `openai/gpt-6-luna`。プロバイダーは組み込みのプラグインで、それぞれが自分のモデルを登録します。サインイン後に、ライブのモデル一覧を取得するものもあります。

## プロバイダー

| プロバイダー | プラグイン | 認証 | 環境変数 |
| --- | --- | --- | --- |
| `openai` | `provider-openai` | ChatGPT サブスクリプション (ブラウザーまたはデバイスコード)、または API キー | `OPENAI_API_KEY` |
| `anthropic` | `provider-anthropic` | API キー | `ANTHROPIC_API_KEY` |
| `google` | `provider-google` | Gemini API キー | `GEMINI_API_KEY`、`GOOGLE_API_KEY` |
| `opencode-go` | `provider-opencode-go` | opencode Go API キー | `OPENCODE_API_KEY` |
| `deepseek` | `provider-openai-compat` | API キー | `DEEPSEEK_API_KEY` |
| `kimi` | `provider-openai-compat` | Moonshot API キー | `MOONSHOT_API_KEY` |
| `kimi-code` | `provider-openai-compat` | Kimi Code API キー | `KIMI_API_KEY` |
| `openrouter` | `provider-openai-compat` | API キー | `OPENROUTER_API_KEY` |
| `ollama` | `provider-openai-compat` | なし (`http://localhost:11434/v1`) | |
| `lmstudio` | `provider-openai-compat` | なし (`http://localhost:1234/v1`) | |

`provider-faux` は、テストやオフラインのデモ向けのスクリプト化されたプロバイダーです。実際の作業には向きません。

mare は次の順序でキーを探します。`init.lua` の `mare.provider` で設定した `api_key`、次に `mare login` が保存した認証情報 (`~/.config/mare/auth.json`、モード 0600)、最後にプロバイダーの環境変数です。

## サインイン

```sh
mare login                              # 設定済みモデルのプロバイダーを試行 (既定なしなら TTY でピッカー、非 TTY は終了コード 2)
mare login openai                       # ChatGPT サブスクリプション (openai) を明示指定
mare login openai --oauth               # ブラウザー OAuth を明示的に指定
mare login openai --device-auth         # SSH やブラウザーなし環境向けのデバイスコードログイン
printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key
printenv OPENAI_API_KEY | mare login openai --with-api-key
mare login status                       # 各プロバイダーの認証状態を表示
mare logout openai                      # ローカル認証情報を削除 (OAuth 失効は openai-codex のみベストエフォート)
```

- 引数なしの `mare login` は設定済みモデルのプロバイダーを優先し、既定がなければ TTY でピッカーを開き、非 TTY では終了コード `2` で終了します (ChatGPT を明示する場合は `mare login openai`)。
- `--with-api-key` は標準入力からのみキーを読み取り、stdin が端末の場合は拒否してシェル履歴への混入を防ぎます。
- セッション内の `/login [provider]` はプロバイダー名のみを `llm` に渡し、成功後にモデル一覧をバックグラウンド更新します。
- `--rpc` の対話ログインには接続済み UI が必要です。API キーを明示する場合は RPC メソッド `llm.login` の `key` パラメーターを使います。
- ディスプレイがない環境 (SSH、`DISPLAY` / `WAYLAND_DISPLAY` 未設定の Linux) では、ChatGPT ログインはデバイスコードを優先提示します。

### プロバイダーのログイン契約と `auth_keys`

`ctx.llm:register_provider` で登録するカスタムプロバイダープラグインは、次のフィールドを宣言することで `/login`、`mare login`、`mare login status`、`mare logout` に統合できます。

- `login = { methods = { "oauth", "device", "api_key" }, run = function(io) ... end }`: 対応するログイン方式を宣言します。mare は `run(io)` (`io` は `say`、`ask`、`choose`、`method`、任意の `key` を提供) を呼ぶ前に `--oauth` / `--device-auth` / `--with-api-key` の妥当性を検証します。`/login <provider>` のように `method` を省略した呼び出しでは `io.method` は `nil` のまま渡され、`method = "auto"` を明示した場合にのみ具体的な方式が解決されて渡されます。
- `auth_keys = { ... }`: そのプロバイダーが `auth.json` 内で所有するキー一覧 (既定は `{ provider }`)。たとえば `openai` は `auth_keys = { "openai-codex", "openai" }` を宣言しているため、`mare logout openai` は両方のローカルエントリを `auth.json` から削除します。ただしサーバー側のトークン失効は `openai-codex` の OAuth トークンに対してベストエフォートで行うだけで、別途貼り付けた OpenAI API キーはローカルから消えるのみで OpenAI 側では失効されません。

Lua プラグインは `ctx.llm:auth_keys(prefix?)`（`auth.json` に保存されている認証キー名のソート済み配列を返す。リテラル接頭辞での絞り込み可）または `ctx.llm:list_auth_keys(prefix?)`（`{ data = string[] }` を返す）でキー名を取得できます。RPC 側では `llm.list_auth_keys`（`{ prefix? } -> { data: string[] }`）、`llm.login`、`llm.logout` は primary かつ管理操作メソッド（owner / 管理者接続のみ呼び出し可能でピア接続には非公開。`llm.list_auth_keys` はキー名だけを返し、シークレット値は返さずキー取得コマンドも実行しません）であり、ピア接続に許可できるのは `llm.list_providers` です。

非対話での利用では、代わりに環境変数を設定します。ディスクには何も書き込まれません。

```sh
export ANTHROPIC_API_KEY=...
mare -p -m anthropic/claude-sonnet-5-5 "summarize the diff"
```

パスワードマネージャーからキーを読み取るには、`api_key` にコマンドを指定します。コマンドは必要なときに実行され、その出力は、ヘルパーが `expires_at` を報告するか、リクエストが 401 を返すまでキャッシュされます。

```lua
mare.provider("anthropic", { api_key = { cmd = { "pass", "show", "anthropic/api-key" } } })
```

## モデルを選ぶ

```sh
mare -m openai/gpt-6-luna               # this run
mare -m sonnet                          # an alias
```

セッション内では、`/model` でピッカーが開きます。認証情報のないプロバイダーは、ログインが必要なものとして末尾にまとめられます。`/model <provider/id>` や `/model <alias>` で直接切り替えられ、`/model <text>` はそのテキストで絞り込んだピッカーを開きます。ピッカーを開くと、10 分より古いモデル一覧がバックグラウンドで更新されます。`Alt+P` は入力中の下書きを失わずにピッカーを開き、`Alt+M` はお気に入りのモデルを順に切り替えます。お気に入りがなければ、最近使ったモデルを順に切り替えます。

既定値を設定するには、`init.lua` を使います。

```lua
mare.setup {
  model = "openai/gpt-6-luna",
  favorite_models = { "openai/gpt-6-luna", "anthropic/claude-sonnet-5-5" },
  fallback_models = { "google/gemini-2.5-pro" },   -- tried when the current model is overloaded or out of quota
}
```

`MARE_MODEL` でも設定できます。`init.lua` (またはフラグや環境変数) に明示した `model` は、`/model` で最後に選んで mare が `state.json` に記憶しているモデルより優先されます (後述)。組み込みの別名 `sonnet`、`opus`、`haiku` は、そのファミリーの最新の Anthropic モデルを選びます。独自の別名は `mare.model` で定義します。

```lua
mare.model { provider = "openai", id = "gpt-6-luna", alias = "main" }
```

これ以降、`-m main`、`/model main`、`fallback_models` で `main` を使えます。

## 推論の強度

effort は、モデルが回答前にどれだけ推論するかを制御します。レベルは `off`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max` で、モデルごとに提供されるのはその一部です。

```sh
mare -e high "refactor this module"
```

セッション内では、`/effort` が現在のモデルの対応レベルを一覧表示します。`/effort high` はこのモデルにレベルを設定して記憶し、`/effort high --session` は現在のセッションだけを変更します。`Alt+,` でレベルを下げ、`Alt+.` で上げます。モデルが対応していないレベルを指定すると、mare は最も近い対応レベルを使い、その旨を知らせます。黙って丸めることはありません。`/show-reasoning` は、1 行の要約ではなく思考テキスト全文を表示するかどうかを切り替えます。

effort パラメーターのないモデルでは、その旨が報告され、そのモデルが持つパラメーターが一覧表示されます。

## モデル一覧を更新する

モデルを一覧表示できるプロバイダーは、一覧を `~/.local/state/mare/models-cache/<provider>.json` にキャッシュし、1 日より古い場合は起動後にバックグラウンドで更新します。価格、入力タイプ、上限などのメタデータは models.dev から統合されます。今すぐ更新するには次を実行します。

```sh
mare model refresh
```

認証情報のあるすべてのプロバイダーのモデル一覧と、models.dev のメタデータを取得します。models.dev を使わないようにするには、`llm` プラグインのオプション `models_dev = false` を設定します。

```lua
mare.plugin("llm", { models_dev = false, catalog_refresh_ms = 7 * 24 * 3600 * 1000 })
```

## モデルごとの上書き

`mare.setup` の `models` は、モデルのフィールドを上書きするか、プロバイダーが一覧に含めないモデルを定義します。キーは `provider/id` の参照です。

```lua
mare.setup {
  models = {
    ["openai/gpt-6.1-terra"] = { context_window = 921000, max_output = 64000 },
    ["openrouter/qwen/qwen3-coder"] = { name = "Qwen3 Coder", context_window = 262144, reasoning = true },
  },
}
```

よく使うフィールドは、`name`、`context_window`、`max_output`、`reasoning`、`efforts`、`input` (`{ "text", "image" }` のようなリスト)、`cost` (`input`、`output`、`cache_read`。100 万トークンあたり) です。それ以外で情報のないモデルには、設定するまで 128,000 トークンのコンテキストウィンドウと 16,384 の最大出力が割り当てられます。コンテキストウィンドウは重要です。[自動コンパクション](sessions.md) はこれを基準に発動します。

## カスタムプロバイダーを追加する

mare の組み込みワイヤープロトコルのいずれかに対応するサーバーは、`mare.provider` で追加できます。組み込みでない名前と `base_url` を指定すると、新しいプロバイダーが定義されます。同じサービスの 2 つのアカウントは、2 つの名前になります。

プロトコルは `wire_api` で指定します (既定は `"chat"`):

| `wire_api` | ワイヤー名 (`wire`) | リクエストエンドポイント | ネイティブ認証ヘッダー |
| --- | --- | --- | --- |
| `"chat"` (既定) | `openai-chat` | `<base_url>/chat/completions` | `Authorization: Bearer <key>` |
| `"responses"` | `openai-responses` | `<base_url>/responses` | `Authorization: Bearer <key>` |
| `"anthropic"` | `anthropic-messages` | `<base_url>/v1/messages` (`base_url` がすでに `/v1` で終わる場合は `<base_url>/messages`) | `x-api-key: <key>` + `anthropic-version` |
| `"google"` | `google-generate` | `<base_url>/models/<model>:streamGenerateContent?alt=sse` | `x-goog-api-key: <key>` |

例 — OpenAI Chat Completions エンドポイント (既定の `wire_api = "chat"`):

```lua
mare.provider("lab", {
  base_url = "http://10.0.0.5:8080/v1",
  api_key = "LAB_API_KEY",            -- 環境変数名、{ cmd = {...} } ヘルパー、またはリテラル
  default_model = "qwen3",
  default_context = 131072,
  models = {
    { model_id = "qwen3", context_window = 131072, max_output = 8192, reasoning = true, efforts = { "low", "medium", "high" } },
  },
})
```

例 — Anthropic Messages エンドポイント (`wire_api = "anthropic"`):

```lua
mare.provider("claude-proxy", {
  wire_api = "anthropic",
  base_url = "https://gateway.example.com/anthropic",
  api_key = "CLAUDE_PROXY_API_KEY",
  headers = { ["x-team"] = "infra" },
  default_model = "claude-sonnet-5-5",
  models = {
    { model_id = "claude-sonnet-5-5", context_window = 200000, max_output = 32000, reasoning = true, adaptive = true },
  },
})
```

```sh
mare -m lab/qwen3
mare -m claude-proxy/claude-sonnet-5-5
```

4 つのワイヤー共通の主なフィールド: 認証のないサーバー向けの `no_key = true`、モデル一覧取得 (`GET /models` や `GET /v1/models`) を無効化する `list_models = false`、`api_key`、`api_key_env`、`default_model`、`default_context`、`default_max_output`、`default_reasoning`、`key_prefixes`、`models_dev` (models.dev のプロバイダー ID、または補完を無効化する `false`)、`headers`、`query_params`、`retries`、`stream_idle_timeout_ms`、`connect_timeout_ms`、`drop_fields`、`transport`、`proxy`。`anthropic` (`anthropic-messages`) エンドポイントでは、プロバイダー単位の `auth` (`"x-api-key"` または `"bearer"`)、`replay_unsigned_reasoning`、`fine_grained_tool_streaming`、`reasoning_budgets`、`reasoning_display`、`supports_long_cache_retention`、および明示的に有効化する `tool_update = "addition_only"` (カスタムエンドポイントでは既定で無効) も利用できます。`chat` (`openai-chat`) エンドポイントではさらに、`dialect` (`"deepseek"`、`"openrouter"`、`"kimi"`、`"cerebras"`、`"zai"`、`"together"`、`"qwen"` など)、`effort_param` (`"reasoning_effort"` または `"openrouter"`)、`reasoning_format`、`cache_control_format`、`needs_reasoning_placeholder` によりプロバイダー名に依存せずプロトコル固有の挙動を指定できます。より具体的なフィールドが `dialect` に優先し、その次にホストやモデル ID からの自動判定が適用されます。

### オーバーレイと地域エンドポイント

既存の組み込み名に対して `mare.provider` を呼ぶと、プリセットの上にフィールド単位で重ね書きされます (`models` を指定するとシード一覧全体を置き換えます。データエントリで `wire` や `wire_api` を変更すると指定したワイヤーへ移動します。コード実装プロバイダーへの `wire`/`wire_api` は無視され `mare doctor` に警告が出ます)。組み込みプリセットは既定で国際エンドポイントを指します。中国本土エンドポイントや社内ゲートウェイへ向ける場合は、方言や環境変数を保ったまま `base_url` だけを上書きします。

```lua
mare.provider("kimi", { base_url = "https://api.moonshot.cn/v1" })
mare.provider("anthropic", { base_url = "https://gateway.example.com", headers = { ["x-team"] = "infra" } })
```

### カスタムエンドポイントとプロバイダープラグイン

既存のワイヤーと互換性のあるサービスは `mare.provider` で設定できます。独自の認証、署名、またはリクエスト形式が必要な場合は、プロバイダープラグインを作成してください。

## `state.json` に保存される実行時の選択

セッション内でモデルや effort を選ぶと、mare は `init.lua` を編集せずに `~/.local/state/mare/state.json` に保存します。このファイルには、`model`、`recent_models`、`model_params` (モデルごとに記憶された effort) のほか、ステータスラインのセグメントなどの選択が入ります。次回の実行はそれらから始まります。ただし、`init.lua`、環境変数、フラグが同じ項目を設定している場合を除きます。無視するには、キーを削除するか、`--clean` で実行します。`/config` は各オプションの値と、その由来を表示します。
