# クイックスタート

プロバイダーへログインし、リポジトリで `mare` を起動し、ツール呼び出しを承認して `~/.config/mare/init.lua` を作成します。初回起動時に任意プラグインを選ぶか、後で `/plugins` → Discover から導入できます。

## プロバイダーにログインする

mare にはモデルプロバイダーが 1 つ必要です。組み込みのプロバイダーは次のとおりです。

| プロバイダー名 | 認証情報 | 環境変数 |
| --- | --- | --- |
| `openai` | ChatGPT ログイン (ブラウザーまたはデバイスコード)、または API キー | `OPENAI_API_KEY` |
| `anthropic` | API キー | `ANTHROPIC_API_KEY` |
| `google` | API キー | `GEMINI_API_KEY` または `GOOGLE_API_KEY` |
| `opencode-go` | API キー | `OPENCODE_API_KEY` |
| `deepseek` | API キー | `DEEPSEEK_API_KEY` |
| `kimi` | API キー | `MOONSHOT_API_KEY` |
| `kimi-code` | API キー | `KIMI_API_KEY` |
| `openrouter` | API キー | `OPENROUTER_API_KEY` |
| `ollama` | ローカルサーバー (`http://localhost:11434/v1`)、既定でキー不要 | — |
| `lmstudio` | ローカルサーバー (`http://localhost:1234/v1`)、既定でキー不要 | — |

環境変数に API キーがあれば、ログインしなくても使えます。`mare login` は認証情報を `~/.config/mare/auth.json` (モード `0600`) に保存し、保存済みのログインは環境変数より優先されます。

ChatGPT アカウントで OpenAI にログインします。

```sh
mare login openai
```

> 引数を省いた `mare login` はまず設定済みモデルのプロバイダーを試します。使える既定がなければ TTY 上でピッカーを開き、TTY がなければ終了コード `2` で終了します。

mare はブラウザーを開きます。SSH 接続など、ブラウザーのないマシンではデバイスコードを使います。

```sh
mare login openai --device-auth
```

API キーを使うプロバイダーでは、キーを標準入力から渡します。mare は端末でキーの入力を求めることはありません。

```sh
printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key
```

どのプロバイダーにログインしていて、各認証情報がどこから来ているかを確認するには、次を実行します。

```sh
mare login status
```

認証情報は `mare logout [provider]` で削除します。

この手順は省略することもできます。どのプロバイダーにも認証情報がない場合、対話型 UI は初回起動のガイドを開始します。プロバイダーの選択、ログイン、既定モデルの選択を行います。セッション内から `/login [provider]` でも同じことができます。

## プロジェクトで起動する

```sh
cd ~/src/my-project
mare
```

引数として最初のプロンプトを渡すと、質問を済ませた状態で起動します。

```sh
mare "what does this project do?"
```

### プロジェクトを信頼する

一部のプロジェクトファイルは、mare が実行する内容を変えます。`.mare/init.lua`、`.mare/plugins`、`.mare/skills`、`.mare/prompts`、`.mare/SYSTEM.md`、`.mare/APPEND_SYSTEM.md`、`.agents/skills` です。mare はこれらを、信頼済みのプロジェクトでのみ読み込みます。これらのいずれかがあるディレクトリ、または git リポジトリ内のディレクトリで、まだ何も保存されていない状態で起動すると、確認が表示されます。

| 選択肢 | 効果 |
| --- | --- |
| Trust and load | このフォルダーを信頼済みとして記憶し、プロジェクトのファイルを今すぐ読み込む |
| Trust this session only | このセッション限りでプロジェクトのファイルを読み込み、`trust.json` には保存しない |
| Don't trust | 判断を記憶し、以後読み込まない |
| Not now (または Esc) | 今回は何も読み込まず、次回また確認する |

確認ダイアログは見つかったリソースを種別ごとに表示します (`.mare/init.lua` に対するフル API 実行の警告、プラグイン、スキル、プロンプトテンプレート、システムプロンプトファイル、サニタイズ済みの MCP サーバー概要)。6 件を超えるグループがあるときは `e` で全件表示を切り替えられます。ディレクトリを恒久的に信頼すると、それらのファイルに後から加えられた変更も信頼することになります。実行してよいコードだけを信頼してください。判断は `~/.local/state/mare/trust.json` に保存され、サブディレクトリにも適用されます。シェルから決めることもできます。

```sh
mare trust            # 現在のプロジェクトを信頼 (ディレクトリ指定は mare trust <dir>)
mare trust --revoke   # revoke it
mare --trust        # trust for this run only, save nothing
mare --no-trust     # distrust for this run only
```

## 質問する

質問を入力して Enter を押します。最初は、コードの読み解きを頼むのがよいでしょう。

```text
How is the configuration loaded? Point me at the files.
```

エージェントはツールでファイルを読み、答えます。ツール呼び出しはトランスクリプトに表示されます。Ctrl+O でツール出力と思考の展開と折りたたみ、Ctrl+T でトランスクリプト全体を表示します。

