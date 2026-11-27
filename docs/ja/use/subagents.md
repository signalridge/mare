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

- **起動時の継承**: サブエージェントは起動時点で親セッションに適用されている権限プリセット・サンドボックスポリシー・承認ポリシーを継承します。親セッションを `full-access` (`FULL`) に切り替えた後に起動した `general` サブエージェントは `full-access` をそのまま継承します。明示的な読み取り専用エージェント (`explore`、`plan`、`/review`、または `permission = "read-only"`) は親のプリセットにかかわらず常に `read-only` に固定されます。
- **親の厳格化は即時伝播**: サブエージェント実行中に親セッションがより厳しいサンドボックスポリシーへ切り替わった場合 (例: `full-access` → `auto` / `read-only`、またはプロジェクトの信頼取り消し)、アタッチ済みのすべての子孫 VM のサンドボックス上限を即時に引き締め、予約済み子孫にはアタッチ時に適用して以降のツール呼び出しへ反映し、古い一時許可を破棄します。すでに実行中のツール呼び出しは終了まで開始時の凍結ポリシーを維持します。サンドボックスポリシーを変えず承認挙動だけを厳しくするプリセット変更 (例: `danger-full-access` のまま `like = "read-only"` のカスタムプリセット) は新規起動または続行 (`subagent_steer`) 時に反映されます。
- **親の権限昇格時のルール**:
  - 権限を固定していない継承型サブエージェント (`general` など) は、親を `full-access` (`FULL`) などへ切り替えた後に `subagent_steer` / `mare.subagent.steer` で続行すると、権限プリセットとサンドボックス上限が親の現在プリセットへ再同期され、以降の書き込みや `request_permissions` で確認ダイアログは出ません。
  - 実行中の未固定サブエージェントが、親の昇格後にサンドボックス承認要求を送った場合も、親側は現在のプリセットを基準に判定し、親の現在プリセットで許可される範囲はログ記録のうえ自動で許可します (危険コマンド、明示的な `prompt` ルール、機密パスは引き続き確認し、`forbidden` ルールは拒否します)。
  - 明示的に読み取り専用へ固定されたサブエージェント (`explore`、`plan`、`/review`、または `permission = "read-only"` / `permission_ceiling` 指定) は、続行時・承認時のいずれでも親の権限昇格によって緩和されません。

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
