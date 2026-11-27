# 故障排查

如何查明 mare 为什么无法启动、无法连接模型、行为与你的配置不符，或在你的终端里显示异常。

## 先运行 mare doctor

```sh
mare doctor
```

`mare doctor` 运行时不会调用模型。它每项检查输出一行，任何检查失败就以 `1` 退出（否则为 `0`）。对勾表示通过，`!` 是不导致失败的警告，叉号表示失败。`mare doctor --json` 把同样的检查结果作为一个 JSON 文档输出，`mare doctor --fix` 会创建缺失的目录、收紧权限，并尝试修复损坏的包锁定文件（运行本身仍会做正常的启动工作，例如准备会话索引和运行时文件）。

在会话内，`/doctor` 会以浮层形式显示正在运行的会话所知道的检查项。

| 检查 | 它告诉你什么 |
| --- | --- |
| `directories`、`permissions` | 配置、状态、数据、会话和日志目录是否存在；`auth.json`、状态目录和会话目录是否为私有权限（`0600` / `0700`），会话目录下直接存放的 `.jsonl` 文件也一样；子目录中按项目存放的会话文件不会被检查。`--fix` 可修复这两项 |
| `terminal capabilities` | mare 检测到的终端，以及它是否支持 truecolor、宽 emoji、kitty 键盘协议和括号粘贴，另有该终端的已知问题 |
| `config`、`model`、`config options` | `init.lua` 能否加载、设置了哪个模型，以及 `mare.setup`、`mare.o` 和 `mare.plugin` 中未知的选项名或类型错误 |
| `auth.json`、`auth.json mode` | 无法解析的凭据文件会被报告，而不会被当作“没有凭据”；mare 会保留一份副本 `auth.json.bak` |
| `sandbox` 和 `sandbox ...` | 本机上的沙箱后端（例如 Linux 上的 `bwrap` 或 `landlock`，macOS 上的 Seatbelt）以及它能强制执行哪些能力 |
| `tmux clipboard`、`tmux keys` | 仅在 tmux 内：`set-clipboard` 和 `extended-keys` 是否开启 |
| `session files`、`session index` | 每个已保存的会话是否都能解析；写到一半的最后一条记录会作为警告报告；会话索引是否一致 |
| `runtime`、`plugins` | 运行时目录是否已解压；有多少插件处于活动状态以及每个插件的问题，包括哪些插件覆盖了内置插件 |
| `permission policy`、`key conflicts` | 正在使用的权限服务和会话沙箱级别；你的终端或操作系统可能拦截的键位 |
| `credential sources` | 每个 provider 的凭据来自哪里（已保存的登录、环境变量或没有）。密钥绝不会被打印 |
| connectivity | 你已配置的 provider 是否可达，而不是固定的列表 |
| `mcp ...`、`packages`、`skills`、`git` | 由插件提供的检查：MCP 服务器、已安装的包和锁定文件、已加载的技能、`PATH` 上的 `git` |

在 Windows 上，`mare doctor` 还会报告旧版控制台宿主，以及 Windows Terminal 是否转发 `Shift+Enter` 和 `Alt+Enter`。

插件通过监听 `kernel/doctor` 事件添加自己的检查；见[服务与事件](../extend/services-and-events.md)。

## 找出原因

下面这些标志可以缩小问题范围。可以与正常启动组合使用。

| 标志 | 效果 |
| --- | --- |
| `--clean` | 跳过 `init.lua`、包、用户插件和项目插件。只加载内置插件，且不信任项目 |
| `--strict-config` | 配置有警告时拒绝启动（退出码 `2`），而不是把警告打印出来。适用于无界面和 RPC 运行 |
| `--no-trust` | 本次运行不信任此项目，无论之前保存了什么决定 |
| `--no-session` | 不读取也不写入会话 |
| `--set KEY=VALUE` | 在本次运行中覆盖一个配置值，例如 `--set plugins.tools.edit_mode=replace` |

