# JSON イベントストリーム

`mare --json` は、1 つのプロンプトをヘッドレスで実行し、1 行につき 1 つの JSON レコードを stdout に書き出します。ストリームは `session` レコードで始まり、実行の進行に合わせて公開されているエージェントイベントを運び、最終テキストと終了コードを持つ `settled` レコードで終わります。このページは、スクリプトが頼りにできる契約です。

## 実行する

```sh
mare --json "Run the tests and fix any failures"
```

`--json` は `--mode json` の短縮形です。プロンプトは引数で渡すか、`-` を渡した場合や引数がなく stdin がパイプの場合は stdin から渡します。すべてのレコードは `{"type": ..., "data": ...}` という 1 行です。stdout にはレコードだけが出力されます。警告と拒否された承認は stderr に出力されます。

開始できない実行は、次のようになります。不正な引数でも `settled` レコードで終わるため、読み手は必ず結果を得られます。

```json
{"type":"session","data":{"protocol_version":"3.0","cwd":"/work/app"}}
{"type":"turn.failed","data":{"error":{"code":"invalid_arguments","message":"unexpected argument '--bogus' found"}}}
{"type":"settled","data":{"text":"","error":{"code":"invalid_arguments","message":"unexpected argument '--bogus' found"},"exit_code":2,"reason":"usage"}}
```

## レコード

各行には 2 つのキーがあります。

| キー | 意味 |
|---|---|
| `type` | `session`、イベント名、`lagged`、`dropped`、`turn.failed`、`settled` のいずれか。 |
| `data` | レコードのペイロード。常にオブジェクト。 |

読み手は、知らないキーを無視してください。新しいフィールドは予告なく追加されますが、既存のフィールドの意味は変わりません。

### `session`

常に最初の行です。

| フィールド | 意味 |
|---|---|
| `protocol_version` | プロトコルのバージョン (`major.minor`)。 |
| `session_id` | セッション ID。`--no-session` では再開できない一時的な ID です。 |
| `path` | セッションファイル。`--no-session` ではありません。 |
| `cwd` | 実行の作業ディレクトリ。 |
| `model` | モデルの参照 (分かっている場合)。 |
| `ephemeral` | `--no-session` では `true`。セッションの記録は保存されず、実行を再開できず、クラッシュからの復旧もありません。それ以外の書き込み (ランタイムファイル、スナップショット、ツールの出力) は行われることがあります。 |

あとから `mare -p --resume <session_id> "..."` で会話を続けるには、`session_id` を使います ([セッション](../use/sessions.md) を参照)。

### イベントレコード

`session` と `settled` の間では、`type` は公開されているエージェントイベントの名前で、`data` はそのペイロードです。イベント名は `area/name` の形式です。組み込み公開イベントの一覧（配信クラス、無視可能フラグ、ペイロードフィールド、例）は [イベントリファレンス](../reference/events.md) を参照してください。

内部のプラグインイベントが現れることはありません。独自のプラグインイベントを公開ストリームに出すには、サービス契約で `expose = "both"` を宣言します。RPC では、プライマリ接続から `events.subscribe { session_id: "...", include: [...] }`（または `{ scope: "all", include: [...] }`）で追加の JSON カーネルイベントを購読することもできます。

組み込みイベントは、配信前にスキーマに照らして検査されます。ペイロードはプレーンな JSON である必要があります。予約された名前 (`session`、`turn.failed`、`settled`、`lagged`、`dropped`) を使うプラグインイベントは、ストリームエラーで実行を失敗させます。ペイロードのスキーマは `mare rpc schema` で出力できます。ストリーム自体のレコードは、その `headless` キーの下にあります。

ツール呼び出しが完了すると、`item/completed` が `item.type == "tool_call"` のアイテムを運びます。ここには `tool_call_id`、`name`、`message`、`result_status` (`success`、`error`、`denied`、`cancelled`、`persist_failed`)、`execution_status` が含まれます。これらのステータスは、副作用が起きたかどうかを示すものではありません。

### アイテム単位とデルタ

既定では、ストリームはチャンクごとに発生する `ignorable` なデルタ／補助イベント（テキスト・推論・ツール出力の `item/delta` など）を省き、ライフサイクル境界とアイテムイベント（`turn/*`、`step/*`、`item/started`、`item/completed`）を配信します。

`item/delta` などの `ignorable` イベントも含めるには `--stream-deltas` を追加します。

```sh
mare --json --stream-deltas "Explain src/main.rs" | jq -r 'select(.type=="item/delta" and .data.kind=="text") | .data.delta'
```

### `lagged` と `dropped`

`{"type":"lagged","data":{"from_seq":A,"to_seq":B}}` は、読み手が mare より遅かったためにスキップされた連続シーケンス範囲を示し、`{"type":"dropped","data":{"count":N}}` は負荷や拒否により省略された未保持 `lossy` イベントの件数を示します。`delivery = "required"` のイベント（`turn/*`、`step/*`、`item/started`、`item/delta`、`item/completed`、`subagent/*`）は空きができるのを待ち、静かに破棄されることはありません。

