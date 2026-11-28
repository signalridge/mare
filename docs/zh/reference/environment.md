# 环境变量

mare 读取的所有环境变量（按控制内容分组），以及 mare 为它所启动的命令设置的变量。对于 `MARE_<OPTION>` 配置类变量，环境变量优先于 `init.lua`，而命令行标志优先于环境变量（其他分节如有不同优先级，会在对应条目中说明，例如 provider 凭据）。

> 本参考页由人工维护（不经 `scripts/gen_docs.py` 自动生成）。

## 目录

mare 在所有平台（包括 Windows 和 macOS）上都使用 XDG 风格的位置。每个目录中放什么，见[文件](files.md)。

| 变量 | 效果 |
|---|---|
| `MARE_HOME` | 四个目录的根。配置、状态、数据和缓存分别变为它下面的 `config`、`state`、`data` 和 `cache`。优先于 `XDG_*` 变量。适合一次性运行和测试。 |
| `XDG_CONFIG_HOME` | 配置目录为 `$XDG_CONFIG_HOME/mare`。默认 `~/.config/mare`。 |
| `XDG_STATE_HOME` | 状态目录为 `$XDG_STATE_HOME/mare`。默认 `~/.local/state/mare`。 |
| `XDG_DATA_HOME` | 数据目录为 `$XDG_DATA_HOME/mare`。默认 `~/.local/share/mare`。 |
| `XDG_CACHE_HOME` | 缓存目录为 `$XDG_CACHE_HOME/mare`。默认 `~/.cache/mare`。 |
| `HOME`、`USERPROFILE` | 上述默认值以及路径中 `~` 所依据的主目录。`USERPROFILE` 是 Windows 上的后备。 |

空值视为未设置。

## 配置

配置层按以下顺序应用，靠后的优先：选项默认值、`state.json`、用户 `init.lua`、项目 `.mare/init.lua`、下面的 `MARE_<OPTION>` 变量，以及命令行标志（`--set`、`-m`、`-e`）。

| 变量 | 效果 |
|---|---|
| `MARE_<OPTION>` | 设置同名的核心选项（大写）：`MARE_THEME`、`MARE_BORDER`、`MARE_REDUCE_MOTION`、`MARE_UPDATE_CHECK` 等。每个核心选项都有一个（见[选项](../configure/options.md)）。值按 JSON 解析，不是 JSON 时作为字符串使用：`MARE_REDUCE_MOTION=true`、`MARE_PERF='{"highlight_cache":64}'`。以 `{`、`[` 或 `"` 开头但无法解析的值会被忽略并给出警告。 |
| `MARE_MODEL`、`MARE_EFFORT` | 本次调用的模型和推理强度。`MARE_EFFORT` 仅适用于本次调用。 |
| `MARE_TRUST_DEFAULT` | 没有任何记录时的默认项目信任：`ask`、`trust` 或 `distrust`（也接受 `always` 和 `never`）。 |
| `MARE_WATCHDOG_MS` | 从不 yield 的 Lua 代码的单个回调时间限制，单位为毫秒。默认 10000。看门狗在 `init.lua` 之前启动，所以只能在这里或用 `--set watchdog_ms=...` 设置。 |
| `MARE_LUA_MEMORY_MB` | Lua 虚拟机的内存上限（单位 MiB）。Lua 虚拟机在 `init.lua` 之前创建，所以只能在这里或用 `--set lua_memory_mb=...` 设置。 |
| `MARE_APPROVAL_POLICY` | 覆盖 `approvals` 插件的 `policy` 审批策略（`ask`、`never`、`untrusted` 或细粒度 JSON）。 |
| `MARE_WEB_SEARCH_PROVIDER`、`MARE_WEB_FETCH_PROVIDER` | 覆盖 `web` 插件的 `search_provider` 与 `fetch_provider` 选项。 |
| `MARE_MCP_CONFIG` | 额外的 MCP 配置文件的路径。其中的服务器只有在批准后才会启动（`--approve-mcps` 无需询问即可批准它们）。 |
| `MARE_DEV_PLUGINS` | 内置插件源码的目录，从磁盘加载，而不是使用嵌入二进制文件的副本。用于开发 mare 本身。 |

无效的 `MARE_<OPTION>` 值会在启动时报告；`--strict-config` 会把这些警告变成拒绝启动。

## Provider 凭据

provider 按以下顺序读取密钥：`init.lua` 中的 `api_key` 选项、`mare login` 保存的凭据，然后是下面这些变量。见[模型](../use/models.md)。

| 变量 | Provider |
|---|---|
| `OPENAI_API_KEY` | OpenAI |
| `ANTHROPIC_API_KEY` | Anthropic |
| `GEMINI_API_KEY`、`GOOGLE_API_KEY` | Google（先检查 `GEMINI_API_KEY`） |
| `OPENCODE_API_KEY` | opencode Go |
| `DEEPSEEK_API_KEY` | DeepSeek |
| `MOONSHOT_API_KEY` | Moonshot（`kimi`、`kimi-cn`） |
| `KIMI_API_KEY` | Kimi Code（`kimi-code`、`kimi-code-global`） |
| `MINIMAX_API_KEY` | MiniMax（`minimax`、`minimax-cn`） |
| `ZAI_API_KEY` | Z.ai（`zai`） |
| `ZHIPU_API_KEY` | 智谱 GLM（`zhipu-cn`） |
| `OPENROUTER_API_KEY` | OpenRouter |

