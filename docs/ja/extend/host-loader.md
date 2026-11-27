# `loader` ホストプラグイン

プラグインツリーは cordis-lua のローダー (`cordis.loader`) の上で動きます。mare はこれをホストプラグイン `mare_kernel.plugins.loader` としてラップし、ブートフラグメント (`boot.lua`) がルートコンテキストへ組み込んで `loader` サービスを提供します。型定義は `types/mare/host/loader.lua` にあります。

ホストプラグインは型付きで文書化されたプリミティブのみを使います。以下のテーブルはブートフラグメントから設定として渡され、実行中の `prims` オブジェクトはホストから渡されるためグローバル変数でも `require` 可能なモジュールでもありません (一方、内部の `mare_kernel.*` ソースモジュール自体はオープン `require` モデルで `require` できますが、`types/` には公開されていません)。

## 設定

```lua
root:use(require("mare_kernel.plugins.loader"), {
  component = component, -- fun(name): プラグイン名に対応する cordis コンポーネントを返す関数
  prims = prims,         -- 行プリミティブ群。prims.refused と prims.profile_record も参照される
})
```

プラグインツリーがコンポーネントを取得する手段は `component(name)` だけです。未知の名前を渡すと `module '<name>' not found:` が発生し、ローダーはその行だけのエラーとして報告します。

## 行プリミティブ

| プリミティブ | 戻り値 |
|---|---|
| `prims.rows()` | 現在の定義と設定に対応する行をロード順で返し、末尾に設定で無効化された行を並べます。 |
| `prims.refused()` | 静的な `provide` 重複チェックで拒否された定義を返します。 |
| `prims.profile_record(label, ms)` | 起動プロファイル (`/profile`、`--startuptime`) に 1 件記録します。ローダーは各プラグインの `apply` にかかった CPU 時間を `plugin <name> (<origin>)` として記録します。 |

各行は `{ id, name, config, disabled, origin, path, overrides }` のテーブルです。`id` と `name` にはプラグイン名が入り、`plugins.<name>.enabled = false` または最小プリセットによって無効になった行では `disabled` が `true` になります。ローダーは `id`、`name`、`config`、`disabled` を解釈し、`origin`、`path`、`overrides` は `dump()` 向けに保持します。

他の定義がすでに提供しているサービスを宣言したプラグインは拒否され、行リストには入りません (行はプラグイン名でキー付けされるため、拒否された行が実行中のプラグインを置き換えてしまうのを防ぐためです)。`dump()` は拒否された定義を理由付きで行一覧の末尾に出力し、それらが実行されることはありません。

## `loader` サービス

| メソッド | 呼び出し元 | 動作 |
|---|---|---|
| `loader:apply_rows(rows)` | 起動時、`/reload config` | ツリーを `rows` に追従させます。新規行を起動し、削除・無効化された行を破棄してクリーンアップを待ちます。設定が変わった行の fiber は新しい設定で更新し、変更のない fiber には触れません。前回失敗していた fiber は設定更新時に一度再試行されます。 |
| `loader:replace(name, component)` | `/reload <name>` | 実行中の定義を差し替え、新しい定義で対象 fiber を再起動します。新しい定義の適用に失敗した場合は旧定義を戻し、Future がエラーを送出します。 |
| `loader:dump()` | `mare plugin tree`、`/plugins` | 各行について origin、path、overrides、disabled、fiber の状態、保留中プラグインが待機している不足サービス (`missing`)、提供サービス、effect 数を返し、続いて拒否された定義を返します。 |

`apply_rows` と `replace` は cordis の Future を返すため、`:await()` で待機します。

`missing` は各 fiber の `_store` から読み取ります (cordis-lua の `api-core.md` の "Diagnostics" に記載された読み取り専用の診断フィールドです)。mare の他のコードが `_store` を参照することはありません。

## `mare plugin tree`

`mare plugin tree` は `loader:dump()` の結果を 1 プラグイン 1 行 (id、origin、状態: fiber 状態 / `disabled` / `refused`、詳細) で表示します。`mare plugin tree --json` を付けると構造化ダンプをそのまま出力します。
