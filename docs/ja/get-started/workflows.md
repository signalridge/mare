# ワークフロー

## 読み取り専用でコードベースを調べる

```sh
mare --permission read-only "Give me a map of this repository: the main modules, the entry points and how they connect."
```

`read-only` は書き込みとネットワークをブロックします。ネットワーク制御がない Windows 既定層では、`all` / `danger-full-access` が承認・許可されていない限りコマンドツール実行前に確認します。

続けて、より絞った質問をし、`@` でファイルを添付します。

```text
Where is the retry policy decided? Compare @src/client.rs with @src/config.rs.
```

モデルが呼び出せるものを最初から制限するには、ツール名を指定します。`read` と `bash` は組み込みツールの 2 つです (ほかに `write` や `edit` などがあります)。

```sh
mare --tools read,bash --permission read-only
```

プロジェクトで一度 `/init` を実行すると、エージェントがビルドコマンド、アーキテクチャのメモ、規約をまとめた `AGENTS.md` を書きます。以降のセッションはそれをコンテキストとして読み込みます。

## 承認を受けながらバグを修正する

症状を説明し、まず失敗するテストを書くよう頼みます。

```sh
mare "The CSV export drops the last row when the file has no trailing newline. Reproduce it with a failing test, then fix it."
```

既定の `auto` プリセットでは、ワークスペース内の編集はそのまま通り、呼び出しがサンドボックスの外に出る場合に mare が確認します。承認ボックスには `y` (1 回だけ)、`a` (このセッションの間)、`n` (拒否して理由を伝える) で答えます。すべての書き込みを自分で承認するには、`/permission read-only` を実行します。各回答の背後にあるルールは [権限](../use/permissions.md) を参照してください。

コミットする前に、何が起きたかを確認します。

- `/diff` は、各ターンで変更されたファイルを新しい順に表示します。
- `Ctrl+R` で変更ビューアーが開きます。ターンごとの mare 自身の編集、または git の未ステージ、ステージ済み、直近コミットの差分を表示できます。
- `!git diff --stat` と入力すると、コマンドを自分で実行し、結果をコンテキストに入れられます。

## 変更をレビューする

`/review` は、コードをレビューする別の読み取り専用セッションを開始し、結果をトランスクリプトに表示します。結果があなたの会話のコンテキストに入ることはありません。

```text
/review                     pick a target from a list
/review uncommitted         staged, unstaged and untracked changes
/review <sha>               one commit
/review base main           the changes against a base branch
/review focus on error handling and concurrency
```

対象として解釈できないテキストは、カスタムのレビュー指示として扱われます。`/review changes [path]` は、レビューを実行せずに変更ビューアーを開きます。

## スクリプトや CI でヘッドレス実行する

`-p` は 1 つのプロンプトを実行し、最終的な回答を stdout に出力して終了します。

```sh
mare -p "Summarize the changes in @CHANGELOG.md in three bullets"
```

stdin または stdout が端末でない場合も、mare は自動的にヘッドレスになります。パイプで渡した入力はプロンプトに追加されます。`-` を渡した場合やプロンプトがない場合は、その入力がプロンプトになります。

```sh
git diff main... | mare -p "Review this diff for bugs"
echo "list the TODO comments in src/" | mare -p -
```

ヘッドレス実行では何も質問できません。権限レベルを事前に設定し、実行に上限を設けてください。

```sh
mare -p --permission read-only --max-turns 20 --timeout 10m "Find unused exported functions"
```

| 終了コード | 意味 |
| --- | --- |
| `0` | 完了 |
| `1` | エラー |
| `2` | 引数または設定が不正 |
| `3` | 承認が「拒否して中止」で答えられ、ターンが停止した |
| `4` | `--max-turns` または `--timeout` に達した |
| `130` | 中断された |

機械可読な出力には `--json` を使います。1 行につき 1 つの JSON オブジェクトを出力し、各行は `{"type": ..., "data": ...}` の形です。最初に `session` レコードが、最後に最終テキスト、使用量、`exit_code`、`reason` を持つ `settled` レコードが出力されます。

```sh
mare --json "Run the tests and report failures" | tail -n 1 | jq '.data | {exit_code, reason}'
```

`--stream-deltas` を付けると、トークン単位のイベントが得られます。ヘッドレス実行がプロジェクトの信頼について尋ねることはありません。保存済みの判断があればそれが適用され、なければ `--trust` を渡さない限り、プロジェクトの `.mare/` のファイルは読み込まれません。プロジェクトが宣言した MCP サーバーには `--approve-mcps` が必要です。CI では、`--no-session` (セッションを保存せず再開もできない。ランタイムファイルやツールによる書き込みは行われる) と `--strict-config` (設定に警告があれば起動を拒否する) で、再現性のある実行にします。[ヘッドレスモード](../use/headless.md) と [JSON ストリーム](../integrate/json-stream.md) を参照してください。

