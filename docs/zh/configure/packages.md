# 包

用 mare.plugins.add 从 git 安装插件、主题和技能：声明与固定版本、锁定文件、mare plugin 命令以及启动行为。

## 声明包

`mare.plugins.add` 接受一个列表。每个条目是字符串或表。

```lua
-- ~/.config/mare/init.lua
mare.plugins.add {
  "someone/mare-git-tools",                              -- newest semver tag, else the default branch
  { "someone/mare-extras", version = "^1.2" },           -- a semver range over the tags
  { "someone/monorepo:mare", only = { "tidy" } },        -- a subdirectory; load only some plugins
  { "git@example.com:team/internal-tools.git", branch = "main" },
  { dir = "~/code/my-plugin" },                          -- a local directory, never installed
  { import = "packages" },                               -- more declarations from lua/packages.lua
}
```

短名称 `owner/repo` 会通过 `plugins.url_format` 选项展开（默认为 `https://github.com/%s.git`）。完整地址也可以：URL、`ssh` 地址或路径。只使用 `https`、`ssh` 和 `file` 传输，git 从不提示输入密码。普通的 `http://` 和 `git://` 需要 `plugins = { allow_insecure = true }`。

表条目的字段：

| 字段 | 含义 |
|---|---|
| `[1]` | 来源：`owner/repo`、`owner/repo:subdir` 或 git 地址。 |
| `name` | 显示名称。没有 `[1]` 时，它就是来源。 |
| `only` | 要从包中加载的插件名称。其余的被忽略。 |
| `subdir` | 仓库的子目录（等同于 `owner/repo:subdir`）。 |
| `version`、`tag`、`branch`、`commit`、`rev` | 要检出的内容。见下文。 |
| `pin` | `true`：`update` 让它保持原位。 |
| `submodules` | `false`：不获取 git 子模块。 |
| `dir` | 本地目录，从那里加载，从不安装也不锁定。 |
| `dependencies` | 先加载的包（字符串或完整条目）。 |
| `enabled` | `false`，或返回布尔值的函数。禁用的包既不安装也不加载；它的锁定条目保留。 |
| `required` | `true`：该包安装或加载失败时会阻断模型请求，直到修复并 `/reload`（与仅控制声明合并的 `optional` 独立）。 |
| `optional` | 仅控制声明合并：只有在别处也声明了该包时，这个条目才算数。 |
| `opts` | 按插件的默认值 `{ pluginname = { ... } }`，或返回它们的函数。 |
| `opts_extend` | `opts` 中的点分路径，其列表在多次声明之间追加而不是替换。 |
| `init` | `function(pkg)`，在包的插件加载之前运行。 |
| `config` | `function(pkg, opts)`，在包的插件加载之后运行，`/reload` 时再次运行。 |
| `import` | `lua/` 下返回更多声明的模块。 |

多次声明的包会被合并：单值字段以最后一个值为准，`opts` 深度合并，`dependencies` 和 `only` 累加，最后一个显式的 `enabled` 值生效（后面的声明省略该字段时保留此前的值）。来自 `opts` 的选项位于 `mare.plugin` 之下：你用 `mare.plugin` 设置的值始终优先。

```lua
mare.plugins.add {
  {
    "someone/mare-extras",
    version = "^1.2",
    opts = { tidy = { max_lines = 80 } },
    config = function(pkg, opts)
      mare.log("info", pkg.name .. " ready")
    end,
  },
}
```

### 把声明拆分到多个文件

`{ import = "packages" }` 会加载 `lua/packages.lua`、`lua/packages/init.lua`、每个 `lua/packages/*.lua` 以及每个 `lua/packages/<子目录>/init.lua`。每个文件返回一个声明列表。导入最多嵌套 16 层，每个模块只导入一次，循环会被跳过并给出警告。

```lua
-- ~/.config/mare/lua/packages/git.lua
return {
  "someone/mare-git-tools",
  { "someone/mare-extras", enabled = function() return mare.os ~= "windows" end },
}
```

## 固定版本

下面这些字段中，出现的第一个决定目标，顺序如下：

| 字段 | 目标 |
|---|---|
| `commit = "<sha>"` | 该提交。`update` 从不移动它。 |
| `tag = "v1.4.0"` | 该标签。 |
| `branch = "main"` | 该分支的头部。 |
| `version = "^1.2"` | semver 范围内最新的标签。 |
| `rev = "<ref>"` | 任意 git 修订。`rev = "=<sha>"` 是精确固定。 |
| 无 | 最新的 semver 标签，否则为默认分支。 |

加上 `pin = true` 可以让任何包保持原位，直到你修改声明。

## 锁定文件

