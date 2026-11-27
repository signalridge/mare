# サービスとイベント

プラグイン同士がやり取りする方法を説明します。サービスの提供と置き換え、イベントのディスパッチ方式、ポリシーイベントと書き換えイベント、組み込みサービスと主なイベントを扱います。

## サービス

### サービスを使う

必要なサービスを `inject` に指定します。すると `ctx.<name>` または `ctx:get(name)` で使えるようになります。メソッドはコロンで呼び出します。

```lua
return {
  name = "show-model",
  inject = { "llm", "commands" },
  apply = function(ctx)
    ctx.commands:register("which-model", {
      description = "Show the current model",
      run = function()
        local m = ctx.llm:current()
        ui.notify(m and m.ref or "no model")
      end,
    })
  end,
}
```

`ctx.llm` は読むたびに現在の提供元に届きます。その提供元が再読み込みされたり置き換えられたりすると、それを inject しているプラグインも一緒に再起動します。

なくても動作できるサービスは `inject` に入れません。`ctx:get(name)` はそのサービスを返し、提供元がない間は `nil` を返します。そこに何かを登録するには `ctx:inject({ name }, fn)` を使います。サービスに提供元がある間 `fn(ctx)` が実行され、その `ctx` を通じて登録したものはサービスがなくなると取り下げられ、次の提供元が現れると `fn` が再び実行されます。組み込みの `profile` プラグインは、この方法で `/profile` を追加しています。

```lua
return {
  name = "show-model",
  inject = { "llm" },
  apply = function(ctx)
    -- /which-model exists while the commands service does
    ctx:inject({ "commands" }, function(sub)
      sub.commands:register("which-model", {
        description = "Show the current model",
        run = function()
          local m = ctx.llm:current()
          ui.notify(m and m.ref or "no model")
        end,
      })
    end)
  end,
}
```

### サービスを提供する

`provide` に名前を、`contracts` にメソッドを宣言し、`apply` で `mare.Service` クラスとして提供します。コンストラクターがサービス名を指定し、コントラクトの各メソッドがクラスのメソッドになり、`ctx:plugin(Class)` がそれを提供します。メソッドの中では `self.ctx` が呼び出し元プラグインの `ctx` です。そのため、メソッドが `self.ctx` の effect として登録したものは呼び出し元に属し、呼び出し元がなくなると取り下げられます。組み込みの `commands` サービスは、この方法でコマンドを登録しています。

```lua
-- ~/.config/mare/plugins/greeter.lua
return {
  name = "greeter",
  provide = { "greeter" },
  contracts = { greeter = { required = { "greet", "remember" } } },
  apply = function(ctx)
    local known = {}
    local Greeter = require("cordis").class("Greeter", mare.Service)
    function Greeter:constructor(c)
      Greeter.super.constructor(self, c, "greeter")
    end
    function Greeter:greet(who)
      return known[who] and ("hello again, " .. who) or ("hello " .. who)
    end
    -- remembered until the calling plugin goes away
    function Greeter:remember(who)
      return self.ctx:effect(function()
        known[who] = true
        return function() known[who] = nil end
      end)
    end
    ctx:plugin(Greeter)
  end,
}
```

```lua
-- ~/.config/mare/plugins/greeter-user.lua
return {
  name = "greeter-user",
  inject = { "greeter" },
  apply = function(ctx)
    ctx.greeter:remember("world")
    ui.notify(ctx.greeter:greet("world"))   -- hello again, world
  end,
}
```

サービスは公開される前にコントラクトと照合されます。コントラクトがない場合や必須のメソッドが欠けている場合は拒否されます。サービスは提供されず、理由はそのプラグインの問題として記録されます。組み込みがすでに定義している名前のコントラクトを、再宣言することはできません。

コントラクトはメソッドを 1 つずつ記述することもでき、そうすると Lua の外の呼び出し元にメソッドが公開されます。`required` の一覧の代わりに `methods` を書き、メソッドごとに `doc`、`params` と `result` の JSON Schema、位置引数の順序 `order`、`expose = "both"` を指定します。カーネルは引数と結果をスキーマで検査し、コントラクトから JSON アダプタを派生します。そのメソッドは RPC メソッド `service.method` になります。

