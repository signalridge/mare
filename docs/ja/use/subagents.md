# サブエージェントとサイドスレッド

mare にはメインの会話から作業を切り離す仕組みが 2 通りあります。別スレッドでタスクを進める永続的な子エージェント (`subagent`) と、セッション履歴を汚さずちょっとした質問ができる一時的なサイドスレッド (`/side`、`/btw`) です。

## サブエージェント

`subagent` プラグインを使うと、独自のセッションコンテキストを持つ子セッションをモデルから起動できます。作業ディレクトリは既定で親を引き継ぎ (`cwd` で指定可能)、ツール許可リストとモデルは種別または呼び出し引数で選びます。子スレッドはホストの `AgentRegistry` (`crates/mare-host`) が追跡し、コアの `agents` 上限で制御され、`sessions/<project>/agents/<run>/<seq>-<label>.jsonl` に永続化されます。

### モデルに公開されるツール

| ツール | 役割 |
|---|---|
| `subagent` | `prompt`、任意の `type`、`fork` (`"none"`、`"all"`、またはターン数)、`name`、`cwd`、`model`、`effort`、`tools`、`system`、`wait` (既定は `false`) を指定して子エージェントを起動します。 |
| `subagent_wait` | 子エージェントの終了 (または `timeout_ms`) まで待機し、最終出力を返します。 |
| `subagent_steer` | 実行中の子エージェントへステアリングメッセージを送ります。 |
| `subagent_cancel` | サブエージェントとその配下の子スレッドをキャンセルします。 |
| `subagent_list` | 現在のセッションで起動したサブエージェントの ID、名前、状態、タスクを一覧表示します。 |

既定では `subagent` はバックグラウンドで起動し、完了時に親セッションへ 1 行の完了サマリー (`[subagent <id> finished: <status>...]`) を届けます。出力本文またはエラーがある場合にのみ `<subagent-output agent="…">` ブロックが付きます。結果をその場で待ちたい場合は `wait = true` を渡します。

バックグラウンドサブエージェントは、それを起動した mare プロセスに属します。サブエージェントの完了前にそのプロセスが終了またはクラッシュした場合、親セッションを再開すると未完了のサブエージェントは `interrupted` とマークされます。`subagent_list` と `subagent_wait` でその状態を確認でき、親セッションにはサブエージェントの識別子と記録ファイルの場所を含む通知が 1 件届くため、作業をやり直せます。

### 組み込みサブエージェント種別

| 種別 | 用途 | ツール範囲 |
|---|---|---|
| `general` | 通常の実装や調査の委譲タスク | 親セッションのツールと権限上限を継承 |
| `explore` | ファイルを変更せずにコードを調査して根拠を報告 | `read`、`ls`、`bash` (読み取り専用サンドボックス) |
| `plan` | ファイルを変更せずに具体的な実装計画を設計 | `read`、`ls`、`bash` (読み取り専用サンドボックス) |

### カスタムエージェント定義

再利用するエージェント種別は、`~/.config/mare/agents/*.md` または信頼済みプロジェクトの `.mare/agents/*.md` に YAML フロントマター付き Markdown として置けます。

```markdown
---
name: security-auditor
description: Audits changes for auth, path traversal, and secret leaks.
when_to_use: Use before merging changes to auth, RPC, or sandbox code.
tools:
  - read
  - ls
model: anthropic/claude-sonnet-5-5
effort: high
---

You are a read-only security reviewer. Report concrete findings with file and line numbers.
```

フロントマターは `name`、`description` (必須)、`when_to_use`、`tools`、`disallowed_tools`、`model`、`effort` を受け付け、Markdown 本文がシステムプロンプトになります。

> `tools` はツール名の絞り込みのみで、権限上限は親セッションから継承されます。読み取り専用を強制するには `bash` などの変更系ツールを外すか、`mare --permission read-only --agent security-auditor` を指定してください (組み込みの `explore`、`plan`、`/review` は `permission = "read-only"` を設定します)。

検索ディレクトリを追加するには `init.lua` で設定します (プロジェクト相対パスにはプロジェクトの信頼が必要です)。

```lua
mare.plugin("subagent", {
  extra_agent_dirs = { ".mare/custom-agents" },
  notify_inline_bytes = 4096,
})
```

### 上限と承認の転送

