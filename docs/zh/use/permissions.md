# 权限与沙箱

mare 如何决定智能体可以做什么：权限预设、审批、规则、项目信任和操作系统沙箱，以及哪些内容不受保护。

两种机制协同工作。沙箱由操作系统强制执行，限制所启动的进程能写什么、读什么、能通过网络访问什么。审批决定在工具调用超出沙箱范围之前，mare 何时询问你。两者都由内置的 `permissions` 插件提供；如果你禁用该插件，mare 将以完全访问权限运行。

## 预设

预设同时设定沙箱级别和审批行为。每个会话选择一个。

| 预设 | 状态栏 | 沙箱 | 询问 |
| --- | --- | --- | --- |
| `read-only` | `READ` | 任意位置可读。不可写。网络被阻止（默认 Windows 层级除外，它无法阻止网络；见下文沙箱一节）。 | 写文件之前，以及任何需要超出沙箱所给权限的操作之前。 |
| `auto`（默认） | `AUTO` | 可写入工作区和临时目录。网络开放，除非你关闭。 | 模型请求离开沙箱时，以及不受沙箱约束的工具。 |
| `full-access` | `FULL` | 无。 | 显式 `prompt` 规则、当前作用域存在限制性规则时的无法解析命令，以及危险命令（见下文）。 |

```sh
mare --permission read-only          # for this run
mare --yolo                          # same as --permission full-access
```

```lua
-- ~/.config/mare/init.lua
mare.plugin("permissions", { preset = "auto" })
```

在会话内，`/permission` 打开选择器，`/permission read-only`（或 `auto`、`full-access`）直接切换，`Shift+Tab` 按 `read-only` → `auto` → `full-access` → `read-only` 顺序循环。`/yolo` 切换到 `full-access`；mare 会请你确认。`/permission log` 显示做出了什么决定及原因，`/permission caps` 显示本机沙箱后端能强制执行什么。会话会记录它的预设，所以恢复会话时会还原。

在不受信任的项目中（见下文），显式传入的命令行标志预设（`--permission` 或 `--yolo`）仍优先；未通过这两个标志指定时，mare 会使用可配置的 `untrusted_preset`（默认为 `read-only`），而不使用常规 `preset`（包括 `--set plugins.permissions.preset=...`），撤销项目信任后也会按该上限降级。在未受信任目录中，按 `Shift+Tab` 仍可直接降级到 `read-only`（例如从 `full-access` 切回 `read-only`）；若下一档为 `auto` 或 `full-access`，则会打开与 `/permission` 相同的选择器并默认选中当前档，而不会单次按键直接提权。按 `Esc` 保持当前预设不变，显式选择 `auto` 立即生效，选择 `full-access` 仍需确认；切换预设不会改变目录的信任状态。

在 `auto` 和 `read-only` 下，shell 命令在沙箱内运行而无需询问。如果命令因为沙箱拒绝了某些操作而失败，模型可以重试，并请求拒绝信息中指出的最小额外权限：网络访问（对某个主机）、对某个路径的写权限，或全部权限。这次重试会询问你。审批只适用于那一次调用，绝不会自动重试。

默认 Windows 层级无法控制网络，mare 会接受这一缺口而不拒绝运行。为尽量兑现 `read-only` 的承诺，mare 在那里会在每个 shell 或命令工具（`info.command_tool == true`，涵盖 `bash`、`powershell` 及 `mare.tool.command` 注册的命令工具）执行前询问，除非该次无限制访问（`all` / `danger-full-access`）已被显式批准或有对应规则放行。若要严格拒绝，请显式设置 `accept_partial`（见下文）。

### 自定义 `auto`

```lua
mare.plugin("permissions", {
  preset = "auto",
  writable = { "~/.cache/pip", "/data/scratch" },   -- extra writable directories (must exist; use physical paths, not symlinks)
  deny_read = { "/home/alice/.ssh" },               -- literal absolute paths the sandbox hides from reads
  accept_partial = { "fs_deny_read" },              -- required for deny_read; merges with default partial acceptances
  network = { default = "deny", allow = { "github.com", "*.npmjs.org" } },
})
```

相对路径的额外可写目录会按工作目录解析。不存在或不是目录的会被跳过并给出提示，`mare doctor` 会标出。已存在但本身是符号链接、或位于符号链接之后的目录，会被沙箱策略拒绝而不是跳过；请使用它的物理路径。配置 `deny_read` 时需要同时设置 `accept_partial = { "fs_deny_read" }`，因为各后端尚未验证路径变更、别名或特殊挂载（在 Windows 上还需运行 `mare sandbox setup` 以通过增强层级 ACL 强制生效）。`network` 可以是 `true`（开放）、`false`（阻止），或包含 `allow`、`deny`、`default` 和 `allow_local` 的表。表会通过本地过滤代理强制执行；见下文的网络策略。

