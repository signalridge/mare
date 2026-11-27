"""Read protocol and contract event metadata for generated documentation."""

import json
import os
import re
import tempfile
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_seams import rpc_exchange, rpc_initialize


def _scan_balanced(text: str, start: int, open_ch: str, close_ch: str) -> tuple[str, int]:
    """Return the substring inside balanced delimiters starting after `open_ch`, plus end index."""
    depth = 1
    pos = start
    in_str = False
    esc = False
    while pos < len(text) and depth > 0:
        ch = text[pos]
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
        else:
            if ch == '"':
                in_str = True
            elif ch == open_ch:
                depth += 1
            elif ch == close_ch:
                depth -= 1
        pos += 1
    return text[start : pos - 1], pos


def parse_event_registry(path: Path) -> list[dict]:
    """Read `protocol_events!` entries from crates/mare-protocol/src/event_registry.rs."""
    src = path.read_text(encoding="utf-8")
    marker = src.find("protocol_events! {")
    if marker == -1:
        raise ValueError(f"cannot find protocol_events! in {path}")
    block, _ = _scan_balanced(src, src.find("{", marker) + 1, "{", "}")
    pat = re.compile(r'"([^"]+)"\s*=>\s*([A-Za-z0-9_]+)\s*\([^)]*\)\s*\{')
    entries: list[dict] = []
    pos = 0
    while True:
        match = pat.search(block, pos)
        if not match:
            break
        body, pos = _scan_balanced(block, match.end(), "{", "}")
        doc_match = re.search(r'\bdoc:\s*"((?:\\.|[^"\\])*)"', body)
        ex_match = re.search(r'\bexample:\s*json!\(', body)
        if not doc_match or not ex_match:
            raise ValueError(f"incomplete protocol_events! entry for {match.group(1)}")
        ex_raw, _ = _scan_balanced(body, ex_match.end(), "(", ")")
        example = json.loads(re.sub(r",(\s*[}\]])", r"\1", ex_raw))
        entries.append(
            {
                "name": match.group(1),
                "mode": re.search(r"\bmode:\s*(\w+)", body).group(1).lower(),  # type: ignore[union-attr]
                "delivery": re.search(r"\bdelivery:\s*(\w+)", body).group(1).lower(),  # type: ignore[union-attr]
                "ignorable": re.search(r"\bignorable:\s*(true|false)", body).group(1) == "true",  # type: ignore[union-attr]
                "retain": re.search(r"\bretain:\s*(true|false)", body).group(1) == "true",  # type: ignore[union-attr]
                "on_error": re.search(r"\bon_error:\s*(\w+)", body).group(1).lower(),  # type: ignore[union-attr]
                "doc": json.loads('"' + doc_match.group(1) + '"'),
                "example": example,
            }
        )
    return entries


def event_payload_schemas(catalog: dict) -> dict[str, dict]:
    """Map public event name to its protocol event payload schema."""
    out: dict[str, dict] = {}
    for variant in catalog.get("headless", {}).get("protocol_event", {}).get("oneOf", []):
        props = variant.get("properties", {})
        name = props.get("type", {}).get("const")
        if isinstance(name, str) and "data" in props:
            out[name] = props["data"]
    return out


def contract_event_catalog(mare: str) -> list[dict]:
    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    with tempfile.TemporaryDirectory(prefix="mare-doc-events-", dir=tmp_root) as tmp:
        requests = [
            rpc_initialize(),
            {"jsonrpc": "2.0", "id": 2, "method": "events.list", "params": {}},
            {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {}},
        ]
        env = {"PATH": "/usr/bin:/bin", "HOME": tmp, "MARE_HOME": tmp, "MARE_TMPDIR": tmp, "TMPDIR": tmp, "TEMP": tmp, "TMP": tmp}
        replies = rpc_exchange(Path(mare).resolve(), requests, env=env, cwd=Path(tmp))
        reply = next((r for r in replies if r.get("id") == 2), {})
        if "result" not in reply:
            raise RuntimeError(f"events.list failed: {reply}")
        return reply["result"]["data"]
