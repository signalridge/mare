"""Verifier helpers shared by every mare eval task (copied into each task's tests/ by
`evals/run.py`; edit this file, not the copies). Standard library only.

The mare adapter (evals/agent/mare_agent.py) writes, inside the task container:
  /logs/agent/mare.jsonl        headless event stream (first line `session`, last `settled`)
  /logs/agent/exit_code         mare's exit code
  /logs/agent/mare/state/sessions/**.jsonl   the session log (tool calls and results)
"""

import json
import os
from pathlib import Path, PurePosixPath

LOGS = "/logs/agent"


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def _invalid_constant(_):
    raise ValueError("nonfinite JSON number")


def _records(path):
    """Read complete UTF-8 JSONL without hiding malformed or truncated records."""
    try:
        data = Path(path).read_bytes().decode("utf-8")
    except (OSError, UnicodeError) as exc:
        raise ValueError(f"cannot read complete JSONL: {Path(path).name}") from exc
    if not data or not data.endswith("\n"):
        raise ValueError(f"truncated JSONL: {Path(path).name}")
    rows = []
    for number, line in enumerate(data.split("\n")[:-1], 1):
        try:
            row = json.loads(line, parse_constant=_invalid_constant, object_pairs_hook=_unique_object)
        except ValueError as exc:
            raise ValueError(f"invalid JSONL at {Path(path).name}:{number}") from exc
        if not isinstance(row, dict):
            raise ValueError(f"invalid JSONL object at {Path(path).name}:{number}")
        rows.append(row)
    return rows


def events(logs=None, *, allow_incomplete=False):
    root = Path(logs if logs is not None else LOGS)
    rows = _records(root / "mare.jsonl")
    if rows[0].get("type") != "session" or (
        not allow_incomplete and (len(rows) < 2 or rows[-1].get("type") != "settled")
    ):
        raise ValueError("incomplete session event stream")
    start = rows[0].get("data")
    if not isinstance(start, dict) or start.get("protocol_version") != "3.0":
        raise ValueError("session requires protocol 3.0")
    sid, path = start.get("session_id"), start.get("path")
    if not isinstance(sid, str) or not sid or (path is not None and (not isinstance(path, str) or not path)):
        raise ValueError("invalid session id or path")
    active = {"turn": set(), "step": set(), "item": set()}
    seen = {"turn": set(), "step": set(), "item": set()}
    retry_steps = set()
    sequences = {}
    session_epoch = None
    saw_loss = False
    for index, event in enumerate(rows):
        kind = event.get("type")
        if not isinstance(kind, str) or not isinstance(event.get("data"), dict):
            raise ValueError("invalid event envelope")
        if kind in {"dropped", "lagged", "stream.failed"}:
            if not allow_incomplete:
                raise ValueError(f"incomplete event stream: {kind}")
            for candidate in (event.get("session_id"), event["data"].get("session_id")):
                if candidate is not None and candidate != sid:
                    raise ValueError("event belongs to another session")
            saw_loss = True
            continue
        if (kind == "session" and index != 0) or (kind == "settled" and index != len(rows) - 1):
            raise ValueError("duplicate session or settled record")
        if event.get("session_id") is not None and event["session_id"] != sid:
            raise ValueError("event belongs to another session")
        if kind not in {"session", "settled", "turn.failed"}:
            epoch, seq = event.get("session_epoch"), event.get("seq")
            if epoch is not None and (not isinstance(epoch, str) or not epoch):
                raise ValueError("invalid session epoch")
            if epoch is not None:
                if session_epoch is not None and epoch != session_epoch:
                    raise ValueError("session epoch changed during the run")
                session_epoch = epoch
            if type(seq) is not int or seq <= sequences.get(epoch, 0):
                raise ValueError("invalid or duplicate event sequence")
            sequences[epoch] = seq
        if kind in {f"{group}/{phase}" for group in active for phase in ("started", "completed")}:
            group, phase = kind.split("/")
            data = event["data"]
            ident = data.get(f"{group}_id")
            turn = data.get("turn_id")
            if not isinstance(ident, str) or not ident:
                raise ValueError(f"missing {group} id")
            if group != "turn" and turn is not None and turn not in active["turn"] and not (allow_incomplete and saw_loss):
                raise ValueError(f"{group} belongs to an inactive turn")
            if event.get("turn_id") is not None and event["turn_id"] != turn:
                raise ValueError("event turn id mismatch")
            key = ident if group == "turn" else (turn, ident)
            if phase == "started":
                if key in seen[group]:
                    if group != "step" or key not in retry_steps or key in active[group]:
                        raise ValueError(f"duplicate {group} id")
                    retry_steps.remove(key)
                seen[group].add(key)
                active[group].add(key)
            else:
                if key not in active[group]:
                    if not (allow_incomplete and saw_loss):
                        raise ValueError(f"unpaired {group} id")
                else:
                    if group == "turn" and any(pair[0] == key for g in ("step", "item") for pair in active[g]) and not (allow_incomplete and saw_loss):
                        raise ValueError("turn ended with unpaired step or item")
                    active[group].remove(key)
                if group == "step" and data.get("will_retry") is True:
                    retry_steps.add(key)
    if not allow_incomplete and (not seen["turn"] or any(active.values())):
        raise ValueError("incomplete turn or item lifecycle")
    return rows


