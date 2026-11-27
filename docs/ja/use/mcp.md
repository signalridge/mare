# MCP サーバー

Model Context Protocol サーバーを mare に接続します。設定する場所、ツールがモデルに届く仕組み、プロジェクトのサーバーを承認する方法、OAuth ログインを扱います。

`mcp` プラグインは組み込みで、既定で有効です。サーバーを設定するまでは何もしません。stdio サーバー (mare が起動するコマンド) と、ストリーミング可能な HTTP サーバー (URL) に対応しています。従来の SSE トランスポートには対応していません。サーバーのストリーミング可能な HTTP エンドポイントを使ってください。多くの場合、`/sse` ではなく `/mcp` です。

## サーバーを設定する

サーバーは 5 か所から設定できます。同じ名前のサーバーについては、後のソースが前のソースを上書きします。

| 順序 | ソース | 備考 |
| --- | --- | --- |
| 1 | `init.lua` 内の `mare.plugin("mcp", { ... })` の `servers` | |
| 2 | `~/.config/mare/mcp.json` | ユーザーのファイル。 |
| 3 | `$MARE_MCP_CONFIG` で指定されたファイル | 承認が必要 (後述)。 |
| 4 | プロジェクトリソースルートの `.mcp.json` | プロジェクトのファイル (git ルートまで遡って最寄りの `.mare/` または `.mcp.json` を持つディレクトリ。作業ディレクトリの親にある場合も含む)。承認が必要。 |
| 5 | プロジェクトリソースルートの `.mare/mcp.json` | 同じプロジェクトリソースルートのプロジェクトファイル。承認が必要。 |

