# 文件与目录

mare 读取或写入的每个文件和目录：四个用户目录、每个目录中的内容，以及项目可以携带的文件。路径用 `~` 表示主目录；在 Windows 上请理解为 `%USERPROFILE%`。

> 本参考页由人工维护（不经 `scripts/gen_docs.py` 自动生成）。

## 位置

mare 在所有平台上都使用 XDG 风格的目录。macOS 和 Windows 不使用 `Library` 或 `AppData` 位置。

| 目录 | 默认位置 | 覆盖方式 |
|---|---|---|
| 配置 | `~/.config/mare` | `$XDG_CONFIG_HOME/mare` |
| 状态 | `~/.local/state/mare` | `$XDG_STATE_HOME/mare` |
| 数据 | `~/.local/share/mare` | `$XDG_DATA_HOME/mare` |
| 缓存 | `~/.cache/mare` | `$XDG_CACHE_HOME/mare` |

`MARE_HOME` 优先于以上所有设置：四个目录变为 `$MARE_HOME/config`、`state`、`data` 和 `cache`。见[环境变量](environment.md)。Lua 代码以 `mare.paths.config`、`mare.paths.state`、`mare.paths.data`、`mare.paths.auth`、`mare.paths.sessions` 和 `mare.paths.packages` 获得相同的路径。

`mare doctor` 会检查这些目录是否存在且可写，并在 Unix 上检查 `auth.json` 的权限是否为 0600、会话目录和状态目录的权限是否为 0700。

## 配置目录

你编写和编辑的文件。

| 路径 | 用途 |
|---|---|
| `init.lua` | 你的配置。`mare init` 会创建它，且从不覆盖。见[选项](../configure/options.md)。 |
| `.luarc.json` | Lua 语言服务器设置，指向 mare 的类型文件。由 `mare init` 写入，且只在内容仍与 mare 上次写入的一致时才重写。 |
| `lua/` | 你在 `init.lua` 中 `require` 的 Lua 模块。 |
| `plugins/` | 用户插件。见[插件](../extend/plugins.md)。 |
| `skills/` | 用户技能，每个技能一个带 `SKILL.md` 的目录。 |
| `prompts/` | 会成为斜杠命令的提示词模板。 |
| `themes/` | 主题。 |
| `AGENTS.md` | 你自己的指令，加载到每个会话中。每个目录中，mare 加载 `AGENTS.override.md`、`AGENTS.md`、`CLAUDE.md` 中第一个存在的。 |
| `SYSTEM.md` | 替换默认的系统提示词。 |
| `APPEND_SYSTEM.md` | 追加到系统提示词的文本。 |
| `mcp.json` | MCP 服务器。见 [MCP](../use/mcp.md)。 |
| `auth.json` | `mare login` 保存的凭据，权限 0600。旁边有：登录或刷新期间的 `auth.json.lock`，以及修复之后的 `auth.json.bak`。 |
| `mare-lock.json` | 包锁定文件：每个已安装包的确切修订。见[包](../configure/packages.md)。 |
| `plugins.json` | 你启用的可选包附加项。 |

## 状态目录

mare 写入的运行时状态。可重建的会话索引在下表单独标明；请保留尚未恢复的 `quarantine/` 记录和活动会话的锁、提交及回滚文件（存储层重新打开会话时会读取恢复记录）。

| 路径 | 用途 |
|---|---|
| `state.json` | 运行时选择，例如用 `/model` 选的模型，下次启动时作为默认值。`init.lua` 中显式的值优先。`state.json.lock` 保护写入。 |
| `trust.json` | 项目信任决定，由 `mare trust` 和信任提示写入。 |
| `sessions/` | 已保存的会话。见下文。 |
| `log/mare.log` | 日志。在 10 MiB 时轮转，保留五个旧文件，`mare.log.1` 到 `mare.log.5`。用 `MARE_LOG` 设置级别。 |
| `crashes/<timestamp>/report.txt` | 脱敏的崩溃报告，在 panic 导致 mare 结束时写入。 |
| `history.jsonl` | 编辑器的提示词历史。 |
| `lua-history` | Lua 控制台的历史。 |
| `permissions.json` | 你从审批提示中保存的权限规则。`permissions.json.lock` 保护写入。 |
| `news.json` | 包动态状态。 |
| `models-cache/<provider>.json` | 由 `mare model refresh` 获取的模型列表。 |
| `skills-cache/*.json` | 技能发现缓存。 |
| `domains/ns-*.json` | 命名域状态文件。 |
| `snapshots/run-<pid>-<uuid>/` | 无会话文件运行时的文件快照（有会话文件 `abc.jsonl` 时位于同级 `abc.jsonl.snapshots/`）。 |
| `attachments/<session-id 或 unsaved>/` | 无会话文件时压缩卸载图片的回退目录（有会话文件 `abc.jsonl` 时位于同级 `abc.attachments/`）。 |
| `spill/<前缀>-*.log` | 长时间运行的命令的完整输出，前缀表示写入方：shell 命令为 `bash-*.log`，钩子命令为 `hooks-*.log`。 |
| `spill-results/` | 可恢复的工具输出，仅属于拥有它的会话（见 [RPC](../integrate/rpc.md) 中的 `spill.*` 方法）。 |
| `host/token`、`host/.token.lock` | 本地 HTTP/WS Web 监听器的 Bearer 令牌（`mare --web`，可用 `mare token rotate` 轮换，权限 0600）及其令牌锁文件。 |
| `web/<pid>-<id>.json` | 运行中宿主 Web 监听器的端点元数据，供 `mare web url` 读取（位于 `host/` 之外，关闭时移除，过期条目自动清理），权限 0600。 |
| `quarantine/` | 常规会话持久化失败时隔离保存的未落盘会话 JSONL 记录。 |
| `mcp.log` | MCP 服务器的输出。 |
| `mcp-approvals.json` | 已保存的项目和 `$MARE_MCP_CONFIG` MCP 服务器审批记录（按服务器名与配置哈希索引）。 |
| `mcp-disabled.json` | 按项目记录的已禁用 MCP 服务器。 |
| `mcp-tools/` | MCP 服务器的工具 schema 缓存。 |
| `mcp-output/` | 被截断的 MCP 结果的完整形式。 |
| `mcp-oauth/<server>-<hash>.json` | MCP 服务器的 OAuth 令牌。 |
| `peers/<pid>.json`、`peers/<pid>.sock` | 运行中会话的会话注册表和对等套接字。退出时移除；过期条目会被清理。见 [RPC](../integrate/rpc.md)。 |
| `pgroups/` | mare 启动的进程组的注册表，用于崩溃后的清理。 |
| `crash-guard.json` | 曾使 mare 崩溃的请求，这样恢复的会话不会永远重发某个请求。`crash-guard.lock` 保护它。 |
| `kitty/` | 在崩溃后保持终端键盘模式状态一致的标记。仅限 Unix。 |
| `key-conflicts` | 最近一次显示的键位冲突警告，以免重复。 |
| `luarc.generated` | mare 上次写入的 `.luarc.json`。 |
| `terminal-hint-shown` | 仅限 Windows：在显示一次性终端提示后设置。 |

