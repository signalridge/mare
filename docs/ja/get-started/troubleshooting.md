# トラブルシューティング

mare が起動しない、モデルに接続できない、設定どおりに動かない、ターミナルでの描画が崩れる、といった場合の原因の調べ方です。

## まず mare doctor を実行する

```sh
mare doctor
```

`mare doctor` はモデルを呼び出さずに実行されます。チェックごとに 1 行を表示し、失敗したチェックが 1 つでもあれば `1` で、なければ `0` で終了します。チェックマークは成功、`!` は実行の失敗にはならない警告、バツ印は失敗を表します。`mare doctor --json` は同じチェックを 1 つの JSON ドキュメントとして出力し、`mare doctor --fix` は足りないディレクトリの作成、権限の引き締め、壊れたパッケージロックファイルの修復を試みます (実行自体は通常の起動時の処理、たとえばセッションインデックスやランタイムファイルの準備も行います)。

セッション内では `/doctor` が、実行中のセッションが把握しているチェックをオーバーレイで表示します。

| チェック | 分かること |
| --- | --- |
| `directories`、`permissions` | 設定、状態、データ、セッション、ログの各ディレクトリが存在すること。`auth.json`、状態ディレクトリ、セッションディレクトリが非公開 (`0600` / `0700`) であること。その直下の `.jsonl` ファイルも同様です。プロジェクトごとのサブディレクトリにあるセッションファイルは確認されません。`--fix` は両方を修復します |
| `terminal capabilities` | mare が検出したターミナルの種類と、トゥルーカラー、幅広の絵文字、kitty キーボードプロトコル、ブラケットペーストに対応しているか、そのターミナルの既知の癖 |
| `config`、`model`、`config options` | `init.lua` が読み込めるか、どのモデルが設定されているか、`mare.setup`、`mare.o`、`mare.plugin` にある未知のオプション名や型エラー |
| `auth.json`、`auth.json mode` | 解析できない認証情報ファイルは報告され、「認証情報なし」としては扱われません。mare はコピーを `auth.json.bak` として保持します |
| `sandbox` と `sandbox ...` | このマシンのサンドボックスバックエンド (たとえば Linux では `bwrap` や `landlock`、macOS では Seatbelt) と、強制できる機能 |
| `tmux clipboard`、`tmux keys` | tmux 内でのみ。`set-clipboard` と `extended-keys` がオンかどうか |
| `session files`、`session index` | 保存済みのセッションがすべて解析できること。途中まで書かれた最後のエントリは警告として報告されます。セッションインデックスの整合性 |
| `runtime`、`plugins` | ランタイムディレクトリが展開済みであること。有効なプラグインの数と各プラグインの問題。組み込みを上書きしているプラグインも含みます |
| `permission policy`、`key conflicts` | 使用中の権限サービスとセッションのサンドボックスレベル。ターミナルや OS が横取りしそうなキーバインド |
| `credential sources` | プロバイダーごとの認証情報の取得元 (保存済みのログイン、環境変数、またはなし)。秘密情報は表示されません |
| connectivity | 設定済みのプロバイダーへの到達性。固定のリストは使いません |
| `mcp ...`、`packages`、`skills`、`git` | プラグインが追加するチェック。MCP サーバー、インストール済みパッケージとロックファイル、読み込み済みのスキル、`PATH` 上の `git` |

Windows では、`mare doctor` はレガシーなコンソールホストと、Windows Terminal が `Shift+Enter` と `Alt+Enter` を転送するかどうかも報告します。

プラグインは `kernel/doctor` イベントをリッスンして独自のチェックを追加できます。[サービスとイベント](../extend/services-and-events.md) を参照してください。

## 原因を切り分ける

次のフラグで問題を絞り込めます。通常の起動と組み合わせて使います。

| フラグ | 効果 |
| --- | --- |
| `--clean` | `init.lua`、パッケージ、ユーザープラグイン、プロジェクトプラグインをスキップします。組み込みプラグインだけが読み込まれ、プロジェクトは信頼されません |
| `--strict-config` | 設定に警告があるとき、表示するだけでなく起動を拒否します (終了コード `2`)。ヘッドレスと RPC の実行に適用されます |
| `--no-trust` | 保存済みの判断にかかわらず、今回の実行ではこのプロジェクトを信頼しません |
| `--no-session` | セッションを読み書きしません |
| `--set KEY=VALUE` | 今回の実行に限り、設定値を 1 つ上書きします。例: `--set plugins.tools.edit_mode=replace` |

