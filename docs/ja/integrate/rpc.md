# RPC

`mare --rpc` は、stdin と stdout 上の JSON-RPC 2.0 で提供する mare の完全な双方向プログラマティックインターフェースです ([ヘッドレスモード](../use/headless.md)、[ACP](acp.md)、[Web リスナー](../use/web.md)と並ぶ統合手段)。プロセスを起動して行を読み書きできる任意のプログラムから、言語固有の SDK なしでセッション操作、ツールやフックの登録、承認への応答を行えます。このページでは、トランスポート、ハンドシェイク、メソッドのグループ、mare が送り返すコールバック、ピアソケットを扱います。

## サーバーを起動する

```sh
mare --rpc --no-session
```

`--mode rpc` も同じものです。すべてのメッセージは、1 行に 1 つの JSON オブジェクトです。stdout にはプロトコルメッセージだけが流れ、ログはログファイルに出力されます ([ファイル](../reference/files.md) を参照)。このプロセスは、stdin と stdout 上で 1 つのホストにサービスを提供します。ポートも認証もありません。パイプを保持しているのは親プロセスだけだからです。

stdin を閉じる (または読み取りエラーが起きる) と、セッションが終了し、アクティブなターンが取り消され、終了コード 0 で終了します。1 回限りのプロンプトには、代わりにヘッドレスモードを使ってください ([ヘッドレスモード](../use/headless.md) を参照)。

プロトコルの JSON Schema は、次のコマンドで出力できます。

```sh
mare rpc schema > mare-rpc.schema.json
```

出力はカタログであり、メッセージ全体の単一のスキーマではありません。最上位のキーは `envelope`、`methods`、`events`、`callbacks`、`notifications`、`client_notifications`、`errors`、`headless`、`ui_image_resource`、`ui_output_image_key` と `protocol_version` です (`events` はネイティブプロトコルイベントと組み込みサービスコントラクトで宣言されたイベントを統合します)。エントリ同士は `$ref` で参照し合うため、完全なカタログに対して解決してください。プロトコル全体のフィールド名とカタログキーは `snake_case` に統一されています（`protocol_version`、`callback_timeout_ms`、`turn_id`、`session_id`、`registration_id`、イベント、コントラクトサービスメソッド）。

## 最小のクライアント

この Python クライアントは、mare を起動し、ハンドシェイクを行い、イベントを購読してプロンプトを 1 つ送り、回答を待ちます。標準ライブラリだけを使います。そのまま実行できる CLI 引数付きサンプルは [`examples/rpc`](../../../examples/rpc/README.md)（英語）にあります。

```python
import json
import subprocess

proc = subprocess.Popen(
    ["mare", "--rpc", "--no-session"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
)
next_id = 0


def send(msg):
    msg["jsonrpc"] = "2.0"
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def handle(msg):
    """Notifications and callbacks that arrive while we wait for a response."""
    method = msg["method"]
    if method == "event":
        print("event", msg["params"]["type"])
    elif method == "approval.request":
        send({"id": msg["id"], "result": {"decision": "deny"}})
    elif "id" in msg:  # a callback we did not declare support for
        send({"id": msg["id"], "error": {"code": -32601, "message": "not supported"}})


def call(method, params=None):
    global next_id
    next_id += 1
    rid = next_id
    send({"id": rid, "method": method, "params": params or {}})
    for line in proc.stdout:
        msg = json.loads(line)
        if "method" in msg:
            handle(msg)
        elif msg.get("id") == rid:
            if "error" in msg:
                raise RuntimeError(msg["error"])
            return msg["result"]
    raise RuntimeError("mare closed stdout")


call("initialize", {
    "protocol_version": "3.0",
    "client": {"name": "demo", "version": "0.1"},
    "capabilities": {"approvals": True},
})
session = call("session.create")
sid = session["session_id"]
call("events.subscribe", {"session_id": sid})
started = call("agent.prompt", {"session_id": sid, "input": "Summarize this repository"})
result = call("agent.wait", {"session_id": sid, "turn_id": started["turn_id"]})
print(result.get("text", ""))
call("shutdown")
proc.wait()
```

mare からのリクエスト (コールバック) には `id` と `method` があり、クライアントはそのすべてに応答する必要があります。通知には `method` があり、`id` はありません。上記のループは、自分への応答を待つ間に、その両方を処理します。

## ハンドシェイク