スクリプトから 1 つのセッションで複数のプロンプトを実行するには、先にセッションを作成します。

```sh
id=$(mare session new --name nightly)
mare -p --resume "$id" "Step 1: list failing tests"
mare -p --resume "$id" "Step 2: fix the first one"
```

## セッションの続行とフォーク

```sh
mare -c                  # continue the most recent session in this directory
mare -r                  # open the session picker
mare -r -2               # the second most recent session in this directory
mare -r nightly          # by name, id or unambiguous id prefix
mare -c --fork-session   # continue in a copy and leave the original unchanged
mare -c --all            # most recent session in any directory
```

セッション内では次のコマンドを使えます。

| コマンド | 内容 |
| --- | --- |
| `/new` (別名 `/clear`) | 新しいセッションを開始する |
| `/resume` (別名 `/sessions`) | セッションを選ぶ。`/resume all` はすべてのディレクトリを一覧表示し、`/resume search <text>` は検索する |
| `/name <title>` (別名 `/rename`、`/title`) | セッションに名前を付け、`-r <name>` で見つけられるようにする |
| `/fork` | 最新のメッセージから新しいセッションにフォークする。`/fork pick` ではメッセージを選べる |
| `/tree` | セッションツリーをたどり、以前の任意の時点から分岐する |
| `/retry` | 直前の回答を再生成する。古い回答はそのブランチに残る |
| `/btw <question>` | 補足の質問をする。回答は会話に追加されない |
| `/export [path]` | 会話を Markdown としてエクスポートする |

セッションは `~/.local/state/mare/sessions` にある JSONL ファイルです。[セッション](../use/sessions.md) を参照してください。

## 取り消しと巻き戻し

取り消しには 2 つのレベルがあり、それぞれ別のことをします。

- `/undo` は、会話をあるプロンプトの前まで巻き戻します。`/undo` だけだと選択画面になり、`/undo N` は後ろから N 番目を指定します。ファイルはそのままです。エージェントがアイドル状態のときに Esc を 2 回押すと、同じ選択画面が開きます。
- `/rewind [turn]` は、ファイルをそのターンの前の状態に戻します。会話もその時点まで巻き戻すこともでき (`/tree` のブランチとして行うので、元の会話は残ります)、そのターン以降の会話を要約にまとめることもできます。

mare は、どのツールであっても、ファイルを書き込み、削除、名前変更する前にその元の内容を保存します。mare の外で変更されたファイルは一覧に表示され、確認した後にのみ復元されます。シェルコマンドによる変更はスナップショットできず、そのターンにその旨が表示されます。

## バックグラウンドジョブ

時間のかかるコマンドは、エージェントがバックグラウンドで開始し、作業を続けます。完了すると、出力の短い一部とともに通知されます。

- 依頼する: 「開発サーバーをバックグラウンドで起動して、ポート 8080 で応答することを確認して」。
- コマンドの実行中に `Ctrl+B` を押すと、バックグラウンドに移せます。
- `/jobs` はジョブを一覧表示し、ジョブの出力を開いたり停止したりできます。入力ボックスの上の行に、実行中のジョブが表示されます。

## エージェントの作業中に誘導またはキューに入れる

ターンの終了を待つ必要はありません。

| キー | エージェントの作業中の効果 |
| --- | --- |
| Enter | メッセージをキューに入れる。ターンの終了時に送信される |
| `Ctrl+S` | 誘導 (steer): 入力を実行中のターンに送る |
| `Alt+Enter` | 明示的にフォローアップとしてキューに入れる (Windows では `Ctrl+Q`) |
| Esc | ターンを中断する |
| `Ctrl+C` | 中断する、または入力を消去する。2 回押すと終了 |

メッセージがキューにある状態で、空の入力に対して Enter を押すと、3 つの選択肢のメニューが表示されます。今すぐ誘導する、中断して送信する、キューに残す、です。キューにあるメッセージは入力欄の上に表示され、Up で 1 つ選んで Enter を押すと、その場で編集できます。

## 画像を扱う

- 入力欄で `Ctrl+V` を押すと、クリップボードの画像を貼り付けられます (ターミナルが `Ctrl+V` を使う Windows では `Alt+V`)。
- エージェントに画像ファイルを見るよう頼みます: 「docs/mockup.png を見て、UI の要素を一覧にして」。read ツールは PNG、JPEG、GIF、WebP のファイルを画像として返します。

Kitty、iTerm2、sixel の画像プロトコルに対応したターミナルでは、mare はトランスクリプト内に画像をインライン表示します。それ以外では、トランスクリプトに `[image]` と表示されます。どの画像を描画するかは `images` プラグインが制御します。[UI](../configure/ui.md) を参照してください。