`mare --clean` では動くのに `mare` では動かない場合、原因は `init.lua`、パッケージ、プラグインのいずれかにあります。`init.lua` の一部をコメントアウトして二分探索してください。問題がプロジェクトにある場合は、`mare --no-trust` で `.mare/` のファイルが原因かどうかが分かります。

本来の環境に触れずに、まったく新しいディレクトリで mare を試すには、`MARE_HOME` を空のディレクトリに向けます: `MARE_HOME=$(mktemp -d) mare doctor`。

## 起動が遅い

```sh
mare --profile-startup            # start the UI, quit when ready, print the time of each stage
mare --startuptime /tmp/start.txt # normal start, write the timing to a file (like nvim --startuptime)
```

セッション内では、`/profile` が起動のタイムラインと各プラグインの読み込み時間を表示します。モデルへのリクエストの間に起こったツール一覧の変更も表示します (ツール一覧が変わると、その時点以降はプロバイダーのプロンプトキャッシュが無効になります)。

## 設定エラー

mare は警告とエラーを区別します。

- 警告は、未知のオプション名、誤った型、解析できない `MARE_*` 環境変数の値です。近い名前があれば、オプション名には「もしかして」のヒントが付きます。ヘッドレスと RPC の実行では警告を stderr に出力します。`--strict-config` を付けると、起動を拒否します (終了コード `2`)。`/config` はすべてのオプションと、その値、設定された場所を一覧表示します。
- エラーは、`init.lua` (または信頼済みプロジェクトの `.mare/init.lua`) の実行に失敗した場合です。対話型 UI は、ファイル、行、エラーの最初の行を示すダイアログを表示し、3 つの選択肢を提供します。エディターでその行を開く (その後、設定がもう一度実行されます)、終了する、続行する、です。続行すると UI は起動しますが、ファイルを直して `/reload` が成功するまで、モデルへのリクエストはすべてブロックされたままです。ヘッドレスと RPC の実行にはダイアログがなく、`2` で終了します。
- 任意のプラグインやパッケージの読み込みが失敗した場合は警告を報告してスキップされます。`/plugins` はすべてのプラグインを由来とともに一覧表示し、その下に各失敗をトレースバック付きで表示します。トレースバックは、完全な `/reload` で消えるまで残ります。起動時の通知には「fix it and /reload (or start clean: mare --clean)」と表示されます。
- 必須サービス、`required = true` のプラグイン、または必須パッケージ (`required = true`) の読み込みが失敗すると、修正して `/reload` する (または無効にする) までモデルへのリクエストがブロックされます。`mare plugin sync` は `init.lua` で宣言されたパッケージをインストールし、`mare plugin list` はそれらを一覧表示します。`init.lua` で必須パッケージを無効にするとブロックが解除されます。

編集後は再読み込みします。`/reload` はプラグイン全体を、`/reload <plugin>` は 1 つのプラグインを、`/reload config` は設定を再読み込みします。

## 認証情報とモデル

