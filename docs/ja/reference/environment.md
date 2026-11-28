# 環境変数

mare が読み取るすべての環境変数を、制御する対象ごとにまとめ、mare が起動するコマンドに設定する変数も示します。`MARE_<OPTION>` 系の設定変数では環境変数が `init.lua` より優先され、コマンドラインフラグが環境変数より優先されます (プロバイダー認証情報など独自の優先順位がある項目は各節で説明します)。

> このリファレンスページは手動で保守されています（`scripts/gen_docs.py` による自動生成ではありません）。

## ディレクトリ

mare は、Windows と macOS を含むすべてのプラットフォームで XDG 形式の場所を使います。各ディレクトリの中身は [ファイル](files.md) を参照してください。

| 変数 | 効果 |
|---|---|
| `MARE_HOME` | 4 つのディレクトリすべてのルート。設定、状態、データ、キャッシュは、その下の `config`、`state`、`data`、`cache` になります。`XDG_*` 変数より優先されます。使い捨ての実行やテストに便利です。 |
| `XDG_CONFIG_HOME` | 設定ディレクトリは `$XDG_CONFIG_HOME/mare` になります。既定は `~/.config/mare`。 |
| `XDG_STATE_HOME` | 状態ディレクトリは `$XDG_STATE_HOME/mare` になります。既定は `~/.local/state/mare`。 |
| `XDG_DATA_HOME` | データディレクトリは `$XDG_DATA_HOME/mare` になります。既定は `~/.local/share/mare`。 |
| `XDG_CACHE_HOME` | キャッシュディレクトリは `$XDG_CACHE_HOME/mare` になります。既定は `~/.cache/mare`。 |
| `HOME`、`USERPROFILE` | 上記の既定値の元になるホームディレクトリで、パス中の `~` の元でもあります。`USERPROFILE` は Windows でのフォールバックです。 |

空の値は、未設定として扱われます。

## 設定

設定のレイヤーは次の順序で適用され、後のものが優先されます。オプションの既定値、`state.json`、ユーザーの `init.lua`、プロジェクトの `.mare/init.lua`、下記の `MARE_<OPTION>` 変数、そしてコマンドラインフラグ (`--set`、`-m`、`-e`) です。

| 変数 | 効果 |
|---|---|
| `MARE_<OPTION>` | 同じ名前のコアオプションを、大文字にした名前で設定します: `MARE_THEME`、`MARE_BORDER`、`MARE_REDUCE_MOTION`、`MARE_UPDATE_CHECK` など。すべてのコアオプションに対応する変数があります ([オプション](../configure/options.md) を参照)。値は JSON として解析され、JSON でない場合は文字列として使われます: `MARE_REDUCE_MOTION=true`、`MARE_PERF='{"highlight_cache":64}'`。`{`、`[`、`"` で始まるのに解析できない値は、警告付きで無視されます。 |
| `MARE_MODEL`、`MARE_EFFORT` | 今回の起動に使うモデルと推論の強度。`MARE_EFFORT` は今回の起動にのみ適用されます。 |
| `MARE_TRUST_DEFAULT` | 何も記録されていない場合の、プロジェクトの信頼の既定値: `ask`、`trust`、`distrust` (`always` と `never` も受け付けます)。 |
| `MARE_WATCHDOG_MS` | yield しない Lua コードに対する、コールバックごとのミリ秒単位の上限。既定は 10000。ウォッチドッグは `init.lua` の前に起動するため、ここか `--set watchdog_ms=...` でしか設定できません。 |
| `MARE_LUA_MEMORY_MB` | Lua VM のメモリ上限 (MiB)。Lua VM は `init.lua` の前に作成されるため、ここか `--set lua_memory_mb=...` でのみ有効になります。 |
| `MARE_APPROVAL_POLICY` | `approvals` プラグインの `policy` 承認ポリシー (`ask`、`never`、`untrusted`、または細粒度 JSON) を上書きします。 |
| `MARE_WEB_SEARCH_PROVIDER`、`MARE_WEB_FETCH_PROVIDER` | `web` プラグインの `search_provider` と `fetch_provider` オプションを上書きします。 |
| `MARE_MCP_CONFIG` | 追加の MCP 設定ファイルのパス。そのサーバーは承認後にのみ起動します (`--approve-mcps` を指定すると、確認なしで承認されます)。 |
| `MARE_DEV_PLUGINS` | バイナリに埋め込まれたコピーの代わりに、ディスクから読み込む組み込みプラグインのソースのディレクトリ。mare 自体の開発用です。 |

