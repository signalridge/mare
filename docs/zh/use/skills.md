# 技能与提示词模板

技能通过 `SKILL.md` 打包可复用指令，平时不占上下文窗口、按需读取；提示词模板将 Markdown 文件映射为支持参数展开的 `/命令`。

## 技能

技能是一个包含 `SKILL.md` 的目录，文件顶部写 YAML frontmatter，下面写 Markdown 指令：

```text
~/.config/mare/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft release notes from commits since the last tag. Use when asked to prepare a release.
argument-hint: "[tag]"
---

1. 运行 `git describe --tags --abbrev=0` 找到上一个标签。
2. 按模块整理自该标签以来的提交。
3. 把草稿写入 `RELEASE_NOTES.md`。
```

进入系统提示词的只有每个技能的 `name`、`description` 和文件路径。任务命中某个技能的描述时，模型再调用 `read` 工具读取 `SKILL.md` 正文。装几十个技能也不会拖大每轮请求的上下文。

### 发现顺序与项目信任

`context` 插件按下面顺序扫描目录。两个技能同名时，最先找到的生效：

| 顺序 | 目录 | 作用域 | 需要项目信任 |
|---|---|---|---|
| 1 | `~/.config/mare/skills/` | 用户级 | 否 |
| 2 | `~/.agents/skills/` | 用户级，与其他工具共享 | 否 |
| 3 | `.mare/skills/` | 项目资源根目录（到 `.git` 为止最近的 `.mare/` 或 `.mcp.json`） | 是 |
| 4 | `.agents/skills/` | 同样按项目资源根目录（`mare.project.root`）解析 | 是 |
| 5 | `<package>/skills/` | 已安装的[包](../configure/packages.md) | 否 |

在 `init.lua` 中用 `skill_dirs` 调整扫描目录。相对路径按 `mare.project.root` 解析为单个根目录，需要[项目信任](permissions.md#项目信任)。

```lua
mare.plugin("context", {
  skill_dirs = { "~/.config/mare/skills", "~/.agents/skills", ".mare/skills", ".agents/skills" },
})
```

> 启动时的项目信任探测会上溯到 `.git` 查找 `.agents/skills`，祖先目录里的 `.agents/skills/` 可能触发信任提示，但技能加载器只扫描解析出的项目根。

### Frontmatter 字段与校验

| 字段 | 必填 | 作用 |
|---|---|---|
| `name` | 否 | 默认取目录名。仅限小写 `a-z`、`0-9` 和连字符 `-`，最长 64 个字符。 |
| `description` | 是 | 展示给模型和 `/skills`。缺少 `description` 的技能会被跳过。最长 1024 个字符。 |
| `argument-hint` | 否 | 在 `/` 补全菜单里跟在 `/skill:<name>` 后面提示参数。 |
| `disable-model-invocation` | 否 | 设为 `true` 时不放进系统提示词，只能手动输入 `/skill:<name>` 调用。 |
| `compatibility` | 否 | 长度上限 500 个字符，超出时给出诊断警告。 |
| `allowed-tools` | 否 | 会解析记录，但 mare 不按该字段限制工具权限。 |

命名或长度问题只会记录警告，不会阻止启动。用 `/skills` 或 `mare doctor` 可以查看警告和被跳过的技能。

### 手动调用技能

任何已加载的技能都能当成斜杠命令直接调用，后面可附带参数文本：

```text
/skill:release-notes v0.2.0
```

直接输入 `/skills` 会打开可搜索的技能列表，并打印扫描阶段收集到的警告。每次发提示前，mare 会比对 `SKILL.md` 的修改时间和文件大小，有变化就重建缓存的系统提示词。

## 提示词模板

放在 `~/.config/mare/prompts/` 或 `.mare/prompts/` 下的 Markdown 文件，会按文件名（去掉 `.md`）注册成一个斜杠命令。已安装的包也可以在 `prompts/` 下自带模板。

同名冲突时，项目模板覆盖用户模板，用户模板覆盖包内模板。提示词模板绝不会覆盖 `/review` 这类内置或插件命令。

```markdown
---
description: 提交前审阅已暂存的改动
argument-hint: "[focus]"
---
检查 `git diff --staged` 的输出。
重点关注：${@:-正确性、边界条件和测试}。
```

保存为 `~/.config/mare/prompts/review-staged.md` 后，在会话里运行：

```text
/review-staged 并发与错误处理路径
```

### 参数替换

模板支持带引号的 shell 分词（例如 `"error paths"`）和这些占位符：

| 占位符 | 展开结果 |
|---|---|
| `$1`–`$9` | 单个数字的位置参数（未传则为空字符串；裸写 `$10` 会按 `$1` 加字面量 `0` 展开） |
| `$@`、`$ARGUMENTS` | 用空格拼接的全部参数 |
| `${N:-default}` | 第 `N` 个位置参数（支持多位数字，如 `${10:-fallback}`），为空或未传时取 `default` |
| `${@:-default}` | 全部参数，未传参数时取 `default` |
| `${@:N}` | 从第 `N` 个起的所有参数（支持多位数字） |
| `${@:N:L}` | 从第 `N` 个起最多 `L` 个参数（支持多位数字） |

在 `init.lua` 里用 `prompt_dirs` 覆盖模板目录：

```lua
mare.plugin("context", {
  prompt_dirs = { "~/.config/mare/prompts", ".mare/prompts" },
})
```

`prompt_dirs` 里的相对路径同样需要项目处于受信任状态（`mare trust`）。

## 相关页面

- [上下文与指令文件](context.md)：`AGENTS.md`、`SYSTEM.md` 与 `APPEND_SYSTEM.md`
- [包](../configure/packages.md)：通过 git 包分发技能和提示词模板
- [权限与沙箱](permissions.md#项目信任)：项目信任记录的保存与检查方式