`~/.config/mare/mare-lock.json`（以及受信任项目中的 `.mare/mare-lock.json`）为每个包记录它的 URL、提交、锁定时所依据的约束、它跟随的分支，以及检出内容的哈希。把它和你的 dotfiles 或仓库一起提交，就能在每台机器上得到相同的包。mare 在每个包完成后写入它，所以某个包失败不会丢失其他包。无法解析的锁定文件会阻止常规包管理命令且不改动已有检出；优先运行 `mare plugin restore --repair`（或 `mare doctor --fix`）从干净的已安装检出重建锁文件，或手工修复、删除后运行 `mare plugin update`。

`plugins.json` 使用版本 2：`{ "version": 2, "plugins": [{ "source": "owner/repo", "name": "plugin" }] }`。用户或项目作用域仍引用的共享检出会保留。

检出内容位于 `~/.local/share/mare/packages/<host>/<path>`。

## 命令

| 命令 | 作用 |
|---|---|
| `mare plugin sync` | 在锁定的提交上安装缺失的包。如果 `init.lua` 中的约束发生了变化，会重新解析。 |
| `mare plugin update [name]` | 把包（或其中一个）移到其约束允许的最新修订，列出新的提交，并重写锁定文件。精确固定的保持不变。 |
| `mare plugin check [name...] [--json]` | 获取并比较修订，不移动检出或修改锁定文件。 |
| `mare plugin restore [name]` | 将全部来源或指定来源检出到锁定的提交，锁定文件不变。 |
| `mare plugin restore --repair` | 从干净的已安装检出重建缺失或损坏的 lock。 |
| `mare plugin list [--json]` | 列出已声明的来源及其状态。 |
| `mare plugin tree [--json]` | 显示运行时插件树。 |
| `mare plugin install <name...>` | 用短名称启用目录插件并安装所需来源。 |
| `mare plugin uninstall <name...>` | 用短名称禁用目录插件并清理不再使用的来源。 |
| `mare plugin clean` | 移除不再声明的检出内容和过期的锁定条目。 |

```sh
mare plugin sync
mare plugin update someone/mare-extras
mare plugin list
```

获取并行进行（`plugins.concurrency`，默认 8），每个 git 操作都有超时（`plugins.timeout`，默认 120 秒），Ctrl-C 会取消它们。带有本地修改的检出内容会被标记，并且 `clean` 绝不会删除它。`mare plugin` 命令会按 `--trust`、`--no-trust` 或已保存的信任决定加载配置：在受信任的项目中会读取并执行 `.mare/init.lua`，且项目作用域可使用 `.mare/mare-lock.json`。如果只想操作用户级配置，请传入 `--no-trust`。

在 TUI 内，`/plugins` 打开一个面板，提供针对全部或某一个包的相同操作，另外有日志和差异视图，以及一个用于开关目录中包的 Discover 标签页（保存在 `~/.config/mare/plugins.json`）。它还会报告每一行的状态：已安装、落后或领先于锁定、有本地更改、缺失、已禁用、本地目录，或未加载及原因。`plugins` 选项见[选项](options.md)：

首次交互式打开时，mare 会提议获取官方插件目录并选择可选插件；默认没有勾选任何项。可以跳过，以后在 `/plugins` → Discover 中选择。用 `mare.plugin("onboarding", { plugins = false })` 关闭引导；当 `plugins.json` 不存在时，删除 `state.json` 中的 `onboarding.plugins` 可以重新引导。

```lua
mare.setup {
  plugins = { url_format = "https://git.example.com/%s.git", timeout = 60, concurrency = 4 },
}
```

## 启动与重新加载

启动时，在插件加载之前，mare 会在锁定的提交上安装缺失的已声明包，并在 TUI 中显示进度。然后运行每个包的 `init` 钩子，按依赖顺序加载每个已安装包的插件，最后运行每个包的 `config` 钩子。

必需包及其钩子、核心插件或显式必需插件失败时，请求会被阻止，修复并重载后恢复。可选包或插件失败只产生警告，其他插件和会话继续运行；provider 失败只影响使用其模型的请求。

包的变化（同步、升级、对 `init.lua` 的修改）在 `/reload` 时生效：新包被安装并启动，被移除的包被卸载，`config` 钩子再次运行。当磁盘上的 `init.lua` 或 `plugins.json` 发生变化时，mare 也会注意到并提示你运行 `/reload`。

`--clean` 会完全跳过包。

## 包里有什么

| 目录 | 由谁加载 |
|---|---|
| `plugins/` | 插件加载器：`name.lua` 或 `name/init.lua` 文件。见[插件](../extend/plugins.md)。 |
| `skills/`、`prompts/` | 上下文：技能和提示词模板。 |
| `themes/` | 主题文件。见[外观](ui.md)。 |
| `types/` | LuaLS 定义，会为你的编辑器镜像一份。 |

根目录中可选的 `mare-plugin.toml` 可以设置 `requires = ">=0.4"`（需要更新 mare 的包不会加载）、`dependencies`、`name`、`description` 和 `version`。要发布你自己的包，推送一个包含这些目录的仓库，并用 `v1.0.0` 这样的 semver 标签给版本打标签。[编写插件](../extend/writing-a-plugin.md)展示了目录布局。
