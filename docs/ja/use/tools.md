# ツールとシェルモード

mare の組み込みツールはすべて Lua プラグイン (`tools`、`tools-apply-patch`、`jobs`、`web`、`subagent`、`permissions`) が登録しており、引数検証、ポリシー判定、OS サンドボックス適用に加え、mare 自身のファイルツール (`write`、`edit`、`apply_patch`) についてはファイルスナップショットを取得する単一のガード付きパイプラインで実行されます (シェル子プロセスによるファイル変更はスナップショット対象外です)。

## 組み込みツール

| ツール | プラグイン | 用途 |
|---|---|---|
| `read` | `tools` | UTF-8 テキスト (`offset`/`limit` 指定可)、ディレクトリ一覧、画像 (`png`、`jpg`、`jpeg`、`gif`、`webp`) を読み取ります。 |
| `write` | `tools` | 上書き前にスナップショットを取り、ファイルを作成・上書きします。通常ファイルはアトミックに書き込み、ハードリンクが複数あるファイルはリンク関係を維持するためその場で上書き (`atomic = false`) します。 |
| `edit` | `tools` | 完全一致の文字列置換 (`old_string`、`new_string`、`replace_all`)。`edit_mode = "replace"` のとき、または `"auto"` でモデルの `edit_tool` やプロバイダー既定値が `apply_patch` を選ばない場合のフォールバックとして有効になります。 |
| `apply_patch` | `tools-apply-patch` | 複数ファイル対応のパッチ形式 (`*** Begin Patch` / `*** End Patch`)。明示的な `edit_mode` が優先されます。`"auto"` ではモデルの `edit_tool` またはプロバイダー既定値を使い (組み込み判定は GPT と Codex を対象)、未指定時は `replace` にフォールバックします。 |
| `ls` | `tools` | ドットファイルを含め、`ls_limit` (既定 `500`) までディレクトリエントリを一覧表示します。 |
| `bash` | `tools` | OS サンドボックス内の永続シェルまたはジョブ管理シェルでコマンドを実行します。 |
| `powershell` | `tools` | Windows 上で PowerShell コマンドを実行します (`powershell` オプションは既定 `"auto"`、`"replace"` で `bash` を置き換えます)。 |
| `job_output`, `job_list`, `job_write`, `job_kill` | `jobs` | バックグラウンドジョブの出力取得、一覧表示、標準入力・EOF 送信、停止を行います。 |
| `web_search`, `web_fetch` | `web` | Web 検索や HTTP(S) ページ取得を行います (`web-fetch-http` の `http-text` バックエンドまたはプロバイダー標準検索を使用)。 |
| `tool_search` | `tools` | 遅延ロード対象のツール (遅延 MCP ツールなど) をキーワードまたは `select:<name>` で検索し、入力スキーマを取得します。 |
| `call_tool` | `tools` | ネイティブの会話途中ツール追加 (`tool_update = "addition_only"`) に非対応のモデルで、名前と引数を指定して遅延ツールを呼び出します (`tools[]` を固定しプロンプトキャッシュを保ちます)。 |
| `subagent`, `subagent_*` | `subagent` | 子エージェントの起動と管理 ([サブエージェントとサイドスレッド](subagents.md) を参照)。 |
| `request_permissions` | `permissions` | 理由と `turn` / `session` スコープを添えて追加の読み書きパスやネットワークホストを申請します ([権限とサンドボックス](permissions.md#追加権限の申請) を参照)。 |

### 編集前の読み取り確認と編集モード

`tools` プラグインは既定で `read_before_edit = true` を有効にしており、そのセッション内で一度 `read` していない既存ファイルに対する `edit`、`apply_patch`、上書き `write` を拒否します。読み取り後にディスク上でファイルが更新されていた場合も失敗します。

編集モードや制限値は `init.lua` や CLI から切り替えられます。

```lua
mare.plugin("tools", {
  edit_mode = "auto",        -- "auto" | "apply_patch" | "replace"
  read_before_edit = true,
  bash_timeout_ms = 120000,
  max_lines = 2000,
  max_bytes = 50 * 1024,
})
```

### ツールの無効化とコマンドツールの追加

`init.lua` から組み込みツールを無効化したり独自コマンドツールを登録したりできます。起動ごとに除外したい場合は CLI の `--exclude-tools` を使います (繰り返し指定・カンマ区切り・`*` ワイルドカード対応。例: `mare --exclude-tools bash,write`、`mare --exclude-tools "mcp__*"`)。

```lua
mare.tools.disable({ "ls" })

mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
  readonly = true,
}
```

`mare.tool.command` で登録したコマンドは、認証情報を除いた環境 (`KEY`、`PASSWORD`、`SECRET`、`TOKEN` および継承された `MARE_*` 変数を削除済み) で実行され、`tools` プラグインの `env` 設定に従います。`env.include` は削除対象の継承変数を残し、`env.exclude` は追加の変数を除外し、`env.set` は削除後に値を設定します。加えて mare 自身が実行時変数 (`MARE_CWD`、`MARE_SESSION_ID`、`MARE_SESSION_MODEL`、`MARE_SESSION_EFFORT`、ピア有効時は `MARE_PEER_SOCKET`) を注入します。`readonly = true` を付けない場合、副作用のあるツールとして扱われます。

## Web 検索とページ取得

`web` と `web-fetch-http` プラグインが `web_search` と `web_fetch` を提供します。`web-fetch-http` は認証情報を埋め込んだ URL や、プライベート・ループバック・リンクローカル・クラウドメタデータ IP アドレスへの接続をブロックし (`auto` プリセットでも有効)、取得された本文は信頼できない外部データであることを示す境界マーク付きでモデルへ渡されます。

セッション中に `/web` コマンドで現在の状態確認やバックエンド切り替えができます。

- `/web`: 現在の検索モード (既定は `cached`) と利用可能なバックエンドを表示します。
- `/web mode disabled|cached|indexed|live`: 検索モードを切り替えます。
- `/web search <id|auto>`、`/web fetch <id|auto>`: 使用するバックエンドを選択します (fetch には組み込みの `http-text` があります)。

タイムアウトや出力上限は `init.lua` で調整します。

```lua
mare.plugin("web", {
  web_search = "cached",
  search_timeout_ms = 30000,
  fetch_max_output_chars = 200000,
})
```

## `@file` 参照と画像添付

### `@file` 補完 (`file-ref`)

入力欄で `@` を打つと、作業ディレクトリに加えて起動時の `--add-dir <DIR>` またはセッション中の `/add-dir` で追加したルート配下のパスをあいまい補完できます (スペースを含むパスは `@"path with spaces"` のように書きます)。`--add-dir` は繰り返し指定でき、ホスト・プロジェクトの保護パスと重ならない既存の物理絶対ディレクトリのみを受け付けます (`mare acp` では `--add-dir` ではなく ACP の `additionalDirectories` を使用してください)。

- 既定の上限は 1 ファイル `100 KiB` (`max_bytes`)、1 メッセージ合計 `256 KiB` (`max_total_bytes`) です。
- ディレクトリを指定するとエントリ一覧が添付されます。
- 読み取り不可、バイナリ、拒否対象、または上限超過のパスを黙って落とすことはありません。理由と `read` ツールの案内を記した 1 行の注記を添付します。

### 画像の貼り付けと表示 (`images`)

クリップボードの画像は `Ctrl+V` (Windows では `Alt+V`) で貼り付けられます。モデルが `read` で画像ファイルを開くこともできます。Kitty、iTerm2、Sixel に対応したターミナルでは `images` プラグインがインライン描画を行い、非対応ターミナルでは `[image]` と表示します。

```lua
mare.plugin("images", {
  user = true,
  read = true,
  tools = true,
  max_rows = 12,
  max_cols = 60,
})
```

## 出力のスピル保存 (`spill`)

- シェル出力ログ: `bash` 出力が `shell_buffer_bytes` (`1 MiB`) または切り詰め上限 (`2000` 行 / `50 KiB`) を超えると、全文を `~/.local/state/mare/spill/bash-*.log` に書き出し、省略結果とパスを返します。終了済みプロセスのログは自動削除されます。
- セッション単位の `spill` サービス: ツール・レンダラー・RPC ホストは `spill.save` / `spill.read` でプライベートなテキスト／バイナリ (最大 `8 MiB`／件) を保存します。所有 `session_id` に限定され、`cleanup_period_days` (既定 `30` 日) 後に削除されます。

## `!` シェルモードとバックグラウンドジョブ

### 対話シェルモード (`shell-mode`)

入力欄が空の状態で先頭に `!` を入力すると、`bash` ツールと同じシェル・`prelude`・`MARE_*` 環境を使って、承認プロンプトなしで手動コマンドを実行できます。

```text
!git status --short      # 実行し、コマンドと出力をモデルのコンテキストへ追加
!!cat .env.example       # 実行し、スクロールバックにのみ表示してコンテキストには入れない
```

実行中の `!` コマンドは `Esc` または `Ctrl+C` で中断でき、空行で `Backspace` を押すとシェルモードを抜けます。モデルに渡される出力は前後を残して最大 `2000` 行 / `50 KiB` に切り詰められます。

### バックグラウンドジョブ (`jobs`)

時間のかかるコマンドを会話を止めずに裏へ回せます。

- フォアグラウンドコマンドの実行中に `Ctrl+B` を押すとバックグラウンドへ移せます。
- モデルからは `run_in_background = true` (対話プロセスなら `tty = true` や `stdin = true`) で起動し、`job_output` (`until = "<regex>"` や `tail = N` に対応)、`job_write`、`job_kill` で操作します。
- `/jobs` を実行すると実行中・終了済みジョブの一覧、末尾ログ (`view_tail_bytes`) の閲覧、停止ができます。バックグラウンドジョブが終わると、出力の先頭 `notify_inline_bytes` (2048 バイト) 付きでエージェントへ通知が届きます。

```lua
mare.plugin("jobs", {
  max_running = 8,
  max_jobs = 64,
  kill_grace_ms = 3000,
})
```

## 関連項目

- [権限とサンドボックス](permissions.md): OS による隔離と `bash` の許可ルール
- [MCP サーバー](mcp.md): 外部ツールと `tool_search` による遅延読み込み
- [出力ベース画像リソース（英語）](https://github.com/signalridge/mare/blob/main/docs/image-resources.md): アーティファクト画像のキャッシュ仕様
