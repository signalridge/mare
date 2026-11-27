# Headless mode

Run mare without the interactive UI, for scripts and CI: print the final answer, or stream machine-readable events, then exit with a status code.

```sh
mare -p "list the public functions in src/lib.rs"
```

`-p` (`--print`) runs the prompt and prints the final answer to stdout. mare also switches to this mode by itself when stdin or stdout is not a terminal. Nothing is asked of you while it runs: approvals that would need an answer are denied (see below).

## Giving the prompt

```sh
mare -p "explain this error"                     # prompt as arguments
git diff | mare -p "review this diff"            # piped stdin is appended as <stdin>...</stdin>
cat prompt.md | mare -p                          # no prompt argument: stdin is the prompt
mare -p - < prompt.md                            # "-" reads the prompt from stdin explicitly
mare -p explain @src/main.rs                     # a separate @path argument attaches that text file
```

- With a prompt argument and piped stdin, the stdin text is added after the prompt inside `<stdin>` tags. If nothing arrives on stdin within one second, mare ignores it so an inherited pipe cannot hang the run.
- Stdin must be UTF-8 and at most 64 MiB. Empty stdin with no prompt is a usage error (exit code 2).
- An `@path` argument is a whole shell word that starts with `@`. A text file is attached inline; images and unreadable paths stay as written.

Continue earlier work with the session options from [Sessions](sessions.md):

```sh
mare -p -c "now add tests"                         # continue the last session here
mare -p --resume 3f9a1c2e "and update the docs"    # by id; see `mare session new`
mare -p --no-session "one-off question"            # do not save this session
```

## Machine-readable output

```sh
mare -p --json "fix the failing test" > run.jsonl
mare -p --json --stream-deltas "write a haiku"
```

`--json` (shorthand for `--mode json`) prints one JSON object per line. The first line is a `session` record, then the public agent events (turn starts, messages, tool calls, usage), `turn.failed` if the run failed, and `settled` last. Without `--stream-deltas` the stream is item level; with it, text and tool-output deltas are included too. The record formats are in [JSON stream](../integrate/json-stream.md).

```sh
mare -p --json "summarize CHANGELOG.md" | jq -r 'select(.type == "settled") | .data.text'
mare -p --json "..." | jq -r 'select(.type == "settled") | "\(.data.reason) \(.data.exit_code)"'
```

If the reader of stdout goes away (`... | head`), mare stops the run instead of spending tokens for nobody and exits with 141.

For a long-lived two-way connection use `--rpc` instead; see [RPC](../integrate/rpc.md).

### Structured output (`--output-schema`)

Pass `--output-schema FILE` in headless mode (`-p` or `--json`) to constrain the final answer to a JSON Schema and validate it before exiting.

Minimal schema (`summary.schema.json`):

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

Run and read the validated JSON object:

```sh
# JSON stream: save the stream first so mare's exit code is not masked by jq, then extract `.data.structured`
mare -p --json --no-session --output-schema summary.schema.json \
  "Inspect src/auth.rs and summarize risk." > run.jsonl || exit $?
jq -ce 'select(.type == "settled") | .data.structured' run.jsonl > summary.json

# Print mode: stdout receives the validated JSON text directly
mare -p --no-session --output-schema summary.schema.json \
  "Inspect src/auth.rs and summarize risk." > summary.json
```

Validation and error semantics:

- `--output-schema` is accepted only in headless runs (`-p` / `--json`). Using it outside headless mode, or passing a missing file, invalid JSON, or an unsupported/invalid schema exits with code `2` before starting a turn.
- If the model's final text does not satisfy the schema, mare fails the run (`error_kind = "invalid_output"`) and exits with code `1`. In `-p` mode stdout stays empty and the validation error is printed to stderr; in `--json` mode mare emits `turn.failed` and `settled` with `error` populated and `structured` omitted.

## Limits and tools

| Option | Effect |
| --- | --- |
| `--max-turns N` | Stop after `N` model turns in one prompt. Exit code 4. |
| `--timeout DURATION` | Stop the run after this long. Accepts `90`, `90s`, `1500ms`, `5m`, `1h`. Exit code 4. Headless only. |
| `--output-schema FILE` | Validate the final response against a JSON Schema file (`structured` field in `--json`). Headless only. |
| `--tools a,b` | Offer the model only these tools, for example `--tools read,ls`. |
| `--exclude-tools TOOLS` | Exclude matching tools (repeatable or comma-separated; supports `*` globs), for example `--exclude-tools bash,write`. Excluded tools are hidden from requests and refused at execution. |
| `--add-dir DIR` | Add an existing physical directory to the session workspace (repeatable; absolute paths without symlinks, outside protected paths; not accepted by `mare acp`). |
| `--append-system-prompt TEXT` | Add text to the end of the system prompt for this run. |
| `-m`, `-e` | Model and reasoning effort; see [Models](models.md). |
| `--permission`, `--yolo`, `--approval-policy` | Permission preset and approval policy; see below. |
| `--no-session` | Do not persist the run. |
| `--set KEY=VALUE` | Override one option, for example `--set plugins.tools.edit_mode=replace`. |
| `--strict-config` | Refuse to start if the configuration has warnings. |
| `--clean` | Ignore user and project config, user plugins, packages and `state.json`. |