### 組み込みサービスを置き換える

1 つのサービスの提供元は 1 つです。同じ名前を `provide` に宣言した 2 つ目のプラグインは実行前に拒否され、エラーは両方のファイルを示し、最初のものがサービスを持ち続けます。フォールバックはありません。提供元がなくなると、どれかのプラグインが再び提供するまでサービスは存在しません。

組み込みサービスを置き換えるには、`init.lua` で `mare.plugin("status", { enabled = false })` としてそのプラグインをオフにし (この名前のすべてのソースがオフになり、自分のものも含まれます)、自分のプラグインからサービスを提供して、コントラクトの必須メソッドをすべて実装します。

```lua
-- ~/.config/mare/plugins/my-status.lua
return {
  name = "my-status",
  provide = { "status" },
  apply = function(ctx)
    local Status = require("cordis").class("Status", mare.Service)
    function Status:constructor(c)
      Status.super.constructor(self, c, "status")
    end
    function Status:get()
      return { state = "idle", message = "", cwd = mare.cwd, pid = mare.pid, seq = 0, session = {} }
    end
    ctx:plugin(Status)
  end,
}
```

あるいは、自分のプラグインに組み込みプラグインと同じ名前を付けて `override = true` を設定します。その場合、置き換えたプラグインのサービスに対する責任も引き継ぎます。基幹サービスの置き換えが起動に失敗した場合、サービスなしで動作するのではなく、リクエストがブロックされます。

サービスを置き換えずに動作を変えるには、イベントリスナーを使うか、各部分の API で 1 か所だけを変更することをお勧めします。`ctx.tools:shadow(name, def)` は、変更したツールのコピーを再登録し、UI コンポーネントはラップできます ([外観](../configure/ui.md) を参照)。

### 組み込みサービス

| サービス | 提供元 | 役割 |
|---|---|---|
| `agent` | `agent` | エージェントループを実行します: `prompt` (`follow_up` 配信キュー対応)、`steer`、`cancel`、`activity` (`agent:activity().busy`)、`messages`。 |
| `llm` | `llm` | プロバイダーのレジストリ、モデルカタログ、ログイン、パラメーター: `current`、`set_model`、`resolve`、`recent`、`cycle_param`、`login`、`logout`、`auth_keys`。 |
| `session` | `session` | ディスク上のセッションツリー: `info`、`create`、`load`、`fork`、`close`、`list`、`entries`、`record`、`records`。 |
| `context` | `context` | システムプロンプトの組み立て: `section`、`suppress`、`variable`、`system_prompt`、`skills`、`templates`。 |
| `compact` | `compact` | 段階的なコンテキストのコンパクション: `run`、`register_stage`、`measure`、`threshold`。 |
| `tools` | `tools` | ツールのレジストリと、保護された唯一の実行経路: `register`、`shadow`、`command`、`get`、`list`、`execute`、`disable`、`enable`。 |
| `permissions` | `permissions` | サンドボックスと承認のプリセット: `preset`、`request`、`grants`。 |
| `approvals` | `approvals` | 独立した承認ポリシーとレビュアー: `policy`、`set`、`request`、`register_reviewer`。 |
| `commands` | `commands` | スラッシュコマンド: `register` (`{ alias_of = target }` で別名を登録)、`run`、`list`、`get`。 |
| `ui` | `ui` | プラグイン単位のインターフェース部品: `widget`、`status`、`invalidate`、`key`、`action`、`renderer`、`style`。 |
| `theme` | `theme-default` | テーマ: `list`、`get`、`register`、`select`、`preview`、`cancel_preview`、`register_override`。 |
| `jobs` | `jobs` | バックグラウンドプロセス: `start`、`track`、`get`、`list`、`background`、`cancel` (モデル向けツール名は `job_kill`)、`read`。 |
| `usage` | `usage` | トークンとコストの合計: `totals`、`context_tokens`、`cost_text`、`rate_limits`。 |
| `status` | `status` | 統合されたエージェントの状態: `get`。 |
| `mcp` | `mcp` | MCP サーバー: `status`。 |
| `changes` | `changes` | ターンごとのファイル変更と巻き戻し: `turns`、`diffs`、`rewind`。 |
| `review` | `review` | コードレビュー: `run`、`viewer`。 |
| `images`、`copy`、`plugins`、`update-check`、`spill` | 同名のプラグイン | 画像表示、クリップボードへのコピー、プラグインカタログ UI、アップデート確認、結果の保存。 |