無効な `MARE_<OPTION>` の値は起動時に報告されます。`--strict-config` を指定すると、その警告は起動の拒否になります。

## プロバイダーの認証情報

プロバイダーは、次の順序でキーを読み取ります。`init.lua` の `api_key` オプション、`mare login` が保存した認証情報、そしてこれらの変数です。[モデル](../use/models.md) を参照してください。

| 変数 | プロバイダー |
|---|---|
| `OPENAI_API_KEY` | OpenAI |
| `ANTHROPIC_API_KEY` | Anthropic |
| `GEMINI_API_KEY`、`GOOGLE_API_KEY` | Google (`GEMINI_API_KEY` が先に確認されます) |
| `OPENCODE_API_KEY` | opencode Go |
| `DEEPSEEK_API_KEY` | DeepSeek |
| `MOONSHOT_API_KEY` | Moonshot (`kimi`、`kimi-cn`) |
| `KIMI_API_KEY` | Kimi Code (`kimi-code`、`kimi-code-cn`) |
| `MINIMAX_API_KEY` | MiniMax (`minimax`、`minimax-cn`) |
| `ZAI_API_KEY` | Z.ai (`zai`) |
| `ZHIPU_API_KEY` | Zhipu GLM (`zhipu-cn`) |
| `OPENROUTER_API_KEY` | OpenRouter |

自分で定義した OpenAI 互換のプロバイダーは、`api_key_env` で独自の変数を指定できます。環境変数の名前である `api_key` オプションは、その変数から読み取られます。MCP サーバーの設定も、環境から値を取り込めます。文字列内の `${VAR}`、`${env:VAR}`、`${VAR:-default}`、`bearer_token_env_var`、`env_http_headers` です。

## ネットワークと証明書

| 変数 | 効果 |
|---|---|
| `HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY` (大文字小文字どちらでも) | mare 自身のリクエスト用の HTTP と HTTPS のプロキシ。mare が起動するコマンドは、これらをそのまま継承します。ただしネットワークを制限するサンドボックスでは、mare が自前のローカルフィルタリングプロキシをプロキシ変数に設定し、`NO_PROXY` を空にします。SOCKS プロキシと PAC ファイルには対応しておらず、直接接続せずにエラーで失敗します。 |
| `NO_PROXY` (大文字小文字どちらでも) | プロキシを経由しないホスト。プロキシが設定されていて `NO_PROXY` がない場合、mare が起動するコマンドには `NO_PROXY=localhost,127.0.0.1,::1` が設定されます。 |
| `MARE_CA_CERTIFICATE` | 追加で信頼する CA 証明書の PEM バンドル。設定した場合、これが唯一のカスタムの取得元になります。 |
| `SSL_CERT_FILE` | PEM バンドル。`MARE_CA_CERTIFICATE` が設定されていない場合に使われます。 |
| `SSL_CERT_DIR` | PEM または `.crt` ファイルのディレクトリのリスト (`:` 区切り、Windows では `;`)。上記に追加されます。 |

証明書はシステムのルートに追加されます。mare 自身は何も同梱していません。指定されたファイルが読み取れない、または証明書を含まない場合は、エラーになります。

`transport = "auto"` の場合、WebSocket クライアントは HTTP プロキシを通らないため、`HTTPS_PROXY` または `ALL_PROXY` (大文字小文字どちらでも) が設定されていると、OpenAI のリクエストは WebSocket ではなく HTTP を使います。

## ターミナルと表示