你定义的与 OpenAI 兼容的 provider 可以用 `api_key_env` 指定自己的变量。值为环境变量名的 `api_key` 选项会从该变量读取。MCP 服务器配置也可以从环境中取值：字符串中的 `${VAR}`、`${env:VAR}` 和 `${VAR:-default}`，以及 `bearer_token_env_var` 和 `env_http_headers`。

## 网络与证书

| 变量 | 效果 |
|---|---|
| `HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY`（大小写均可） | mare 自身请求使用的 HTTP 和 HTTPS 代理。mare 启动的命令原样继承它们，但在限制网络的沙箱中例外：那里 mare 会把代理变量设为自己的本地过滤代理，并清空 `NO_PROXY`。不支持 SOCKS 代理和 PAC 文件，会报错失败，而不是直接连接。 |
| `NO_PROXY`（大小写均可） | 绕过代理的主机。设置了代理而没有设置 `NO_PROXY` 时，mare 启动的命令会得到 `NO_PROXY=localhost,127.0.0.1,::1`。 |
| `MARE_CA_CERTIFICATE` | 额外受信任 CA 证书的 PEM 包。设置后，它是唯一的自定义来源。 |
| `SSL_CERT_FILE` | PEM 包，在未设置 `MARE_CA_CERTIFICATE` 时使用。 |
| `SSL_CERT_DIR` | PEM 或 `.crt` 文件的目录列表（以 `:` 分隔，Windows 上为 `;`），会加到上述来源之上。 |

证书会添加到系统根证书之上；mare 自己不捆绑任何证书。指定的文件无法读取或不含证书，属于错误。

当 `transport = "auto"` 时，如果设置了 `HTTPS_PROXY` 或 `ALL_PROXY`（大小写均可），OpenAI 请求会使用 HTTP 而不是 WebSocket，因为 WebSocket 客户端不经过 HTTP 代理。

## 终端与显示

| 变量 | 效果 |
|---|---|
| `NO_COLOR` | 任何非空值都会关闭颜色。 |
| `MARE_COLOR` | 强制颜色深度：`truecolor`、`256` 或 `16`。会被 `NO_COLOR` 覆盖。 |
| `COLORTERM` | `truecolor` 或 `24bit` 选择真彩色。 |
| `TERM` | 名称中含 `256` 时选择 256 色。`dumb` 将界面切换为纯逐行输出。`linux` 搭配非 UTF-8 区域设置时选择 ASCII 图标。 |
| `TERM_PROGRAM`、`TERM_PROGRAM_VERSION` | 标识终端，用于确定颜色深度、图标集、宽度规则和图像协议。 |
| `COLUMNS` | 纯文本（`TERM=dumb`）模式下的宽度后备值。 |
| `MARE_NERD_FONT`、`NERD_FONT` | `1` 选择 Nerd Font 图标集。 |
| `LC_ALL`、`LC_CTYPE`、`LANG` | 设置了的第一个决定区域设置是否为 UTF-8。 |
| `MARE_WIDTH_METHOD` | 强制字符宽度的测量方式：`unicode` 或 `wcwidth`。 |
| `MARE_NO_TERM_PROBE` | 设置后跳过所有启动时终端查询，包括背景色和能力探测。 |
| `MARE_NO_OSC11` | 设置后只跳过 OSC 11 背景色查询；其他能力探测仍会执行。 |

mare 还会根据终端设置的变量识别终端，以选择按键、图像和剪贴板行为：`KITTY_WINDOW_ID`、`GHOSTTY_RESOURCES_DIR`、`WEZTERM_EXECUTABLE`、`WEZTERM_PANE`、`ALACRITTY_WINDOW_ID`、`ALACRITTY_SOCKET`、`WT_SESSION`、`KONSOLE_VERSION`、`VTE_VERSION`、`VSCODE_INJECTION`、`ITERM_SESSION_ID`、`WARP_SESSION_ID`，以及多路复用器标记 `TMUX`、`TMUX_PANE`、`STY` 和 `ZELLIJ`。这些不需要你自己设置。剪贴板复制还会查看 `SSH_TTY`、`SSH_CONNECTION` 和 `WSL_DISTRO_NAME`，以在原生复制和 OSC 52 复制之间选择。

## 编辑器、shell 与登录

