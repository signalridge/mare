# ACP サーバー

`mare acp` は、mare を stdio 上の Agent Client Protocol サーバーとして実行します。ACP に対応したエディターから mare を起動し、パネル内で対話できます。このページでは、mare の登録方法、サーバーが実装している内容、プロトコル全体に対して実装されていない部分を説明します。

## エディターに mare を登録する

ACP クライアントはエージェントをサブプロセスとして起動し、その stdin と stdout 上で JSON-RPC をやり取りします。どのクライアントでも、必要なのは次の 2 つ、コマンドとその引数です。

```json
{
  "command": "mare",
  "args": ["acp"]
}
```

このブロックを書く場所はエディターによって異なります。カスタムまたは外部の ACP エージェントの設定を探し、コマンドに `mare`、引数に `acp` を入力してください。エディターが参照する `PATH` に `mare` が含まれていることを確認してください。含まれていない場合は、フルパスを指定します。

`mare acp` は stdin でプロトコルメッセージを待ち、最初のメッセージを受け取るまで何も出力しません。`mare acp --help` でオプションを確認できます。

## オプション

`mare acp` は、起動オプションの一部を受け付けます。`args` に追加してください。

| オプション | 効果 |
|---|---|
| `-m`、`--model <MODEL>` | モデル。例: `openai/gpt-6.1-terra`。 |
| `--agent <NAME>` | 初期セッションに指定の登録済みエージェント種別を使います。 |
| `-e`、`--effort <EFFORT>` | 推論の強度。 |
| `--tools <TOOLS>` | これらのツールだけを有効にします (カンマ区切り)。 |
| `--exclude-tools <TOOLS>` | これらのツールを除外します (繰り返し指定・カンマ区切り可、`*` 対応)。 |
| `--append-system-prompt <TEXT>` | システムプロンプトにテキストを追加します。 |
| `--name <NAME>` | セッションに名前を付けます。 |
| `--set <KEY=VALUE>` | 設定値を上書きします。例: `--set plugins.permissions.preset=read-only`。 |
| `--strict-config` | 設定に警告がある場合は起動を拒否します。 |
| `--trust`、`--no-trust` | 今回の実行に限りプロジェクトの信頼判断を上書きします (保存しません)。 |
| `--approve-mcps` | 確認なしでプロジェクトおよび `$MARE_MCP_CONFIG` の MCP サーバーを承認します。 |
| `--clean` | ユーザー設定とプロジェクト設定、ユーザープラグイン、パッケージを使いません。 |
| `--startuptime <FILE>` | 起動時間の計測結果をファイルに書き出します。 |
| `-C`、`--cwd <CWD>` | 既定の作業ディレクトリ。 |

たとえば、1 つのモデルに固定した読み取り専用のサーバーは次のとおりです。

```json
{ "command": "mare", "args": ["acp", "--model", "openai/gpt-6.1-terra", "--set", "plugins.permissions.preset=read-only"] }
```

セッションフラグ (`-c`、`-r`、`--no-session`) と `--permission` は `mare acp` のオプションではありません。代わりに `--set plugins.permissions.preset=...` を使ってください。値は `read-only`、`auto`、`full-access` です ([権限](../use/permissions.md) を参照)。

## 実装されている内容

サーバーは次のメッセージを実装しています。

| メッセージ | 種類 | 動作 |
|---|---|---|
| `initialize` | リクエスト | クライアントの要求バージョンとサーバーの `ProtocolVersion::LATEST` のうち低い方を返し、HTTP MCP、画像と埋め込みコンテキストのプロンプト、`loadSession`、セッションの一覧・再開・終了を宣言します。 |
| `authenticate` | リクエスト | サーバーは認証を要求せず、空の成功応答を返します。 |
| `session/new` | リクエスト | クライアントが送った `cwd` で新しい mare セッションを開始し、その ID を返します。 |
| `session/load` | リクエスト | 保存済みの非アクティブなセッションを元の `cwd` で再開し、応答前に `session/update` で履歴を再生します（コンパクション済みのセッションでは圧縮後のコンテキストのみを再生します）。 |
| `session/resume` | リクエスト | 保存済みの非アクティブなセッションを元の `cwd` で再開します。 |
| `session/list` | リクエスト | 保存済みの非アクティブなセッションを一覧表示します。`cwd` で絞り込むこともできます。 |
| `session/close` | リクエスト | アクティブなセッションを終了します。 |
| `session/set_config_option` | リクエスト | セッションのモデルまたは推論の強度を変更します。 |
| `session/prompt` | リクエスト | 1 ターンを実行し、終了時に停止理由を返します。 |
| `session/cancel` | 通知 | そのセッションで実行中のターンを中止します。 |
| `session/request_permission` | サーバーからのリクエスト | ツールまたはネットワーク操作の承認をクライアントに求めます。選択肢は `allow-once`、`reject-once` と、提示された場合の `allow-always` です。 |