完全なシグネチャは、ランタイムディレクトリ (`mare runtime`) の Lua 型ファイルにあります。[Lua API](lua-api.md) のページは、これらから生成されています。

### セッションプロバイダーのコントラクト

`session` サービスはライフサイクルのオーケストレーションとセッションの永続化を分離しており、カスタム `session` プロバイダーが TUI・CLI・マルチセッション RPC/Web/ACP ホストで一貫して動作するようになっています。

- **ホスト提供のライフサイクルメソッド**: `session.create`・`session.load`・`session.fork`・`session.close` はホストのセッションオーケストレーターが提供します (コントラクト上 `provider = "host"`)。プロジェクト境界チェック・`request_id` 冪等レシート・`expected_epoch` 検証・ランタイム管理を Lua・スラッシュコマンド・RPC 呼び出し元すべてで共通化します。カスタム `session` プロバイダーはこれら 4 メソッドを実装してはなりません。
- **マルチランタイムホストでの起動引数 (`mare.args`)**: マルチランタイムホスト (`mare --rpc`・Web・ACP) がセッションを作成・読み込み・フォークする際、オーケストレーターは専用ランタイムを起動し `app/ready` 前に `mare.args` を設定します。
  - `session_id`: 新規作成・フォーク時に採用する事前割り当てセッション ID。
  - `resume`: 起動時に読み込む既存セッションのパスまたはセレクター。
  - `fork_session` (真偽値、任意の `fork_before_entry` と併用): 復元元をその場で開く代わりにフォークする。
  - `name`: 初期セッション名。
  - `fork_images` (`prepare_fork` がメモリ上の画像を返す場合にオーケストレーターが渡す任意マップ): フォーク用の検証済み画像ペイロード。プロバイダーは起動時に `mare.args.fork_images` を読み取った後クリアする。
  - 起動時の読み込みやフォークに失敗した場合は、`session:info().resume_error` にエラーを記録してオーケストレーターがランタイムを安全に終了・ロールバックできるようにします。
- **Lua 専用プロバイダーフック (`expose = "lua"`)**:
  - `session.switch(opts)`: **シングルランタイム**ホスト上でオーケストレーターから呼ばれ、現在のアクティブセッションをその場で切り替えます (`opts.kind = "new" | "load" | "fork"`、任意の `cwd`・`name`・`session_id`・`path`・`before_item_id`・`id`)。切り替え後のライブセッションスナップショットを返し、`ctx.kernel:begin_session_switch` / `commit` によりホスト ID・セッション限定信頼の失効・実行中呼び出しガードを原子的に同期します (`handle:commit(session_id)` は実行中のセッションエクスポート呼び出しを自動的にコミット済みとしてマークします)。カスタムプロバイダーが `switch` を省略した場合、シングルランタイムのライフサイクル呼び出しは `unavailable` を返します。
  - `session.prepare_fork()`: マルチランタイムのフォーク前に稼働中のソースランタイム上でオーケストレーターから呼ばれ、サイドリクエストの完了待ちとジャーナルのフラッシュを行い `{ path, images? }` (未永続化時は `nil`) を返します。未実装の場合、稼働中ソースからのマルチランタイムフォークは `unavailable` で失敗します。
