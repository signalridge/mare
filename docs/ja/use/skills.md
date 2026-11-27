# スキルとプロンプトテンプレート

スキルは `SKILL.md` 形式で手順をまとめ、必要なときだけ読み込みます。プロンプトテンプレートは Markdown ファイルを引数展開付きの `/command` に変換します。

## スキル

スキルは、YAML フロントマターと Markdown の手順を書いた `SKILL.md` を含むディレクトリです。

```text
~/.config/mare/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft release notes from commits since the last tag. Use when asked to prepare a release.
argument-hint: "[tag]"
---

1. Run `git describe --tags --abbrev=0` to find the previous tag.
2. Group commits since that tag by area.
3. Write the draft in `RELEASE_NOTES.md`.
```

システムプロンプトに入るのは、各スキルの `name`、`description`、ファイルパスだけです。タスクが `description` に一致すると、モデルは `read` ツールで `SKILL.md` 本文を読み込みます。そのため、多数のスキルを入れても毎ターンのプロンプトが肥大化しません。

### 探索順とプロジェクトの信頼

`context` プラグインは次のディレクトリを上から順にスキャンします。同じ `name` を持つスキルが複数ある場合は、最初に見つかったものが使われます。

| 順序 | ディレクトリ | スコープ | プロジェクト信頼 |
|---|---|---|---|
| 1 | `~/.config/mare/skills/` | ユーザー | 不要 |
| 2 | `~/.agents/skills/` | ユーザー (エージェント間で共有) | 不要 |
| 3 | `.mare/skills/` | プロジェクトリソースルート (`.git` までの直近の `.mare/` または `.mcp.json`) | 必要 |
| 4 | `.agents/skills/` | 同じプロジェクトリソースルート (`mare.project.root`) に対して解決 | 必要 |
| 5 | `<package>/skills/` | インストール済み[パッケージ](../configure/packages.md) | 不要 |

検索パスは `init.lua` の `skill_dirs` で変更します。相対パスは `mare.project.root` から 1 ルートに解決され、[プロジェクトの信頼](permissions.md#プロジェクトの信頼) が必要です。

```lua
mare.plugin("context", {
  skill_dirs = { "~/.config/mare/skills", "~/.agents/skills", ".mare/skills", ".agents/skills" },
})
```

> 起動時の信頼プローブは `.git` まで親ディレクトリを遡って `.agents/skills` を探すため、親ディレクトリの `.agents/skills/` で信頼確認が出る場合がありますが、スキルローダーがスキャンするのは解決されたプロジェクトルートのみです。

### フロントマターのフィールドと検証

| フィールド | 必須 | 動作 |
|---|---|---|
| `name` | 任意 | 省略時はディレクトリ名になります。小文字 `a-z`、`0-9`、ハイフン (`-`) で 64 文字までです。 |
| `description` | 必須 | モデルと `/skills` に表示されます。`description` がないスキルは読み込まれません。最大 1024 文字です。 |
| `argument-hint` | 任意 | `/` 補完メニューで `/skill:<name>` の横に表示されます。 |
| `disable-model-invocation` | 任意 | `true` にするとシステムプロンプトから除外され、ユーザーが `/skill:<name>` を実行したときだけ動きます。 |
| `compatibility` | 任意 | 500 文字まで検証され、超過すると警告が出ます。 |
| `allowed-tools` | 任意 | パースされますが、mare 側でのツール制限としては強制されません。 |

名前や文字数の問題は警告として記録され、起動は止まりません。スキップされたスキルや警告は `/skills` や `mare doctor` で確認できます。

### スキルを手動で実行する

読み込まれたスキルはスラッシュコマンドとして呼び出せます。引数を渡すと指示文の末尾に追記されます。

```text
/skill:release-notes v0.2.0
```

引数なしで `/skills` を実行すると、検索可能なスキル一覧が開き、スキャン時の警告も表示されます。mare は各プロンプトの送信前に `SKILL.md` の更新時刻とファイルサイズを確認し、変更があればキャッシュされたシステムプロンプトを作り直します。

## プロンプトテンプレート

`~/.config/mare/prompts/` または `.mare/prompts/` に Markdown ファイルを置くと、ファイル名 (`.md` なし) をコマンド名とするスラッシュコマンドが登録されます。インストール済みパッケージの `prompts/` にあるテンプレートも読み込まれます。

コマンド名が重複した場合、プロジェクトのテンプレートがユーザー設定を上書きし、ユーザー設定がパッケージを上書きします。組み込みコマンドや `/review` などのプラグインコマンドは上書きできません。

```markdown
---
description: Review staged changes before committing
argument-hint: "[focus]"
---
Inspect the output of `git diff --staged`.
Focus on: ${@:-correctness, edge cases, and tests}.
```

これを `~/.config/mare/prompts/review-staged.md` に保存すると、次のように使えます。

```text
/review-staged concurrency and error paths
```

### 引数の展開

テンプレートはシェル風のクォート (たとえば `"error paths"`) と次のプレースホルダーに対応しています。

| プレースホルダー | 展開結果 |
|---|---|
| `$1`–`$9` | 1 桁の位置引数 (未指定時は空文字。裸の `$10` は `$1` とリテラル `0` に展開されます) |
| `$@`, `$ARGUMENTS` | 全引数をスペース区切りで連結した文字列 |
| `${N:-default}` | `N` 番目の位置引数 (`${10:-fallback}` のような複数桁に対応)、空または省略時は `default` |
| `${@:-default}` | 全引数、引数がない場合は `default` |
| `${@:N}` | `N` 番目以降のすべての引数 (複数桁に対応) |
| `${@:N:L}` | `N` 番目から最大 `L` 個の引数 (複数桁に対応) |

テンプレートディレクトリを変更するには `init.lua` で `prompt_dirs` を指定します。

```lua
mare.plugin("context", {
  prompt_dirs = { "~/.config/mare/prompts", ".mare/prompts" },
})
```

`prompt_dirs` の相対パスは、プロジェクトが `mare trust` で信頼された後にのみ読み込まれます。

## 関連項目

- [コンテキストと指示ファイル](context.md): `AGENTS.md`、`SYSTEM.md`、`APPEND_SYSTEM.md` の読み込み順
- [パッケージ](../configure/packages.md): git パッケージ経由でのスキルやテンプレートの配布
- [権限とサンドボックス](permissions.md#プロジェクトの信頼): プロジェクト信頼の保存方法と検証
