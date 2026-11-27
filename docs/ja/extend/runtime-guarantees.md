# プラグインのランタイム保証

ライフサイクル、サービス、登録、リロード、クリーンアップについて mare がプラグイン作者に約束することです。プラグインはここに挙げたどれにも依存できます。いずれかを破る変更は互換性のない変更です。ここにない振る舞いも [プラグイン](plugins.md) と [サービスとイベント](services-and-events.md) に書かれていますが、この一覧には含まれません。

各保証にはキーがあります。ある保証のテストを探すにはキーで検索します: `rg 'guarantee: effects-dispose-in-reverse-start-order'`。

## 依存と待機

| キー | 保証 |
|---|---|
| `inject-waits-instead-of-failing` | `inject` に誰も提供していないサービスがあるプラグインは失敗ではありません。待機し（PENDING）、ホストが何を待っているかを報告し、提供者が現れるとすぐに起動します。 |
| `inject-cycle-refused-and-the-rest-starts` | 互いにしか提供しないサービスを inject し合うプラグインは実行前に拒否されます。問題には循環が示され、循環の外のプラグインは通常どおり起動します。 |
| `dependents-stop-before-their-provider-and-restart-on-the-new-one` | 提供者がリロードまたは置き換えられると、そのサービスを inject するプラグインは提供者より先に破棄され、新しい提供者の上で再び起動します。古い提供者に残されることはありません。 |
| `dependent-waits-while-its-provider-is-gone` | 提供者がいなくなると、依存するプラグインは破棄され（クリーンアップが実行され）待機します。これは失敗ではなく、提供者が戻ると再び起動します。 |
| `provider-outlives-dependents-in-flight-cleanup` | 提供者の破棄は、依存側が実行中のクリーンアップを待ちます。そのため依存側のクリーンアップは、起動時に使ったサービスをまだ呼べます。 |

## サービス

| キー | 保証 |
|---|---|
| `one-provider-per-service-name` | サービス名の提供者は一つです。同じ名前を `provide` に宣言する二つ目の定義は実行前に拒否され、エラーは両方のファイルを示し、最初の定義がサービスを保持します。 |
| `no-fallback-when-a-provider-leaves` | 実行時にすでに使われている名前へ提供すると `already_provided` で拒否され、保持者が応答し続けます。保持者がいなくなっても別の提供者へは切り替わらず、プラグインが再び提供するまでその名前には提供者がいません。 |
| `undeclared-raw-provide-refused` | 宣言済みのサービスは `ctx:plugin` と `mare.Service` クラスでのみ提供され、すべての呼び出しが契約で検査されます。宣言済みの名前で直接 `ctx:provide` すると拒否されます。 |
| `host-methods-stay-with-the-host` | サービス契約でホストが実装するメソッドは、プラグインが実装したり置き換えたりできません。 |

## 登録の所有

| キー | 保証 |
|---|---|
| `ctx-registrations-withdrawn-on-dispose` | プラグインの `ctx` を通じて登録したもの（リスナー、サービス、タイマー、タスク、ツール、コマンド、キーバインド、ステータス要素、effect）はすべて、プラグインの破棄時に取り消されます。 |
| `registration-withdrawn-on-caller-unload` | 他のプラグインのサービスを通じた登録は呼び出し側のものです。呼び出し側がいなくなると取り消され、受け取った解除関数は何度呼んでもよく、明示的に解除したものが再び解除されることはありません。 |
| `release-after-provider-is-gone-does-nothing` | 提供者が先にいなくなった場合、元の呼び出し側が持つ解除関数は何もせず、アンロード済みの提供者を呼ぶこともありません。 |
| `no-registration-escapes-an-unload` | プラグインのアンロード中にクリーンアップから行った登録はプラグインより長く残りません。その後に実行されることはありません。 |
| `shared-cleanup-runs-once-per-registration` | 二つのプラグインが登録した同じクリーンアップ関数は二つの登録です。片方を破棄すると一度だけ実行され、もう片方の登録は残ります。 |

## クリーンアップ