ルート全体で共有されるスレッド上限は `mare.setup` に書きます。

```lua
mare.setup {
  agents = {
    max_active = 16, -- ツリー全体の同時実行子孫スレッド上限 (0 で委譲を無効化)
    max_depth = 4,   -- ルートセッションからの最大ネスト深さ
  },
}
```

サブエージェントが自分のサンドボックス範囲を超える承認を必要とした場合、`agent-approvals` プラグインが親セッションの UI (`agent/approval-dialog`) へ承認要求を転送します。入力欄の下のキュー行に保留中の承認が到着順に表示され、ダイアログには要求元の子エージェント名が出ます。

#### 権限の継承と親セッション変更時のルール

- **起動時のスナップショット**: サブエージェントは起動時点で親セッションに適用されている権限プリセット・サンドボックスポリシー・承認ポリシーを継承し、それを上限 (`ceiling`) として保持します。親セッションを `full-access` (`FULL`) に切り替えた後に起動した `general` サブエージェントは `full-access` をそのまま継承します。明示的な読み取り専用エージェント (`explore`、`plan`、`/review`、または `permission = "read-only"`) は親のプリセットにかかわらず常に `read-only` に制限されます。
- **親の厳格化は即時伝播**: サブエージェント実行中に親セッションがより厳しいプリセットやサンドボックスポリシーへ切り替わった場合 (またはプロジェクトの信頼が取り消された場合)、実行中のすべての子孫サブエージェントの上限を即時に引き締め、古い一時許可を破棄します。
- **親の権限昇格は実行中のサブエージェントを自動緩和しない**: 親が `read-only` や `auto` の状態でサブエージェントを起動した後、親を `full-access` (`FULL`) へ切り替えても、すでに実行中のサブエージェントの権限は自動的には広がりません。実行中のサブエージェントは起動時の上限を維持し、その範囲を超える操作は引き続き親の承認ダイアログへ転送されます。親を切り替えた**後に**新しく起動した `general` サブエージェントから新しいプリセットを継承します。

### `/agents` での確認

`/agents` を実行する (または入力欄が空の状態で `Left` を押す) とエージェント一覧が開きます。実行中のサブエージェント、ワークフローメンバー、バックグラウンドジョブに加えて、Peers タブでは同じマシン上の他の実行中 `mare` セッションも確認できます。

- `Enter` を押すと、選択したサブエージェントのトランスクリプトを読み取り専用で閲覧します。
- `Alt+Left` / `Alt+Right` でサブエージェント表示を切り替えます。
- `Esc` で一覧へ戻り、`Ctrl+G` で `/resume` へ移動します。

## サイドスレッド (`/side` と `/btw`)

`conversations` プラグインは、現在のセッションの確定済み履歴から分岐した、親に影響しない一時的な会話ウィジェットを提供します。サイドスレッドのやり取りはセッション JSONL ファイルに書き込まれません。

```text
/side why did the test fixture skip line 42?
/btw what does EADDRINUSE mean here?
```

`/side` と `/btw` は同じ入口を共有しています。

| コマンド | 動作 |
|---|---|
| `/side` または `/btw` | エディターの上に空のサイドスレッドウィジェットを開きます |
| `/side <question>` / `/btw <question>` | サイドスレッドを開き、そのまま質問を送信します |
| `/side toggle` | アクティブなサイドスレッドの表示・非表示を切り替えます |
| `/side back`、`/side close`、または `Esc` | サイドスレッドを閉じて破棄します |

サイドスレッドを開いている間は、入力欄から送信したプロンプトはメインエージェントではなくサイドスレッドに送られます。ウィジェットのヘッダーには親エージェントの現在状態 (`idle`、`working`、`blocked`) が表示されます。入力欄から実行できるのは安全な参照コマンド (`/side`、`/btw`、`/help`、`/status`、`/session`) のみであり、サイドスレッド内でのツール呼び出しも `ephemeral-read-only` の実行ライフタイム制約を受けます。サイドスレッドでのトークン消費とコストも `usage` プラグインに加算されます。

## 関連項目

- [権限とサンドボックス](permissions.md): 子エージェントの承認転送とサンドボックス継承
- [RPC ピアソケット](../integrate/rpc.md#ピアソケット): ピアソケットと `mare peer send`
- [オプション](../configure/options.md): `agents` の上限設定
