# ヘッドレスモード

対話型 UI なしで mare を実行します。スクリプトや CI 向けで、最終的な回答を出力するか、機械可読なイベントをストリームし、ステータスコードで終了します。

```sh
mare -p "list the public functions in src/lib.rs"
```

`-p` (`--print`) はプロンプトを実行し、最終的な回答を stdout に出力します。stdin または stdout が端末でない場合も、mare は自動的にこのモードに切り替わります。実行中にあなたへ質問することはありません。回答が必要になる承認は拒否されます (後述)。

## プロンプトの渡し方

```sh
mare -p "explain this error"                     # prompt as arguments
git diff | mare -p "review this diff"            # piped stdin is appended as <stdin>...</stdin>
cat prompt.md | mare -p                          # no prompt argument: stdin is the prompt
mare -p - < prompt.md                            # "-" reads the prompt from stdin explicitly
mare -p explain @src/main.rs                     # a separate @path argument attaches that text file
```

- プロンプト引数とパイプされた stdin の両方がある場合、stdin のテキストは `<stdin>` タグで囲まれてプロンプトの後ろに追加されます。1 秒以内に stdin から何も届かなければ、継承されたパイプで実行が止まらないよう、mare は stdin を無視します。
- stdin は UTF-8 で、64 MiB 以下である必要があります。プロンプトがなく stdin も空の場合は使い方の誤りです (終了コード 2)。
- `@path` 引数は、`@` で始まるシェルの 1 語全体です。テキストファイルはインラインで添付されます。画像と読み取れないパスは、書かれたままになります。

以前の作業の続きは、[セッション](sessions.md) のセッションオプションで再開します。

```sh
mare -p -c "now add tests"                         # continue the last session here
mare -p --resume 3f9a1c2e "and update the docs"    # by id; see `mare session new`
mare -p --no-session "one-off question"            # do not save this session
```

## 機械可読な出力

```sh
mare -p --json "fix the failing test" > run.jsonl
mare -p --json --stream-deltas "write a haiku"
```

`--json` (`--mode json` の短縮形) は、1 行につき 1 つの JSON オブジェクトを出力します。最初の行は `session` レコードで、続いて公開されているエージェントイベント (ターンの開始、メッセージ、ツール呼び出し、使用量) が並び、実行が失敗した場合は `turn.failed`、最後に `settled` が出力されます。`--stream-deltas` がない場合、ストリームはアイテム単位です。付けると、テキストとツール出力のデルタも含まれます。レコードの形式は [JSON ストリーム](../integrate/json-stream.md) にあります。

```sh
mare -p --json "summarize CHANGELOG.md" | jq -r 'select(.type == "settled") | .data.text'
mare -p --json "..." | jq -r 'select(.type == "settled") | "\(.data.reason) \(.data.exit_code)"'
```

stdout の読み手がいなくなった場合 (`... | head`)、mare は誰のためでもないトークンを使い続けないよう、実行を停止して 141 で終了します。

長期間維持する双方向の接続には、代わりに `--rpc` を使います。[RPC](../integrate/rpc.md) を参照してください。

### 構造化出力 (`--output-schema`)

ヘッドレスモード (`-p` または `--json`) で `--output-schema FILE` を渡すと、最終回答を指定した JSON Schema に制約し、終了前に検証できます。

最小のスキーマ例 (`summary.schema.json`):

```json
{
  "type": "object",
  "required": ["risk", "findings"],
  "additionalProperties": false,
  "properties": {
    "risk": { "type": "string", "enum": ["low", "medium", "high"] },
    "findings": { "type": "array", "items": { "type": "string" } }
  }
}
```

実行と結果の読み取り:

