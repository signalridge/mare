# フック、ステータスライン、通知

mare のコアには組み込みのシェルコマンドフックランナーがありません。その代わり、`mare.on`、`ctx:on`、`mare.policy.voter`、`ctx.policy:rewriter` などの Lua リスナーや、`statusline`・`notify` といったプラグイン、または外部フックパッケージで同等の処理を書けます。

## `init.lua` とプラグインのライフサイクルフック

`~/.config/mare/init.lua` では `mare.on` でカーネルイベントを購読し、`mare.policy.voter` で承認・実行の判断に投票できます。

```lua
-- ~/.config/mare/init.lua
mare.on("turn/started", function(ev)
  mare.log("info", "turn started: " .. tostring(ev.turn_id))
end)

mare.policy.voter("tool/before", function(call)
  if call.name == "bash" and (call.arguments.command or ""):find("rm%s+%-rf%s+/%s*$") then
    return { decision = "deny", reason = "refusing rm -rf /" }
  end
end)
```

プラグイン側で使える API (`ctx:on`、`ctx.policy:rewriter`、`ctx.bus:waterfall`) の詳細は [サービスとイベント](../extend/services-and-events.md) にまとめています。

### 主なライフサイクルとポリシーのフックポイント

| イベント / ポイント | 種別 | 主な用途 |
|---|---|---|
| `app/ready`, `app/shutdown` | Notify | UI とプラグインの起動直後や終了直前の処理 |
| `session/started`, `session/loaded`, `session/changed` | Notify | セッションの新規開始・読み込み・切り替えへの追従 |
| `input/submit`, `input/transform` | Notify / Waterfall | ターン開始前のユーザー入力の検査や書き換え |
| `request/prepare`, `request/ready`, `request/build` | Checked / Policy / Waterfall checked | モデルへの送信リクエストの検証、拒否、加工 |
| `context/system`, `context/transform` | Waterfall | システムプロンプトへの動的追記やメッセージ列の加工 |
| `tool/rewrite`, `tool/before`, `tool/approval`, `tool/after` | Rewrite / Policy / Waterfall | 引数の補正、危険な呼び出しの拒否、ツール出力の変換 |
| `turn/started`, `turn/completed`, `agent/settled` | Notify | ターンの開始・終了を追跡 (`turn/completed` が先に発火し、全リスナーが戻った後に `busy` 付きの `agent/settled` が発火) |
| `option/changed`, `reload/completed` | Notify | 実行時の設定変更や `/reload` 完了への反応 |

### エラー時の挙動

エラーの扱いはディスパッチ種別によって異なります。

- **通常の通知 (`emit`)**: ターン中にリスナーがエラーを投げた場合、mare はトレースバックをログへ記録し、ターンの下に 1 行だけエラー表示 (`plugin <name> hook <event> failed: ...`) を出して、他のリスナーやプラグイン自体は止めずに続行します。
- **検証付きフック (`emit_checked`、`waterfall_checked`)**: 例外は呼び出し元へそのまま伝播します。`request/prepare` や `request/build` で例外が起きると、プロバイダーへの I/O に入る前にそのリクエストが中断されます。
- **ポリシーゲート**: `request/ready` の voter が拒否した場合は、リクエストは送られずプロンプト文字列が入力欄へ戻ります。`tool/before` や `tool/approval` の voter がエラーまたはタイムアウト (`policy_timeout_ms`、既定 30 秒) を起こした場合はフェイルクローズとして `deny` になります。

### シェルフック用パッケージ (`mare-plugins`)

Claude Code や Codex のフックイベント名 (`PreToolUse`、`PostToolUse`、`UserPromptSubmit`、`SessionStart` など) と互換のある JSON / シェルコマンド形式の宣言的フックを使いたい場合は、`init.lua` に自前のプロセス起動を書く代わりに `signalridge/mare-plugins` のフックパッケージを `/plugins` や `mare.plugins.add` から導入できます。

## カスタムステータスライン (`statusline`)

`statusline` プラグインは入力エディターの下にバーを描画します (`mode`、`model`、`effort`、`context`、`cwd`、`branch`、`pr`、`changes`、`cost`、`quota`)。

### 組み込みレイアウトとセグメント切り替え

セッション中で `/statusline` を実行すると各セグメントの表示・非表示 (`state.json` に保存) を切り替えられます。レイアウト自体は `init.lua` で設定します。

```lua
mare.plugin("statusline", {
  layout = "combined", -- "combined" | "classic" | "codex" | "kimi" | カスタム look テーブル
  hidden = { "pr", "cost" },
  cwd_width = 24,
  gauge_width = 6,
  changed_files = true,
  pr = true,
  layout_opts = { sep = "space", flash = 0 },
})
```

### 外部コマンドでのステータスライン出力

`command` にスクリプトを指定すると、起動時、モデル変更時、各ターン完了後に実行されます。標準入力に JSON オブジェクト (`session_id`、`transcript_path`、`cwd`、`width`、`model`、`context`、`cost`) が渡され、標準出力の 1 行目がバーに表示されます。

```lua
mare.plugin("statusline", {
  command = "~/bin/mare-statusline.sh",
  command_timeout_ms = 1000,
  command_replaces = false, -- true にすると組み込みセグメントを隠して置換
})
```

Lua プラグインからは `ctx.ui:status(id, { align = "right", order = 20, render = ... })` で独自セグメントを追加できます。

## 通知 (`notify`)

`notify` プラグインは、`min_turn_ms` (既定 5000 ms) を超えるターンが完了したときや、ユーザーの入力・承認待ちになったときにターミナル通知 (OSC 9、OSC 777、Kitty OSC 99、tmux/screen パススルー、ベル、および Windows Terminal 以外の Windows でのネイティブ通知) を送ります。

```lua
mare.plugin("notify", {
  on_done = true,
  on_input = true,
  only_unfocused = true,
  min_turn_ms = 5000,
  protocol = "auto", -- "auto" | "bel" | "native"
  bell = false,
  dedupe_ms = 10000,
})
```

### 外部通知コマンド (`notify.cmd`)

TUI・ヘッドレス・RPC を問わず任意の実行モードでデスクトップ通知、Webhook、効果音などを鳴らしたい場合は `cmd` を設定します。標準入力に 1 件の JSON オブジェクト (`type` が `"agent-turn-complete"` または `"input-required"`、`session-id`、`turn-id`、`cwd`、`last-assistant-message` または `message`) が渡されます。

```lua
mare.plugin("notify", {
  cmd = { "notify-send-mare" },
  cmd_timeout_ms = 10000,
})
```

## 関連項目

- [サービスとイベント](../extend/services-and-events.md): Lua でのイベントおよびポリシーフックの実装方法
- [イベントリファレンス](../reference/events.md): 公開プロトコルイベントのスキーマ一覧
- [外観](ui.md): テーマ、枠線、UI コンポーネントの上書き
