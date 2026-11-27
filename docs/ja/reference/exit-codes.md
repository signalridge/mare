# 終了コード

`mare` のプロセス終了コードを、ヘッドレス実行、RPC と ACP サーバー、対話型インターフェースとサブコマンドごとに示し、最後の `settled` レコードの `reason` の値も説明します。

> このリファレンスページは手動で保守されています（`scripts/gen_docs.py` による自動生成ではありません）。

## ヘッドレス実行

`mare -p` と `mare --json` は、次のいずれかのコードで終了します。`--json` を指定した場合、同じコードと対応する `reason` が最後のレコードに現れます ([JSON イベントストリーム](../integrate/json-stream.md) を参照)。

| コード | `settled.reason` | 意味 |
|---|---|---|
| 0 | `completed` | ターンがエラーなく完了した。 |
| 1 | `error` | プロバイダー、ツール、ランタイムのエラー。`-c` または `--resume` が見つけられなかったセッション、SIGTERM、SIGHUP も含む。 |
| 2 | `usage` | 引数または設定が不正: 未知のフラグ、プロンプトがない、stdin を読み取れない、`init.lua` の失敗、`--strict-config` での設定の警告、信頼の問題。実行は開始されなかった。 |
| 3 | `approval_unavailable` | 承認が「拒否して中止」(`abort`) で答えられ、ターンが停止された。 |
| 4 | `max_turns` | `--max-turns` に達した。 |
| 4 | `timeout` | `--timeout` に達した。 |
| 5 | `save_failed` | セッションの永続化に失敗した。最終回答は引き続き stdout に出力され、未保存のレコードは `<state>/quarantine/` に書き出され、stderr に警告が出力される。 |
| 130 | `interrupted` | SIGINT (Ctrl+C)。 |
| 141 | なし | 読み手によって stdout が閉じられた。例: `mare --json ... \| head`。ターンは停止され、`settled` レコードは書かれない。 |

これらの選び方について、いくつか補足します。

- コード 3 は、承認が `abort` で答えられて停止した実行にのみ使われます。誰も答えられなかった承認は拒否された呼び出しであって停止ではありません。モデルには拒否が返り、モデルがそれを回避すればターンは 0 で完了します。終了コードにかかわらず、すべての拒否は stderr に一覧表示されます。
- `--timeout` は、出力の配信を含め、実行全体に上限を設けます。発動すると、mare はターンを取り消し、`turn.failed` と (`timeout` を持つ) `settled` を書こうとし、4 で終了します。stdout の読み手がブロックしている場合、これらなしでストリームが終わることがあるため、`settled` がなければ失敗として扱ってください。
- 最初の SIGINT はターンを取り消し、130 で正常に終了します。2 回目の SIGINT は `settled` レコードを書かずに、ただちに 130 で終了します。
- `--json` での使い方の誤りでも、`session`、`turn.failed`、`settled` の行が出力されるため、ストリームを読むスクリプトは常に結果を得られます。
- `--timeout` は、ヘッドレス実行 (`-p` と `--json`) 以外では 2 で拒否されます。

```sh
mare -p --timeout 5m --max-turns 30 "Fix the failing tests"
case $? in
  0) echo done ;;
  3) echo "stopped by an approval abort: rerun with a permission preset that allows it" ;;
  4) echo "ran out of turns or time" ;;
  *) echo "failed" ;;
esac
```

## サーバーとインターフェース

| モード | コード | 条件 |
|---|---|---|
| `mare --rpc` | 0 | stdin が閉じられた、`shutdown` を受信した、または SIGTERM か SIGINT。 |
| `mare --rpc` | 2 | `--strict-config` での設定の警告、壊れた `init.lua`、または起動時の信頼の問題。mare はメッセージを stderr に書きます。 |
| `mare acp` | 0 | エディターが接続を閉じた。 |
| 対話型 | 0 | 通常の終了、または起動中の SIGTERM。 |
| 対話型 | 130 | インターフェースが引き継ぐ前の SIGINT。 |
| 対話型 | 2 | 設定ファイルの失敗を知らせる起動ダイアログで、終了を選んだ。 |

## サブコマンド

| コマンド | コード |
|---|---|
| `mare doctor` | すべてのチェックに合格すれば 0、失敗したチェックがあれば 1。`--json` も同じコードを使います。 |
| `mare session ...` | `new`、`list`、`fork`、`archive`、`unarchive`、`delete`: 成功なら 0 (`new` と `fork` は ID を出力)。`new` でセッションが作成されなかった場合、`delete` が取り消された場合、または操作失敗時は 1。無効な `--limit`、解決不能または曖昧なセッション指定、非 TTY の stdin で `--yes` なしの `delete` は 2。 |
| `mare test` | すべてのテストに合格すれば 0、1 件以上失敗すれば 1、引数エラー・スペック検出失敗・ホスト起動失敗・読み込み/構文エラーは 2。 |
| `mare token rotate` | 成功なら 0、失敗なら 1。 |
| `mare web url` | 成功なら 0 (`?token=` 付きログイン URL を出力)、リスナー未起動またはトークン読み取り失敗時は 1。 |
| `mare login`、`mare logout`、`mare model refresh` | 成功なら 0、失敗なら 1。ホストを起動できなかった、または `--with-api-key` で stdin にキーがない場合は 2。 |
| `mare mcp ...` | 0。アクションが設定エラーや失敗を報告した場合は 1。 |
| `mare peer send` | 受信側のセッションがメッセージを受け入れたら 0 (`held`、`delivered`、`handled` を出力)。宛先がない、メッセージが拒否された、30 秒以内に応答がない場合は 1。 |
| `mare sandbox status` | 0。`--require-full` を指定すると、すべてのサンドボックス分離機能が完全に強制されていない限り 1（現在は Windows で拡張セルフテストに成功しても、呼び出しごとのネットワーク分離が強制されないため 1 を返します）。 |
| `mare sandbox setup` | 成功なら 0、失敗なら 1。Windows 以外のプラットフォームでは 2。 |
| `mare trust`、`mare init`、`mare runtime`、`mare plugin ...`、`mare completion`、`mare rpc schema` | 成功なら 0。エラーは stderr に報告され、終了コード 1。 |

不明なサブコマンドなど、コマンドラインパーサー自体の引数エラーは 2 で終了します。プロセスを終了させるクラッシュは、`<state>/crashes/` の下にレポートを書き、0 以外で終了します ([ファイル](files.md) を参照)。