```sh
# JSON ストリーム: パイプで mare の終了コードを隠さないよう一度ファイルへ保存し、jq -ce で .data.structured を取り出す
mare -p --json --no-session --output-schema summary.schema.json \
  "Inspect src/auth.rs and summarize risk." > run.jsonl || exit $?
jq -ce 'select(.type == "settled") | .data.structured' run.jsonl > summary.json

# プリントモード: 検証を通過した JSON テキストがそのまま stdout に出る
mare -p --no-session --output-schema summary.schema.json \
  "Inspect src/auth.rs and summarize risk." > summary.json
```

検証エラー時の挙動:

- `--output-schema` はヘッドレス実行 (`-p` / `--json`) 専用です。ヘッドレス以外での指定、ファイル欠如、JSON 構文エラー、不正なスキーマはターン開始前に終了コード `2` で終了します。
- モデルの最終出力がスキーマ検証に失敗すると、`error_kind = "invalid_output"` で終了コード `1` になります。`-p` モードでは stdout に何も書かず検証エラーを stderr に出力します。`--json` モードでは `turn.failed` に続いて、`error` を含み `structured` を省いた `settled` レコードを出力します。

## 制限とツール

| オプション | 効果 |
| --- | --- |
| `--max-turns N` | 1 つのプロンプトでモデルのターンが `N` 回に達したら停止します。終了コード 4。 |
| `--timeout DURATION` | この時間が経過したら実行を停止します。`90`、`90s`、`1500ms`、`5m`、`1h` を指定できます。終了コード 4。ヘッドレスのみ。 |
| `--output-schema FILE` | 最終回答を JSON Schema ファイルで検証します (`--json` では `settled.data.structured` に格納)。ヘッドレスのみ。 |
| `--tools a,b` | モデルには、これらのツールだけを提供します。例: `--tools read,ls`。 |
| `--exclude-tools TOOLS` | 一致するツールを除外します (繰り返し指定・カンマ区切り・`*` ワイルドカード対応。例: `--exclude-tools bash,write`)。除外されたツールはリクエストから隠され、実行時も拒否されます。 |
| `--add-dir DIR` | 既存の物理ディレクトリをワークスペースに追加します (繰り返し指定可。シンボリックリンクを含まない絶対ディレクトリで、保護対象パスと重ならないこと。`mare acp` では使えません)。 |
| `--append-system-prompt TEXT` | 今回の実行に限り、システムプロンプトの末尾にテキストを追加します。 |
| `-m`、`-e` | モデルと推論の強度。[モデル](models.md) を参照。 |
| `--permission`、`--yolo`、`--approval-policy` | 権限プリセットと承認ポリシー。後述。 |
| `--no-session` | 実行を永続化しません。 |
| `--set KEY=VALUE` | オプションを 1 つ上書きします。例: `--set plugins.tools.edit_mode=replace`。 |
| `--strict-config` | 設定に警告がある場合、起動を拒否します。 |
| `--clean` | ユーザー設定とプロジェクト設定、ユーザープラグイン、パッケージ、`state.json` を無視します。 |

```sh
mare -p --tools read,ls --max-turns 8 --timeout 5m \
  --append-system-prompt "Answer in one paragraph." \
  "what does this repository do?"
```

## 権限と承認

通常のメインセッションは、信頼済みプロジェクトでは既定で `auto`、未信頼プロジェクトでは `untrusted_preset` (既定は `read-only`) を使います。明示的な `--permission` 指定が優先されます。ヘッドレス実行では応答者のいない承認要求は拒否され、ツールはその拒否結果をモデルに返すため、モデルは別の操作を続行できます。

- mare は stderr に `mare: N approval(s) were needed and nobody could answer (denied)` と、拒否された呼び出しごとの 1 行を出力します。
- 実行がそれ以外で完了すれば、終了コードは 0 のままです。終了コード 3 は、「拒否して停止」と答えられた承認が原因で実行が停止したことを意味します。

作業内容に合うプリセットを選びます。