- **ランタイム容量ごとの `session.close` の挙動**:
  - **マルチランタイムホスト**: 対象セッションのランタイムを終了・アンロードし、ロックを解放して `{}` を返します。
  - **シングルランタイムホスト**: バインド済みセッション (完全一致 ID・パス・4 文字以上の一意なプレフィックス) を閉じると、`session.switch({ kind = "new" })` によりホスト終了やプラグイン/MCP 再起動を伴わずその場で新しい空セッションへ切り替えて旧ロックを解放し、`{}` を返します。未読み込みセッションへの close は `{}` のノーオペレーションとなり、曖昧なプレフィックスは `invalid_arguments`、不明な ID は `unknown_session`、古い `expected_epoch` は `stale_session`、ターン実行中は `busy` を返します。

## イベント

### リッスン

`ctx:on(event, fn, opts)` で、プラグインが存続する間イベントを購読します。リスナーは登録順に実行され、`opts.prepend = true` で先頭に置けます。購読を解除する関数を返します。`ctx:once(event, fn)` は最初の呼び出しの後に取り除かれます。`init.lua` では同じ種類のリスナーである `mare.on(event, fn)` を使います。`tool/before` のような決定点に投票するには `mare.policy.voter(point, fn, opts)` を使います。

```lua
ctx:on("model/changed", function(ev)
  ui.notify("model is now " .. ev.ref)
end)
```

waterfall のリスナーは `(payload, ev)` を受け取ります。`ev.next(payload)` (または `ev(payload)`) を呼ぶと、その値で後続リスナーを実行して (`ev.next()` は現在の値をそのまま渡します) 結果を返します。リスナー自身の戻り値が最終結果になり、`ev.next` を呼ばない場合はそこでチェーンが終わります。

```lua
ctx:on("context/system", function(payload, ev)
  payload.text = payload.text .. "\nAnswer in English."
  return ev.next(payload)
end)
```

### イベントのディスパッチ方法

ディスパッチ方式は、発行する側が選びます。それによって、あなたの戻り値の意味が決まります。

| 方式 | 発行側の呼び出し | リスナー | あなたの戻り値 |
|---|---|---|---|
| Notify | `ctx.bus:emit` | すべて、順番に。 | 無視されます。エラーは報告され、残りは引き続き実行されます。 |
| Checked notify | `ctx.bus:emit_checked` | すべて、順番に。 | エラーでディスパッチが止まり、発行側に届きます。 |
| Bail | `ctx.bus:bail` | `nil` 以外を返すものが現れるまで、順番に。 | 最初の `nil` 以外の値が結果になります。 |
| Waterfall | `ctx.bus:waterfall`、`ctx.bus:waterfall_checked` | 最初に登録されたものが最も外側で、それぞれが `next` で残りを実行します。 | `next` に渡した値が先へ進みます。最も外側のリスナーの戻り値が結果です。`waterfall_checked` は、エラーを発生させたリスナーのエラーを発行側に渡します。 |
| Policy | `ctx.policy:decide(point, args, opts)` | 投票者、順番に。 | 投票。後述。`opts.require_explicit_approval` を指定すると、投票者がいない場合やすべて `nil` の場合は `unavailable` の拒否になります。 |
| Rewrite | `ctx.policy:rewrite(point, call)` | 書き換え者、順番に。 | 引数を保持するには `nil`、または新しい引数のテーブル。成功時は書き換え後の引数テーブルを直接返し、失敗時は `policy.rewrite_refused` を送出する。 |

プラグインも、同じ呼び出しで独自のイベントを発行できます (`inject = { "bus" }`)。発行するすべてのイベントはコントラクト (`contracts.<root>.events`) で宣言する必要があります。`name/verb` 形式の名前を使い、プラグイン内で一意にしてください。宣言された各イベントの `expose` は 3 段階の可視性を指定します：