| 症状 | 確認すること |
| --- | --- |
| "X is not configured: run /login X or set ..." | `mare login status` で各プロバイダーを確認します。ログインするか、プロバイダーの環境変数を export してください ([クイックスタート](quickstart.md#プロバイダーにログインする) を参照) |
| 環境変数のキーが無視される | `auth.json` に保存されたログインは環境変数より優先されます。`mare doctor` は、設定されているのに使われていない取得元を示します。`mare logout <provider>` で保存済みのものを削除できます |
| `mare login anthropic` がキーを標準入力から渡すよう求める | API キー方式のプロバイダーは、キーを標準入力からのみ読み取ります: `printenv ANTHROPIC_API_KEY \| mare login anthropic --with-api-key` |
| ChatGPT のブラウザーログインでブラウザーを開けない | SSH 経由や、`DISPLAY` も `WAYLAND_DISPLAY` もない Linux では `mare login openai --device-auth` を使います |
| `/model` でプロバイダーが "needs login" と表示される | そのプロバイダーにはまだ認証情報がありません。`/login <provider>` を実行します |
| モデル一覧が古い、またはモデルが足りない | `mare model refresh` が、ログイン済みのすべてのプロバイダーのモデル一覧を読み込み直します |
| `auth.json` が壊れている | 直るまで、保存済みの認証情報は読み取りも変更もできません。ファイルを修正するか、削除して再度ログインしてください。mare が作成したコピーは `auth.json.bak` にあります |
| `mare logout` でログインを削除したのにプロバイダーが使える | 認証情報が環境変数や `init.lua` にもあります。`logout` が、どれを削除すべきかを教えてくれます |

どのプロバイダーにも認証情報がない場合、対話型 UI では初回起動のフローが自動的に始まります。

## ターミナルの問題

- `/terminal-setup` を実行すると、`Shift+Enter` と `Alt+Enter` が mare に届いているかを確認でき、直し方も表示されます。`Shift+Enter` が使えない場合は、`\` を入力して Enter を押すと改行できます。
- tmux 内では、`tmux.conf` に `set -g extended-keys on` がないと `Shift+Enter` は Enter として届き、コピーは `set -g set-clipboard on` がある場合にしかシステムのクリップボードに届きません。`mare doctor` は両方を確認します。
- `TERM=dumb` を指定すると、カーソル移動のない、1 行ずつ出力する簡易モードになります。
- `mare doctor` は、ターミナルや OS が横取りしやすいキーバインド (`key conflicts`) を、対処法とともに一覧表示します。`mare.keymap.set` で再割り当てしてください。[キーマップ](../configure/keymaps.md) を参照してください。
- プログラムが直接ターミナルに書き込んだ後に画面が乱れた場合は、`Ctrl+L` で再描画します。子プロセスを含め、mare の stdout と stderr に書かれた出力は入力ボックスの上に取り込まれます。画面を乱せるのは、ターミナルデバイスを自分で開くプログラムだけです。
- 画像が `[image]` と表示される場合、そのターミナルは Kitty、iTerm2、sixel のいずれのプロトコルにも対応していません。`mare doctor` で検出された機能を確認できます。
- Windows では、`Ctrl+V` はターミナルのテキスト貼り付けになるため、画像は `Alt+V` で貼り付けます。また `Ctrl+Z` では mare を一時停止できません。

## セッション

- `mare doctor` の `session files ... damaged`: 該当のファイルを解析できません。続行するのではなく、読み取り専用で開くか、フォークしてください。
- `session torn writes`: 最後のエントリが途中まで書かれています (通常はクラッシュの後)。無視され、次回の再開時に切り捨てられます。
- `session index ... degraded`: `/resume` はセッションファイルのスキャンにフォールバックします。
- ターンの途中でクラッシュや強制終了が起きた後は、再開したセッションで `/continue` を実行すると、中断されたターンを続行できます。

## ログ、クラッシュレポート、状態

| 項目 | 場所 |
| --- | --- |
| ログ | `~/.local/state/mare/log/mare.log`。10 MB でローテーションし、古いファイルを 5 つ保持します。秘匿処理済みで、モードは `0600` |
| クラッシュレポート | `~/.local/state/mare/crashes/<time>/report.txt`。パニックで mare が終了したときだけ書かれます |
| セッション | `~/.local/state/mare/sessions/` |
| 実行時の選択 (モデル、effort) | `~/.local/state/mare/state.json` |
| 信頼の判断 | `~/.local/state/mare/trust.json` |
| 認証情報 | `~/.config/mare/auth.json` |

既定のログレベルは `info` です。1 回の実行だけ上げるには、`tracing` のフィルターを受け取る `MARE_LOG` 環境変数を使います。

```sh
MARE_LOG=debug mare
MARE_LOG=info,mare_binding=trace mare
```

セッション内では、`/debug` が最新のログ行と直近のクラッシュレポートを表示します。ディレクトリは `XDG_STATE_HOME`、`XDG_CONFIG_HOME`、`XDG_DATA_HOME`、`MARE_HOME` で移動できます。

## サンドボックスと権限

`mare doctor` は、サンドボックスバックエンドと、それが強制できること、できないことを表示します。`sandbox deny-read` に `!` が付いている場合は、このマシンでは読み取りの制限が使えないため、承認されたエスカレーションは承認されたとおりにサンドボックスの外へ出ます。セッション内では、`/permission caps` が同じ機能の概要を、`/permission log` がセッションの権限判断を表示し、`/permission` でプリセットを変更します。`--permission full-access` (`--yolo`) はサンドボックスを外し、確認を行いません。ただし危険なコマンドは例外で、どのプリセットでも確認され (ヘッドレスでは拒否されます)。

ヘッドレスの実行が `3` で終了した場合、承認が「拒否して中止」(`abort`) で答えられ、それによってターンが停止したことを意味します。誰も答えられなかった承認は拒否されるだけで、モデルには拒否が返り、実行は `0` で終了することもあります (拒否された呼び出しは stderr に一覧表示されます)。拒否を避けるには、その承認の要らないプリセットを `--permission` で選ぶか、`--approval-policy never` を指定して、確認せずにリクエストを拒否させてください。
