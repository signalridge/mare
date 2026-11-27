# 配置文件

用 Lua 配置 mare：init.lua 放在哪里、各配置层如何组合、把配置拆分为模块、重新加载，以及出错时会发生什么。

## 配置存放在哪里

| 路径 | 用途 |
|---|---|
| `~/.config/mare/init.lua` | 你的配置。由 `mare init` 创建。 |
| `~/.config/mare/lua/` | 你在 `init.lua` 中用 `require` 加载的模块。 |
| `~/.config/mare/plugins/` | 你自己的插件。见[插件](../extend/plugins.md)。 |
| `<project>/.mare/init.lua` | 项目资源根（最近含 `.mare/` 或 `.mcp.json` 的目录，止于 git 根）中的项目配置。仅在受信任的项目中运行。 |
| `<project>/.mare/lua/`、`<project>/.mare/plugins/` | 项目的模块和插件。 |
| `~/.local/state/mare/state.json` | 运行时做出的选择（`/model`、`/effort`、`/theme`、`/config` 的编辑）。不要手动编辑。 |
| `~/.config/mare/mare-lock.json` | 包锁定文件。见[包](packages.md)。 |

设置了 `$XDG_CONFIG_HOME` 和 `$XDG_STATE_HOME` 时，它们会取代 `~/.config` 和 `~/.local/state`。`MARE_HOME=<dir>` 会把一切移到 `<dir>/config`、`<dir>/state`、`<dir>/data` 和 `<dir>/cache` 之下，适合做一次性的环境。

如果你对信任提示回答了“是”、在项目中运行过 `mare trust`，或传入了 `--trust`，项目就是受信任的。`mare trust --revoke` 可以撤销前两种。`--no-trust` 在单次运行中忽略该项目。

## 配置层顺序

每个选项可以来自六个层。后面的层覆盖前面的层。

| # | 层 | 示例 |
|---|---|---|
| 1 | 默认值 | [选项](options.md)中列出的值。 |
| 2 | `state.json` | 由 `/model`、`/effort`、`/theme` 保存。 |
| 3 | 用户 `init.lua` | `mare.setup { model = "..." }` |
| 4 | 项目 `.mare/init.lua` | API 相同，仅限受信任的项目。 |
| 5 | 环境变量 | `MARE_BORDER=single` |
| 6 | 命令行 | `--set border=single`、`-m`、`-e` |

实践中有两个细节很重要：

- `state.json` 只填充 `init.lua` 没有设置的顶层选项。如果 `init.lua` 设置了 `models = {...}`，已保存的 `models` 会整体被忽略。
- 环境变量的值会替换整个选项。`--set` 的值则合并进去，所以 `--set plugins.tools.edit_mode=replace` 只改变一个键，其余保持不变。

## 第一个 init.lua

`mare init` 会写出一个带注释的起点和一个 `.luarc.json`。它从不覆盖已有的 `init.lua`。

```sh
mare init
```

```lua
-- ~/.config/mare/init.lua
mare.setup {
  model = "openai/gpt-6.1-terra",
  border = "single",
}

mare.plugin("tools", { bash_timeout_ms = 120000 })
```

`mare.setup` 接受核心选项（`effort` 仅限单次调用通过 `--effort` 或 `MARE_EFFORT` 覆盖，写在 `init.lua` 顶层的全局 `effort` 会被忽略）。表会与已设置的内容递归合并；列表和普通值则替换它。[选项](options.md)页面列出了每个核心选项及其类型和默认值。

## 插件选项

`mare.plugin(name, opts)` 覆盖某个插件的默认值。它使用与 `mare.setup` 相同的合并规则。

```lua
mare.plugin("statusline", { cwd_width = 32, changed_files = false })
mare.plugin("update-check", { enabled = false })
```

`enabled = false` 关闭插件。未知的插件名或选项键会产生警告，并附带“你是不是想写”的提示。同样的值也可以在命令行上给出：

```sh
mare --set plugins.tools.edit_mode=replace
```

## 在运行时读写选项

`mare.o` 读取选项（找不到时回退到默认值）并写入选项。在 `init.lua` 中，写入与 `mare.setup` 相同，区别是表会被替换而不是合并。启动之后，例如通过 `/lua`，写入立即生效，并触发 `option/changed` 事件。

```lua
mare.o.border = "double"
mare.o.plugins.url_format = "https://git.example.com/%s.git"   -- dotted write into a table option
local current = mare.o.model                          -- read; falls back to the default
```

## 模块与 require

如果 `~/.config/mare/lua/` 存在，`init.lua` 中的 `require` 会从那里加载。名称 `a.b` 解析为 `lua/a/b.lua` 或 `lua/a/b/init.lua`。模块名可以包含字母、数字、`_` 和 `-`。模块看到的 `mare` API 与 `init.lua` 相同。

