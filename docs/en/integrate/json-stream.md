# JSON event stream

`mare --json` runs one prompt headless and writes one JSON record per line to stdout. The stream starts with a `session` record, carries the public agent events as the run proceeds, and ends with a `settled` record that holds the final text and the exit code. This page is the contract a script can rely on.

## Run it

```sh
mare --json "Run the tests and fix any failures"
```

`--json` is shorthand for `--mode json`. The prompt is an argument, or stdin when you pass `-` or no argument and stdin is a pipe. Every record is a single line `{"type": ..., "data": ...}`. Only records go to stdout. Warnings and denied approvals go to stderr.

This is what a run that never starts looks like. A bad argument still ends with a `settled` record, so a reader always gets an outcome:

```json
{"type":"session","data":{"protocol_version":"3.0","cwd":"/work/app"}}
{"type":"turn.failed","data":{"error":{"code":"invalid_arguments","message":"unexpected argument '--bogus' found"}}}
{"type":"settled","data":{"text":"","error":{"code":"invalid_arguments","message":"unexpected argument '--bogus' found"},"exit_code":2,"reason":"usage"}}
```

## Records

Each line has two keys:

| Key | Meaning |
|---|---|
| `type` | `session`, an event name, `lagged`, `dropped`, `turn.failed` or `settled`. |
| `data` | The record payload, always an object. |

Readers should ignore keys they do not know. New fields are added without notice and do not change the meaning of existing ones.

### `session`

Always the first line.

| Field | Meaning |
|---|---|
| `protocol_version` | The protocol version, `major.minor`. |
| `session_id` | Session id. With `--no-session` it is a temporary id that cannot be resumed. |
| `path` | Session file. Absent with `--no-session`. |
| `cwd` | Working directory of the run. |
| `model` | Model reference, when known. |
| `ephemeral` | `true` with `--no-session`: no session transcript is saved, the run cannot be resumed, and there is no crash recovery. Other writes (runtime files, snapshots, tool output) can still happen. |

Use `session_id` to continue the conversation later with `mare -p --resume <session_id> "..."` (see [Sessions](../use/sessions.md)).

### Event records

Between `session` and `settled`, `type` is the name of a public agent event and `data` is its payload. Event names have the form `area/name`; the full generated list of built-in public events (with delivery class, ignorability, payload fields, and examples) is in the [Event reference](../reference/events.md).

Internal plugin events never appear. To publish custom plugin events on the public stream, declare them in a service contract with `expose = "both"`. Over RPC, the primary connection can also opt into additional JSON kernel events with `events.subscribe { session_id: "...", include: [...] }` (or `{ scope: "all", include: [...] }`).

Built-in events are checked against their schemas before delivery. Payloads must be plain JSON. A plugin event that uses a reserved name (`session`, `turn.failed`, `settled`, `lagged`, `dropped`) fails the run with a stream error. Print the payload schemas with `mare rpc schema`; the stream's own records are under its `headless` key.

When a tool call finishes, `item/completed` carries an item with `item.type == "tool_call"`, `tool_call_id`, `name`, `message`, `result_status` (`"success"`, `"error"`, `"denied"`, `"cancelled"` or `"persist_failed"`), and `execution_status`. Those statuses do not say whether side effects happened.

### Item level and deltas

By default the stream omits `ignorable` delta and advisory events (such as `item/delta`, which fires per chunk of text, reasoning, or tool output) and delivers lifecycle boundaries and completed items (`turn/*`, `step/*`, `item/started`, `item/completed`).

Add `--stream-deltas` to include ignorable events such as `item/delta`:

```sh
mare --json --stream-deltas "Explain src/main.rs" | jq -r 'select(.type=="item/delta" and .data.kind=="text") | .data.delta'
```

### `lagged` and `dropped`

`{"type":"lagged","data":{"from_seq":A,"to_seq":B}}` reports a contiguous sequence range skipped because the reader was slower than mare, and `{"type":"dropped","data":{"count":N}}` counts unretained `lossy` or refused events omitted under pressure. Events with `delivery = "required"` (`turn/*`, `step/*`, `item/started`, `item/delta`, `item/completed`, `subagent/*`) wait for room instead of being dropped silently.

