# Minimal RPC client example

[`minimal_client.py`](minimal_client.py) is a self-contained Python 3 script (standard library only) that spawns `mare --rpc` over stdio and runs a complete JSON-RPC 2.0 round trip:

1. `initialize` (protocol `3.0`)
2. `events.subscribe` for the active session
3. `agent.prompt` followed by `agent.wait` while handling incoming `event` notifications and `approval.request` callbacks
4. `shutdown`

## Usage

```sh
python3 examples/rpc/minimal_client.py --no-session --prompt "Summarize this repository"
```

Pass `--mare /path/to/mare` (or set `MARE_BIN`) to target a specific binary, `--model <provider/model>` to pick a model, and `--json` for machine-readable summary output.

## Documentation

- [RPC guide (English)](../../docs/en/integrate/rpc.md)
- [RPC 指南（简体中文）](../../docs/zh/integrate/rpc.md)
- [RPC ガイド（日本語）](../../docs/ja/integrate/rpc.md)