実行中のターンは Esc で中断します。エージェントの作業中に Enter を押すと、ターンの終了後に送るメッセージとしてキューに入ります。[ワークフロー](workflows.md#エージェントの作業中に誘導またはキューに入れる) を参照してください。

## ツール呼び出しを承認する

既定の権限プリセットは `auto` です。エージェントはワークスペースと一時ディレクトリには確認なしで書き込めますが、呼び出しがサンドボックスの外に出る場合は mare が確認します。承認の流れをすぐに見るには、ファイルの書き込みに承認が必要な `read-only` プリセットで起動します。

```sh
mare --permission read-only "add a one-line comment to the top of README.md"
```

> OS サンドボックスが対応する場合は外部ネットワーク接続もブロックされます。ネットワークを制御できない Windows の既定層では、無制限アクセスが明示的に承認済みでない限り、シェルやその他のコマンドツールを実行する前に確認します。

承認ボックスにはツール名が表示され、次の回答を選べます。

| キー | 回答 |
| --- | --- |
| `y` | 1 回だけ許可 |
| `a` | このセッションの間は許可 |
| `A` | 許可し、提案されたルールを記憶する (mare がルールを提案できる場合に表示) |
| `n` | 拒否し、モデルに理由を伝える |
| Esc | この呼び出しを拒否する。ターンは続行 |
| Ctrl+C | 拒否してターンを停止する |

セッション内では `/permission` (または `/permission auto`) でプリセットを切り替えます。プリセットは `read-only`、`auto`、`full-access` の 3 つで、`--yolo` は `--permission full-access` の短縮形です。[権限](../use/permissions.md) を参照してください。

## ファイルのメンションとコマンド実行

`@` とパスを入力すると、メッセージにファイルを添付できます。Tab でパスを補完でき、`@"a path with spaces"` のように引用符で囲むと空白を含むパスも指定できます。

```text
Explain the retry logic in @src/client.rs
```

テキストファイルは 1 つあたり 100 KiB まで (1 メッセージあたり 256 KiB まで) 添付されます。添付できないファイルには、理由を示す 1 行のメモが付き、エージェントは read ツールでそのファイルを読めます。

行頭に `!` を付けると、自分でシェルコマンドを実行します。コマンドとその出力はコンテキストに入り、モデルは次のターンでそれを見ます。`!!` を使うと、何もコンテキストに追加せずにコマンドを実行します。

```text
!git status --short
!!ls -la
```

## モデルを選ぶ

`/model` でピッカーが開きます。認証情報のないプロバイダーは、ログインが必要なものとしてまとめて表示されます。モデル参照を直接指定して切り替えることもできます。

```text
/model openai/gpt-6.1-terra
```

`/effort` は現在のモデルの推論強度を設定します。起動時に `-m/--model` や `-e/--effort` (`MARE_EFFORT`) を渡すこともできます。グローバル設定としての `effort` は起動単位の上書き専用であり、`init.lua` のグローバル `effort` は無視されます。一方、`init.lua` の `model` は `~/.local/state/mare/state.json` の記憶より優先されます。

`/status` は、バージョン、モデル、セッション、トークン使用量、各プロバイダーのログイン状態を表示します。`/help` はすべてのコマンドを一覧表示します。

## セッションを再開する

すべての会話は `~/.local/state/mare/sessions` に保存されます。終了するには、入力欄が空の状態で Ctrl+D を押すか、Ctrl+C を 2 回押します。戻るときは次のようにします。

```sh
mare -c               # continue the most recent session in this directory
mare -r               # pick a session from a list
mare -r -2            # the second most recent session in this directory
mare -r my-name       # by name, id or unambiguous id prefix
mare -c --fork-session   # continue in a copy and leave the original alone
```

`-c` または `-r` に `--all` を付けると、すべてのディレクトリのセッションが対象になります。UI 内では、`/resume`、`/new`、`/name <title>`、`/fork` が対応する操作です。詳しくは [ワークフロー](workflows.md#セッションの続行とフォーク) を参照してください。

## 最初の init.lua を書く

`mare init` を実行してコメント付きの `~/.config/mare/init.lua` を作成し (既存ファイルは保持されます)、編集します:

```lua
-- ~/.config/mare/init.lua
mare.setup {
  model = "openai/gpt-6.1-terra",
}

-- give long shell commands more time
mare.plugin("tools", { bash_timeout_ms = 300000 })

-- /m is a shortcut for /model
mare.command.alias("m", "model")

-- a grep tool backed by ripgrep (grep and find are not built in)
mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
}
```

mare を再起動するか、セッション内で `/reload config` を実行します。`mare.setup` は、未知のオプション名や誤った型を「もしかして」のヒント付きで警告します。`/config` は、すべてのオプションと、その値、設定された場所を表示します。プロジェクトの信頼済み `.mare/init.lua` は、このファイルの後に実行されます。

次に読むもの:

- [ワークフロー](workflows.md): レシピ集。
- [モデル](../use/models.md) と [権限](../use/permissions.md): ここで行った選択の詳細。
- [オプション](../configure/options.md) と [キーマップ](../configure/keymaps.md): `init.lua` で設定できるその他の項目。
