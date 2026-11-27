#!/usr/bin/env python3
"""Minimal JSON-RPC 2.0 client for `mare --rpc`.

Walks the basic lifecycle using only the Python standard library:
initialize -> events.subscribe -> agent.prompt -> agent.wait -> shutdown.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from typing import Any


class RpcClient:
    def __init__(self, cmd: list[str], *, quiet_events: bool = False) -> None:
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        assert self.proc.stdin is not None and self.proc.stdout is not None
        self.stdin = self.proc.stdin
        self.stdout = self.proc.stdout
        self.next_id = 0
        self.quiet_events = quiet_events
        self.events: list[str] = []

    def send(self, msg: dict[str, Any]) -> None:
        payload = {"jsonrpc": "2.0", **msg}
        self.stdin.write(json.dumps(payload) + "\n")
        self.stdin.flush()

    def handle_incoming(self, msg: dict[str, Any]) -> None:
        method = msg["method"]
        if method == "event":
            event_type = msg.get("params", {}).get("type")
            if isinstance(event_type, str):
                self.events.append(event_type)
                if not self.quiet_events:
                    print(f"event: {event_type}", file=sys.stderr)
        elif method == "approval.request" and "id" in msg:
            self.send({"id": msg["id"], "result": {"decision": "deny"}})
        elif "id" in msg:
            self.send(
                {
                    "id": msg["id"],
                    "error": {"code": -32601, "message": f"unsupported callback: {method}"},
                }
            )

    def call(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        self.next_id += 1
        rid = self.next_id
        self.send({"id": rid, "method": method, "params": params or {}})
        for line in self.stdout:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            if "method" in msg:
                self.handle_incoming(msg)
            elif msg.get("id") == rid:
                if "error" in msg:
                    raise RuntimeError(f"{method} failed: {msg['error']}")
                return msg["result"]
        raise RuntimeError(f"mare closed stdout while waiting for {method}")

    def close(self) -> int:
        try:
            self.call("shutdown")
        finally:
            self.stdin.close()
        return self.proc.wait()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Minimal mare --rpc client example.")
    parser.add_argument(
        "--mare",
        default=os.environ.get("MARE_BIN", "mare"),
        help="Path to the mare binary (default: $MARE_BIN or 'mare').",
    )
    parser.add_argument("--model", help="Optional model reference passed to mare --model.")
    parser.add_argument(
        "--no-session",
        action="store_true",
        help="Pass --no-session so mare does not persist a session file.",
    )
    parser.add_argument(
        "--prompt",
        default="Summarize this repository",
        help="Prompt text to send via agent.prompt.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print a JSON summary instead of plain text.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    cmd = [args.mare, "--rpc"]
    if args.no_session:
        cmd.append("--no-session")
    if args.model:
        cmd.extend(["--model", args.model])

    client = RpcClient(cmd, quiet_events=args.json)
    try:
        init = client.call(
            "initialize",
            {
                "protocol_version": "3.0",
                "client": {"name": "mare-rpc-example", "version": "0.1"},
                "capabilities": {"approvals": True},
            },
        )
        sid = (init.get("session") or {}).get("session_id") or client.call("session.create")["session_id"]

        sub = client.call("events.subscribe", {"session_id": sid})
        started = client.call("agent.prompt", {"session_id": sid, "input": args.prompt})
        result = client.call("agent.wait", {"session_id": sid, "turn_id": started["turn_id"]})
    finally:
        exit_code = client.close()

    if exit_code != 0:
        print(f"mare exited with status {exit_code}", file=sys.stderr)
        return exit_code

    text = result.get("text", "")
    if args.json:
        print(
            json.dumps(
                {
                    "protocol_version": init["protocol_version"],
                    "session_id": sid,
                    "subscription": sub,
                    "turn_id": started["turn_id"],
                    "events": client.events,
                    "text": text,
                }
            )
        )
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
