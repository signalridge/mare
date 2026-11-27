# コンテキストと指示

## モデルが見るもの

各リクエストにはシステムプロンプト、ツール定義、会話履歴が含まれます。システムプロンプトは名前付きセクション (アイデンティティ行、一般指針、ツール一覧、現在のサンドボックスと承認ポリシー、指示ファイル、スキル一覧、作業ディレクトリ) から組み立てます。

プロバイダーのプロンプトキャッシュを維持するため、システムプロンプトはセッションとモデルごとに 1 回だけ構築し、入力が変わった場合のみ再構築します。`/context` でセクション別のトークン数、`/context all` で項目別の内訳を確認できます。

## 指示ファイル

mare は次のファイルをこの順に読み込みます。

1. 設定ディレクトリにあるユーザーのファイル: `~/.config/mare/AGENTS.md` (または同じ場所の `AGENTS.override.md`、`CLAUDE.md`)。
2. プロジェクトについては、プロジェクトルートから作業ディレクトリまでのすべてのディレクトリ。プロジェクトルートは、`.git` を含む最も近い祖先ディレクトリです。ない場合は作業ディレクトリだけが対象です。

各ディレクトリでは、最初に見つかった名前が優先されます。`AGENTS.override.md` (コミットしないローカルの上書き)、次に `AGENTS.md`、次に `CLAUDE.md` の順です。すでに読み込んだファイルへのシンボリックリンクやそのコピーは 1 回として数えます。空のファイルは無視されます。

```text
~/.config/mare/AGENTS.md          user instructions, loaded everywhere
/repo/AGENTS.md                   project root
/repo/services/api/AGENTS.md      loaded when you start in services/api
```

プロンプト内では、ファイルは `<project_instructions path="...">` ブロックとして区切られ、ユーザーのファイルが先頭で、その後にルートから作業ディレクトリの順に並びます。合計は 32 KiB (`project_doc_max_bytes`) までに制限されます。上限に達すると、作業ディレクトリから最も遠いファイルから順に除外され、上限をまたぐファイルは `[truncated]` の注記付きで切り詰められ、除外されたファイルは注記に名前が示されます。

モデルが作業ディレクトリより下のファイルを読むと、作業ディレクトリからそのファイルまでの間の各ディレクトリの `AGENTS.md` が、同じサイズの予算の範囲内で、読み取り結果に 1 回だけ付加されます。そのため、モノレポではルールを対象のコードの隣に置けます。

信頼していないプロジェクトの `AGENTS.md` も読み込まれます。モデルへの指示にしかならないためです。ただし、信頼されていないプロジェクト由来であることが明示されます。プロジェクトルートの外を指す指示ファイルや、信頼されていないワークスペース内でシンボリックリンクを辿る指示ファイルは読み込まれずスキップされます。[権限](permissions.md) を参照してください。

`context` プラグインのオプション:

```lua
mare.plugin("context", {
  files = { "AGENTS.override.md", "AGENTS.md", "CLAUDE.md", "GEMINI.md" },  -- names, in priority order
  project_root_markers = { ".git" },       -- {} = only the working directory
  project_doc_max_bytes = 64 * 1024,
})
```

### 作成と編集

- `/init` は、モデルにリポジトリを分析させ、ルートの `AGENTS.md` を作成または改善させます。ビルドとテストのコマンド、アーキテクチャ、実際に使われている規約が対象です。
- `/memory` は、有効な指示ファイルをサイズ付きで一覧表示します。Enter で 1 つの内容を表示できます。`/memory add <text>` はプロジェクトの `AGENTS.md` に箇条書きを追記し、`/memory add --user <text>` はユーザーのファイルに追記します。ファイルがなければ作成されます。
- ファイルは任意のエディターで編集できます。mare は各プロンプトの前に、指示ファイル、`SYSTEM.md`、`APPEND_SYSTEM.md`、すべての `SKILL.md` の更新時刻とサイズを比較し、変更があればシステムプロンプトを作り直します。`/memory add` は、追加したメモが次のセッションから有効になることを伝えます。`/new` で新しいセッションを始められます。

## システムプロンプトファイル

| ファイル | 効果 |
| --- | --- |
| `SYSTEM.md` | 既定のシステムプロンプト全体を置き換えます。 |
| `APPEND_SYSTEM.md` | 既定のプロンプトの末尾にテキストを追加します。 |

mare はまずプロジェクトリソースルート (git ルートまで遡って最初に `.mare/` または `.mcp.json` を含むディレクトリ) の `.mare/` を探し (プロジェクトが信頼されている場合のみ)、次に `~/.config/mare/` を探します。プロンプトを置き換えると、サンドボックスやツールの説明を含め、既定のセクションがすべて削除され、モデルはそれらを見られなくなります。mare は起動時と `mare doctor` でそのことを知らせます。意図的な場合を除いて `APPEND_SYSTEM.md` を使ってください。

同じことは `init.lua` やコマンドラインでも設定できます。

```lua
mare.plugin("context", {
  append = "Always answer in French.",        -- like APPEND_SYSTEM.md
  persona = "You are a careful Rust reviewer.", -- replaces only the identity line
  -- system = "...",                          -- like SYSTEM.md: replaces everything
})
```

