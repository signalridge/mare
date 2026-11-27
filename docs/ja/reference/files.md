# ファイルとディレクトリ

mare が読み書きするすべてのファイルとディレクトリを示します。4 つのユーザーディレクトリとその中身、およびプロジェクトに置けるファイルです。パスではホームディレクトリを `~` で表します。Windows では `%USERPROFILE%` と読み替えてください。

> このリファレンスページは手動で保守されています（`scripts/gen_docs.py` による自動生成ではありません）。

## 場所

mare は、すべてのプラットフォームで XDG 形式のディレクトリを使います。macOS と Windows でも、`Library` や `AppData` の場所は使いません。

| ディレクトリ | 既定値 | 上書き |
|---|---|---|
| 設定 | `~/.config/mare` | `$XDG_CONFIG_HOME/mare` |
| 状態 | `~/.local/state/mare` | `$XDG_STATE_HOME/mare` |
| データ | `~/.local/share/mare` | `$XDG_DATA_HOME/mare` |
| キャッシュ | `~/.cache/mare` | `$XDG_CACHE_HOME/mare` |

`MARE_HOME` はこれらすべてに優先します。4 つのディレクトリは `$MARE_HOME/config`、`state`、`data`、`cache` になります。[環境変数](environment.md) を参照してください。Lua コードには、同じパスが `mare.paths.config`、`mare.paths.state`、`mare.paths.data`、`mare.paths.auth`、`mare.paths.sessions`、`mare.paths.packages` として渡されます。

`mare doctor` は、これらのディレクトリが存在して書き込み可能であること、Unix では `auth.json` がモード 0600 で、セッションと状態のディレクトリがモード 0700 であることを確認します。

## 設定ディレクトリ

あなたが書いて編集するファイルです。

| パス | 用途 |
|---|---|
| `init.lua` | あなたの設定。`mare init` が作成し、上書きすることはありません。[オプション](../configure/options.md) を参照。 |
| `.luarc.json` | mare の型ファイルを指す Lua 言語サーバーの設定。`mare init` が書き出し、mare が最後に書いた内容と一致している間だけ書き換えられます。 |
| `lua/` | `init.lua` から `require` する Lua モジュール。 |
| `plugins/` | ユーザープラグイン。[プラグイン](../extend/plugins.md) を参照。 |
| `skills/` | ユーザーのスキル。スキルごとに 1 つのディレクトリがあり、`SKILL.md` を含みます。 |
| `prompts/` | スラッシュコマンドになるプロンプトテンプレート。 |
| `themes/` | テーマ。 |
| `AGENTS.md` | あなた自身の指示。すべてのセッションに読み込まれます。ディレクトリごとに、存在するものの中で `AGENTS.override.md`、`AGENTS.md`、`CLAUDE.md` の最初のものが読み込まれます。 |
| `SYSTEM.md` | 既定のシステムプロンプトを置き換えます。 |
| `APPEND_SYSTEM.md` | システムプロンプトの末尾に追加されるテキスト。 |
| `mcp.json` | MCP サーバー。[MCP](../use/mcp.md) を参照。 |
| `auth.json` | `mare login` が保存した認証情報。モード 0600。隣に、ログインや更新の実行中は `auth.json.lock`、修復後は `auth.json.bak` があります。 |
| `mare-lock.json` | パッケージのロックファイル。インストール済みのすべてのパッケージの正確なリビジョン。[パッケージ](../configure/packages.md) を参照。 |
| `plugins.json` | 有効にした、任意のパッケージのエクストラ。 |

## 状態ディレクトリ

mare が書き込む実行時状態です。再構築できるセッションインデックスは下の表で個別に示しています。未復旧の `quarantine/` レコードや、アクティブなセッションのロック・コミット・ロールバックファイルは削除しないでください (ストレージ層がセッションを再オープンする際に復旧レコードを読み込みます)。

