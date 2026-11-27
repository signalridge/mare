# スナップショットとコードレビュー

mare のファイルツール (`write`、`edit`、`apply_patch`) がファイルを変更・削除・リネームする直前に、mare は元のファイルバイト列をセッションごとのスナップショットへ記録します。シェル子プロセス (`bash`、`powershell`、`shell`) によるファイル変更はスナップショットに入らず、`/rewind` での復元は保証されません (mare は未記録の変更がある可能性があるターンとして印を付けます)。ターン単位の差分は `/diff` で確認し、スナップショット済みファイルを戻すには `/rewind` (会話だけ戻すなら `/undo`) を使い、読み取り専用のレビューは `/review` で実行します。

## スナップショット、`/diff`、`/rewind`

`changes` プラグインはセッションログの隣 `<session-path>.snapshots/` (または `--no-session` 時は `<state>/snapshots/run-<pid>-<uuid>/`) にバックアップを保存します。総容量は `cap_bytes` (既定 64 MiB)、インデックスは 20,000 レコードで上限管理され、上限を超えると最も古いターンから順に削除されます。単一ファイルが 16 MiB を超える場合、書き込み自体は続行しますがスナップショットは欠損として記録されます。

### `/diff` と `Ctrl+R` で変更を見る

- `/diff`: mare のツールがファイルを書き換えたターンを新しい順に表示するオーバーレイを開き、`+adds / -dels` 行数と unified diff を確認できます。
- `Ctrl+R` (または `/review changes`): mare のターン別差分 (`turns`) に加え、git の `unstaged`、`staged`、`committed` (直近コミット) を切り替えて見られるビューアーを開きます。`collapse_lines` (既定 400 行) を超える差分は折りたたまれた状態で開きます。

`bash`、`powershell`、`shell` を使ったターンでは、シェル側の副作用をファイルガードが捕捉できないため、その旨のマークが付きます。

### `/rewind` でファイルを復元する

`/rewind` (または `/rewind <turn-id>`) を実行してターンを選びます。セッションが会話の巻き戻しに対応し、そのターン直前の履歴位置が記録されている場合は次の 3 つの操作を選べます (そうでない場合はファイルの復元のみを行います)。

| 操作 | 効果 |
|---|---|
| Restore files | 選択したターン以降に変更されたファイルを、そのターン実行前の状態へ戻します。会話履歴は残ります。 |
| Restore files and the conversation | ファイルを元に戻し、セッションツリーをそのターン直前へ分岐させ、当時のプロンプトを入力欄へ戻します。 |
| Summarize from here | ディスク上のファイルには触れず、そのターン以降の会話を要約にまとめます。 |

復元時には次の安全チェックが働きます。

- **外部変更の競合チェック**: スナップショット記録後に mare 以外 (エディターなど) で書き換えられたファイルがある場合、`/rewind` は上書き前に競合パス一覧を出して確認を求めます。
- **リンクへの書き込み拒否**: シンボリックリンクやハードリンク越しの復元は行わず、対象パスがリンクになっていれば拒否して報告します。
- **削除済み・上限超過のバックアップ**: `cap_bytes` や 20,000 レコード上限で古いバックアップが削除済みの場合や、元ファイルが 16 MiB を超えてスナップショットされなかった場合、`/rewind` は推測で復元せず欠損として報告します。

容量上限は `init.lua` で変更できます。

```lua
mare.plugin("changes", {
  cap_bytes = 128 * 1024 * 1024,
})
```

### 会話だけを戻す `/undo`

`/undo` (またはアイドル状態の空の入力欄で `Esc` を 2 回押す) を使うと、ディスク上のファイルを変えずに、指定したプロンプトの直前へ会話状態だけを巻き戻し、そのプロンプトを入力欄へ戻します。取り消されたターンも元のブランチとしてセッション JSONL (`/tree`) には残ります。

## 読み取り専用コードレビュー (`/review`)

`review` プラグインは、`read-only` 権限プリセットと読み取り専用ツール (`read`、`ls`、読み取り専用サンドボックス内の `bash`) を使う単発のサブエージェントを起動します。レビュー結果は画面上のトランスクリプトにそのまま表示され、メイン会話のコンテキストウィンドウには入りません。

```text
/review                                    # 対話的にレビュー対象を選択
/review uncommitted                        # staged / unstaged / 未追跡ファイルをレビュー
/review <sha>                              # 単一コミットをレビュー (/review commit <sha> も可)
/review base main                          # 指定ブランチとの差分をレビュー
/review focus on race conditions and locks # 観点を指定してレビュー
/review changes [path]                     # エージェントを実行せず差分ビューアーを開く
```

設定は `init.lua` の `review` プラグインで行います。

```lua
mare.plugin("review", {
  collapse_lines = 400,
  scopes = { "turns", "unstaged", "staged", "committed" },
  prompt_diff_bytes = 120 * 1024,
  tools = { "read", "ls", "bash" },
})
```

エディターで `Ctrl+R` を無効にするには `mare.keymap.set("editor", "<C-r>", false)` を指定します。履歴検索に使う場合は `"history-search"` を明示的に割り当てます: `mare.keymap.set("editor", "<C-r>", "history-search")`。

## 関連項目

- [セッション](sessions.md): `/tree`、`/fork`、セッション JSONL の永続化
- [権限とサンドボックス](permissions.md): `/review` が使う `read-only` サンドボックス
- [外観](../configure/ui.md): 差分のハイライトグループ (`MareDiffAdd`、`MareDiffDelete`)
