# ブートフラグメントと `task` ホストプラグイン

`crates/mare-kernel/lua/mare_kernel/boot.lua` は `mare.task` とルートコンテキストの上に cordis ホストを構築し、ホストプラグイン (`task`、`loader`) を組み込み、各プラグイン定義を cordis コンポーネント (`component.lua`: 所有者レコード、`lua/` ディレクトリ、失敗報告とリロード報告で使う `apply` の結果) へ変換します。型定義は `types/mare/host/kernel.lua` にあります。

## プリミティブ

ブートフラグメントは Rust 側 (`lua_env/cordis_host.rs`) から `prims` テーブルを受け取り、そのうち型定義された部分を設定としてホストプラグインへ渡します (ブートフラグメント専用の `capture`、`spawn_root`、`run_root` は除く)。

| エントリ | 役割 |
|---|---|
| `handler_failed(plugin, what, err)` | プラグインのコールバックエラーを報告 (問題行への追加、ログ出力、`kernel/error` 発火) |
| `unload_modules(name)` | プラグインの fiber がアンロードされた際、`package.loaded` からそのプラグインのモジュールを削除 |
| `mark_active(name, owner \| nil, config?)` | fiber 稼働中のプラグインインスタンスとその有効な `config` テーブルをカーネルに記録し、`owner` が `nil` のときに解除する |

owner、event、definition、reload テーブルの型は [prims 型定義（英語ソース）](https://github.com/signalridge/mare/blob/main/crates/mare-plugin/preset/types/mare/host/prims.lua) を、row テーブルの構造は [loader ホストプラグイン](host-loader.md) を参照してください。

## 起動とリロード

`booted.start()` と `booted.reload(name, targets)` はホスト自身のコンテキスト (`prims.run_root`) で実行されます。制限付きピアやツール内部から起動されたリロードであっても、再構築される fiber に呼び出し側の権限グラントは引き継がれません。

- 実行中の単一プラグインに対する `reload(name)` は `loader:replace` を使います。新しい定義の適用に失敗した場合は、元の定義が復元されます。
- それ以外の場合は、末尾の行から逆順に 1 行ずつツリーから外します (まず消えた行、次に `targets`)。その後、完全な行リストを使って新しい定義と設定から対象を再構築します。

## `task`

`ctx:after(ms, fn)`、`ctx:every(ms, fn)`、`ctx:spawn(fn, ...)` は呼び出し元の fiber に紐づくタスクを起動し、その fiber がアンロードされると自動で中止されます。使うプラグインは `inject = { "task" }` を宣言します。

## `kernel`

契約済みサービス `kernel` (`mare_kernel.plugins.kernel`、`mare.Service`) は、以前の Ctx メソッド `plugins`、`problems`、`is_builtin`、`provides`、`reload`、`reload_barrier` を置き換えます。使うプラグインは `inject = { "kernel" }` を宣言します。
`kernel:reload(name?)` はリロード処理全体 (Rust が定義を準備したうえでツリーを切り替える経路) を、リロード自身によって破棄される可能性のある呼び出し元から切り離して実行します。