```lua
-- ~/.config/mare/init.lua
require("keys")
require("looks")
```

```lua
-- ~/.config/mare/lua/keys.lua
mare.keymap.set("editor", "<M-g>", "transcript", { desc = "open the transcript" })
```

在项目中，先搜索 `.mare/lua/`，然后是用户目录，最后是标准库。直接或间接地 require 自身的模块是错误。每次对配置求值都从空的模块缓存开始，所以 `/reload config` 会重新运行你的模块。

## 检查结果

`/config` 列出每个选项的值及其来源：`default`、`state.json`、带文件和行号的某一层、`env MARE_<NAME>` 或 `cli`。`/config border` 按名称过滤。你设置的插件选项出现在第二个列表中，配置警告在最后。在完整浮层中，按 Enter 可以编辑字符串、数字或布尔选项；新值立即生效并保存到 `state.json`，但在 `init.lua` 中设置的值在下次启动时仍然优先。`/settings` 是它的别名。

## 重新加载

| 命令 | 效果 |
|---|---|
| `/reload config` | 重新运行 `init.lua` 和项目的 `init.lua`。只重新加载 `plugins.<name>` 选项发生变化的插件（以及依赖它们的插件），并通知发生变化的核心选项。 |
| `/reload` | 包含上面的全部内容，另外会安装新声明的包、启动新包的插件、停止被移除包的插件，并重新加载其他所有插件。 |
| `/reload <plugin>` | 重新加载单个插件。 |

`--set` 的值在重新加载后仍然有效。模型请求正在运行或另一次重新加载正在进行时，重新加载会被拒绝；请稍后再运行一次。包的变化需要完整的 `/reload`，而 `watchdog_ms` 只在启动时读取。

求值失败的重新加载会保持旧配置不变。以下注册会恢复到尝试之前的状态：选项及其来源、来自 `init.lua` 的键位、由配置注册的 UI 组件、由插件配置函数（如 `mare.tool.command`）添加的条目，以及 `mare.on` 订阅。无法撤销的是你的 Lua 在运行期间做的其他任何事：写入的文件、启动的命令、设置的全局变量。请让 `init.lua` 不带副作用。

## init.lua 失败时

`init.lua` 中的错误不会阻止 mare 启动。文件中错误之前的部分已经运行，之后的部分没有。

- 在 TUI 中，对话框会写明文件、行号和错误的第一行。你可以在编辑器中打开文件（`editor` 选项，否则用 `$VISUAL` / `$EDITOR`）、退出或继续。如果继续，所有模型请求都会被阻止，直到文件修复且 `/reload` 成功。状态栏显示 `config error · /reload`。
- 使用 `-p`、`--json` 或 `--rpc` 时没有对话框：mare 打印错误并以退出码 2 退出。

不算错误的失误，例如未知的选项名或类型错误的值，属于警告。它们出现在 `/config` 中，在无界面和 RPC 模式下则以 `mare: config warning: ...` 的形式输出到标准错误。使用 `--strict-config` 时，这两种模式会改为以退出码 2 退出。

```sh
mare -p --strict-config "summarize README.md"
```

要在不带任何配置的情况下启动，使用 `--clean`。它会跳过 `init.lua`、项目配置、用户和项目插件、包以及 `state.json`，所以只剩内置插件和命令行的值。

## 编辑器补全

`mare init` 还会写入 `~/.config/mare/.luarc.json`，让 LuaLS 指向 `mare` API 的 Lua 类型定义，以及已安装包附带的类型文件。mare 版本变化时，定义会被复制到运行时目录；`mare runtime` 会打印该目录（`~/.local/share/mare/runtime`，定义位于 `types/`）。内置插件的源码也在那里，只读，可以阅读，也可以复制到 `~/.config/mare/plugins/`。如果你编辑了 `.luarc.json`，mare 就不再重写它。

## 敏感值

不要把 API 密钥写进 `init.lua`。`mare login` 把凭据保存在 `~/.config/mare/auth.json` 中。对于 provider，`api_key` 接受环境变量名，或一个输出密钥的命令：

```lua
mare.provider("openai", {
  base_url = "https://proxy.example.com/v1",
  api_key = { cmd = { "pass", "show", "proxy/openai" }, refresh_interval_ms = 3600000 },
})
```

`/config` 和 RPC 的 `options.get` 方法会隐藏键名看起来像密钥的值（`api_key`、`token`、`secret`、`password`、`credential`、`authorization`、`cookie`、`headers`、`query`、`env`），以及带有查询字符串或用户名的 URL。可以用同样的方式标记你自己的插件选项：

```lua
mare.config_sensitive("plugins.myplugin.private_value")
```

这只影响 mare 显示的内容，并不会加密文件。