```sh
mare -p --tools read,ls --max-turns 8 --timeout 5m \
  --append-system-prompt "Answer in one paragraph." \
  "what does this repository do?"
```

## Permissions and approvals

Ordinary main sessions default to `auto` in trusted projects and use `untrusted_preset` (default `read-only`) in untrusted projects; an explicit `--permission` flag takes precedence. In headless runs, approvals with no responder are denied and the tool returns the denial to the model so it can continue with other work:

- mare prints `mare: N approval(s) were needed and nobody could answer (denied)` to stderr, followed by one line per denied call.
- The exit code stays 0 if the run otherwise completes. Exit code 3 means the run stopped because of an approval that was answered with "deny and stop".

Choose the preset that matches the job:

```sh
mare -p --permission read-only "audit this repo for hard-coded secrets"   # no writes at all
mare -p "fix the typo in README.md"                                       # trusted project: auto writes in the workspace
mare -p --yolo "run the full migration"                                   # full-access: no sandbox; still asks for prompt rules and dangerous commands
mare -p --approval-policy never "..."                                     # reject anything that would ask
```

Explicit `prompt` rules and dangerous commands such as `rm -f` ask in every preset, including `--yolo`, and are therefore denied in a headless run unless a rule allows that exact command. An unanalyzable command is not automatically denied by parse failure alone: in `full-access` with no restrictive rule (`prompt` or `forbidden`) applying in that directory, it can still be allowed; it falls back to `ask` (and thus denial in headless runs) only when an applicable restrictive rule might match it. See [Permissions](permissions.md).

Project resources (`.mare/plugins`, `.mare/init.lua`, skills, prompts, `.mare/SYSTEM.md`) load only in trusted projects, and headless runs never ask. If the project is not trusted, mare prints a note on stderr and carries on without them. Pass `--trust` to trust the project for this run, or run `mare trust` once. Project MCP servers need `--approve-mcps`; see [MCP](mcp.md).

Run `--yolo` only in a throwaway container or VM.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Completed. |
| 1 | The run failed (provider error, no usable model, session not found, and so on). |
| 2 | Bad arguments or configuration; the run never started. |
| 3 | The run stopped because an approval was answered with "deny and stop" (`abort`). An approval nobody could answer is only denied and does not set this code. |
| 4 | `--max-turns` or `--timeout` was reached. |
| 5 | Session persistence failed (final answer still printed, unsaved records quarantined under `<state>/quarantine/`). |
| 130 | Interrupted (SIGINT). |
| 141 | stdout was closed by the reader. |

With `--json`, the `settled` record carries the same code as `exit_code` and a `reason` (`completed`, `error`, `usage`, `approval_unavailable`, `max_turns`, `timeout`, `save_failed`, `interrupted`). The full list is in [Exit codes](../reference/exit-codes.md).

## CI examples

A shell step that fails the job when the run fails, and keeps the stream as an artifact:

```sh
export ANTHROPIC_API_KEY="$SECRET"
mare -p --json --no-session --permission read-only --timeout 10m \
  -m anthropic/claude-sonnet-5-5 \
  "Review the changes in this pull request and list concrete bugs." > review.jsonl
code=$?
jq -r 'select(.type == "settled") | .data.text' review.jsonl > review.md
exit $code
```

A GitHub Actions step that lets the agent edit files in a disposable runner:

```yaml
- name: Fix lint errors
  env:
    OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
  run: |
    mare -p --max-turns 20 --timeout 15m \
      "Run the linter, fix every error it reports, and re-run it until it passes."
    git diff --stat
```

Checking the result and handling the exit code:

```sh
mare -p --json "..." > out.jsonl
case $? in
  0) echo "ok" ;;
  3) echo "needs a permission the CI preset does not grant" ;;
  4) echo "hit --max-turns or --timeout" ;;
  *) echo "failed" ;;
esac
```

Authenticate with an API key environment variable (see [Models](models.md)); `mare login` stores credentials on disk and is better kept for your own machine.
