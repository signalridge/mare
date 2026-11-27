# 无界面模式

不使用交互式 UI 运行 mare，适用于脚本和 CI：打印最终回答，或流式输出机器可读的事件，然后以状态码退出。

```sh
mare -p "list the public functions in src/lib.rs"
```

`-p`（`--print`）运行提示词并把最终回答打印到标准输出。当标准输入或标准输出不是终端时，mare 也会自动切换到这种模式。运行期间不会向你询问任何事情：需要回答的审批会被拒绝（见下文）。

## 提供提示词

```sh
mare -p "explain this error"                     # prompt as arguments
git diff | mare -p "review this diff"            # piped stdin is appended as <stdin>...</stdin>
cat prompt.md | mare -p                          # no prompt argument: stdin is the prompt
mare -p - < prompt.md                            # "-" reads the prompt from stdin explicitly
mare -p explain @src/main.rs                     # a separate @path argument attaches that text file
```

- 同时有提示词参数和管道标准输入时，标准输入的文本会被加在提示词之后，包在 `<stdin>` 标签内。如果一秒内标准输入没有任何数据，mare 会忽略它，这样继承来的管道就不会让运行挂起。
- 标准输入必须是 UTF-8，且最大 64 MiB。没有提示词且标准输入为空属于用法错误（退出码 2）。
- `@path` 参数是以 `@` 开头的完整 shell 单词。文本文件会被内联附加；图片和无法读取的路径保持原样。

用[会话](sessions.md)中的会话选项继续之前的工作：

```sh
mare -p -c "now add tests"                         # continue the last session here
mare -p --resume 3f9a1c2e "and update the docs"    # by id; see `mare session new`
mare -p --no-session "one-off question"            # do not save this session
```

## 机器可读的输出

```sh
mare -p --json "fix the failing test" > run.jsonl
mare -p --json --stream-deltas "write a haiku"
```

`--json`（`--mode json` 的简写）每行输出一个 JSON 对象。第一行是 `session` 记录，随后是公开的智能体事件（轮次开始、消息、工具调用、用量），如果运行失败则有 `turn.failed`，最后是 `settled`。不带 `--stream-deltas` 时，流以条目为粒度；带上它时，还包含文本和工具输出的增量。记录格式见 [JSON 流](../integrate/json-stream.md)。

```sh
mare -p --json "summarize CHANGELOG.md" | jq -r 'select(.type == "settled") | .data.text'
mare -p --json "..." | jq -r 'select(.type == "settled") | "\(.data.reason) \(.data.exit_code)"'
```

如果标准输出的读取方消失了（`... | head`），mare 会停止运行，而不是为无人读取的输出白白消耗 token，并以 141 退出。

需要长期存在的双向连接时，请改用 `--rpc`；见 [RPC](../integrate/rpc.md)。

### 结构化输出（`--output-schema`）

在无界面模式（`-p` 或 `--json`）下传入 `--output-schema FILE`，可以把最终回复约束到指定的 JSON Schema，并在退出前完成校验。

最小 schema 示例（`summary.schema.json`）：

```json
{
  "type": "object",
  "required": ["risk", "findings"],
  "additionalProperties": false,
  "properties": {
    "risk": { "type": "string", "enum": ["low", "medium", "high"] },
    "findings": { "type": "array", "items": { "type": "string" } }
  }
}
```

调用并读取通过校验的结果：

```sh
# JSON 事件流：先落盘保留 mare 的退出码，再用 jq -ce 提取 .data.structured
mare -p --json --no-session --output-schema summary.schema.json \
  "Inspect src/auth.rs and summarize risk." > run.jsonl || exit $?
jq -ce 'select(.type == "settled") | .data.structured' run.jsonl > summary.json

# 纯打印模式：标准输出直接输出校验通过的 JSON 文本
mare -p --no-session --output-schema summary.schema.json \
  "Inspect src/auth.rs and summarize risk." > summary.json
```

校验与失败语义：

- `--output-schema` 仅限无界面运行（`-p` / `--json`）。在非无界面模式下传入，或文件不存在、JSON 格式错误、Schema 非法时，mare 会在开始轮次前直接以退出码 `2` 结束。
- 如果模型最终输出未通过 Schema 校验，mare 会以 `error_kind = "invalid_output"` 失败并以退出码 `1` 退出：在 `-p` 模式下标准输出为空、校验错误打印到标准错误；在 `--json` 模式下会输出 `turn.failed` 以及带 `error` 字段、不含 `structured` 的 `settled` 记录。

## 限制与工具