### 会话

会话位于 `sessions/<project>/` 下，其中 `<project>` 是工作目录，除字母、数字、`.` 和 `_` 外的每个字符都被替换为 `-`，再用 `--` 包起来。对于 `/home/me/app`，结果是 `--home-me-app--`。

| 路径 | 用途 |
|---|---|
| `<time>_<id8>.jsonl` | 一个会话，只追加的记录日志。这是事实来源。名称是开始时间（`2026-11-14T09-30-00`）和 id 的最后八个字符。 |
| `<time>_<id8>.jsonl.lock` | 唯一写入方的锁。旁边的 `.lock.rollback` 和 `.lock.commit` 文件保存崩溃恢复数据。 |
| `<time>_<id8>.jsonl.v<N>` | 重写会话文件之前制作的备份。 |
| `<time>_<id8>.attachments/` | 与会话一起存储的图片。 |
| `<time>_<id8>.jsonl.snapshots/` | 会话的文件快照日志。 |
| `agents/<run>/<seq>-<label>.jsonl` | 子智能体会话，链接到它们的父会话。 |
| `index.sqlite`（在 `sessions/` 中） | 可重建的 SQLite 会话索引，供 `/resume`、`-c` 和 id 查找使用。索引保存会话元数据和单行用户输入摘要（首次符合条件的用户消息写入 `preview`，最近一次写入 `last_input`）；完整消息保存在 JSONL 文件中。删除后 mare 会根据会话文件重建它。 |

## 数据目录

mare 为自己安装的文件。

| 路径 | 用途 |
|---|---|
| `runtime/` | 内置插件和 Lua 类型文件的只读副本。`mare runtime` 会打印该路径。mare 版本变化时重写。 |
| `packages/` | 已安装的包，每个来源一个目录。见[包](../configure/packages.md)。 |
| `package-types/` | 从已安装包为语言服务器镜像的类型文件。 |

## 缓存目录

| 路径 | 用途 |
|---|---|
| `luac/` | 编译后的 Lua 字节码。可以安全删除。 |

## 项目文件

这些文件位于项目中，相对于工作目录。

| 路径 | 用途 |
|---|---|
| `.mare/init.lua` | 项目配置，在你的 `init.lua` 之后应用。 |
| `.mare/plugins/` | 项目插件。 |
| `.mare/plugins.json`、`.mare/mare-lock.json` | 项目级可选插件选择与包锁定文件。 |
| `.mare/lua/` | 项目 `init.lua` 使用的 Lua 模块。 |
| `.mare/skills/`、`.agents/skills/` | 项目技能，按项目资源根（`mare.project.root`）解析一次（启动信任探测还会向上搜索直到 `.git` 以发现 `.agents/skills`）。 |
| `.mare/prompts/` | 项目提示词模板。 |
| `.mare/SYSTEM.md`、`.mare/APPEND_SYSTEM.md` | 项目系统提示词文件。优先于配置目录中的同名文件。 |
| `.mare/mcp.json`、`.mcp.json` | 项目的 MCP 服务器。 |
| `AGENTS.md` | 项目指令，从工作目录及其父目录一直读取到包含 `.git` 的目录。每个目录中使用 `AGENTS.override.md`、`AGENTS.md`、`CLAUDE.md` 中的第一个。 |

`.mare/` 下所有能运行代码或改变提示词的内容（`init.lua`、`plugins`、`plugins.json`、`skills`、`prompts`、`SYSTEM.md`、`APPEND_SYSTEM.md`）以及 `.agents/skills`，只在受信任的项目中加载。单独一个 `.mare/` 目录不需要做出决定。见[权限](../use/permissions.md)。

## 平台说明

| 平台 | 差异 |
|---|---|
| Linux、macOS | 对等套接字是位于 `<state>/peers/<pid>.sock` 的 Unix 套接字，在 0700 目录中权限为 0600。私有文件权限为 0600。 |
| Windows | 在 `%USERPROFILE%` 下使用相同的目录布局。对等套接字是随机命名的命名管道；它的路径下有一个指明管道的端点文件。私有文件使用受限的 ACL，而不是权限位。增强沙箱把设置保存在 `%ProgramData%\mare\sandbox`（或 `MARE_SANDBOX_DIR`）。 |