```sh
mare --append-system-prompt "Prefer small commits." "tidy up the tests"
```

個々のセクションの追加と削除は、`mare.prompt.section { name = ..., order = ..., text = ... }` と `mare.prompt.remove(name)` で行います。

## スキル

スキルは `SKILL.md` ファイルを含むディレクトリです。`name` と `description` を持つ YAML フロントマターがあり、その後に指示が続きます。mare は Agent Skills のレイアウトに従っているため、他のエージェント向けに書かれたスキルもそのまま使えます。

```text
~/.config/mare/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft release notes from the commits since the last tag. Use when asked to prepare a release.
---

1. Run `git describe --tags --abbrev=0` to find the last tag.
2. List the commits since then and group them by area.
3. Write the notes in `RELEASE_NOTES.md`.
```

スキルは次のディレクトリから検出されます。2 つのスキルが同じ名前を持つ場合は、最初に見つかったものが優先されます。

| ディレクトリ | スコープ |
| --- | --- |
| `~/.config/mare/skills` | ユーザー。 |
| `~/.agents/skills` | ユーザー。他のエージェントと共有。 |
| `.mare/skills` | プロジェクト。プロジェクトリソースルート (`.mare/` または `.mcp.json` を持つ最寄りのディレクトリ、git ルートまで)。信頼済みのプロジェクトのみ。 |
| `.agents/skills` | プロジェクト。同じプロジェクトリソースルート (`mare.project.root`) 基準で解決。信頼済みのプロジェクトのみ。 |
| インストール済み [パッケージ](../configure/packages.md) の `skills/` | パッケージ。 |

一覧は `skill_dirs` オプションで変更できます。相対パスのエントリはプロジェクトのディレクトリとして扱われ、信頼が必要です。

システムプロンプトに入るのは、各スキルの名前、説明、場所だけです。タスクが説明に合致すると、モデルは `read` ツールで `SKILL.md` の全文を読みます。そのため、スキルをいくつインストールしてもプロンプトは小さく保たれます。フロントマターで `disable-model-invocation: true` を設定したスキルは一覧から除外され、あなたが求めたときにだけ使われます。

自分でスキルを読み込むには、`/skill:<name>` を実行します。後ろに任意のテキストを続けられます。

```text
/skill:release-notes for v2.4
```

スキルの指示がメッセージとして送られ、その後にあなたのテキストが続きます。`/skills` はスキルの一覧を開き、スキャンで出た警告を表示します。`description` のないスキルはスキップされます。その他の問題 (小文字の英字、数字、ハイフン以外を含む名前、1,024 文字を超える説明) は警告であり、スキルは読み込まれます。`mare doctor` もこれらを報告します。`allowed-tools` は読み取られますが、強制はされません。

## プロンプトテンプレート

`~/.config/mare/prompts/` または `.mare/prompts/` (プロジェクトのものは信頼済みのプロジェクトが必要) にある Markdown ファイルは、そのファイル名のスラッシュコマンドになります。パッケージも `prompts/` ディレクトリにテンプレートを同梱できます。テンプレートが既存のコマンドを置き換えることはありません。

```markdown
---
description: Review the staged changes
argument-hint: "[focus]"
---
Review the output of `git diff --staged`. Focus on: ${@:-correctness and tests}.
```

`~/.config/mare/prompts/review-staged.md` として保存すると、`/review-staged security` のように実行されます。(組み込みの `/review` のように既存のコマンドと同じ名前のものはスキップされます。)引数は `$1`、`$2` など、すべてをまとめて指す `$@` または `$ARGUMENTS`、既定値を指定する `${1:-default}` と `${@:-default}`、一部を切り出す `${@:N}` または `${@:N:L}` です。引用符で囲んだ語はひとまとまりになります。テンプレートの読み取り元を変更するには `prompt_dirs` を設定します。

## ファイルのメンション

メッセージ内の `@path` は、そのメッセージにファイルの内容を添付します。`@"path with spaces"` はパスを引用符で囲み、ディレクトリを指定するとその一覧を添付します。上限は 1 ファイルあたり 100 KiB、1 メッセージあたり 256 KiB です。[対話型インターフェース](tui.md) を参照してください。コマンドラインでは、`@` で始まる別の引数は、テキストファイルを最初のプロンプトに添付します: `mare "review" @src/lib.rs`。コマンドラインはこれらの上限なしでファイル全体を読み込み、画像や読み取れないパスはそのまま残ります。

## コンパクションがコンテキストを扱う方法

会話がコンテキストウィンドウに近づくと、mare は元の履歴をセッションファイルに残したまま会話メッセージをコンパクションします。システムプロンプト、ツール定義、スキル見出し、指示ファイルは会話履歴に含まれないため、リクエストのたびに再送され、要約されず、ウィンドウを消費し続けます。そのため、非常に大きな `AGENTS.md` はリクエストのたびにトークンを消費します。

コンパクションをまたいでモデルに保持させたい前提は、会話だけでなく `AGENTS.md` に書いてください。手動圧縮時に残したい観点は `/compact <focus>` で指定できます。4 つの圧縮ステージ、保持トークン数、しきい値、各種オプションは [使用量とコンパクション](usage-and-compaction.md) を参照してください。