| 変数 | 効果 |
|---|---|
| `NO_COLOR` | 空でない値であれば、色を無効にします。 |
| `MARE_COLOR` | 色深度を強制します: `truecolor`、`256`、`16`。`NO_COLOR` が優先されます。 |
| `COLORTERM` | `truecolor` または `24bit` でトゥルーカラーを選びます。 |
| `TERM` | 名前に `256` が含まれていれば 256 色を選びます。`dumb` はインターフェースを、1 行ずつ出力する簡易モードに切り替えます。非 UTF-8 のロケールでの `linux` は、ASCII のアイコンを選びます。 |
| `TERM_PROGRAM`、`TERM_PROGRAM_VERSION` | 色深度、アイコンセット、幅の規則、画像プロトコルのために、ターミナルを識別します。 |
| `COLUMNS` | 簡易モード (`TERM=dumb`) での、幅のフォールバック。 |
| `MARE_NERD_FONT`、`NERD_FONT` | `1` で Nerd Font のアイコンセットを選びます。 |
| `LC_ALL`、`LC_CTYPE`、`LANG` | 最初に設定されているものが、ロケールが UTF-8 かどうかを決めます。 |
| `MARE_WIDTH_METHOD` | 文字幅の測定方法を強制します: `unicode` または `wcwidth`。 |
| `MARE_NO_TERM_PROBE` | 設定すると、背景色と機能のプローブを含む起動時の問い合わせをすべてスキップします。 |
| `MARE_NO_OSC11` | 設定すると、OSC 11 による背景色の問い合わせだけをスキップします。他の機能のプローブは実行されます。 |

mare は、キー、画像、クリップボードの動作を選ぶために、ターミナルが設定する変数からもターミナルを識別します: `KITTY_WINDOW_ID`、`GHOSTTY_RESOURCES_DIR`、`WEZTERM_EXECUTABLE`、`WEZTERM_PANE`、`ALACRITTY_WINDOW_ID`、`ALACRITTY_SOCKET`、`WT_SESSION`、`KONSOLE_VERSION`、`VTE_VERSION`、`VSCODE_INJECTION`、`ITERM_SESSION_ID`、`WARP_SESSION_ID`、およびマルチプレクサーのマーカー `TMUX`、`TMUX_PANE`、`STY`、`ZELLIJ` です。これらを自分で設定する必要はありません。クリップボードへのコピーは、ネイティブのコピーと OSC 52 によるコピーのどちらを使うかを選ぶために、`SSH_TTY`、`SSH_CONNECTION`、`WSL_DISTRO_NAME` も参照します。

## エディター、シェル、ログイン

| 変数 | 効果 |
|---|---|
| `VISUAL`、`EDITOR` | 外部エディター。`editor` オプションの後に、この順で使われます。フォールバックは `vi` です。値には `code --wait` のように引数を含められます。 |
| `SHELL` | `tools` プラグインの `shell` オプションが設定されていない場合の、bash ツール用のシェル。`$SHELL` が bash または zsh ならそれを、そうでなければ `bash` を使います。 |
| `SSH_CONNECTION`、`SSH_TTY` | どちらかが設定されていると、`mare login` はローカルのブラウザーがないものと見なし、まずデバイスコードのフローを提示します。MCP の OAuth ログインでは、リダイレクトされたアドレスを貼り付けるよう求められます。 |
| `DISPLAY`、`WAYLAND_DISPLAY` | Linux で両方が空の場合、`mare login` はブラウザーがないものと見なします。 |

## ログと診断

