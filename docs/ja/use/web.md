# ローカル WebSocket / HTTP リスナー

mare には認証付きのホスト HTTP/WebSocket リスナー (`--web` / `webserver`) が組み込まれており、WebSocket 越しの mare RPC プロトコル 3.0 (JSON-RPC 2.0 エンベロープ)、プラグインの HTTP ルート、宣言的な静的アセット (`web = { dir }`) を公開できます。mare 単体にフロントエンドアプリは同梱されておらず、UI はプラグインや外部クライアントがこれらのエンドポイント越しに提供します。

## リスナーの起動

ホストリスナーは RPC モードでのみ起動します。`init.lua` に `webserver.enabled = true` を書いても、通常の TUI (`mare`) を実行しただけではリスナーは立ち上がりません。RPC モードを選ぶ `--web` や `--web-listen` で起動するか、`webserver.enabled = true` と合わせて `mare --rpc` を実行してください。

```sh
mare --web                          # 127.0.0.1:0 にバインド (OS 割り当ての空きポート)
mare --web-listen 127.0.0.1:7432    # 指定した HOST:PORT にバインド
mare --rpc                          # webserver.enabled = true ならリスナーも起動
```

```lua
-- ~/.config/mare/init.lua (`mare --rpc` 実行時に有効)
mare.setup {
  webserver = {
    enabled = true,
    host = "127.0.0.1",
    port = 7432,
  },
}
```

起動すると、リッスン URL、`/readyz` の URL、WebSocket RPC の URL (`ws://.../rpc` または `wss://.../rpc`)、およびホストトークンファイルのパスが出力されます（生のトークンや `?token=` 付き URL は出力しません）。実行中のリスナーに対する認証付きログイン URL を表示するには次を実行してください。

```sh
mare web url
```

その `?token=...` URL をブラウザーで開くとセッション Cookie が発行されてリダイレクトでアドレスバーから `?token=` が消えますが、トークン自体は使い捨てではなく、ローテーションするまで再利用できます。

## 組み込みエンドポイント

| パス | 認証 | 役割 |
|---|---|---|
| `/readyz`, `/healthz` | 不要 | ヘルスチェック (`GET` は `{"status":"ready"}` を返し、`HEAD` はステータス `200`・空ボディを返す) |
| `/rpc`, `/` (WebSocket upgrade) | 必要 | JSON-RPC 2.0 エンベロープによる双方向 mare RPC プロトコル 3.0 ストリーム (1 テキストフレーム 1 JSON オブジェクト、バイナリフレームは `1003` で切断)。接続終了や切断時は待機中のコールバックをキャンセルし、その接続が所有する購読と登録を解放する |
| プラグインルート (`webserver.register_route`) | 必要 | Lua プラグインまたは RPC から登録された動的 HTTP ハンドラー (`GET`、`POST`、`PUT`、`PATCH`、`DELETE`、`HEAD`、`ANY`、上限 256 件) |
| 静的アセット (`web = { dir }`) | 必要 | プラグイン定義または `webserver.register_assets` でマウントされた静的ファイル (上限 64 マウント、1 ファイル最大 32 MiB) |

プラグインルートで `/readyz`、`/healthz`、`/rpc` を上書きすることはできません。

## 認証とトークンローテーション

`/readyz` と `/healthz` 以外のリクエストには認証が必要です。

1. **ホストトークン**: 既定では OS の CSPRNG で生成した 32 バイトのトークンを `~/.local/state/mare/host/token` (Unix では `0600`、Windows では制限付き ACL) に保存します。`init.lua` で固定文字列の `webserver.token` を指定することもできますが、`webserver.token = false` は拒否されます。
2. **ブラウザーの Cookie 交換**: `GET <url>/?token=<token>` を開くと `HttpOnly; SameSite=Strict` のセッション Cookie (TLS 有効時は `Secure` 付き) をセットし、`303` リダイレクトで URL から `?token=` を消します。URL からパラメーターを隠す処理であり、トークンを 1 回限りにするわけではありません。
3. **API / WebSocket クライアント**: `Authorization: Bearer <token>` ヘッダー、`X-Mare-Token: <token>` ヘッダー、セッション Cookie、または WebSocket 接続時の `?token=<token>` を渡します。
4. **ロックアウト**: 同じ IP から認証に 5 回連続で失敗すると、30 秒間ロックアウトされます。

ランタイムのトークンファイルを差し替えるにはシェルから次を実行します。

```sh
mare token rotate
```

`mare token rotate` は `~/.local/state/mare/host/token` を更新し、以前のファイルトークンを即座に無効化します。そのファイルトークンで認証済みだったアクティブな WebSocket 接続は、失効チェック時にクローズコード `4001` で切断されます。`webserver.token` に固定文字列を設定しているリスナーはその文字列を使い続けるため、ファイルローテーションの影響を受けません。

### 起動・ローテーション時の保護チェック (`fs_host_protect`)

既定のランタイムトークンファイルを使う場合、リスナー起動時と `mare token rotate` の両方で、サンドボックスバックエンドがトークンファイルをサンドボックス内コマンドから隠せるか (`fs_host_protect`) を検査します。保護できない環境 (Linux で `bubblewrap` がない場合や、Windows で拡張サンドボックス層が未導入の場合など) では、トークンファイルの作成やリスナー起動の前に `SANDBOX_INSUFFICIENT` で拒否されます。