| 选项 | 效果 |
| --- | --- |
| `--max-turns N` | 一个提示词内经过 `N` 个模型轮次后停止。退出码 4。 |
| `--timeout DURATION` | 运行这么长时间后停止。接受 `90`、`90s`、`1500ms`、`5m`、`1h`。退出码 4。仅限无界面模式。 |
| `--output-schema FILE` | 按 JSON Schema 文件校验最终回复（`--json` 模式下写入 `settled.data.structured`）。仅限无界面模式。 |
| `--tools a,b` | 只向模型提供这些工具，例如 `--tools read,ls`。 |
| `--exclude-tools TOOLS` | 排除匹配的工具（可重复传入或用逗号分隔，支持 `*` 通配符），例如 `--exclude-tools bash,write`。被排除的工具不会发送给模型，执行时也会被拒绝。 |
| `--add-dir DIR` | 向本次会话工作区追加已存在的物理目录（可重复传入；仅限无符号链接且不与受保护路径重叠的绝对目录；不适用于 `mare acp`）。 |
| `--append-system-prompt TEXT` | 本次运行在系统提示词末尾添加文本。 |
| `-m`、`-e` | 模型和推理强度；见[模型](models.md)。 |
| `--permission`、`--yolo`、`--approval-policy` | 权限预设和审批策略；见下文。 |
| `--no-session` | 不持久化本次运行。 |
| `--set KEY=VALUE` | 覆盖一个选项，例如 `--set plugins.tools.edit_mode=replace`。 |
| `--strict-config` | 配置有警告时拒绝启动。 |
| `--clean` | 忽略用户和项目配置、用户插件、包以及 `state.json`。 |

```sh
mare -p --tools read,ls --max-turns 8 --timeout 5m \
  --append-system-prompt "Answer in one paragraph." \
  "what does this repository do?"
```

## 权限与审批

普通主会话在受信任项目默认使用 `auto`；未信任项目使用 `untrusted_preset`（默认 `read-only`）；显式传入 `--permission` 优先。无界面运行时，没有应答者的审批请求会被拒绝，工具把拒绝结果交给模型，模型可以继续执行其他操作：

- mare 向标准错误打印 `mare: N approval(s) were needed and nobody could answer (denied)`，后面每个被拒绝的调用占一行。
- 如果运行在其他方面正常完成，退出码保持为 0。退出码 3 表示运行因为一次被回答为“拒绝并停止”的审批而停止。

选择与任务相符的预设：

```sh
mare -p --permission read-only "audit this repo for hard-coded secrets"   # no writes at all
mare -p "fix the typo in README.md"                                       # 已信任项目：auto 可在工作区内写入
mare -p --yolo "run the full migration"                                   # full-access: no sandbox; still asks for prompt rules and dangerous commands
mare -p --approval-policy never "..."                                     # reject anything that would ask
```

显式 `prompt` 规则以及 `rm -f` 这类危险命令在所有预设下都会询问（包括 `--yolo`），因此在无界面运行中会被拒绝，除非有规则允许该确切命令。仅因命令无法可靠解析并不会一律触发询问或拦截：在 `full-access` 下，若当前目录没有适用的限制性规则（`prompt` 或 `forbidden`），无法解析的命令仍可获 `allow`；只有当前作用域存在可能命中的限制性规则时，才会回退为 `ask`（并在无界面运行中被拒绝）。见[权限](permissions.md)。

项目资源（`.mare/plugins`、`.mare/init.lua`、技能、提示词、`.mare/SYSTEM.md`）只在受信任的项目中加载，且无界面运行从不询问。如果项目不受信任，mare 会在标准错误上打印一条说明，并在没有这些资源的情况下继续。传入 `--trust` 可在本次运行中信任项目，或运行一次 `mare trust`。项目的 MCP 服务器需要 `--approve-mcps`；见 [MCP](mcp.md)。

只在一次性的容器或虚拟机中使用 `--yolo`。

## 退出码

| 代码 | 含义 |
| --- | --- |
| 0 | 已完成。 |
| 1 | 运行失败（provider 错误、没有可用模型、找不到会话等）。 |
| 2 | 参数或配置有误；运行从未开始。 |
| 3 | 运行因一次被回答为“拒绝并停止”（`abort`）的审批而停止。无人回答的审批只会被拒绝，不会设置此退出码。 |
| 4 | 达到 `--max-turns` 或 `--timeout`。 |
| 5 | 会话持久化失败（仍输出最终答案，未保存的记录隔离写入 `<state>/quarantine/`）。 |
| 130 | 被中断（SIGINT）。 |
| 141 | 标准输出被读取方关闭。 |

使用 `--json` 时，`settled` 记录中的 `exit_code` 与此相同，并带有一个 `reason`（`completed`、`error`、`usage`、`approval_unavailable`、`max_turns`、`timeout`、`save_failed`、`interrupted`）。完整列表见[退出码](../reference/exit-codes.md)。

## CI 示例

一个在运行失败时让任务失败、并把流保留为产物的 shell 步骤：

```sh
export ANTHROPIC_API_KEY="$SECRET"
mare -p --json --no-session --permission read-only --timeout 10m \
  -m anthropic/claude-sonnet-5-5 \
  "Review the changes in this pull request and list concrete bugs." > review.jsonl
code=$?
jq -r 'select(.type == "settled") | .data.text' review.jsonl > review.md
exit $code
```

一个让智能体在一次性运行器中编辑文件的 GitHub Actions 步骤：

```yaml
- name: Fix lint errors
  env:
    OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
  run: |
    mare -p --max-turns 20 --timeout 15m \
      "Run the linter, fix every error it reports, and re-run it until it passes."
    git diff --stat
```

检查结果并处理退出码：

```sh
mare -p --json "..." > out.jsonl
case $? in
  0) echo "ok" ;;
  3) echo "needs a permission the CI preset does not grant" ;;
  4) echo "hit --max-turns or --timeout" ;;
  *) echo "failed" ;;
esac
```

使用 API 密钥环境变量进行认证（见[模型](models.md)）；`mare login` 会把凭据存到磁盘上，更适合只在你自己的机器上使用。