如果 `mare --clean` 能正常工作而 `mare` 不能，问题就出在 `init.lua`、某个包或某个插件上。可以通过注释掉 `init.lua` 的部分内容来二分定位。如果问题出在某个项目里，`mare --no-trust` 可以告诉你是否是它的 `.mare/` 文件导致的。

要在不动真实目录的情况下用全新的目录试用 mare，把 `MARE_HOME` 指向一个空目录：`MARE_HOME=$(mktemp -d) mare doctor`。

## 启动慢

```sh
mare --profile-startup            # start the UI, quit when ready, print the time of each stage
mare --startuptime /tmp/start.txt # normal start, write the timing to a file (like nvim --startuptime)
```

在会话内，`/profile` 会显示启动时间线和每个插件的加载时间，以及两次模型请求之间工具列表的变化（工具列表变化会使 provider 的提示词缓存从该点起失效）。

## 配置错误

mare 区分警告和错误。

- 警告包括未知的选项名、错误的类型，以及无法解析的 `MARE_*` 环境变量值。当存在相近的名称时，选项名会附带“你是不是想写”的提示。无界面和 RPC 运行会把警告打印到标准错误；加上 `--strict-config` 可以让它们拒绝启动（退出码 `2`）。`/config` 列出每个选项、它的值以及来自哪里。
- 错误是指 `init.lua`（或受信任项目的 `.mare/init.lua`）运行失败。交互式 UI 会显示一个对话框，包含文件、行号和错误的第一行，并提供三个选择：在编辑器中于该行打开文件（之后配置会重新运行）、退出，或继续。如果选择继续，UI 会启动，但在文件修复且 `/reload` 成功之前，所有模型请求都会被阻止。无界面和 RPC 运行没有对话框，会以 `2` 退出。
- 普通可选插件或包加载失败会报告警告并跳过。`/plugins` 列出每个插件及其来源，并在下方列出每个失败及其回溯；回溯会一直保留，直到完整的 `/reload` 将其清除。启动提示会说“fix it and /reload (or start clean: mare --clean)”。
- 关键服务、声明 `required = true` 的插件或必需包（`required = true`）加载失败会阻断模型请求，直到修复并 `/reload`（或禁用）。`mare plugin sync` 安装 `init.lua` 声明的包，`mare plugin list` 显示它们，在 `init.lua` 中禁用必需包可解除阻断。

编辑后重新加载：`/reload` 重新加载插件，`/reload <plugin>` 重新加载单个插件，`/reload config` 重新加载配置。

## 凭据与模型