解決するには Linux で `bwrap` を導入するか Windows で `mare sandbox setup` を実行してください。用途に応じて固定トークンへ切り替えるか、保護不足を明示的に受け入れることもできます。

```lua
mare.setup {
  webserver = { accept_partial = { "fs_host_protect" } },
}
```

```sh
mare token rotate --accept-partial fs_host_protect
```

## Host、Origin、TLS の検証

認証前に `Host` と `Origin` ヘッダーを検証します。

- ワイルドカード禁止: TLS の有無にかかわらず `0.0.0.0`、`[::]`、`::ffff:0.0.0.0`、`*` は拒否されます。ループバック (`127.0.0.1`、`[::1]`) または単一の具体的なインターフェース IP を指定してください。
- 非ループバックの TLS / 平文 HTTP: 非ループバックでは `tls_cert` と `tls_key` が必須です。単一の IPv4/IPv6 リテラル (IPv6 は `%zone` 可、ホスト名不可) と `plain_http = true` を併用した場合のみ平文 HTTP を許可します (`localhost` はループバック扱い)。`%zone` 付き IPv6 では `mare web url` が標準 `?token=` URL を生成できないため、非リンクローカル IP またはヘッダー認証を使用します。
- `Host` 検証: ループバックではループバックの `Host` を許可します。具体的な非ループバックではバインドした IP とポートを自動信頼し、独自ホスト名は `trusted_hosts` に追加します。
- `Origin` 検証: `Sec-Fetch-Site: cross-site` は `allowed_origins` 確認前に `403` を返します。その他のクロスオリジン要求は `allowed_origins` に登録が必要です。
- ローカル管理者と `remote_admin = false`: TCP 接続元 IP、`Host` ヘッダー、バインドアドレスの 3 つがすべてループバックの場合のみローカル管理者権限が付きます。非ループバック接続は既定で `remote_admin = false` となり、管理メソッド (`options.set`、`services.register`、`workspace.set_trust`、`subagent.register_type`、`hooks.subscribe`)、応答者リース (`capabilities.approvals` / `elicitation` / `ui`、`ui.attach { responder: true }`)、任意パスでの `session.load { path }` を禁止し、復元されるプリセット・サンドボックス・追加ディレクトリをホスト上限に抑えます。
- 同一ホストのリバースプロキシ: mare は `X-Forwarded-For` / `Forwarded` を信頼しません。同一ホストのプロキシが外部トラフィックを `127.0.0.1:<port>` へ転送し `Host` をループバックに書き換えると、リモート接続がローカル管理者と判定されます。SSH トンネル / Tailscale / WireGuard を使うか、具体的な非ループバック IP に TLS と `remote_admin = false` でバインドしてください。

### コンテナでの利用と公開デプロイ

- **コンテナ**: コンテナ内でもワイルドカードバインド (`0.0.0.0` / `[::]`) は拒否されます。`--web-listen` (または `webserver.host`) にコンテナの単一の具体的なインターフェース IP リテラルを指定し、TLS または `plain_http = true` と組み合わせてください。

```sh
mare --web-listen "$(hostname -i | cut -d' ' -f1):7432" --set webserver.plain_http=true
```

- **公開デプロイ**: リスナーをインターネットへ直接公開しないでください。ループバック (`127.0.0.1`) またはプライベートインターフェースにバインドしたまま、SSH トンネル (例: `ssh -L 7432:127.0.0.1:7432 user@host`) やプライベートなメッシュネットワーク (Tailscale、WireGuard など) 越しに接続するか、具体的なインターフェース上で TLS と明示的な `trusted_hosts` / `allowed_origins` を設定して運用してください。

```lua
mare.setup {
  webserver = {
    enabled = true,
    host = "192.168.1.20",
    port = 8443,
    tls_cert = "/etc/mare/cert.pem",
    tls_key = "/etc/mare/key.pem",
    trusted_hosts = { "mare.internal:8443" },
    allowed_origins = { "https://mare.internal:8443" },
    remote_admin = false,
  },
}
```

## プラグインのアセットとルートの提供

プラグインは自身の定義テーブルの `web` フィールドで静的ディレクトリを公開したり、`webserver` サービスからハンドラーを登録したりできます。

```lua
return {
  name = "my-panel",
  web = { dir = "dist", prefix = "/panel", index = "index.html", fallback = "index.html" },
  inject = { "webserver" },
  apply = function(ctx)
    ctx.webserver:register_route("/api/ping", {
      method = "GET",
      handle = function()
        return { status = 200, headers = { ["content-type"] = "application/json" }, body = '{"ok":true}' }
      end,
    })
  end,
}
```

登録されたマウントとルートは `HostManager` 全体で有効になり、所有するプラグインまたはセッションが破棄されると自動で解除されます。

## 関連項目

- [mare RPC プロトコル 3.0](../integrate/rpc.md): `/rpc` で使われるプロトコル仕様
- [オプション](../configure/options.md): `webserver` の全オプションと既定値
- [プラグイン](../extend/plugins.md): プラグインのライフサイクルと `web` 定義