| パス | 用途 |
|---|---|
| `state.json` | `/model` で選んだモデルなどの実行時の選択。次回の起動の既定値として使われます。`init.lua` に明示した値が優先されます。`state.json.lock` が書き込みを保護します。 |
| `trust.json` | プロジェクトの信頼の判断。`mare trust` と信頼の確認が書き込みます。 |
| `sessions/` | 保存されたセッション。後述。 |
| `log/mare.log` | ログ。10 MiB でローテーションし、古いファイルを `mare.log.1` から `mare.log.5` まで 5 つ保持します。レベルは `MARE_LOG` で設定します。 |
| `crashes/<timestamp>/report.txt` | 秘匿処理済みのクラッシュレポート。パニックで mare が終了したときに書かれます。 |
| `history.jsonl` | エディターのプロンプト履歴。 |
| `lua-history` | Lua コンソールの履歴。 |
| `permissions.json` | 承認の確認から保存した権限ルール。`permissions.json.lock` が書き込みを保護します。 |
| `news.json` | パッケージのお知らせの状態。 |
| `models-cache/<provider>.json` | `mare model refresh` が取得したモデル一覧。 |
| `skills-cache/*.json` | スキル検出のキャッシュ。 |
| `domains/ns-*.json` | 名前付きドメインの状態ファイル。 |
| `snapshots/run-<pid>-<uuid>/` | セッションファイルなしで実行したときのスナップショット (セッションファイル `abc.jsonl` がある場合は隣りの `abc.jsonl.snapshots/` を使用)。 |
| `attachments/<session-id または unsaved>/` | セッションファイルがない場合のコンパクション退避画像のフォールバック先 (セッションファイル `abc.jsonl` がある場合は隣りの `abc.attachments/` を使用)。 |
| `spill/<prefix>-*.log` | 長時間のコマンドの完全な出力。接頭辞は書き込み元を表す：シェルコマンドは `bash-*.log`、フックコマンドは `hooks-*.log`。 |
| `spill-results/` | 復元可能なツール出力。所有するセッション専用です ([RPC](../integrate/rpc.md) の `spill.*` メソッドを参照)。 |
| `host/token`、`host/.token.lock` | ローカル HTTP/WS Web リスナー用の Bearer トークン (`mare --web`、`mare token rotate` でローテーション、モード 0600) とそのロックファイル。 |
| `web/<pid>-<id>.json` | 実行中のホスト Web リスナーのエンドポイント情報 (`host/` の外に保存され、`mare web url` が参照。終了時に削除され、古いエントリは整理されます)。モード 0600。 |
| `quarantine/` | 通常のセッション永続化に失敗したとき退避保存される未保存セッション JSONL レコード。 |
| `mcp.log` | MCP サーバーの出力。 |
| `mcp-approvals.json` | 保存済みのプロジェクトおよび `$MARE_MCP_CONFIG` の MCP サーバー承認 (サーバー名と設定ハッシュでキー付け)。 |
| `mcp-disabled.json` | プロジェクトごとに無効化した MCP サーバー。 |
| `mcp-tools/` | MCP サーバーのツールスキーマキャッシュ。 |
| `mcp-output/` | 切り詰められた MCP の結果の完全な形。 |
| `mcp-oauth/<server>-<hash>.json` | MCP サーバーの OAuth トークン。 |
| `peers/<pid>.json`、`peers/<pid>.sock` | 実行中のセッションのセッションレジストリとピアソケット。終了時に削除され、古いエントリは整理されます。[RPC](../integrate/rpc.md) を参照。 |
| `pgroups/` | mare が起動したプロセスグループのレジストリ。クラッシュ後の後始末に使います。 |
| `crash-guard.json` | mare をクラッシュさせたリクエスト。再開したセッションが同じものを永遠に再送しないようにします。`crash-guard.lock` が保護します。 |
| `kitty/` | クラッシュ後も、ターミナルのキーボードモードの状態を一貫させておくためのマーカー。Unix のみ。 |
| `key-conflicts` | 最後に表示したキー競合の警告。繰り返し表示しないために使います。 |
| `luarc.generated` | mare が最後に書いた `.luarc.json`。 |
| `terminal-hint-shown` | Windows のみ。一度きりのターミナルのヒントを表示した後に作成されます。 |

### セッション

セッションは `sessions/<project>/` の下にあります。`<project>` は、作業ディレクトリのうち Unicode の英数字と `.`、`_` を残し、それ以外を `-` に置き換えて `--` で囲んだものです。`/home/me/app` なら `--home-me-app--` になります。