```sh
mare -p --permission read-only "audit this repo for hard-coded secrets"   # no writes at all
mare -p "fix the typo in README.md"                                       # 信頼済みプロジェクト: auto はワークスペース内に書き込み可
mare -p --yolo "run the full migration"                                   # full-access: no sandbox; still asks for prompt rules and dangerous commands
mare -p --approval-policy never "..."                                     # reject anything that would ask
```

明示的な `prompt` ルールや `rm -f` のような危険なコマンドは、`--yolo` を含むすべてのプリセットで確認が入るため、そのコマンド自体を許可するルールがない限りヘッドレス実行では拒否されます。一方、解析不能なコマンドは解析失敗だけで一律に確認・拒否されるわけではありません。`full-access` でそのディレクトリに適用される制限ルール (`prompt` または `forbidden`) がない場合は `allow` になり得ます。該当する制限ルールが一致する可能性がある場合に限り `ask` (ヘッドレスでは拒否) へフォールバックします。[権限](permissions.md) を参照してください。

プロジェクトのリソース (`.mare/plugins`、`.mare/init.lua`、スキル、プロンプト、`.mare/SYSTEM.md`) は信頼済みのプロジェクトでのみ読み込まれ、ヘッドレス実行が信頼を尋ねることはありません。プロジェクトが信頼されていなければ、mare は stderr にメモを出力し、それらなしで続行します。今回の実行に限りプロジェクトを信頼するには `--trust` を渡すか、一度 `mare trust` を実行します。プロジェクトの MCP サーバーには `--approve-mcps` が必要です。[MCP](mcp.md) を参照してください。

`--yolo` は、使い捨てのコンテナーや VM の中でのみ実行してください。

## 終了コード

| コード | 意味 |
| --- | --- |
| 0 | 完了。 |
| 1 | 実行が失敗した (プロバイダーのエラー、使えるモデルがない、セッションが見つからないなど)。 |
| 2 | 引数または設定が不正。実行は開始されなかった。 |
| 3 | 承認が「拒否して中止」(`abort`) で答えられたため、実行が停止した。誰も答えられなかった承認は拒否されるだけで、このコードにはなりません。 |
| 4 | `--max-turns` または `--timeout` に達した。 |
| 5 | セッションの永続化に失敗した（最終回答は出力され、未保存のレコードは `<state>/quarantine/` に隔離される）。 |
| 130 | 中断された (SIGINT)。 |
| 141 | 読み手によって stdout が閉じられた。 |

`--json` を指定すると、`settled` レコードには同じコードが `exit_code` として、また `reason` (`completed`、`error`、`usage`、`approval_unavailable`、`max_turns`、`timeout`、`save_failed`、`interrupted`) が含まれます。完全な一覧は [終了コード](../reference/exit-codes.md) にあります。

## CI の例

実行が失敗したらジョブを失敗させ、ストリームをアーティファクトとして残すシェルのステップ:

```sh
export ANTHROPIC_API_KEY="$SECRET"
mare -p --json --no-session --permission read-only --timeout 10m \
  -m anthropic/claude-sonnet-5-5 \
  "Review the changes in this pull request and list concrete bugs." > review.jsonl
code=$?
jq -r 'select(.type == "settled") | .data.text' review.jsonl > review.md
exit $code
```

使い捨てのランナー上で、エージェントにファイルを編集させる GitHub Actions のステップ:

```yaml
- name: Fix lint errors
  env:
    OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
  run: |
    mare -p --max-turns 20 --timeout 15m \
      "Run the linter, fix every error it reports, and re-run it until it passes."
    git diff --stat
```

結果の確認と終了コードの処理:

```sh
mare -p --json "..." > out.jsonl
case $? in
  0) echo "ok" ;;
  3) echo "needs a permission the CI preset does not grant" ;;
  4) echo "hit --max-turns or --timeout" ;;
  *) echo "failed" ;;
esac
```

認証には API キーの環境変数を使います ([モデル](models.md) を参照)。`mare login` は認証情報をディスクに保存するため、自分のマシンでの利用にとどめるのが適切です。