- `expose = "process"`：厳密にプロセス内専用です。外部イベントログには入らず、`include` の設定に関わらず RPC から観測・フックできません。
- `expose = "lua"`（既定）：プロセス内の Lua プラグインから見え、既定の公開 RPC/JSON イベントストリームには流れません。主 RPC 接続の `include` / フック購読で選択できるため、`"lua"` は機密性の境界ではありません。
- `expose = "both"`：プロセス内の Lua と既定の公開 RPC/JSON イベントストリームの両方に配信されます。

別のローカルチャネルはありません。

### ポリシーイベント

決定点は、プラグインに何かを許可するか拒否するかを問います。フェイルクローズ (失敗時は拒否) です。

- 投票者は、`nil` (異議なし)、`"allow"`、`"deny"`、`"ask"`、またはテーブル `{ decision = "deny", reason = "..." }` を返します。未知のフィールドや判断は投票を無効にし、無効な投票は deny です。
- 最初の `deny` でディスパッチが終了し、それが最終結果です。それ以外では、`ask` が `allow` に優先します。
- エラーを発生させた投票者、またはタイムアウト内に答えない投票者は `deny` と数えられます。既定のタイムアウトは 30 秒です。変更するには投票者のオプションに `timeout_ms` を設定します。`0` は制限をなくし、人の応答を待つ投票者にはこれが必要です。
- `slot = "name"` は、状態を持つチェックに 1 つの判断スロットを与えます。そのスロットで最初に登録された投票者だけが実行されます。
- 投票が何かを書き換えることはありません。引数は投票の前に凍結されます。

```lua
ctx.policy:voter("tool/before", function(call, info)
  local command = call.name == "bash" and call.arguments.command or ""
  if command:find("rm%s+%-rf%s+/%s*$") then
    return { decision = "deny", reason = "refusing rm -rf /" }
  end
end)
```

プラグインは上のように policy サービスの `voter` で投票します (`inject = { "policy" }`)。決定点のリスナー (`ctx:on`) は投票者ではありません。`init.lua` では `mare.policy.voter(point, fn, opts)` が同じものです。

書き換えは、それより前の `tool/rewrite` のチェーンで行われます。各書き換え者は、その時点の引数を持つ呼び出しのコピーを受け取り、`nil` または新しい引数のテーブルを返します。エラー、タイムアウト、テーブル以外の結果は書き換えの失敗となり、呼び出しは拒否されます。

```lua
ctx.policy:rewriter("tool/rewrite", function(call)
  if call.name == "bash" then
    local args = {}
    for k, v in pairs(call.arguments) do args[k] = v end
    args.command = (args.command or ""):gsub("^%s+", "")
    return args
  end
end)
```

決定点は、`tool/before`、`tool/approval`、`net/approval`、`request/ready`、`permission/verdict` に加え、編集ツールが書き込む前の `fs/edit-intent` と `fs/write-intent` です。

### 主なイベント

![mare の 1 ターンのデータフロー](../../assets/diagrams/turn-dataflow.svg)

公開ストリームやプロトコルイベントは [イベントリファレンス](../reference/events.md) に登録表から自動生成されます。以下は代表的なプロセス内ライフサイクル／フックイベントです。ペイロードは Lua のテーブルで、残りはランタイムディレクトリの `types/mare/events.lua` のコメントに記載されています。