| キー | 保証 |
|---|---|
| `effects-dispose-in-reverse-start-order` | プラグインのクリーンアップは後入れ先出しの順に開始され、それぞれちょうど一度実行されます。順序が重要なクリーンアップは一つの effect にまとめてください。 |
| `cleanup-error-does-not-stop-the-rest` | 一つのクリーンアップがエラーを投げても他は止まりません。そのエラーは一度だけ報告されます。 |
| `yielding-cleanups-run-concurrently` | 処理を譲るクリーンアップ（タイマー、プロセス、ネットワークを待つもの）は並行して実行されます。互いの終了を待たず、すべてが終わるとアンロードが終わります。 |
| `turning-a-plugin-off-waits-for-its-cleanup` | プラグインを無効にすると、非同期部分を含むクリーンアップが終わるまで待ってからリロードの完了が報告されます。 |

## 失敗とロールバック

| キー | 保証 |
|---|---|
| `failed-apply-rolled-back-before-it-is-reported` | `apply` がエラーを投げた場合、失敗が報告される前に、それが提供・登録したものは取り消され、クリーンアップは実行済みです。 |
| `failed-reload-keeps-the-old-definition-serving` | リロードの新しい定義が `apply` で失敗すると、以前の定義が新しいインスタンスとして再び動き（以前のメモリ上の状態は戻りません）、依存側が再適用され、失敗が報告されます。完全な `/reload` でもそのプラグインは動き続けます。 |
| `broken-definition-reload-changes-nothing` | 新しい定義が読めない、または `apply` の前の検証で失敗するリロードは、実行中のインスタンスに手を触れません。 |
| `dependents-put-back-with-their-provider` | 一回のリロードで提供者と依存側の新しい定義がすべて失敗した場合、それぞれ以前の定義に戻り、依存側は提供者の後に戻ります。 |
| `handler-error-reported-and-plugin-keeps-running` | リスナー、タスク、タイマーがエラーを投げると毎回報告され（`kernel/error`、通知）、プラグインは読み込まれたままです。最新のエラーは完全な `/reload` まで問題の一覧に残ります。 |

## リロード

| キー | 保証 |
|---|---|
| `config-change-restarts-only-its-plugin` | あるプラグインのオプションを変えると、そのプラグインだけが再起動します。 |
| `rejected-config-keeps-the-previous-one` | プラグインのスキーマが拒否した設定は適用されず、以前の設定のまま動きます。修正した編集は適用されます。 |
| `cancelled-reload-waits-for-disposal` | クリーンアップ中に取り消されたリロードは、破棄が半分で止まったプラグインを残しません。再試行はまずその破棄を待ちます。 |
| `full-reload-keeps-startup-and-session` | 完全な `/reload` はすべてのプラグインを再起動しますが、起動処理を再実行せず（`app/ready` も `session/start` も二度目はありません）、ユーザーが今いるセッションを保ちます。 |
| `reload-rereads-plugin-modules` | プラグインをリロードすると、それが `require` したモジュールは破棄され、編集したモジュールファイルが読み直されます。 |

## 終了と止まったプラグイン

| キー | 保証 |
|---|---|
| `shutdown-is-bounded-and-names-unfinished-cleanups` | 終了時はプラグインのクリーンアップを最大 `shutdown_grace_ms` まで待ちます。期限を過ぎると待つのをやめ、まだアンロード中のプラグイン（終了前にアンロードが始まっていたものも含む）を示します。他のプラグインはアンロード済みです。 |
| `unload-stall-reported-with-effect-labels` | `stall_ms`（既定 10 秒、0 で無効）を過ぎてもアンロード中のプラグインは、破棄が始まったときに持っていた effect のラベル（`ctx:effect(fn, label)`）とともに、止まるたびに一度、`/plugins tree`、`mare plugin tree --json`、ログ、`kernel/stalled` 通知で報告されます。ラベルはアンロード開始時のスナップショットで、まだ実行中のものだけでなく開始したすべてのクリーンアップを示します。設定変更による再起動は破棄を経ないため、その報告にはラベルがありません。 |
| `load-stall-reported-without-abort` | `stall_ms` を過ぎても `apply` が戻らないプラグインも同じように報告されます。何も中断・隔離されず、それを始めたリロードは待ち続けます。 |
| `pending-stall-reported-as-waiting` | `stall_ms` を過ぎてもサービスを待っているプラグインは、待っているサービスとともに報告されます。PENDING のままで、報告が問題の行を増やすことも、待機を失敗に変えることもありません。 |