def settled():
    s = [e for e in events() if e.get("type") == "settled"]
    return (s[-1].get("data") or {}) if s else {}


def final_text():
    return settled().get("text") or ""


def exit_code():
    try:
        return int((Path(LOGS) / "exit_code").read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return -1


def _session(logs=None, stream=None):
    """Resolve and validate the container session against the synced log root."""
    root = Path(logs if logs is not None else LOGS)
    first = (stream if stream is not None else events(root))[0]["data"]
    prefix = PurePosixPath("/logs/agent/mare/state/sessions")
    try:
        base = (root / "mare/state/sessions").resolve(strict=True)
        if first.get("path") is None:
            matches = []
            for candidate in base.rglob("*.jsonl"):
                try:
                    target = candidate.resolve(strict=True)
                    if not target.is_relative_to(base) or not target.is_file():
                        continue
                    with target.open(encoding="utf-8") as f:
                        header = json.loads(f.readline(), parse_constant=_invalid_constant, object_pairs_hook=_unique_object)
                    if isinstance(header, dict) and header.get("type") == "session" and header.get("session_id") == first["session_id"]:
                        matches.append(target)
                except (OSError, UnicodeError, ValueError, RuntimeError):
                    continue
            if len(matches) != 1:
                raise ValueError("expected exactly one matching session file")
            target = matches[0]
        else:
            path = PurePosixPath(first["path"])
            if ".." in path.parts:
                raise ValueError("invalid session path")
            relative = path.relative_to(prefix)
            target = (base / str(relative)).resolve(strict=True)
        if not target.is_relative_to(base) or not target.is_file():
            raise ValueError("invalid session path")
    except OSError as exc:
        raise ValueError("invalid or missing session path") from exc
    rows = _records(target)
    if rows[0].get("type") != "session" or rows[0].get("session_id") != first["session_id"]:
        raise ValueError("session file header does not match session id")
    return target, rows


def session_file(logs=None, stream=None):
    return _session(logs, stream)[0]


class Log:
    """Tool calls (with the index of the assistant message that issued them) and results."""

    def __init__(self, logs=None):
        self.calls, self.results = [], {}
        self.events = events(logs)
        _, rows = _session(logs, self.events)
        entries, calls = set(), set()
        n = 0
        for e in rows[1:]:
            if e.get("type") == "session":
                raise ValueError("duplicate session header")
            entry_id = e.get("entry_id")
            if entry_id is not None:
                if not isinstance(entry_id, str) or not entry_id or entry_id in entries:
                    raise ValueError("invalid or duplicate session entry id")
                entries.add(entry_id)
            m = e.get("message", {})
            if not isinstance(m, dict):
                raise ValueError("invalid session message")
            role = m.get("role")
            if role == "toolResult":
                raise ValueError("legacy tool result is not supported")
            if role in {"assistant", "tool_result"}:
                content = m.get("content")
                if not isinstance(content, list) or any(not isinstance(b, dict) or not isinstance(b.get("type"), str) for b in content):
                    raise ValueError("invalid message content")
            if role == "assistant":
                n += 1
                for b in content:
                    if isinstance(b, dict) and b.get("type") == "toolCall":
                        raise ValueError("legacy tool call is not supported")
                    if isinstance(b, dict) and b.get("type") == "tool_call":
                        cid, name = b.get("id"), b.get("name")
                        if not isinstance(cid, str) or not cid or cid in calls or not isinstance(name, str) or not name:
                            raise ValueError("invalid or duplicate tool call id")
                        calls.add(cid)
                        self.calls.append({"msg": n, "id": cid, "name": name, "raw": json.dumps(b)})
            elif role == "tool_result":
                cid = m.get("tool_call_id")
                if not isinstance(cid, str) or cid not in calls or cid in self.results:
                    raise ValueError("orphan or duplicate tool result id")
                if any(b.get("type") == "text" and not isinstance(b.get("text"), str) for b in content):
                    raise ValueError("invalid tool result text")
                text = "\n".join(b.get("text", "") for b in content if isinstance(b, dict))
                self.results[cid] = {"text": text, "is_error": bool(m.get("is_error"))}
        if calls != self.results.keys():
            raise ValueError("unpaired tool call id")

    def named(self, name):
        return [c for c in self.calls if c["name"] == name]

    def result(self, call):
        return self.results.get(call["id"])

    def unpaired(self):
        return [c["id"] for c in self.calls if c["id"] not in self.results]

    def max_batch(self, name):
        per = {}
        for c in self.named(name):
            per[c["msg"]] = per.get(c["msg"], 0) + 1
        return max(per.values(), default=0)


def says_number(text, n):
    import re

    return n in re.split(r"[^0-9]+", text)


def finish(ok, details, checks):
    """Write the reward (1 when every check passed) and a details file; print a summary."""
    os.makedirs("/logs/verifier", exist_ok=True)
    details = dict(details)
    details["checks"] = checks
    with open("/logs/verifier/reward.txt", "w") as f:
        f.write("1" if ok else "0")
    with open("/logs/verifier/details.json", "w") as f:
        json.dump(details, f, indent=2, ensure_ascii=False)
    for k, v in checks.items():
        print(f"{'PASS' if v else 'FAIL'}  {k}")
    for k, v in details.items():
        if k != "checks":
            print(f"      {k}: {v}")
    print("REWARD", 1 if ok else 0)


def common_checks(log):
    """What every task checks: mare exited 0, a session log exists, every tool call has a result."""
    return {
        "exit code 0": exit_code() == 0,
        "session log exists": session_file() is not None,
        "every tool call has a result": bool(log.calls) and not log.unpaired(),
    }


def run_pytest(args, cwd, pythonpath=None, name="pytest"):
    """Run `python -m pytest <args>` and count results from a JUnit XML report.

    Returns {"passed", "failed", "errors", "skipped", "total", "rc", "tail"}; the full output goes
    to /logs/verifier/<name>.log. Used by the dev-* tasks for hidden and original test suites."""
    import subprocess
    import xml.etree.ElementTree as ET

    os.makedirs("/logs/verifier", exist_ok=True)
    xml = f"/tmp/{name}.junit.xml"
    if os.path.exists(xml):
        os.remove(xml)
    env = dict(os.environ)
    env.pop("PYTEST_ADDOPTS", None)
    if pythonpath:
        env["PYTHONPATH"] = pythonpath
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    try:
        p = subprocess.run(
            ["python3", "-m", "pytest", "-p", "no:cacheprovider", "-q", f"--junitxml={xml}", *args],
            check=False,
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            timeout=240,
        )
        out, rc = p.stdout + p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        out, rc = f"timeout: {e}", -1
    with open(f"/logs/verifier/{name}.log", "w") as f:
        f.write(out)
    r = {"passed": 0, "failed": 0, "errors": 0, "skipped": 0, "total": 0, "rc": rc}
    try:
        root = ET.parse(xml).getroot()
        for s in [root] if root.tag == "testsuite" else root.iter("testsuite"):
            r["total"] += int(s.get("tests", 0))
            r["failed"] += int(s.get("failures", 0))
            r["errors"] += int(s.get("errors", 0))
            r["skipped"] += int(s.get("skipped", 0))
    except (OSError, ET.ParseError):
        pass
    r["passed"] = r["total"] - r["failed"] - r["errors"] - r["skipped"]
    r["tail"] = out.strip().splitlines()[-1] if out.strip() else ""
    return r


def all_pass(r, at_least=1):
    return r["rc"] == 0 and r["failed"] == 0 and r["errors"] == 0 and r["passed"] >= at_least