### `turn.failed`

When the run ends with an error, `{"type":"turn.failed","data":{"error":{"code":"...","message":"..."}}}` (with optional `data`) is written just before `settled`.

### `settled`

Always the last line. Nothing follows it.

| Field | Meaning |
|---|---|
| `text` | The final assistant text. |
| `usage` | Token counts, plus `cost` when the usage plugin reported one. |
| `error` | Structured error object `{code, message, data?}`, when the run failed. |
| `exit_code` | The process exit code. |
| `reason` | Why the run ended. |

`reason` and `exit_code` pair up like this (see [Exit codes](../reference/exit-codes.md)):

| `reason` | `exit_code` | Meaning |
|---|---|---|
| `completed` | 0 | The turn finished. |
| `error` | 1 | A provider, tool or runtime error, or the process got SIGTERM or SIGHUP. |
| `usage` | 2 | Bad arguments or configuration; the run never started. |
| `approval_unavailable` | 3 | An approval was answered with "deny and stop" (`abort`) and the turn was stopped. An approval nobody could answer is only denied and the run can still complete with `0`. |
| `max_turns` | 4 | `--max-turns` was reached. |
| `timeout` | 4 | `--timeout` was reached. |
| `save_failed` | 5 | Session persistence failed; unsaved records were quarantined under `<state>/quarantine/`. |
| `interrupted` | 130 | SIGINT. |

## Guarantees

- The first record is `session` and the last is `settled`, except when stdout closes or output stops being delivered (below).
- Records are written by one writer, in order, and each one is flushed before the next is admitted.
- Event order is the order the agent produced them. `lagged` markers keep their place in the sequence.
- `settled.exit_code` equals the process exit code.
- If the reader goes away (`mare --json ... | head`), mare stops the turn instead of spending tokens for nobody, and exits with 141.
- `--timeout` covers output delivery as well as agent work. If a reader blocks forever, the stream can end without a `settled` record, with a message on stderr and a non-zero exit code. Treat a missing `settled` as a failed run.
- The first SIGINT cancels the turn and still writes `settled` with `interrupted`. A second one exits at once with code 130 and no `settled`.

## jq

Print the final text, then exit with mare's code (Bash):

```sh
mare --json "List the TODOs in src/" \
  | tee run.jsonl \
  | jq -r 'select(.type == "settled") | .data.text'
code=${PIPESTATUS[0]}
exit "$code"
```

Follow tool calls as they finish:

```sh
mare --json "Update the changelog" \
  | jq -r 'select(.type == "item/completed" and .data.item.type == "tool_call") | "tool: \(.data.item.name)"'
```

Show only the outcome:

```sh
mare --json "Run the linter" | jq -c 'select(.type == "settled") | .data | {reason, exit_code, usage}'
```

## A script

This Python script runs a prompt, prints the session id and each finished tool call, and exits with the run's own exit code. It fails loudly when the stream ends without `settled`.

```python
import json
import subprocess
import sys

proc = subprocess.Popen(
    ["mare", "--json", *sys.argv[1:]],
    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True,
)
settled = None
for line in proc.stdout:
    record = json.loads(line)
    kind, data = record["type"], record["data"]
    if kind == "session":
        print("session", data.get("session_id"), data["cwd"])
    elif kind == "item/completed" and (data.get("item") or {}).get("type") == "tool_call":
        print("tool", data["item"].get("name"))
    elif kind == "settled":
        settled = data
code = proc.wait()
if settled is None:
    sys.exit(f"stream ended without a settled record (exit code {code})")
print(settled["reason"], settled["text"][:200])
sys.exit(settled["exit_code"])
```

Run it as `python3 run.py "Fix the failing test"`. For two-way control (approvals, host tools, steering a running turn) use the [RPC](rpc.md) interface instead.