`initialize` は最初のリクエストであり、接続につき 1 回だけ実行できます。接続で `initialize` が成功するまで、`shutdown` を除く他のリクエストは `-32600` で失敗します。後続の能力変更には `connection.set_capabilities` を使ってください。

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version":"3.0",
  "client":{"name":"my-host","version":"1.0"},
  "capabilities":{"callbacks":true,"ui":true,"approvals":true,"elicitation":false},
  "callback_timeout_ms":30000,
  "approval_timeout_ms":120000}}
```

| パラメーター | 意味 |
|---|---|
| `protocol_version` | 必須の `major.minor`。欠落、null、文字列以外、不正な値、異なるメジャーバージョンは `invalid_arguments` / `unsupported_version` で失敗します（修正して再試行できます）。 |
| `client` | `{name, version?}`。情報提供用。 |
| `capabilities` | 応答できるコールバック: `callbacks`（ツール、フック、サービス、プロバイダー、Web、レンダラー向けの統一 `callback.call`）、`ui`、`approvals`、`elicitation`。すべて既定は `false` です。mare は、宣言していないコールバックを送りません。 |
| `callback_timeout_ms` | 通常のコールバックの期限（ミリ秒）。既定は 30000、`1..=3600000` にクランプ。 |
| `approval_timeout_ms` | 人による承認の期限。指定しない場合、承認は期限なしで待ちます。 |

結果には、`protocol_version`、`server` (`{name, version}`)、`capabilities`、および起動時にセッションが開かれた場合は `session` スナップショット（`session_id` と `session_epoch` を含む）が含まれます。

| フィールド | 意味 |
|---|---|
| `methods` | この接続が呼び出せる、ホストから mare へのメソッド。 |
| `callbacks` | 宣言した機能のうち有効なもの。ピア接続では、`ui`、`approvals`、`elicitation` は `false` です。 |
| `session` | 起動時に開かれたセッションスナップショット（任意）。セッションスコープのメソッドには `session.session_id` を `session_id` として渡します。 |

マイナーバージョンでは、任意のフィールド、メソッド、イベントが追加されます。何かを削除または名前変更する場合は、メジャーバージョンが上がります。`3.0` に `2.x` 互換レイヤーはありません。

## ホストから mare へのメソッド

| グループ | メソッド |
|---|---|
| セッション | `session.create`、`session.load` `{session_id?, path?}`、`session.get` `{session_id}`、`session.fork` `{session_id, before_item_id?}`、`session.list`、`session.list_items` `{session_id}`、`session.add_record`、`session.list_records`、`session.archive`、`session.unarchive`、`session.delete`、`session.rewind`、`session.search`、`session.snapshot`、`session.close` |
| プロンプト | `agent.prompt` `{session_id, input}`、`agent.wait` `{session_id, turn_id?}`、`agent.steer` `{session_id, input}`、`agent.cancel` `{session_id}`、`agent.continue` `{session_id}`、`agent.get` `{session_id}`、`agent.list_messages` `{session_id}`、`agent.list_queue` / `agent.update_queued` / `agent.delete_queued`、`compact.run` `{session_id, instructions?}` |
| 状態 | `state.snapshot`、`state.compare_exchange`、`status.get`、`llm.list_models`、`flags.list`、`flags.get`、`workspace.get` `{cwd}`、`workspace.set_trust` `{cwd, trusted, save?}`、`permissions.get` `{session_id}`、`permissions.list` `{session_id}`、`permissions.set` `{session_id, name, confirm?}`、`commands.run` `{line, input?, session_id?}` |
| オプション | `options.get` `{session_id, keys?, describe?}`、`options.set` `{session_id, values}`、`config.describe` `{keys?}` |
| 拡張 | `events.list`、`events.subscribe` `{session_id?, scope?, session_epoch?, last_seq?, include?, exclude?}`、`baseline.read`、`tools.register` `{name, description, parameters, callbacks: ["execute"], side_effect?, scope?, session_id?, ...}`、`hooks.subscribe` `{event, session_id?, scope?, callbacks: ["handle"], ...}` |
| サービス | `services.list`、`services.register`、`registrations.release` `{registration_id}`。コントラクトのリモートメソッドはすべて直接 `service.method` として提供（`theme.*`、`llm.*`、`permissions.*`、`usage.get`、`web.*`、`ui.*`。`mare rpc schema` を参照） |
| 出力の復元 | `spill.list_backends`、`spill.save`、`spill.read`、`spill.delete` |
| ライフサイクル | `initialize`、`shutdown` |
| ピア | `peer.info`、`peer.message` ([ピアソケット](#ピアソケット) を参照) |

### プロンプト

`agent.prompt` は `{session_id, input, when_busy?, request_id?, output_schema?}` を受け取り、`{status, turn_id?, input_id?, duplicate?}` を返します。`input` は文字列またはコンテンツ配列です。ビジー時は既定でキューに入り、`when_busy: "reject"` で拒否できます。`agent.wait {session_id, turn_id}` で結果を取得するか、`event` 通知で追跡します。実行中の入力には `agent.steer`、中断には `agent.cancel` を使います。`request_id` はセッション内の冪等な再試行に使えます。

#### 安全な再送（`agent.prompt` と `agent.steer`）

エラー応答は、何も起きなかったことを必ずしも意味しません。`error.data.code` で次のように分けます。

- **コミット前に拒否:** キュー登録・ターン開始・メッセージ記録のいずれも起きておらず、`request_id` も記録されていません。原因を直して（または再接続して）同じリクエストを送り直せます: `parse_error`、`invalid_request`、`method_not_found`、`overloaded`、`denied`、`invalid_arguments`、`unknown_session`、`unsupported`、`request_id_conflict`、`busy`、`agent.rejected`、`agent.no_active_turn`、`agent.turn_mismatch`。
- **不確定:** 入力がすでにコミットされている可能性があります: `internal`、`cancelled`、`stale_session`、`unavailable`、`invalid_result`、`persist_failed`、`timed_out`、`disconnected`、その他のコード、および応答の前に接続が閉じた場合。

`request_id_conflict` は元のリクエストとその結果を変更しません。`busy` と `agent.rejected` は `agent.prompt` だけ、`agent.no_active_turn` と `agent.turn_mismatch` は `agent.steer` だけが返します。

不確定な失敗の後は、同じ `request_id` で同じリクエストを送り直してください。コミット済みの `agent.prompt` は元の `turn_id` または `input_id` を、コミット済みの `agent.steer` は元の `input_id` を `duplicate: true` 付きで返し、二重に実行されません。フロントエンドが入力を別の宛先（たとえばフォーカス中のサイド会話）へ回した場合も同じで、最初のリクエストの処理中に届いた再送はその結果を待ちます。`request_id` を付けていない場合は、送り直す前に `agent.list_queue` と `agent.list_messages` を確認してください。

### オプション

`options.get` は、`/config` と同様に、機密の値 (認証情報、プロバイダーのキーとヘッダー) を伏せます。`describe: true` を指定すると、キーごとに `{effective, default?, source?, description?, type?, deprecated?, sensitive, redacted}` を返します。`options.set {session_id: "...", values: {model: "...", effort: "..."}}` は、`mare.o.<name>` を設定することの RPC 版です。プライマリ接続の既定セッションで確定した `options.set` の書き込みは、`session.create` 時のみランタイム既定値として再適用されます（`cli` 設定層には昇格しません）。`session.load` と `session.fork` は `options.set` の既定値を再適用せず、対象セッションに記録された状態を復元します。[オプション](../configure/options.md) を参照してください。

### ホストツール

```json
{"jsonrpc":"2.0","id":2,"method":"tools.register","params":{
  "name":"lookup",
  "description":"Look up a ticket",
  "side_effect":"read",
  "parameters":{"type":"object","properties":{"ticket_id":{"type":"string"}},"required":["ticket_id"]},
  "callbacks":["execute"]}}