| 変数 | 効果 |
|---|---|
| `MARE_LOG` | `<state>/log/mare.log` のログフィルター。レベル (`debug`、`trace`) またはモジュールごとのフィルター (`mare_binding=trace`)。既定は `info`。 |
| `MARE_NO_HARDENING` | `1` で起動時のハードニングをスキップします。コアダンプの禁止、デバッガーのアタッチの禁止、mare とそのコマンドからの `LD_*` / `DYLD_*` 変数の削除が行われなくなります。 |
| `MARE_FRAME_LOG` | ファイルパス。TUI の実行中、mare は 1 行につき 1 回分の描画時間 (マイクロ秒) を書き込みます。 |
| `MARE_SANDBOX_BACKEND` | `none` で OS サンドボックスのバックエンドを無効にし、Linux で `landlock` を指定すると Landlock バックエンドを強制します。 |
| `MARE_SANDBOX_CAPS` | カンマ区切りのサンドボックス機能 (例: `fs_write,net_block`)。検出されたバックエンドが主張できる範囲を絞り込みます。 |
| `MARE_SANDBOX_DIR` | Windows: 拡張サンドボックスのディレクトリ。既定は `%ProgramData%\mare\sandbox`。 |
| `MARE_SANDBOX_HELPER` | Windows: `mare` 自身の代わりに、サンドボックスランナーとして使う実行ファイル。 |
| `MARE_UPDATE_SNAPSHOTS` | `mare test` のスナップショット更新を制御します (`always` で `--update-snapshots` と同様にスナップショットファイルを直接更新)。 |
| `MARE_PROFILE_LUA_EVERY` | 起動プロファイリング時の Lua 命令サンプリング間隔。正の整数、既定は `1000000`。 |
| `MARE_VALIDATE_RECORDS` | `1` を指定するとリリースビルドでもストレージレコード検証を有効にします。 |
| `MARE_FAUX_SCRIPT` | `faux` テストプロバイダーを動かす JSON 配列を含むファイル (デバッグビルドまたは `faux` 機能付きビルドでのみ有効。リリースバイナリには含まれません)。 |

## mare が起動するコマンド

`bash` ツールなどのツールが実行するコマンドは、あなたの環境から始まり、2 点だけ変更されます。`notify.cmd` のようなフックのコマンドには 1 点目 (認証情報の除去) だけが適用され、以下の追加は適用されません。

認証情報らしい名前は削除されます。名前に `KEY`、`PASSWORD`、`SECRET`、`TOKEN` (大文字小文字を問わない) を含む変数と、すべての `MARE_*` 変数は取り除かれます。`bash` ツールおよび `mare.tool.command` で宣言したコマンドのいずれも `tools` プラグインの `env` オプションで調整でき、`include`、`exclude`、`set` のすべてが両方に適用されます。

```lua
mare.plugin("tools", {
  env = {
    include = { "GH_TOKEN" },   -- keep a name the scrub would drop
    exclude = { "AWS_PROFILE" }, -- drop a name it would keep
    set = { PAGER = "less" },    -- set a value for every tool command
  },
})
```

その後、mare はツールのコマンドに次の変数を追加します。

| 変数 | 値 |
|---|---|
| `TERM` | `dumb` |
| `PAGER`、`GIT_PAGER`、`GH_PAGER` | `cat` |
| `NO_COLOR` | `1` |
| `LANG`、`LC_CTYPE` | `C.UTF-8` (macOS では `en_US.UTF-8`)。`LANG`、`LC_ALL`、`LC_CTYPE` のいずれも設定されていない場合のみ。Windows では設定されません。 |
| `MARE_CWD` | セッションの作業ディレクトリ。 |
| `MARE_SESSION_ID` | セッション ID。 |
| `MARE_SESSION_MODEL`、`MARE_SESSION_EFFORT` | 現在のモデルの参照と effort。入れ子の mare が設定として読み取らないよう、`MARE_SESSION_*` という名前になっています。 |
| `MARE_PEER_SOCKET` | このセッションのピアソケット (ある場合)。`mare peer send` が使います。 |

`status` プラグインのコマンド ([サービスとイベント](../extend/services-and-events.md) を参照) は、`MARE_STATE`、`MARE_MESSAGE`、`MARE_SESSION_ID`、`MARE_SESSION_PATH`、`MARE_CWD`、`MARE_SESSION_MODEL`、`MARE_PEER_SOCKET` も受け取ります。

TUI を開いている間、mare はコマンド用に `SUDO_ASKPASS`、`MARE_ASKPASS_SOCK`、`MARE_ASKPASS_TOKEN` も設定します。これにより、`sudo` のパスワード入力は、端末がなくて失敗するのではなく、TUI の伏せ字入力ボックスに届きます。