| 现象 | 检查什么 |
| --- | --- |
| “X is not configured: run /login X or set ...” | `mare login status` 显示每个 provider 的状态。登录，或导出该 provider 的变量（见[快速上手](quickstart.md#登录-provider)） |
| 环境中的密钥被忽略 | `auth.json` 中已保存的登录优先于环境变量。`mare doctor` 会指出已设置但未使用的来源。`mare logout <provider>` 会删除已保存的登录 |
| `mare login anthropic` 提示要通过标准输入传入密钥 | API 密钥类 provider 只从标准输入读取密钥：`printenv ANTHROPIC_API_KEY \| mare login anthropic --with-api-key` |
| ChatGPT 浏览器登录无法打开浏览器 | 通过 SSH，或在没有 `DISPLAY` 或 `WAYLAND_DISPLAY` 的 Linux 上，使用 `mare login openai --device-auth` |
| `/model` 把某个 provider 显示为“needs login” | 该 provider 还没有凭据；运行 `/login <provider>` |
| 模型列表过旧或缺少某个模型 | `mare model refresh` 会重新读取每个已登录 provider 的模型列表 |
| `auth.json` 已损坏 | 在修复之前，已保存的凭据既不能读取也不能修改。修复该文件，或删除后重新登录；`auth.json.bak` 保存着 mare 做的副本 |
| `mare logout` 删除了登录，但 provider 仍然可用 | 凭据还来自环境变量或 `init.lua`；`logout` 会告诉你该删除哪一个 |

只要没有任何 provider 有凭据，交互式 UI 就会自动启动首次运行引导。

## 终端问题

- 运行 `/terminal-setup`，检查 `Shift+Enter` 和 `Alt+Enter` 是否能传到 mare，并查看如何修复。没有 `Shift+Enter` 时，输入 `\` 再按 Enter 即可换行。
- 在 tmux 内，除非 `tmux.conf` 中有 `set -g extended-keys on`，否则 `Shift+Enter` 到达时就是 Enter；只有设置了 `set -g set-clipboard on`，复制的内容才能到达系统剪贴板。`mare doctor` 会检查这两项。
- `TERM=dumb` 会选择不带光标移动的纯逐行模式。
- `mare doctor` 会列出终端或操作系统通常会拦截的键位（`key conflicts`）及修复方法。用 `mare.keymap.set` 重新绑定；见[键位映射](../configure/keymaps.md)。
- 某个程序直接向终端写入后屏幕出现乱码：按 `Ctrl+L` 重绘。写到 mare 标准输出和标准错误的输出（包括子进程的）会被捕获并显示在输入框上方；只有自己打开终端设备的程序才会干扰屏幕。
- 图片显示为 `[image]`：终端不支持 Kitty、iTerm2 或 sixel 中的任何一种协议。`mare doctor` 会显示检测到的能力。
- 在 Windows 上，`Ctrl+V` 是终端的文本粘贴，所以请用 `Alt+V` 粘贴图片，并且 `Ctrl+Z` 不能挂起 mare。

## 会话

- `mare doctor` 中的 `session files ... damaged`：指出的文件无法解析。请以只读方式打开它，或分叉它，而不要继续它。
- `session torn writes`：最后一条记录写到一半（通常发生在崩溃之后）。它会被忽略，并在下次恢复时被截掉。
- `session index ... degraded`：`/resume` 会退回到扫描会话文件。
- 在一轮进行中崩溃或被终止后，在恢复的会话中运行 `/continue` 来继续被中断的那一轮。

## 日志、崩溃报告和状态

| 内容 | 位置 |
| --- | --- |
| 日志 | `~/.local/state/mare/log/mare.log`，达到 10 MB 时轮转，保留 5 个旧文件，已脱敏，权限 `0600` |
| 崩溃报告 | `~/.local/state/mare/crashes/<time>/report.txt`，仅在 panic 导致 mare 结束时写入 |
| 会话 | `~/.local/state/mare/sessions/` |
| 运行时选择（模型、强度） | `~/.local/state/mare/state.json` |
| 信任决定 | `~/.local/state/mare/trust.json` |
| 凭据 | `~/.config/mare/auth.json` |

默认日志级别是 `info`。可以用环境变量 `MARE_LOG` 为单次运行调高级别，它接受 `tracing` 过滤器：

```sh
MARE_LOG=debug mare
MARE_LOG=info,mare_binding=trace mare
```

在会话内，`/debug` 显示最近的日志行和最新的崩溃报告。目录会随 `XDG_STATE_HOME`、`XDG_CONFIG_HOME`、`XDG_DATA_HOME` 和 `MARE_HOME` 移动。

## 沙箱与权限

`mare doctor` 会打印沙箱后端，以及它能和不能强制执行的内容。`sandbox deny-read` 上的 `!` 表示本机无法限制读取，因此经批准的提权会按批准的内容离开沙箱。在会话内，`/permission caps` 显示同样的能力摘要，`/permission log` 显示本会话的权限决定，`/permission` 切换预设。`--permission full-access`（`--yolo`）会移除沙箱且不再询问，危险命令除外：它们在所有预设下仍会询问（在无界面运行中会被拒绝）。

如果无界面运行以 `3` 退出，说明有一次审批被回答为“拒绝并停止”（`abort`）并因此停止了这一轮。无人回答的审批只会被拒绝：模型得到一次拒绝，运行仍可能以 `0` 退出，被拒绝的调用会列在标准错误上。要避免拒绝，请选择不需要该审批的预设（`--permission`），或设置 `--approval-policy never`，这样请求会被直接拒绝而不再提示。