```

1 回の `tools.register` 呼び出しで、この接続が所有するツールを 1 つ登録して `{registration_id}` を返します（単一セッションに向ける場合は任意の `scope: "all"|"session"` と `session_id` を指定します）。`name`・`description`・`parameters`・`callbacks: ["execute"]` は必須で、副作用は `side_effect`（`"read"`・`"write"`・`"external"`）で分類します。リモートツールは常にローカルサンドボックス外で非制限実行されます。`capabilities.callbacks: true` が必要で、mare は `callback.call`（`callback: "execute"`、`arguments: {args, ctx: {tool_call_id?, idempotency_key?, cwd?}}`）を呼び出して文字列または `{content, is_error?, terminate?, details?}` を期待します。実行中の途中経過は、保留中 `callback.call` の JSON-RPC リクエスト `id` を使って `callback.update {id, partial}` 通知で送ります。解除には `registrations.release {registration_id}`（戻り値 `{}`）を使い、接続が切断された際も登録は削除されます。

### フック

`hooks.subscribe {event, prepend?, timeout_ms?, scope?, session_id?, callbacks: ["handle"]}` の購読には `capabilities.callbacks: true` が必要で、`{registration_id}` を返します（`registrations.release` で解除します）。`prepend: true` は先に登録されたリスナーより前に置きます。`hooks.subscribe` に `mode` パラメーターは渡しません。カーネルが `event` からディスパッチモード（`agent/queue` と `status/changed` は `emit`、`request/prepare` は `serial`、`request/build`・`tool/result`・`tool/rewrite` は `waterfall`、`request/ready`・`tool/before`・`tool/approval`・`net/approval` などの判断点は `policy`）を判定し、`callback.call`（`callback: "handle"`、`arguments: {event, mode, args}`）で配送します。

### サービスと出力の復元

`capabilities.callbacks: true` を宣言したホストは、`services.register {name, methods, scope?, session_id?}` でサービスを登録して `{registration_id, name}` を受け取れます（`registrations.release` で解除）。サービス名は小文字英数字・`_`・`-` から成り、プロトコルのルート名とは重複できません。すでに契約があり全メソッドがワイヤー越しに提供できるサービスでは、`parameters`・`result`・`order`・`expose`・`access`・`scope` を省略する（または完全一致で繰り返す）必要があります。新しいサービスでは各メソッドに `parameters`・`result` と、任意の `order`・`doc`・`optional`・`expose`（既定 `"both"`、または `"lua"`）・`access`（既定 `"primary"`、または `"peer"`・`"peer_default"`）・`scope`（既定 `"host"`、または `"workspace"`・`"session"`）を宣言します。`services.list` は可視なリモート契約メソッドを一覧し、呼び出しは直接 `service.method` で行います。

復元可能なツール出力はセッションごとに管理され、`spill.list_backends {}`、`spill.save {session_id, content|data_base64, backend?}`（`{spill_id, size}` を返却。base64 はデコード後最大 8 MiB）、`spill.read {session_id, spill_id, encoding?, offset?, limit?}`（`encoding: "text"|"base64"`、`{content|data_base64, next_offset?, size?}` を返却）、`spill.delete {session_id, spill_id}` で操作します。`spill_id` は所有セッションに紐づき、ホストのファイルシステムパスを受け取ったり露出したりすることはありません。

## mare からのコールバック

mare はこれらをリクエストとして送ります。それぞれに、結果またはエラーで応答してください。

| コールバック | パラメーター | 結果 |
|---|---|---|
| `callback.call` | `{registration_id, callback, session_id?, turn_id?, tool_call_id?, caller_id?, arguments}` | 登録時に宣言された戻り値（ツールは `{content, is_error?, terminate?}`。バッチのすべてが `terminate: true` なら `agent/turn-stopping` が続行しない限り停止。フックは `null`・`{"next":true}`・`{"value":...}`、サービス/プロバイダー/Web/レンダラーは宣言済み JSON 結果） |
| `approval.request` | `{session_id, turn_id?, kind: "tool"\|"network", approval_id?, tool_call_id?, tool?, arguments, command?, diff?, host?, cwd, reason?, side_effect?, unconfined, proposed_amendment?, requested_permissions?, scope?}` | `{decision, permissions?, scope?, amendment?, reason?}` |
| `mcp.elicitation` | `{session_id, turn_id?, server, request_id?, mode?, message, requested_schema?, url?, elicitation_id?, _meta?}` | `{action, content?}` |
| `ui.select` | `{title, body?, items: [{label, detail?}], selected?}` | `{index}` (`null` = 取り消し) |
| `ui.input` | `{title, placeholder?, secret?}` | `{text}` (`null` = 取り消し) |
| `ui.confirm` | `{prompt, default?}` | `{confirmed}` |
| `ui.editor` | `{title, prefill}` | `{text}` (`null` = 取り消し) |
| `ui.questions` | `{questions}` | `{answers}` |

長時間実行される `callback.call`（ホストツールや登録プロバイダーのストリームなど）では、クライアントから mare へ `callback.update` 通知 `{id, partial}` を送ります。`id` は実行中の `callback.call` リクエスト `id`、`partial` は JSON 値（ツールでは `{"output": "..."}` のようなオブジェクト）です。

### 承認

`approval.request` は、`capabilities.approvals` を宣言したプライマリ接続にのみ送られます。`decision` の値は次のとおりです。

| 判断 | 効果 |
|---|---|
| `allow` | 1 回だけ実行します。 |
| `allow_session` | 実行します。permissions プラグインがこの許可をセッションの間記憶します。 |
| `allow_amend` | 実行し、`amendment` を permissions プラグインに渡して、ルールとして永続化させます。 |
| `deny` | 拒否します。 |
| `abort` | 拒否して、現在のターンを終了します。 |

応答がない、応答が不正、タイムアウト、切断はいずれも `deny` として扱われます。mare が知らない判断の値は拒否され、`deny` として扱われます。ネットワークアクセスには明示的な肯定の回答が必要で、棄権したリスナーは許可を与えません。

### フックの応答

`waterfall` では、続行に `{"next":true}`、値を書き換えて次のリスナーに渡す場合は `{"next":true,"value":...}`、短絡に `{"value":...}` を返します。`{}` は `invalid_result` です。`tool/rewrite` の `value` は `arguments` を含む呼び出しオブジェクトです。例: `{"value":{"id":"call-1","name":"tool","arguments":{"key":"new"}}}`。`policy` 判断は `{"value":{"decision":"allow"}}` のように返します（`ask`、`deny` や任意の投票フィールドも可）。`value` がない場合は棄権、未知の判断やフィールド、不正な応答、タイムアウト、切断、`capabilities.callbacks` の撤回は deny です。最初の deny でディスパッチが停止します。

### 取り消しとタイムアウト

mare はコールバックがもうアクティブでなくなると、`$/cancel_request {id}` を送ります（クライアントも実行中の協調リクエストを取り消すために `$/cancel_request {id}` を送れます）。その UI を閉じ、以降の応答は無視してください。通常のコールバックは `callback_timeout_ms` 後にタイムアウトします。タイムアウトまたは切断時の結果は次のとおりです。

| コールバックの種類 | 結果 |
|---|---|
| ホストツール | ツールエラー。 |
| ポリシーフックと `approval.request` | `deny`。 |
| `serial` と `waterfall` のフック | エラーはディスパッチに伝播します。 |
| UI 呼び出し | 取り消し。 |

## 通知

| 通知 | パラメーター | 備考 |
|---|---|---|
| `event` | `{type, data, session_id?, session_epoch?, seq, turn_id?}` | `turn_id` は `agent.prompt` の結果と一致します。`seq` は `(session_id, session_epoch)` ごとに連続増加します。 |
| `snapshot` | `{session_id?, session_epoch, seq, baseline_id, retained, parts}` | `mode = "baseline"` の `events.subscribe` 応答直後、またはカーソル追い出し時の `lagged` 直後に送られます。 |
| `lagged` | `{from_seq, to_seq}` | 新しい `snapshot` を再作成する前の追い出し範囲。 |
| `dropped` | `{count}` | ストリームから省略された未保持 `lossy` または拒否イベントの件数。 |
| `event` (`status/changed`) | `{type: "status/changed", data: {state, message, session, seq}}` | `state` は `idle`、`working`、`blocked` のいずれか。単独通知ではなく、`events.subscribe` で選択された場合にのみ `event` 通知として配信されます。最後に見たものより小さい `seq` は無視してください。 |
| `fs/changed` | `{watch_id, changed_paths, truncated?}` | 有効な `fs.watch` 購読に対するポリシー適用済みのファイル変更通知（8192 件超で `truncated: true`）。 |
| `ui.notify` | `{text, ...}` | ユーザー向けのメッセージ。 |
| `ui/select-updated`、`ui/select-closed`、`ui/component-updated` | [RPC リファレンス](../reference/rpc.md) を参照 | 開いている `ui.select` モーダルやアタッチ済み UI コンポーネントの差分更新。 |
| `stream.failed` | `{error}` | イベントストリームが不完全で、接続は閉じられます。 |
| `$/cancel_request` | `{id}` | コールバックまたは協調リクエストが取り消されました。 |
| `peer.message_status` | 後述 | ピアソケットのみ。 |

既定では、`event` は `mare --json` と同じ公開イベント（サービス契約で `expose = "both"` として宣言されたイベントを含む）を運びます ([JSON イベントストリーム](json-stream.md) を参照)。`events.subscribe {session_id: "...", include: [...]}`（プライマリ接続では `{scope: "all", include: [...]}` も可）は、名前または `prefix*` で、さらに多くの JSON カーネルイベントを購読します。プロセスローカルのイベントとプロバイダー固有のイベントが配信されることはありません。

### 再開とベースライン

`events.subscribe` が `mode = "baseline"` を返したとき（または `lagged` に続いて新しい `snapshot` が `seq = S` で届いたとき）、`snapshot.retained` を適用し、次の順で状態を復元します。

1. ベースラインのページ取得中に届く **S 以降のイベント（`seq >= S + 1`）をバッファリング**します。
2. `snapshot.parts`（`"entries"`、`"active_items"`）の各パートを、`next_cursor` が返らなくなるまで `baseline.read { baseline_id, part, cursor? }` でページ取得します。60 秒アイドル、エポック変更、再購読、完了後などで JSON-RPC エラー `error.data.code = "baseline.stale"` が返った場合は、再購読して新しいベースラインを取得します（無効なパート名は `baseline.unknown_part`、サイズ超過項目は `baseline.item_too_large` を返します）。
3. ベースラインを読み終えてから **バッファリングしたイベントを順番に適用**し、`item/delta.offset`（各コンテンツブロックの `offset`）がローカル長と一致することを検証します。一致しない場合は `last_seq` なしで `events.subscribe` を呼び直して再構築してください。

## エラー

すべてのエラーは `error.data.code` を持ちます。これは Lua のエラー値と共通の語彙の名前で、`error.data.data` にそのコードの追加データが入ります。数値は分類だけです。JSON-RPC 標準の 5 つ、取消の `-32800`、それ以外はすべて `-32000` です。サービス独自のコードは `<サービス>.<名前>` で、サービスの契約が宣言し `services.list` に表示されます。宣言されていない、またはエンコードできないエラーは `internal` で、メッセージには長さの上限があります。全一覧は [RPC リファレンス](../reference/rpc.md#エラーコード) にあります。

| `data.code` | 数値 | 意味 |
|---|---|---|
| `parse_error` | `-32700` | パースエラー。 |
| `invalid_request` | `-32600` | 不正なリクエスト。`initialize` 前の呼び出し、実行中 id の重複、サイズ上限を超える行も含みます。 |
| `method_not_found` | `-32601` | 不明なメソッド、または peer ソケットで許可されていないメソッド。 |
| `invalid_arguments` | `-32602` | 不正なパラメーター。 |
| `internal` | `-32603` | 内部エラー。メッセージには上限があり、元のエラー値は含みません。 |
| `cancelled` | `-32800` | 取消 (`$/cancel_request`、所有者の解放、終了)。 |
| `overloaded` | `-32000` | 実行中のリクエストが 256 を超えた。 |
| `busy` | `-32000` | 接続ごとの上限、または peer の受信キューが満杯。 |
| `unsupported_version` | `-32000` | 非対応のプロトコルバージョン。 |
| `request_id_conflict` | `-32000` | `agent.prompt` の `request_id` が異なる内容で再利用された。 |
| `unsupported` | `-32000` | 非対応の機能またはメディア入力。`data.data.capability` が対象を示します。 |
| `timed_out`、`disconnected`、`denied` | `-32000` | サービス呼び出しのタイムアウト、コールバックの切断、この接続には許可されない。構造化コードのないリモート実装のエラーは `internal` です。 |

## 制限

- リクエストの行は 64 MiB までです。それより長い行はスキップされ、`-32600` で応答されます。
- 接続ごとに、選択されたイベントは最大 4096 件、シリアライズされたイベントレコードは最大 64 MiB まで受け入れられ、出力キューは最大 4096 行と 64 MiB を保持します。予算を超えたレコードや行は、その接続を失敗させます。
- 選択されたイベントは、受け入れられた時点で確定します。`events.subscribe` の変更は、以降のイベントに適用されます。
- レスポンスは、すでに受け入れられたイベントの後にキューイングされます。
- イベントをエンコードまたは配信できない場合、出力がまだ書き込み可能であれば、mare は `stream.failed` を送ります。書き込めない場合は、EOF または途中で切れた行が合図になります。
- stdout の失敗やクローズは、アクティブなターンを取り消します。ピアの失敗は、共有しているセッションを取り消しません。

## ピアソケット

送信用のピアクライアントは Lua の `mare.peers` から作成します（`peers` プラグイン契約が所有し、アイドル待機には `status/changed` を使用します）。各メッセージの `from.session_id` には送信時のセッション ID が入ります。

対話型セッションは `<state>/peers/<pid>.sock` でもリッスンします。`--rpc` またはヘッドレスの実行も、`--peer` を付けて起動するとリッスンします。同じユーザーの他のプログラムが、これを通じて通信できます。ソケットは、このプロトコルの最小権限のサブセットを扱います。

- ソケットは、モード 0700 のディレクトリ内のモード 0600 のファイルです（ディレクトリがシンボリックリンクまたは他ユーザー所有の場合はリッスンしません）。接続時に mare は相手のユーザー ID を検証し（Linux `SO_PEERCRED`、macOS `getpeereid`）、別ユーザーを切断します。Windows では DACL が現在ユーザーの SID と SYSTEM のみを許可する（リモートクライアント拒否）名前付きパイプを使い、ランダムな 128 ビット名をソケットパスのエンドポイントファイルに書いてクライアント側で検証します。
- ピアが呼び出せるのは、`initialize`、`peer.info`、`status.get`、`peer.message`、`events.subscribe`（`status/changed` 明示指定のみ）、`shutdown` (その接続だけを終了) です。それ以外は `-32601`（またはより広い `events.subscribe` フィルタでは `denied`）で失敗します。
- 許可を広げるには、`peer` オプションに正確なメソッド名（イベントストリーム全体なら `"events.subscribe"`）を列挙します。ワイルドカードやプレフィックス指定は受け付けません。

```lua
mare.setup {
  peer = { name = "main", allow = { "agent.prompt", "tools.register", "events.subscribe" } },
}
```

`peer = false` でソケットを無効にします。プライマリ専用メソッド（`options.set`、`commands.run`、`hooks.subscribe`、およびその他の `access = "primary"` 操作）はピアに付与できません。ピアが UI や承認を乗っ取ることはありません。ピアソケットのリクエスト行は 1 MiB までに制限され、ユーザーの応答を待てる受信メッセージは同時に 32 件までです。

| メソッド | パラメーター | 結果 |
|---|---|---|
| `peer.info` | `{}` | `{name, pid, cwd, version, allowed}` |
| `status.get` | `{}` | 統合された状態。`status/changed` と同じ形。 |
| `peer.message` | `{from: {name?, pid?, cwd?, session_id?}, text, queue_only?, policy?}` | `{message_id, status}` |

送信者が `from` と `policy` で自分について述べた内容は、受信側のユーザーに表示されるだけで、何の権限も与えません。`text` が空の場合は `-32602` で失敗し、ソケットが接続元プロセスの認証情報を返す環境ではそれに一致しない `from.pid` や登録済み `from.name` も `-32602` で拒否されます。メッセージは、受信側のユーザーが受け入れるか拒否するまで保留され、その結果は `delivered` または `rejected` を持つ `peer.message_status {message_id, status}` 通知として届きます（UI がない場合は拒否されます）。配信されたメッセージは、他のセッションからのものであることを示すデータ境界の中で、フォローアップとしてエージェントに届きます。

実行中のセッションは `<state>/peers/<pid>.json` に `{name, pid, cwd, socket, mode, started, status, plugins?}` を登録します。`plugins` にはプラグインが `mare.peers.publish(namespace, fields)` で公開した任意の名前空間が入ります（最大 200 ms ごとに更新され、終了時に削除、一覧取得時に終了済みプロセスの項目を整理します）。

### シェルからメッセージを送る

```sh
mare peer send --to main "tests are green on the integration branch"
printf 'see the diff in /tmp/x.patch' | mare peer send --to main
```

`--to` には、ソケットのパスまたは登録されたセッション名を指定します。省略すると、mare は `$MARE_PEER_SOCKET` を使います。これは、mare のセッションが起動したツールとフックが継承します。コマンドは結果のステータス (`held`、`delivered`、`handled`) を出力し、受信側がメッセージを受け入れた場合は 0 で終了します。テキストは引数から、引数がない場合は stdin から取得されます。

プラグインマネージャーはプロセス内の Lua イベント `plugins/completed { action, name?, status }` を発行します。内容は[サービスとイベント](../extend/services-and-events.md)を参照してください。