JSON ファイルは一般的なレイアウト `{ "mcpServers": { ... } }` を使います (`mcp_servers` と `servers` も読み取られます)。他のクライアントが使うキャメルケースの表記 (`enabledTools`、`bearerTokenEnvVar`、`startupTimeoutMs` など) も受け付けます。

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" }
    },
    "docs": {
      "url": "https://example.com/mcp",
      "headers": { "x-team": "platform" }
    }
  }
}
```

`init.lua` では次のように書きます。

```lua
mare.plugin("mcp", {
  servers = {
    github = {
      command = "npx", args = { "-y", "@modelcontextprotocol/server-github" },
      env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" },
      tier = "direct",
    },
    linear = {
      url = "https://mcp.linear.app/mcp", oauth = true,
      tools = { delete_issue = "hidden" },
    },
  },
})
```

サーバー名には、英字、数字、`_`、`-` を使えます。文字列内では `${VAR}`、`${env:VAR}`、`${VAR:-default}` が展開されます。設定されていない変数は、`/mcp` と `mare doctor` が報告します。

`command`、`url`、`type` のいずれもないプロジェクトのエントリは、同じ名前のユーザーのサーバーに対する上書きとして扱われます。変更できるのは `enabled`、`tier`、`tools`、`enabled_tools`、`disabled_tools`、`description`、`required` だけです。

### サーバーのキー

| キー | 意味 |
| --- | --- |
| `command`、`args`、`env`、`env_file`、`cwd`、`env_include`、`env_exclude` | stdio サーバー。`env_file` は dotenv ファイルで、`env` がそれを上書きします。 |
| `url`、`headers`、`env_http_headers`、`bearer_token`、`bearer_token_env_var` | ストリーミング可能な HTTP サーバー。`env_http_headers` はヘッダー名を環境変数に対応付けます。 |
| `oauth` | `true`、または `scopes`、`client_id`、`client_secret`、`client_name`、`callback_port`、`auth_server_metadata_url` を持つテーブル。 |
| `tls` | プライベート認証局向けの `{ ca_bundle = "..." }`。 |
| `enabled` | `false` でサーバーを無効にします。 |
| `required` | `true`: リクエストはこのサーバーが接続するまで時間制限なしで待ちます。失敗した場合は、理由を添えてリクエストが拒否されます。 |
| `tier` | 公開レベル。後述。 |
| `tools` | ツールごとの階層。例: `{ delete_issue = "hidden", ["search_*"] = "direct" }`。 |
| `enabled_tools`、`disabled_tools` | 許可リストと拒否リスト。`*` パターンを使えます。 |
| `description` | モデルに表示され、サーバーのツールを見つける手がかりになります。 |
| `startup_timeout_ms`、`call_timeout_ms` | 既定値は 60,000 (最大 300,000) と 120,000。進捗通知で呼び出しのタイムアウトがリセットされます。 |

MCP プラグインのオプションは、`tier` (既定は `deferred`)、`startup_timeout_ms`、`call_timeout_ms`、`text_limit` です。`tool_search` の既定の結果数 (5) は `plugins.tools.search_results` で変更できます。`text_limit` (20 KiB) より長い結果は途中で切り詰められ、全文は結果に示された非公開ファイルに保存されます。

## プロジェクトのサーバーを承認する

`$MARE_MCP_CONFIG`、`.mcp.json`、`.mare/mcp.json` のサーバーは、承認するまで起動しません。リポジトリを開いただけで、そのコマンドが実行されてはならないためです。承認は、サーバー名とハッシュの組で `~/.local/state/mare/mcp-approvals.json` に記憶されます。ハッシュの対象には、実行・認証・トランスポート・公開設定の各フィールド (`command`、`args`、`env`、`env_file`、`env_include`、`env_exclude`、`cwd`、`url`、`headers`、`env_http_headers`、`bearer_token`、`bearer_token_env_var`、`oauth`、`tls`、`type`、`tier`、`tools`、`enabled_tools`、`disabled_tools`、`enabled`、`required`、`description`、および部分的なプロジェクトオーバーレイ) が含まれます。これらを変更するとハッシュが変わり、再度承認を求められます。プロジェクトの stdio サーバーには、プロジェクトが [信頼](permissions.md) されていることも必要です。

- 対話型: `/mcp enable <server>`、または `/mcp` でサーバーを選んで Enable を選びます。
- ヘッドレス: `--approve-mcps` を渡すとすべてを承認します。同じ記憶された承認が `mcp-approvals.json` に書き込まれるため、以降の実行 (ヘッドレスでも対話型でも) では、ハッシュが変わるまで再確認なしでそれらのサーバーが起動します。

```sh
mare -p --trust --approve-mcps "list my open issues"
```

これがない場合、未承認のサーバーは `needs_approval` と表示され、そのツールは使えません。

## ツールがモデルに届く仕組み

各ツールは `mcp__<server>__<tool>` として登録されます。名前が 64 文字を超える場合や他のツールと衝突する場合は、短いハッシュが付加されます。モデルが最初から見るツールの数はサーバーの階層 (tier) で決まり、サーバーが多くてもプロンプトを小さく保てます。

| 階層 | 動作 |
| --- | --- |
| `deferred` (既定) | 起動時にはサーバーに接続しません。モデルは `tool_search` でツールとそのスキーマを検索します。キャッシュがないサーバーには最初の検索時に接続してツールを取得します。`~/.local/state/mare/mcp-tools` の一覧があれば、初回使用まで接続しません。`tool_update = "addition_only"` に対応するモデル (Anthropic Claude、OpenAI Responses の `gpt-5.4` 以降、Kimi K3) は会話途中で遅延ツールを有効化して直接呼び出します。その他のモデルは固定の `call_tool` プロキシ経由で呼び出すため、トップレベルの `tools` 配列とプロンプトのプレフィックスキャッシュがターンをまたいで安定します。 |
| `direct` | 起動時にバックグラウンドで接続し、ツールは組み込みのツールと同様に宣言されます。リクエストがこの接続を待つことはありません (`required` の場合を除く)。まだ接続中のツールは次のターンから加わります。 |
| `hidden` | 利用できません。 |
| `codemode` | コードモードのスクリプトからのみ呼び出せます。codemode プラグインが必要で、ない場合は `deferred` として扱われます。 |

`tool_search` はキーワード (`github issues`) か完全な名前 (`select:mcp__github__create_issue`、1 回のクエリで最大 32 件) を受け取り、固定のトークン予算内で一致するツールのスキーマを返します。システムプロンプトには、サーバーごとに 1 行が入ります。

> **全プロバイダーでのプロンプトキャッシュ:** 遅延ツールの探索では、どのプロバイダーでもシステムプロンプトとトップレベルの `tools` 配列がターンをまたいで固定されます。`tool_update = "addition_only"` に対応するモデル (Anthropic Claude、OpenAI Responses `gpt-5.4` 以降、Kimi K3) はネイティブにツールを追加し、その他のエンドポイント (DeepSeek、GLM、Google Gemini、旧 OpenAI モデル、Kimi K2 など) は `tools[]` を変更せず固定の `call_tool` プロキシ経由で呼び出すため、プレフィックスキャッシュが保たれます。少数のツールを最初から直接宣言したい場合は `tier = "direct"` (または `tools` で個別ツールを `"direct"`) に設定してください。

サーバーがリソースを提供する場合、モデルにはそれを一覧表示して読み取るツールも提供されます。サーバーのプロンプトは `/mcp__<server>__<prompt>` という名前のスラッシュコマンドとして現れます (`name=value` 形式の引数か、位置引数の語を渡します)。サーバーからの elicitation リクエストは TUI ではフォームとして表示され、ヘッドレス実行では常に辞退されます。

### MCP ツールの承認

MCP ツールはサーバーのプロセス内で実行されるため、mare の OS サンドボックスの外です。そのため `auto` プリセットでは、読み取り専用のものを含め、すべての MCP ツール呼び出しで先に確認されます。信頼するツールについて確認を省くには、ルールを追加します。

```lua
mare.plugin("permissions", {
  rules = { { tool = "mcp__github__list_issues", decision = "allow" } },
})
```

または、ダイアログで "Allow and remember" を選びます。[権限](permissions.md) を参照してください。

## サーバーを管理する

```sh
mare mcp list                 # connect to every enabled server; print state, tools and errors (exit 1 on required server problems)
mare mcp list-tools github    # a server's tools with their input schemas
mare mcp disable github       # turn a server off in this project
mare mcp enable github
mare mcp login linear         # OAuth login in the browser
```

`enable` と `disable` はプロジェクトごとに `~/.local/state/mare/mcp-disabled.json` に記憶され、設定ファイルが編集されることはありません。

セッション内では、`/mcp` がサーバーの一覧パネルを開きます。各サーバーの状態 (`connected`、`needs_login`、`needs_approval`、`disabled` など) と、それぞれに応じた操作が表示されます。操作は、ツールとスキーマの表示、再接続、ログインとログアウト、有効化と無効化です。同じ操作は `/mcp login <server>`、`/mcp logout <server>`、`/mcp reconnect <server>`、`/mcp enable <server>`、`/mcp disable <server>`、`/mcp tools <server>` というサブコマンドでも実行できます。`mare doctor` も MCP サーバーを確認します。

## OAuth

HTTP サーバーに `oauth = true` (またはオプションのテーブル) を設定し、一度ログインします。

```sh
mare mcp login linear
```

mare はブラウザーを開いて OAuth フローを完了し、トークンを保存して、そのトークンでサーバーに再接続します。ログインが必要なサーバーは、ログインするまで `needs_login` と表示されます。`/mcp logout <server>` は保存済みのトークンを破棄します。`login` は、`oauth` を設定した HTTP サーバーにのみ適用されます。

mare は、サーバーの保護リソースメタデータ、またはサーバーと同じオリジンの認可サーバーメタデータから認可サーバーを見つけます。メタデータが別のオリジンにある場合、サーバーがメタデータを公開していない場合 (そのときは `auth_server_metadata_url` を設定します)、認可サーバーが PKCE `S256` を宣言していない場合、認可・トークン・登録・失効のエンドポイントがループバック以外のホストで平文の `http` を使う場合は、ログインを拒否します。

代わりに固定のトークンを使うサーバーでは、トークンがファイルに残らないよう `bearer_token_env_var` を使います。

```json
{ "mcpServers": { "internal": { "url": "https://mcp.example.com/mcp", "bearer_token_env_var": "INTERNAL_MCP_TOKEN" } } }
```