| パス | 用途 |
|---|---|
| `<time>_<id8>.jsonl` | 1 つのセッション。エントリの追記専用のログで、これが唯一の情報源です。名前は、開始時刻 (`2026-11-14T09-30-00`) と ID の末尾 8 文字です。 |
| `<time>_<id8>.jsonl.lock` | 唯一のライターのロック。その隣の `.lock.rollback` と `.lock.commit` ファイルは、クラッシュ復旧用のデータを保持します。 |
| `<time>_<id8>.jsonl.v<N>` | セッションファイルを書き換える前に作られるバックアップ。 |
| `<time>_<id8>.attachments/` | セッションとともに保存される画像。 |
| `<time>_<id8>.jsonl.snapshots/` | セッションのファイルスナップショットのジャーナル。 |
| `agents/<run>/<seq>-<label>.jsonl` | サブエージェントのセッション。親にリンクされています。 |
| `index.sqlite` (`sessions/` 内) | `/resume`、`-c`、ID 検索向けの再構築可能な SQLite インデックス。セッションのメタデータと 1 行のユーザー入力サマリー (最初の対象メッセージが `preview`、最新が `last_input`) を保存します。完全なメッセージは JSONL ファイルに保存されます。削除するとセッションファイルから再構築されます。 |

## データディレクトリ

mare が自分自身のためにインストールするファイルです。

| パス | 用途 |
|---|---|
| `runtime/` | 組み込みプラグインと Lua 型ファイルの、読み取り専用のコピー。`mare runtime` がパスを表示します。mare のバージョンが変わると書き換えられます。 |
| `packages/` | インストール済みのパッケージ。ソースごとに 1 つのディレクトリ。[パッケージ](../configure/packages.md) を参照。 |
| `package-types/` | 言語サーバー向けに、インストール済みパッケージからミラーされた型ファイル。 |

## キャッシュディレクトリ

| パス | 用途 |
|---|---|
| `luac/` | コンパイル済みの Lua バイトコード。削除しても安全です。 |

## プロジェクトのファイル

これらは作業ディレクトリを基準として、プロジェクト内にあります。

| パス | 用途 |
|---|---|
| `.mare/init.lua` | プロジェクトの設定。あなたの `init.lua` の後に適用されます。 |
| `.mare/plugins/` | プロジェクトのプラグイン。 |
| `.mare/plugins.json`、`.mare/mare-lock.json` | プロジェクト単位の任意プラグイン選択とパッケージのロックファイル。 |
| `.mare/lua/` | プロジェクトの `init.lua` 用の Lua モジュール。 |
| `.mare/skills/`、`.agents/skills/` | プロジェクトのスキル。プロジェクトリソースルート (`mare.project.root`) 基準で 1 回解決されます (起動時の信頼検出では `.git` まで親ディレクトリも `.agents/skills` を走査します)。 |
| `.mare/prompts/` | プロジェクトのプロンプトテンプレート。 |
| `.mare/SYSTEM.md`、`.mare/APPEND_SYSTEM.md` | プロジェクトのシステムプロンプトファイル。設定ディレクトリにあるものより優先されます。 |
| `.mare/mcp.json`、`.mcp.json` | プロジェクトの MCP サーバー。 |
| `AGENTS.md` | プロジェクトの指示。作業ディレクトリから、`.git` を含むディレクトリまでの親ディレクトリにかけて読み込まれます。ディレクトリごとに `AGENTS.override.md`、`AGENTS.md`、`CLAUDE.md` の最初のものが使われます。 |

コードの実行やプロンプトの変更ができる `.mare/` 配下のすべて (`init.lua`、`plugins`、`plugins.json`、`skills`、`prompts`、`SYSTEM.md`、`APPEND_SYSTEM.md`) と `.agents/skills` は、信頼済みのプロジェクトでのみ読み込まれます。`.mare/` ディレクトリだけがある場合は、判断は必要ありません。[権限](../use/permissions.md) を参照してください。

## プラットフォームごとの補足

| プラットフォーム | 違い |
|---|---|
| Linux、macOS | ピアソケットは `<state>/peers/<pid>.sock` の Unix ソケットで、モード 0700 のディレクトリ内のモード 0600 のファイルです。非公開ファイルはモード 0600 になります。 |
| Windows | `%USERPROFILE%` の下に同じディレクトリ構成。ピアソケットはランダムな名前の名前付きパイプで、そのパスにはパイプ名を示すエンドポイントファイルがあります。非公開ファイルには、モードビットの代わりに制限付きの ACL が設定されます。拡張サンドボックスは、セットアップ内容を `%ProgramData%\mare\sandbox` (または `MARE_SANDBOX_DIR`) に保持します。 |