## 审批

当工具调用需要你来决定时，mare 会显示标题为 `Allow <tool>?` 的对话框，其中有原因、工作目录，对于编辑还有更改的预览。

| 选择 | 按键 | 含义 |
| --- | --- | --- |
| Allow once | `y` | 运行这次调用。 |
| Allow for this session | `a` | 运行这次调用，并在本会话剩余时间内对相同的调用不再询问。对于 shell 命令，“相同”指相同的命令、目录和请求的范围：批准 `git push` 并不批准 `git push --force`。 |
| Allow and remember | `A` | 保存一条规则并允许。对于 shell 命令，规则是形如 `git push` 的 argv 前缀，保存在规则文件中，所有会话共用。对于其他工具，则在本会话内允许对该工具的所有调用。 |
| Deny | `n` | 拒绝这次调用。系统会让你填写可选的原因，模型会在工具结果中读到它。 |
| Deny and stop | `Ctrl+C` | 拒绝并结束这一轮。 |

`Esc` 拒绝该调用并让这一轮继续。对于额外沙箱权限的请求，选项是仅限这一轮或限于本会话。一次只显示一个审批。

如果调用来自子智能体，对话框会写明它的名称。子智能体在启动时继承父会话当前生效的预设与沙箱：父会话收紧会立即收紧运行中的子智能体；父会话放宽预设（例如切到 `full-access`）会在续用（`subagent_steer`）时重同步未锁定子智能体的预设与沙箱上限，父端处理未锁定子智能体的在途沙箱请求时也会依据父当前预设直接放行，而显式只读子智能体始终受限（详见[子代理与临时旁路对话](subagents.md#权限继承与父会话切换规则)）。

### 申请额外权限

`permissions` 插件还注册了内置模型工具 `request_permissions`（见[内置工具与 Shell 模式](tools.md)）。模型可以直接调用它申请额外的 `read` 路径、已存在的物理 `write` 目录或 `network`（布尔值或主机白名单），调用时必须提供 `justification`，并选择 `scope`（默认为 `"turn"`，也可以是 `"session"`）。

你可以只批准其中的一部分。该工具返回的 JSON（`{ permissions, scope }`）只包含本次新增批准或复用的额外权限，基线预设中原本就有的访问权限不会出现在返回值里；被拒绝的请求不授予任何权限。在对应轮次或会话内，后续工具调用可以直接复用已批准的范围，不再重复询问。当宿主沙箱基线发生变化时（触发 `sandbox/changed`，例如切换预设或更新宿主策略），mare 会清空所有临时的轮次与会话授权及会话审批缓存，防止旧授权延续到新基线上。

### 审批策略

审批策略与预设是分开的。它决定本应询问的请求会怎么处理：

```sh
mare --approval-policy never    # reject every request without prompting
```

- `ask`（默认）：向你提示。
- `never`：不提示，直接拒绝请求。沙箱仍然生效。
- `untrusted`：除只读的调用外，每个工具调用之前都询问，即使沙箱本来允许。
- 表 `{ mode = "granular", categories = { tool = true, network = true } }` 只对列出的类别询问，其余一律拒绝。

用 `mare.plugin("approvals", { policy = "never" })` (RPC `options.set {session_id: "...", values: {"plugins.approvals.policy": "never"}}`, `--set plugins.approvals.policy=never`) 或 `/approval never` 设置。子智能体只能收紧它继承的策略。

### 危险命令

有些命令在所有预设下都会询问，包括 `full-access`：`rm -f` / `--force` 和 `find -delete`（在脚本的任何位置，包括经过包装器的情况），以及在 Windows 上的 `Remove-Item -Force`、`del /f` 和 `rd /s /q`。在无界面运行中没有人可以回答，所以它们会被拒绝。只有允许这条确切命令的规则才能让它不经询问而通过。

```lua
mare.plugin("permissions", {
  dangerous = { rules = { { "git", "push", "--force" } } },   -- add your own argv prefixes
})
```

## 规则

规则为匹配的命令固定一个决定。它们针对解析后的命令进行检查：由 `&&`、`||`、`;` 或 `|` 连接的普通词会被拆成多个片段，每个片段都必须匹配某条 allow 规则。包含 `$(...)`、反引号、展开、子 shell、重定向（`/dev/null` 这类固定去向除外）、`eval` 或嵌套 shell 的命令无法分析，因此没有任何 allow 规则适用于它。

```lua
mare.plugin("permissions", {
  rules = {
    { argv = { "git", "status" }, decision = "allow" },
    { argv = { "git", "push" },   decision = "prompt" },
    { argv = { "rm", "-rf" },     decision = "forbidden", justification = "never delete recursively" },
    { tool = "my_unconfined_tool", decision = "allow" },
  },
})
```

`decision` 是 `allow`、`prompt`（总是询问）或 `forbidden`（总是拒绝）。多条规则匹配时，最严格的生效：先是 `forbidden`，然后是 `prompt`，最后是 `allow`。可选字段有 `cwd`（如 `~/src/**` 这样的 glob）、`scope`（`network`、`write:<path>`、`all`）、`deny_args` 和 `expires`。用“Allow and remember”保存的规则存放在 `~/.local/state/mare/permissions.json` 中，多个会话可以安全地共用它；可以编辑它，或设置 `rules_file` 来移动它。

## 项目信任

项目可能带有 mare 会加载的代码和提示词：来自项目资源根（向上直到 git 根最近的含 `.mare/` 或 `.mcp.json` 的目录）的 `.mare/plugins`、`.mare/init.lua`、`.mare/skills`、`.mare/prompts`、`.mare/SYSTEM.md`、`.mare/APPEND_SYSTEM.md`。信任探测会向上检查直到 git 根的 `.agents/skills`；加载技能时，默认相对技能目录只针对 `mare.project.root` 解析一次。只有资源根本身记录了信任（且启动目录到资源根路径上无拒绝），项目资源才会加载。决定保存在 `~/.local/state/mare/trust.json` 中。

```sh
mare trust              # 信任当前项目；如需指定目录，用 mare trust <dir>
mare trust --revoke     # stop trusting it
mare --trust          # trust it for this run only; saves nothing
mare --no-trust       # distrust it for this run, whatever was saved
```

你第一次在含有此类资源（或位于 git 仓库内）的目录中打开 mare 时，交互式 TUI 会弹出信任浮层，按分组展示发现的项目资源（包含 `.mare/init.lua` 的完整 API 风险提示、插件、技能、提示词模板、系统提示词文件，以及仅保留可执行文件名或 URL 主机名的脱敏 MCP 服务器列表；任一分组超过 6 项时可按 `e` 展开或折叠）。浮层提供四个选项：**Trust and load**（写入 `trust.json` 永久记住）、**Trust this session only**（仅在本次会话加载项目资源，不写入磁盘）、**Don't trust**（记住拒绝）和 **Not now**（`Esc`，下次再询问）。无界面和 RPC 运行从不询问：未受信任的资源保持未加载，mare 会向标准错误打印一条说明，除非你传入 `--trust`（仅本次运行信任，不保存）或此前已保存信任。`trust_default` 选项（`ask`、`trust` 或 `distrust`）为没有保存决定的目录设定默认行为。

信任只涵盖加载项目文件。项目的 MCP 服务器有自己的审批；见 [MCP](mcp.md)。

## 操作系统沙箱

沙箱使用会话当前的预设，限制 mare 为工具启动的进程（`bash` 工具、其他命令工具），以及 mare 自身文件原语发起的文件访问。有三个级别：`read-only`、`workspace-write` 和 `danger-full-access`。在 `workspace-write` 中，可写根目录是工作目录、通过 `--add-dir` 或 `/add-dir` 追加的工作区目录（须为已存在、无符号链接且不与宿主或项目受保护路径重叠的绝对物理目录）、平台的临时区域以及 `writable`。在工作区内，当后端报告 `fs_protect` 时，`.git` 和 `.mare` 对受限制的进程保持只读；Landlock 和 Windows 层级没有这项能力。

![三平台操作系统沙箱后端与探测能力对照](../../assets/diagrams/sandbox-backends.svg)

在 git linked worktree 或子模块中，当 `.git` 是已存在的指向文件（`gitdir: ...`）且命令申请 `write:.git` 提权时，mare 会以禁止符号链接的方式严格解析目标 `gitdir`，而不是放宽外层工作区权限。对于合法的 linked worktree（校验 `commondir`、`worktrees/` 父目录关系、`objects`/`refs`/`HEAD` 结构以及匹配的 `gitdir` 反向引用），`write:.git` 会同时授予该 worktree 的 `gitdir` 与共享仓库 `commondir` 的写权限；已存在的符号链接或格式非法的 `.git` 指针和反向引用会被拒绝。如果 `.git` 文件本身不存在，则按常规待创建文件分支处理，请求其已存在父目录的 `write` 范围（仍需通过对应审批）。

| 平台 | 后端 | 说明 |
| --- | --- | --- |
| Linux | bubblewrap（`bwrap`），然后是 Landlock | 必须安装 `bwrap` 并且它能够创建命名空间。它以只读方式挂载主机根目录，带有私有的 `/proc`、`/dev` 和 PID 命名空间，并绑定可写根目录。网络通过网络命名空间阻止；过滤式联网使用仅含回环的命名空间，桥接到代理。如果 `bwrap` 不可用，则使用 Landlock：限制文件的写入和读取，并用 seccomp 阻止套接字，但它无法保护 `.git` 和 `.mare`，也无法提供域名过滤。 |
| macOS | Seatbelt（`/usr/bin/sandbox-exec`） | 一份配置文件，拒绝除可写根目录外的文件写入，并可限制读取、出站网络和 Unix 套接字。 |
| Windows，默认层级 | 受限令牌、低完整性级别、ACL 授权 | 写入受限。读取和网络不受控制，mare 会将其报告为部分生效。mare 默认接受缺少的网络控制（见上文关于审批的段落）。 |
| Windows，增强层级 | 专用沙箱账户、安装时拒绝读取 ACL、防火墙过滤器 | 用 `mare sandbox setup` 安装一次（会请求提权）。安装程序会给沙箱账户在凭据和浏览器路径上配置拒绝读取 ACL，但 `windows-user` 后端不声明每次调用的 `fs_deny_read`（也不声明白名单 `fs_read`），因此带显式 `deny_read` 的严格调用在未接受部分保护时会被拒绝。网络经过过滤，但过滤器的代理端口由沙箱账户在所有被沙箱化的调用之间共享，因此不强制每次调用的网络隔离，沙箱也不会报告 `net_block` 或 `net_proxy`。`mare sandbox status` 运行自检；`--require-full` 在并非全部强制执行时以非零状态退出；由于每次调用的网络隔离并未强制执行，它目前即使自检通过也会以非零状态退出。`mare sandbox setup --remove` 撤销设置。如果自检失败，mare 会回退到默认层级并报告原因。 |

mare 会按需探测每个后端并缓存实际可用的能力（`fs_read`、`fs_deny_read`、`fs_write`、`fs_protect`、`fs_host_protect`、`net_block`、`net_proxy`、`unix_socket`）。如果预设需要后端不具备的能力，会被以 `SANDBOX_INSUFFICIENT` 拒绝并指明该能力，而不是以较弱的方式运行。如果没有任何后端可用，命令会被以 `SANDBOX_UNAVAILABLE` 拒绝；mare 绝不会悄悄地不加限制地运行它们。默认的例外有两个：缺少 `fs_protect`（在所有平台上都会被接受并报告为部分生效），以及默认 Windows 层级上缺少网络控制。要明确接受其他缺口：

```lua
mare.plugin("permissions", { accept_partial = { "network" } })
```

显式的 `accept_partial = {}` 表示严格拒绝；非空的 `accept_partial` 列表会与上述默认豁免项合并。运行 `mare doctor` 查看本机的后端和能力，或在会话中使用 `/permission caps`。

### 网络策略

在 `auto` 下，`network = true` 使沙箱内命令的网络保持开放（内置 `web_fetch` HTTP 客户端默认仍会拦截私有、回环、链路本地和元数据 IP），`false` 阻止网络。带有 `allow` / `deny` 主机和 `default` 的表会让流量经过按主机过滤的本地代理；沙箱只允许命令连接到该代理。网络受限时，Unix 套接字会被阻止；`unix_sockets` 逐套接字放行仅在支持该能力的后端上生效（Linux 的 seccomp `AF_UNIX` 路径不支持逐 socket 放行）。仅设置代理环境变量绝不算作强制执行。

## 哪些内容不受保护

- 读取默认开放：在 `auto` 和 `read-only` 下，受限命令可读取当前用户有权限读取的任意文件（包括 `~/.ssh`），除非加入 `deny_read` 或启用 Windows 增强层级默认规则。读到的内容可能被发给模型 provider。
- `auto` 默认允许出站网络，除非修改 `network`。
- `full-access` 以及经批准的非受限工具调用不经过沙箱。在 `auto` 下，运行在本地沙箱外的工具（如 MCP 工具）执行前会询问。
- Landlock 和 Windows 默认层级不保护 `.git` 与 `.mare`；Windows 默认层级不控制读取和网络。
- 沙箱不是虚拟机：它共享宿主机内核、用户与主目录。批准 `write:.git` 允许命令修改会在沙箱外执行的 git hooks。
- 手动输入的 `!cmd`（及 `!!cmd`）命令在沙箱外运行，且不经过审批提示。
- 沙箱不能消除提示词注入：按 `y` 之前先核对审批内容。
- shell 命令产生的副作用无法用 `/rewind` 撤销，见[会话](sessions.md)。