| イベント | 方式 | ペイロードと意味 |
|---|---|---|
| `app/ready` | notify | UI の準備ができました。 |
| `app/shutdown` | notify | 正常終了。リスナーの完了が待たれます。 |
| `session/started` | notify | `{ session_id, path, resumed }` |
| `session/closed` | notify | `{ session_id, cwd, reason, path? }` (`session_id`・`cwd`・`reason` は必須、`path` は任意)。ルートホストのセッション終了・切り替え・再読み込み時にのみ発行され (子ホストでは発行されない)、依存オブザーバーの破棄より前に届く。 |
| `agent/step-ready` | policy | `{ id, step, cancel, turn_id }`。各モデルリクエストの前。deny で実行を止める。 |
| `agent/step-committed` | notify | `{ step_id, step, turn_id }`。ステップの参加者状態のコミット後。 |
| `turn/completed` | notify | `{ turn_id, status, termination_reason, cancellation_cause?, text, structured, usage, error, ... }`。ターン終了時に発行（`termination_reason` が `"interrupted"` のとき、`cancellation_cause` は `"user"`・`"parent"`・`"hook"`・`"disposed"` のいずれか）。 |
| `agent/settled` | notify | `{ turn_id, busy }`。その実行の `turn/completed` リスナーがすべて戻った後に発行。別の実行や外側の `turn/completed` 配信が続いている間は `busy` が true。 |
| `agent/turn-stopping` | bail | 文字列または `{ message, source? }` を返すと、ループが続行されます（`source` 指定時は続行メッセージに `source` と `notification = true` が付与されます）。 |
| `request/prepare` | checked notify | `{ model, cancel }`。プロンプトの最初のリクエストの前。エラーはリクエストを止めます。 |
| `request/ready` | policy | deny はリクエストを止めます。`not sent: <reason>` と表示されます。 |
| `input/submit` | notify | ユーザーが入力を送信しました。 |
| `input/transform` | waterfall | 送信前に入力を変更します。 |
| `context/transform` | waterfall | リクエストの前にメッセージを変更します。 |
| `request/build` | waterfall checked | 送信リクエストを変更します。例外はプロバイダーへの送信前にリクエストを中断します。 |
| `context/system` | waterfall | `{ text, model }`。`payload.text` を変更して `ev.next(payload)` を返します。 |
| `tool/rewrite` | rewrite | 投票の前に、呼び出しの引数を変更します。 |
| `tool/before` | policy | `(call, { side_effect, idempotent, unconfined })` |
| `tool/approval` | policy | `ask` に対して `{ decision = "allow" }`、`{ decision = "allow", grant = "allow_session" }` (`amendment` を伴う `"allow_amend"` も同様)、`{ decision = "deny" }`、`{ decision = "deny", abort = true }` で答えます。 |
| `tool/after` | waterfall | ツールの実行後、`tool/result` の前に結果を変更します。`result.sandbox_denials` は拒否の一覧です。 |
| `tool/result` | waterfall checked | `tool/after` の後、既定の切り詰めの前に結果を変更します。リスナーが失敗するとエラーツール結果になります。 |
| `provider/request` | waterfall | 送信前の `{ provider, url, headers, body, model }`。 |
| `llm/usage` | notify | プロバイダーへのリクエストごとの使用量（`usage.cache_write_1h?` は `cache_write` の内数、`usage.cost?`）。 |
| `compact/before` | bail | `false` でコンパクションを取り消します。 |
| `option/changed` | notify | `mare.o.<name> = value` または再読み込みの後の `{ name, value }`。 |
| `option/apply` | checked | 実行時のプラグインオプション書き込みで、通知の前に送られる `{ name = "plugins.<p>.<path>", value }`。所有プラグインがここで適用し、エラーなら書き込みは失敗してすべて元に戻ります。 |
| `theme/changed` | notify | テーマのスナップショット。 |
| `theme/palette` | waterfall | スタイルが組み立てられる前に、フレーバーのパレットを調整します。 |
| `ui/caps` | waterfall | ターミナルの機能を追加または上書きします。 |
| `ui/commit-before` | waterfall | スクロールバックに入ろうとしている `{ lines }`。 |
| `plugins/completed` | notify | パッケージ操作の後の `{ action, name?, status }`。 |
| `reload/completed` | notify | `/reload` の後の `{ added, removed, replaced, reloaded, notices }`。 |
| `kernel/reloaded` | notify | プラグインが再読み込みされました。 |
| `kernel/error` | notify | `{ message, plugin, event, what, error }`: リスナー、タスク、タイマー、破棄フックが失敗しました。 |

外部プログラムは、RPC 経由でイベントを購読できます。[RPC](../integrate/rpc.md) を参照してください。独自のイベントを公開ストリームに出すには、サービス契約で `expose = "both"` を宣言します。
