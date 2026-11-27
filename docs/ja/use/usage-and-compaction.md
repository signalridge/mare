# 使用量とコンパクション

mare はセッションごとのトークン数・費用・プロバイダーのクォータ残量 (`usage`) を記録し、コンテキスト上限に達する前に 4 段階で履歴を圧縮 (`compact`) し、起動時間やフレーム描画の負荷 (`profile`) も計測します。

## トークン使用量とコスト (`/usage`)

`usage` プラグインは現在のセッションにおける `input`、`output`、`cache_read`、`cache_write`、`cache_write_1h` を集計します。要約生成や `/side` / `/btw` などのサイドリクエストもセッション合計に含まれます。

セッション内で `/usage` (別名 `/cost`) を実行すると、次の項目が表示されます。

- リクエスト回数 (うちメイン会話外のサイドリクエスト数)
- 入力・出力・キャッシュ読み取り・キャッシュ書き込みの各トークン合計
- プロバイダー定義または `mare.setup { models = ... }` の `cost` 上書き設定に基づく推定費用 (ChatGPT OAuth などのサブスクリプション枠では `subscription, not billed` と表示)
- レスポンスヘッダーから取得した最新のクォータ枠 (`5h`、週間枠、クレジット、リセットまでの残り時間)

集計値はセッション単位です。新規セッションではゼロから始まり、既存セッションを再開またはフォークすると JSONL ファイルから再集計されます。

## 段階的コンパクション (`/compact`)

`compact` プラグインは、元の履歴をセッション JSONL に残したまま、モデルのコンテキストウィンドウ内に収まるよう会話を圧縮します。

### 自動コンパクションの条件

mare はモデルリクエストを送る直前に、種別ごとの優先順位でしきい値を求めます。まず `tokens` (`model_thresholds.<ref>.tokens`、なければ `threshold_tokens`)、次に `percent` (`model_thresholds.<ref>.percent`、なければ `threshold_percent`)、最後に `ratio` (`model_thresholds.<ref>.ratio`、なければ `threshold_ratio`) です。同じ種別の中ではモデル別設定が全体設定より優先されますが、より高優先度の全体設定 (例: `threshold_tokens`) はモデル別の `percent` や `ratio` より優先されます。算出結果は常にそのリクエストのプレッシャー予算で上限が掛けられます。

`min(configured_threshold, context_window - max_output - headroom_tokens)`

既定値は `threshold_ratio = 0.8`、`headroom_tokens = 32768` です。使用量がしきい値の 90% を超えるとステータスラインに警告が出ます。手動で実行したい場合はいつでも `/compact` を呼べます。

```text
/compact
/compact preserve the database schema decisions and failing test names
```

### 4 つの圧縮ステージ

コンパクションは `stages` の順 (`{ "prune", "offload", "remote", "local" }`) に進み、各ステージ後に再計測します。停止条件は呼び出し理由によって異なります。

- **自動コンパクション**: 推定使用量がしきい値以下になった時点でのみ途中で停止します。前のステージ (要約ステージを含む) を終えてもしきい値を超えていれば、条件を満たすか末尾に達するまで次のステージへ進みます。
- **手動 `/compact`**: `prune` や `offload` でしきい値を下回っても止まらず、要約ステージ (`remote` または `local`) が成功するまで進みます。
- **オーバーフロー復旧**: プロバイダーからコンテキスト超過エラーが返った場合 (またはサイレントオーバーフロー検知時) に走り、`prune_min_saving_tokens` の下限を越えて要約ステージ成功か末尾まで進みます。コンテキストが縮めばリクエストを再試行し、`agent` プラグインの `max_overflow_compactions` (既定 `3`) 回連続で続くとエラーで停止します。

各ステージの動作は次のとおりです。

1. **`prune`**: 直近の `prune_protect_tokens` (40,000 トークン) を保護したうえで、`prune_tool_result_chars` (8,192 文字) を超える古いツール出力を先頭 (4,096) と末尾 (1,024) だけに切り詰めます。自動実行では少なくとも `prune_min_saving_tokens` (20,000) を削減できる場合にのみ適用されますが、オーバーフロー復旧ではこの下限を無視できます。
2. **`offload`**: 最新の `offload_keep_images` (2 枚) 以外の古いインライン画像を添付ファイルへ退避し、コンテキスト内にはファイルパスだけを残します。
3. **`remote`**: プロバイダーがリモートコンパクション (OpenAI の `/responses/compact` など) に対応している場合、それを使います。
4. **`local`**: モデル (または軽量な `summary_model`) を呼び出して構造化サマリーを生成し、直近の `keep_recent_tokens` (20,000 トークン) と古いユーザーメッセージ最大 `recent_user_tokens` (20,000 トークン) を原文のまま残します。元の長さより縮まなかった要約は保存前に破棄されます。

### 失敗時のバックオフと停滞 (`stalled`)

自動コンパクションは 1 回でも失敗すると指数バックオフ (`failure_backoff_ms` から `failure_backoff_max_ms` まで倍増) を設定します。自動コンパクションでローカル要約候補を使い切ってもコンテキストがしきい値を下回らない場合は `stalled` 状態に入って現在の実行を停止し、システムプロンプト・要約・保持メッセージのトークン内訳とともに `/new` や大容量ウィンドウモデルへの切り替えを案内します。

### 設定オプション

```lua
mare.plugin("compact", {
  auto = true,
  threshold_percent = 75,              -- または threshold_tokens = 150000
  summary_model = "opencode-go/glm-5.3-flash",
  keep_recent_tokens = 20000,
  stages = { "prune", "offload", "remote", "local" },
  model_thresholds = {
    ["openai/gpt-6.1-terra"] = { percent = 80, headroom_tokens = 32768 },
  },
})
```

現在のコンテキスト内でシステムプロンプト、ツール定義、スキル、指示ファイル、メッセージがそれぞれ何トークン使っているかは `/context` (または `/context all`) で確認できます。

## 起動と実行時の計測 (`/profile`)

`profile` プラグインは起動各段階の所要時間、プラグインごとの `apply` 時間、16 ms を超えた重い UI フレーム、遅い Lua コールバック (`slow_callback_ms`、`slow_callback_record_ms`)、およびプロンプトキャッシュを無効化するセッション途中のツール定義変更を記録します。

- セッション内で `/profile` を実行すると計測結果のオーバーレイが開きます。
- シェルからは `mare --profile-startup` で起動計測を出力して終了でき、`mare --startuptime <file>` でファイルへ保存できます。

## 関連項目

- [セッション](sessions.md): JSONL のコンパクション記録と `/tree`
- [コンテキストと指示ファイル](context.md): コンパクション対象外として毎回送られる要素
- [モデルとプロバイダー](models.md#モデルごとの上書き): `context_window` や `cost` の上書き設定