| 变量 | 效果 |
|---|---|
| `VISUAL`、`EDITOR` | 外部编辑器，在 `editor` 选项之后按此顺序使用。后备是 `vi`。值中可以包含参数，例如 `code --wait`。 |
| `SHELL` | 当 `tools` 插件的 `shell` 选项未设置时，bash 工具所用的 shell：如果 `$SHELL` 是 bash 或 zsh 就用它，否则用 `bash`。 |
| `SSH_CONNECTION`、`SSH_TTY` | 设置了其中任何一个时，`mare login` 会假定没有本地浏览器，并优先提供设备码流程。MCP OAuth 登录会请你粘贴重定向后的地址。 |
| `DISPLAY`、`WAYLAND_DISPLAY` | 在 Linux 上，当两者都为空时，`mare login` 会假定没有浏览器。 |

## 日志与诊断

| 变量 | 效果 |
|---|---|
| `MARE_LOG` | `<state>/log/mare.log` 的日志过滤器。可以是级别（`debug`、`trace`）或按模块的过滤器（`mare_binding=trace`）。默认 `info`。 |
| `MARE_NO_HARDENING` | `1` 跳过启动时的加固：不禁止 core dump 和调试器附加，也不从 mare 及其命令中移除 `LD_*` / `DYLD_*` 变量。 |
| `MARE_FRAME_LOG` | 文件路径。TUI 运行期间，mare 每行写入一次以微秒计的渲染时间。 |
| `MARE_SANDBOX_BACKEND` | `none` 关闭操作系统沙箱后端；在 Linux 上 `landlock` 强制使用 Landlock 后端。 |
| `MARE_SANDBOX_CAPS` | 以逗号分隔的沙箱能力（例如 `fs_write,net_block`），用来收窄检测到的后端被允许声称的能力。 |
| `MARE_SANDBOX_DIR` | Windows：增强沙箱目录。默认 `%ProgramData%\mare\sandbox`。 |
| `MARE_SANDBOX_HELPER` | Windows：用作沙箱运行器的可执行文件，代替 `mare` 本身。 |
| `MARE_UPDATE_SNAPSHOTS` | 控制 `mare test` 的快照更新行为（设为 `always` 时直接更新快照文件，等价于 `--update-snapshots`）。 |
| `MARE_PROFILE_LUA_EVERY` | 启动性能分析时的 Lua 指令采样间隔；须为正整数，默认 `1000000`。 |
| `MARE_VALIDATE_RECORDS` | 设为 `1` 时在发布构建中开启存储记录校验。 |
| `MARE_FAUX_SCRIPT` | 一个文件，内容是为 `faux` 测试 provider 编写脚本的 JSON 数组（仅在 debug 构建或启用 `faux` 特性的构建中可用；正式发布二进制不包含它）。 |

## mare 启动的命令

由 `bash` 工具和其他工具运行的命令从你的环境启动，并有两处改动。`notify.cmd` 这类钩子命令只得到第一处改动（凭据清理），得不到下面新增的变量。

形似凭据的名称会被移除。名称中包含 `KEY`、`PASSWORD`、`SECRET` 或 `TOKEN`（不区分大小写）的变量会被丢弃，所有 `MARE_*` 变量也一样。可以用 `tools` 插件的 `env` 选项同时为 `bash` 工具和 `mare.tool.command` 声明的命令调整，`include`、`exclude` 和 `set` 对两者均生效：

```lua
mare.plugin("tools", {
  env = {
    include = { "GH_TOKEN" },   -- keep a name the scrub would drop
    exclude = { "AWS_PROFILE" }, -- drop a name it would keep
    set = { PAGER = "less" },    -- set a value for every tool command
  },
})
```

然后 mare 为工具命令添加这些变量：

| 变量 | 值 |
|---|---|
| `TERM` | `dumb` |
| `PAGER`、`GIT_PAGER`、`GH_PAGER` | `cat` |
| `NO_COLOR` | `1` |
| `LANG`、`LC_CTYPE` | `C.UTF-8`（macOS 上为 `en_US.UTF-8`），仅当 `LANG`、`LC_ALL`、`LC_CTYPE` 都未设置时。Windows 上不设置。 |
| `MARE_CWD` | 会话的工作目录。 |
| `MARE_SESSION_ID` | 会话 id。 |
| `MARE_SESSION_MODEL`、`MARE_SESSION_EFFORT` | 当前的模型引用和强度。它们命名为 `MARE_SESSION_*`，这样嵌套的 mare 不会把它们当作配置读取。 |
| `MARE_PEER_SOCKET` | 本会话的对等套接字（如果有）。`mare peer send` 会使用它。 |

`status` 插件的命令（见[服务与事件](../extend/services-and-events.md)）还会收到 `MARE_STATE`、`MARE_MESSAGE`、`MARE_SESSION_ID`、`MARE_SESSION_PATH`、`MARE_CWD`、`MARE_SESSION_MODEL` 和 `MARE_PEER_SOCKET`。

TUI 打开期间，mare 还会为命令设置 `SUDO_ASKPASS`、`MARE_ASKPASS_SOCK` 和 `MARE_ASKPASS_TOKEN`，这样 `sudo` 的密码提示会到达 TUI 的掩码输入框，而不会因为没有终端而失败。