### `turn.failed`

実行がエラーで終了すると、`settled` の直前に `{"type":"turn.failed","data":{"error":{"code":"...","message":"..."}}}`（任意の `data` を含む場合あり）が書かれます。

### `settled`

常に最後の行です。この後には何も続きません。

| フィールド | 意味 |
|---|---|
| `text` | アシスタントの最終テキスト。 |
| `usage` | トークン数。usage プラグインがコストを報告した場合は `cost` も。 |
| `error` | 実行が失敗した場合の構造化エラーオブジェクト `{code, message, data?}`。 |
| `exit_code` | プロセスの終了コード。 |
| `reason` | 実行が終了した理由。 |

`reason` と `exit_code` は次のように対応します ([終了コード](../reference/exit-codes.md) を参照)。

| `reason` | `exit_code` | 意味 |
|---|---|---|
| `completed` | 0 | ターンが完了した。 |
| `error` | 1 | プロバイダー、ツール、ランタイムのエラー、またはプロセスが SIGTERM か SIGHUP を受け取った。 |
| `usage` | 2 | 引数または設定が不正。実行は開始されなかった。 |
| `approval_unavailable` | 3 | 承認が「拒否して中止」(`abort`) で答えられ、ターンが停止された。誰も答えられなかった承認は拒否されるだけで、実行は `0` で完了することもある。 |
| `max_turns` | 4 | `--max-turns` に達した。 |
| `timeout` | 4 | `--timeout` に達した。 |
| `save_failed` | 5 | セッションの永続化に失敗した。未保存のレコードは `<state>/quarantine/` に隔離された。 |
| `interrupted` | 130 | SIGINT。 |

## 保証

- 最初のレコードは `session`、最後は `settled` です。ただし、stdout が閉じられた場合や出力が配信されなくなった場合 (後述) を除きます。
- レコードは 1 つのライターによって順番に書かれ、それぞれが、次のレコードが受け入れられる前にフラッシュされます。
- イベントの順序は、エージェントが生成した順序です。`lagged` のマーカーはシーケンス内の位置を保ちます。
- `settled.exit_code` はプロセスの終了コードと等しくなります。
- 読み手がいなくなった場合 (`mare --json ... | head`)、mare は誰のためでもないトークンを使い続けないよう、ターンを停止して 141 で終了します。
- `--timeout` は、エージェントの作業だけでなく出力の配信も対象とします。読み手が永遠にブロックした場合、ストリームは `settled` レコードなしで終了することがあり、stderr にメッセージが出力され、0 以外の終了コードで終了します。`settled` がない場合は、実行が失敗したものとして扱ってください。
- 最初の SIGINT はターンを取り消し、`interrupted` を持つ `settled` を書き出します。2 回目は `settled` なしで、ただちにコード 130 で終了します。

## jq

最終テキストを出力し、その後で mare のコードで終了します (Bash)。

```sh
mare --json "List the TODOs in src/" \
  | tee run.jsonl \
  | jq -r 'select(.type == "settled") | .data.text'
code=${PIPESTATUS[0]}
exit "$code"
```

ツール呼び出しが完了するたびに追跡します。

```sh
mare --json "Update the changelog" \
  | jq -r 'select(.type == "item/completed" and .data.item.type == "tool_call") | "tool: \(.data.item.name)"'
```

結果だけを表示します。

```sh
mare --json "Run the linter" | jq -c 'select(.type == "settled") | .data | {reason, exit_code, usage}'
```

## スクリプト

この Python スクリプトは、プロンプトを実行し、セッション ID と完了した各ツール呼び出しを出力して、実行自体の終了コードで終了します。`settled` なしでストリームが終わった場合は、はっきりと失敗します。

```python
import json
import subprocess
import sys

proc = subprocess.Popen(
    ["mare", "--json", *sys.argv[1:]],
    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True,
)
settled = None
for line in proc.stdout:
    record = json.loads(line)
    kind, data = record["type"], record["data"]
    if kind == "session":
        print("session", data.get("session_id"), data["cwd"])
    elif kind == "item/completed" and (data.get("item") or {}).get("type") == "tool_call":
        print("tool", data["item"].get("name"))
    elif kind == "settled":
        settled = data
code = proc.wait()
if settled is None:
    sys.exit(f"stream ended without a settled record (exit code {code})")
print(settled["reason"], settled["text"][:200])
sys.exit(settled["exit_code"])
```

`python3 run.py "Fix the failing test"` として実行します。双方向の制御 (承認、ホストツール、実行中のターンの誘導) には、代わりに [RPC](rpc.md) インターフェースを使ってください。