`session/new` のたびに、指定された `cwd` に対して別個の mare ホストが作られるため、異なるフォルダーの複数のセッションを並行して実行できます。プロジェクトの信頼はセッションのワークスペースごとに判定されます。ACP は明示的な `--trust` と `--no-trust` を最優先で尊重し、指定がなければ `trust.json` の保存済み判断または `MARE_TRUST_DEFAULT` を使います ([権限](../use/permissions.md) を参照)。ACP は対話的に信頼を確認しません。フォルダーが信頼されるまで、そのプロジェクトのプラグイン、スキル、プロンプトは読み込まれません。そのフォルダーで `mare trust` を実行して記録するか、明示的に `--trust` / `--no-trust` を渡してください。

`session/new`、`session/load`、または `session/resume` で指定された `mcpServers` は、HTTP(S) と stdio トランスポートを受け付けます。stdio のコマンドには絶対パスが必要です。SSE トランスポートには対応していません。mare 自身の MCP 設定については [MCP](../use/mcp.md) を参照してください。

### プロンプト

プロンプトでは、テキストブロック、埋め込みテキストリソース、リソースリンク、画像を元の順序で受け付けます。リソースリンクはテキストとして表され、隣接するテキストは空行で連結されます。音声と埋め込みバイナリリソースは拒否されます。

セッションの現在のモデルが画像入力を宣言していない場合、画像ブロックは不正なパラメーターとして拒否されます。モデルはセッションごとに選択され変更もでき、mare はセッションホストがまだ存在しない初期化時にモデルからこのフラグを決められないため、`initialize` では接続の画像対応を宣言し、画像を含むプロンプトごとに対応を確認します。

ターンは、次のいずれかの停止理由で終了します。

| 停止理由 | 条件 |
|---|---|
| `end_turn` | ターンが完了した。 |
| `cancelled` | `session/cancel` で中止された。 |
| `max_tokens` | 最後のアシスタントメッセージが、長さの制限で停止した。 |
| `max_turn_requests` | `agent.max_turns` に達した。 |
| `refusal` | プロバイダーが応答を拒否またはフィルターした。 |

プロバイダーのエラーと永続化の失敗は、成功した `end_turn` ではなく、JSON-RPC の内部エラーとして返されます。未知のセッション ID は JSON-RPC エラー `-32002`（Resource not found）になります。

### ストリーミング更新

ターンの実行中、mare は `session/update` 通知を送ります。

| ACP の更新 | 送信元 |
|---|---|
| `agent_message_chunk` | アシスタントのテキストのデルタ。 |
| `agent_thought_chunk` | 推論のデルタ。 |
| `tool_call` | ツール呼び出しの開始。`status` は `in_progress`、`title` はツール名とその `path` または `command` 引数、`rawInput` は引数です。 |
| `tool_call_update` | ツール呼び出しの完了。`status` は `completed` または `failed` で、内容はツールのテキスト出力です。 |
| `usage_update` | `last_context` と正の `context_window` を含む `usage/changed` を受けたときに、`used` と `size` を付けて送信します。 |

ツール呼び出しの `kind` はカーネルのツールメタデータから導出されます（`command_tool` が真なら `execute`、`side_effect` に応じて `read` / `edit`、それ以外や履歴再生でメタデータがない場合は `other`）。

## 制限事項

- **セッションモードはありません。** サーバーは ACP のセッションモードを公開せず、`session/set_mode` を実装していません。
- **追加ディレクトリの検証。** `session/new` は `additionalDirectories`（`cwd` 外の実在する物理ディレクトリ、再開時に再検証）を受け付けます。`session/load` と `session/resume` は再検証を通った記録済みの `additionalDirectories` を受け付け、未記録または無効なディレクトリには `Invalid params` を返します。
- **音声プロンプトはありません。** 音声ブロックと埋め込みバイナリリソースは拒否されます。
- **クライアントのファイル操作やターミナルのメソッドはありません。** mare は自分でファイルを読み書きし、コマンドを実行します。エディターの `fs/*` や `terminal/*` メソッドは呼び出しません。
- **認証は不要です。** `initialize` は認証方式を返さず、`authenticate` は空の成功応答を返します。

承認、ホストツール、完全なセッション API を備えたプログラムからの制御には、[RPC](rpc.md) インターフェースを使ってください。
