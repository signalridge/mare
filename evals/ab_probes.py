"""Scenario probe definitions, saved offline records, deterministic scoring, and optional LLM judge.

Every scenario pairs positive (`must_show`) checks with negative (`must_not_show`)
probes and an optional budget. Deterministic checks run purely against the saved
run record (session log calls/results, events, final text, verifier details,
and timings) so a finished run can be re-scored offline without touching Docker,
Harbor, or the agent. An LLM judge is invoked only for probes explicitly marked
with ``{"type": "llm_judge"}``. Standard library only.
"""

from __future__ import annotations

import json
import math
import os
import re
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

from evals.lib import mare_log
from evals.run import EVALS, finite_cost, seconds

RECORD_SCHEMA_VERSION = 1
PROBE_KINDS = ("must_show", "must_not_show")
VALID_VERDICTS = {"present", "absent", "contradicted"}
JUDGE_REASKS = 2

JUDGE_SYSTEM = (
    "You evaluate statements against the saved JSONL view of an agent run. "
    "Each line is a JSON record: header, user/assistant message, tool_call, tool_result, "
    "event, verifier summary, or final_text. "
    'For each statement answer "present" if the run shows it and "absent" if it does not. '
    "Include a short supporting quote from the view. "
    'Answer with a JSON array only: [{"id": "<probe id>", "verdict": "present|absent", "quote": "<evidence>"}].'
)

DETERMINISTIC_CHECK_TYPES = {
    "verifier_check",
    "verifier_detail",
    "verifier_pass",
    "final_text_regex",
    "final_text_contains",
    "tool_call_regex",
    "tool_result_regex",
    "tool_count",
    "tool_batch",
    "event_match",
    "exit_code",
    "budget_within",
    "unpaired_tool_calls",
    "all",
    "any",
    "not",
}


def _validate_check(check: Any, probe_id: str) -> None:
    if not isinstance(check, dict):
        raise TypeError(f"probe {probe_id!r}: check must be an object")
    kind = check.get("type")
    if kind == "llm_judge":
        return
    if kind not in DETERMINISTIC_CHECK_TYPES:
        raise ValueError(f"probe {probe_id!r}: unsupported check type {kind!r}")
    if kind in ("all", "any"):
        subs = check.get("checks")
        if not isinstance(subs, list) or not subs:
            raise ValueError(f"probe {probe_id!r}: {kind} requires non-empty checks list")
        for sub in subs:
            _validate_check(sub, probe_id)
    elif kind == "not":
        _validate_check(check.get("check"), probe_id)
    elif kind == "verifier_check":
        if not isinstance(check.get("name"), str) or not check["name"]:
            raise ValueError(f"probe {probe_id!r}: verifier_check requires non-empty name")
    elif kind == "verifier_detail":
        if not isinstance(check.get("key"), str) or not check["key"]:
            raise ValueError(f"probe {probe_id!r}: verifier_detail requires non-empty key")
    elif kind in ("final_text_regex", "tool_call_regex", "tool_result_regex"):
        if not isinstance(check.get("pattern"), str) or not check["pattern"]:
            raise ValueError(f"probe {probe_id!r}: {kind} requires non-empty pattern")
        re.compile(check["pattern"])
    elif kind == "final_text_contains":
        if not isinstance(check.get("needle"), str) or not check["needle"]:
            raise ValueError(f"probe {probe_id!r}: final_text_contains requires non-empty needle")
    elif kind == "tool_batch":
        if not isinstance(check.get("tool"), str) or not check["tool"]:
            raise ValueError(f"probe {probe_id!r}: tool_batch requires non-empty tool")
    elif kind == "event_match":
        if not isinstance(check.get("event_type"), str) or not check["event_type"]:
            raise ValueError(f"probe {probe_id!r}: event_match requires non-empty event_type")
        if "pattern" in check and check["pattern"] is not None:
            re.compile(check["pattern"])


def validate_probe_spec(spec: dict[str, Any], name: str) -> dict[str, Any]:
    if not isinstance(spec, dict):
        raise TypeError(f"scenario {name!r}: probe specification must be a JSON object")
    budget = spec.get("budget") or {}
    if not isinstance(budget, dict):
        raise TypeError(f"scenario {name!r}: budget must be an object")
    for key in ("seconds", "max_tool_calls"):
        val = budget.get(key)
        if val is not None and (type(val) not in (int, float) or not math.isfinite(val) or val <= 0):
            raise ValueError(f"scenario {name!r}: budget.{key} must be a positive number")
    probes = spec.get("probes")
    if not isinstance(probes, list) or not probes:
        raise ValueError(f"scenario {name!r}: probes must be a non-empty list")
    seen: set[str] = set()
    kinds_present: set[str] = set()
    for probe in probes:
        if not isinstance(probe, dict):
            raise TypeError(f"scenario {name!r}: each probe must be an object")
        pid = probe.get("id")
        kind = probe.get("kind")
        text = probe.get("text")
        if not isinstance(pid, str) or not pid or pid in seen:
            raise ValueError(f"scenario {name!r}: invalid or duplicate probe id {pid!r}")
        if kind not in PROBE_KINDS:
            raise ValueError(f"scenario {name!r}: probe {pid!r} kind must be one of {PROBE_KINDS}")
        if not isinstance(text, str) or not text.strip():
            raise ValueError(f"scenario {name!r}: probe {pid!r} requires non-empty text")
        _validate_check(probe.get("check"), pid)
        seen.add(pid)
        kinds_present.add(kind)
    if "must_show" not in kinds_present or "must_not_show" not in kinds_present:
        raise ValueError(f"scenario {name!r}: probes must include both must_show and must_not_show entries")
    return {"budget": budget, "probes": probes}


def default_probe_spec() -> dict[str, Any]:
    """Generic fallback probes for Harbor tasks that do not ship a custom probes.json."""
    return {
        "budget": {},
        "probes": [
            {
                "id": "verifier-passed",
                "kind": "must_show",
                "text": "Harbor verifier reports reward == 1.0",
                "check": {"type": "verifier_pass"},
            },
            {
                "id": "no-unpaired-tool-calls",
                "kind": "must_not_show",
                "text": "Agent left unpaired tool calls without matching results",
                "check": {"type": "unpaired_tool_calls"},
            },
            {
                "id": "no-nonzero-exit",
                "kind": "must_not_show",
                "text": "Agent exited with a non-zero exit code",
                "check": {"type": "exit_code", "not_equals": 0},
            },
        ],
    }


def load_scenario(
    scenario: str,
    *,
    evals_dir: Path | None = None,
    probes_override: Path | None = None,
) -> dict[str, Any]:
    """Resolve a local Harbor task name or scenario directory and load its probes."""
    base = evals_dir or EVALS
    candidate_dir = Path(scenario)
    if candidate_dir.is_dir() and (candidate_dir / "task.toml").is_file():
        task_dir = candidate_dir.resolve()
        name = task_dir.name
    else:
        task_dir = (base / "tasks" / scenario).resolve()
        if not (task_dir / "task.toml").is_file():
            raise ValueError(f"unknown local Harbor scenario: {scenario!r}")
        name = scenario
    probes_path = probes_override if probes_override is not None else (task_dir / "probes.json")
    if probes_path.is_file():
        spec = validate_probe_spec(json.loads(probes_path.read_text(encoding="utf-8")), name)
    elif probes_override is not None:
        raise ValueError(f"probes file not found: {probes_override}")
    else:
        spec = validate_probe_spec(default_probe_spec(), name)
    return {
        "name": name,
        "task_dir": task_dir,
        "budget": spec["budget"],
        "probes": spec["probes"],
    }


def build_judge_view(record: dict[str, Any]) -> list[dict[str, Any]]:
    """Construct the ordered, self-contained JSONL judge view for a saved run."""
    lines: list[dict[str, Any]] = [
        {
            "type": "header",
            "schema_version": RECORD_SCHEMA_VERSION,
            "arm": record["arm"],
            "scenario": record["scenario"],
            "run_index": record["run_index"],
            "model": record.get("model"),
            "arm_identity": record.get("arm_identity", {}),
            "status": record.get("status"),
            "exit_code": record.get("exit_code"),
            "secs": record.get("secs"),
            "error": record.get("error"),
        }
    ]
    for call in record.get("calls", []):
        lines.append(
            {
                "type": "tool_call",
                "msg": call.get("msg"),
                "id": call.get("id"),
                "name": call.get("name"),
                "raw": call.get("raw"),
            }
        )
        res = call.get("result")
        if isinstance(res, dict):
            lines.append(
                {
                    "type": "tool_result",
                    "tool_call_id": call.get("id"),
                    "is_error": bool(res.get("is_error")),
                    "text": res.get("text", ""),
                }
            )
    for ev in record.get("events", []):
        lines.append({"type": "event", "event_type": ev.get("type"), "data": ev.get("data", {})})
    lines.append(
        {
            "type": "verifier",
            "reward": record.get("verifier", {}).get("reward"),
            "checks": record.get("verifier", {}).get("checks", {}),
            "details": record.get("verifier", {}).get("details", {}),
        }
    )
    lines.append({"type": "final_text", "text": record.get("final_text", "")})
    return lines


def _compact_events(raw_events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    keep = {
        "approval/requested",
        "approval/resolved",
        "compact/started",
        "compact/completed",
        "turn.failed",
        "settled",
        "usage/changed",
    }
    out = []
    for ev in raw_events:
        if ev.get("type") in keep:
            out.append({"type": ev["type"], "data": ev.get("data", {})})
    return out


def build_run_record(
    trial_folder: Path | None,
    *,
    arm: str,
    scenario: str,
    run_index: int,
    model: str,
    arm_identity: dict[str, Any],
    error_override: str | None = None,
) -> dict[str, Any]:
    """Capture a complete, self-contained run record from a Harbor trial folder."""
    record: dict[str, Any] = {
        "schema_version": RECORD_SCHEMA_VERSION,
        "arm": arm,
        "scenario": scenario,
        "run_index": run_index,
        "model": model,
        "arm_identity": arm_identity,
        "status": "ERROR",
        "error": error_override,
        "exit_code": None,
        "secs": None,
        "tokens": {"input": None, "output": None, "cache_read": None},
        "cost_usd": None,
        "verifier": {"reward": None, "checks": {}, "details": {}},
        "final_text": "",
        "calls": [],
        "unpaired_calls": [],
        "events": [],
    }
    if error_override or trial_folder is None or not trial_folder.is_dir():
        record["error"] = error_override or "missing trial folder"
        return record

    result_path = trial_folder / "result.json"
    if not result_path.is_file():
        record["error"] = "missing result.json"
        return record
    try:
        result = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        record["error"] = f"invalid result.json: {exc}"
        return record
    if not isinstance(result, dict):
        record["error"] = "invalid result.json"
        return record

    agent = result.get("agent_result")
    if isinstance(agent, dict):
        cost = agent.get("cost_usd")
        record["secs"] = seconds(result.get("started_at", ""), result.get("finished_at", ""))
        record["tokens"] = {
            "input": agent.get("n_input_tokens"),
            "output": agent.get("n_output_tokens"),
            "cache_read": agent.get("n_cache_tokens"),
        }
        record["cost_usd"] = float(cost) if finite_cost(cost) else None

    agent_logs = trial_folder / "agent"
    exit_path = agent_logs / "exit_code"
    if exit_path.is_file():
        try:
            record["exit_code"] = int(exit_path.read_text(encoding="utf-8").strip())
        except (OSError, ValueError):
            record["exit_code"] = None

    # Read session calls/results and event stream if present.
    try:
        parsed_log = mare_log.Log(agent_logs)
        record["calls"] = [
            {
                "msg": call["msg"],
                "id": call["id"],
                "name": call["name"],
                "raw": call["raw"],
                "result": parsed_log.result(call),
            }
            for call in parsed_log.calls
        ]
        record["unpaired_calls"] = parsed_log.unpaired()
        record["events"] = _compact_events(parsed_log.events)
        settled = [e for e in parsed_log.events if e.get("type") == "settled"]
        if settled:
            record["final_text"] = str((settled[-1].get("data") or {}).get("text") or "")
    except ValueError as exc:
        # Preserve partial evidence from failed/timed-out runs while keeping the error visible.
        try:
            raw_events = mare_log.events(agent_logs, allow_incomplete=True)
            record["events"] = _compact_events(raw_events)
            settled = [e for e in raw_events if e.get("type") == "settled"]
            if settled:
                record["final_text"] = str((settled[-1].get("data") or {}).get("text") or "")
        except ValueError:
            pass
        if record["exit_code"] == 0:
            record["error"] = f"invalid agent evidence: {exc}"

    verifier = result.get("verifier_result")
    reward = verifier.get("rewards", {}).get("reward") if isinstance(verifier, dict) else None
    details_path = trial_folder / "verifier" / "details.json"
    details: dict[str, Any] = {}
    if details_path.is_file():
        try:
            loaded = json.loads(details_path.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                details = loaded
        except (OSError, ValueError):
            record["error"] = record["error"] or "invalid verifier details.json"
    checks = details.get("checks") if isinstance(details.get("checks"), dict) else {}
    record["verifier"] = {
        "reward": reward if type(reward) in (int, float) and math.isfinite(reward) else None,
        "checks": dict(checks),
        "details": {k: v for k, v in details.items() if k != "checks"},
    }

    if record["error"] is not None:
        return record
    if not isinstance(agent, dict):
        record["error"] = "missing agent_result"
        return record
    if record["verifier"]["reward"] is None:
        record["error"] = "missing verifier reward"
        return record
    if record["exit_code"] != 0:
        record["error"] = f"agent exit {record['exit_code']}"
        return record
    if result.get("exception_info"):
        record["error"] = "trial crashed or timed out"
        return record
    record["status"] = "PASS" if record["verifier"]["reward"] == 1.0 else "FAIL"
    return record


def _match_tool_filter(call: dict[str, Any], check: dict[str, Any]) -> bool:
    tool = check.get("tool")
    if isinstance(tool, str) and call.get("name") != tool:
        return False
    tools = check.get("tools")
    if isinstance(tools, list) and call.get("name") not in tools:
        return False
    is_error = check.get("is_error")
    if isinstance(is_error, bool):
        res = call.get("result") or {}
        if bool(res.get("is_error")) != is_error:
            return False
    return True


def _compare_numeric(actual: float, check: dict[str, Any]) -> bool:
    if "eq" in check and actual != check["eq"]:
        return False
    if "min" in check and actual < check["min"]:
        return False
    if "max" in check and actual > check["max"]:
        return False
    if "gt" in check and not (actual > check["gt"]):
        return False
    return not ("lt" in check and not actual < check["lt"])


def eval_deterministic_check(
    check: dict[str, Any],
    record: dict[str, Any],
    budget: dict[str, Any],
) -> tuple[bool, str]:
    """Evaluate a deterministic check against a saved run record.

    Returns ``(shown, evidence)`` where ``shown`` is True when the pattern or
    condition occurred in the run.
    """
    kind = check["type"]
    if kind == "all":
        reasons = []
        for sub in check["checks"]:
            ok, why = eval_deterministic_check(sub, record, budget)
            reasons.append(why)
            if not ok:
                return False, why
        return True, "; ".join(reasons)
    if kind == "any":
        reasons = []
        for sub in check["checks"]:
            ok, why = eval_deterministic_check(sub, record, budget)
            if ok:
                return True, why
            reasons.append(why)
        return False, "; ".join(reasons)
    if kind == "not":
        ok, why = eval_deterministic_check(check["check"], record, budget)
        return (not ok), f"not({why})"

    if kind == "verifier_pass":
        reward = record.get("verifier", {}).get("reward")
        return reward == 1.0, f"reward={reward}"

    if kind == "verifier_check":
        name = check["name"]
        checks = record.get("verifier", {}).get("checks", {})
        val = bool(checks.get(name, False))
        return val, f"verifier.checks[{name!r}]={val}"

    if kind == "verifier_detail":
        key = check["key"]
        details = record.get("verifier", {}).get("details", {})
        present = key in details
        val = details.get(key)
        if "eq" in check:
            return present and val == check["eq"], f"details[{key!r}]={val!r}"
        if "contains" in check:
            needle = check["contains"]
            matched = isinstance(val, (str, list)) and needle in val
            return matched, f"details[{key!r}]={val!r}"
        if any(k in check for k in ("min", "max", "gt", "lt")):
            if type(val) in (int, float) and math.isfinite(val):
                return _compare_numeric(val, check), f"details[{key!r}]={val!r}"
            return False, f"details[{key!r}]={val!r} (non-numeric)"
        return bool(val), f"details[{key!r}]={val!r}"

    if kind == "final_text_regex":
        flags = re.IGNORECASE if check.get("ignore_case") else 0
        text = record.get("final_text") or ""
        m = re.search(check["pattern"], text, flags)
        return bool(m), f"final_text match={m.group(0)!r}" if m else "no match in final_text"

    if kind == "final_text_contains":
        text = record.get("final_text") or ""
        needle = check["needle"]
        if check.get("ignore_case"):
            matched = needle.lower() in text.lower()
        else:
            matched = needle in text
        return matched, f"{needle!r} in final_text={matched}"

    if kind == "tool_call_regex":
        flags = re.IGNORECASE if check.get("ignore_case") else 0
        pat = re.compile(check["pattern"], flags)
        for call in record.get("calls", []):
            if not _match_tool_filter(call, check):
                continue
            raw = str(call.get("raw") or "")
            m = pat.search(raw)
            if m:
                return True, f"{call.get('name')}({call.get('id')}) matched {m.group(0)!r}"
        return False, "no matching tool_call"

    if kind == "tool_result_regex":
        flags = re.IGNORECASE if check.get("ignore_case") else 0
        pat = re.compile(check["pattern"], flags)
        for call in record.get("calls", []):
            if not _match_tool_filter(call, check):
                continue
            text = str((call.get("result") or {}).get("text") or "")
            m = pat.search(text)
            if m:
                return True, f"{call.get('name')}({call.get('id')}) result matched {m.group(0)!r}"
        return False, "no matching tool_result"

    if kind == "tool_count":
        count = sum(1 for call in record.get("calls", []) if _match_tool_filter(call, check))
        return _compare_numeric(count, check), f"tool_count={count}"

    if kind == "tool_batch":
        tool = check["tool"]
        per_msg: dict[int, int] = {}
        for call in record.get("calls", []):
            if call.get("name") == tool:
                msg = int(call.get("msg", 0))
                per_msg[msg] = per_msg.get(msg, 0) + 1
        max_batch = max(per_msg.values(), default=0)
        return _compare_numeric(max_batch, check), f"max_batch({tool})={max_batch}"

    if kind == "event_match":
        event_type = check["event_type"]
        pat = re.compile(check["pattern"]) if check.get("pattern") else None
        for ev in record.get("events", []):
            if ev.get("type") != event_type:
                continue
            if pat is None:
                return True, f"event {event_type} present"
            encoded = json.dumps(ev.get("data", {}), sort_keys=True)
            m = pat.search(encoded)
            if m:
                return True, f"event {event_type} matched {m.group(0)!r}"
        return False, f"no matching event {event_type}"

    if kind == "exit_code":
        code = record.get("exit_code")
        if "equals" in check:
            return code == check["equals"], f"exit_code={code}"
        if "not_equals" in check:
            return code is not None and code != check["not_equals"], f"exit_code={code}"
        return code == 0, f"exit_code={code}"

    if kind == "unpaired_tool_calls":
        unpaired = record.get("unpaired_calls") or []
        return bool(unpaired), f"unpaired_calls={len(unpaired)}"

    if kind == "budget_within":
        max_secs = check.get("seconds", budget.get("seconds"))
        max_calls = check.get("max_tool_calls", budget.get("max_tool_calls"))
        secs = record.get("secs")
        calls = len(record.get("calls", []))
        ok = True
        notes = []
        if max_secs is not None:
            if secs is None or secs > max_secs:
                ok = False
            notes.append(f"secs={secs}/{max_secs}")
        if max_calls is not None:
            if calls > max_calls:
                ok = False
            notes.append(f"calls={calls}/{max_calls}")
        return ok, ", ".join(notes) or "no budget limits"

    raise ValueError(f"unsupported check type: {kind!r}")


def parse_judge_verdicts(raw: str, expected_ids: list[str]) -> tuple[dict[str, dict[str, str]] | None, str | None]:
    """Parse judge verdicts from a JSON array or JSONL (matching Dim's verdict parser)."""
    text = raw.strip()
    if text.startswith("```"):
        text = re.sub(r"^```[a-zA-Z0-9_-]*\n?", "", text)
        text = re.sub(r"\n?```$", "", text).strip()
    items: list[Any] | None = None
    try:
        parsed = json.loads(text)
        if isinstance(parsed, list):
            items = parsed
    except ValueError:
        # Try JSONL fallback (one JSON object per line), matching Dim's parser.
        candidate: list[Any] = []
        for line in text.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except ValueError:
                candidate = []
                break
            if isinstance(obj, dict):
                candidate.append(obj)
        if candidate:
            items = candidate
    if not items:
        return None, "reply did not contain a JSON array or JSONL of verdict objects"
    verdicts: dict[str, dict[str, str]] = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        pid = item.get("id")
        verdict = item.get("verdict")
        if isinstance(pid, str) and isinstance(verdict, str) and verdict.lower() in VALID_VERDICTS:
            verdicts[pid] = {
                "verdict": verdict.lower(),
                "quote": str(item.get("quote") or ""),
            }
    missing = [pid for pid in expected_ids if pid not in verdicts]
    if missing:
        return None, f"missing verdicts for: {', '.join(missing)}"
    return verdicts, None


def run_llm_judge(
    view_lines: list[dict[str, Any]],
    probes: list[dict[str, Any]],
    judge_fn: Callable[[list[dict[str, str]]], str],
) -> tuple[dict[str, dict[str, str]] | None, str | None]:
    """Invoke a semantic judge with up to JUDGE_REASKS retries on malformed replies."""
    expected_ids = [p["id"] for p in probes]
    statements = "\n".join(f"- {p['id']}: {p['text']}" for p in probes)
    view_jsonl = "\n".join(json.dumps(line, ensure_ascii=False) for line in view_lines)
    messages = [
        {"role": "system", "content": JUDGE_SYSTEM},
        {
            "role": "user",
            "content": f"Statements:\n{statements}\n\nRun view (JSONL):\n{view_jsonl}",
        },
    ]
    last_err = None
    for attempt in range(JUDGE_REASKS + 1):
        try:
            reply = judge_fn(messages)
        except Exception as exc:  # noqa: BLE001 - judge transport/API error marks run unjudged
            return None, f"judge call failed: {exc}"
        verdicts, err = parse_judge_verdicts(reply, expected_ids)
        if verdicts is not None:
            return verdicts, None
        last_err = err
        if attempt < JUDGE_REASKS:
            messages.append({"role": "assistant", "content": reply})
            messages.append(
                {
                    "role": "user",
                    "content": (
                        f"Your answer could not be used: {err}. "
                        "Answer again with the JSON array only, one object per statement."
                    ),
                }
            )
    return None, f"unusable judge reply after {JUDGE_REASKS} re-asks: {last_err}"


def make_openai_judge(model: str, *, base_url: str | None = None, api_key: str | None = None) -> Callable[[list[dict[str, str]]], str]:
    """Build a standard-library OpenAI-compatible HTTP judge callable."""
    endpoint = (base_url or os.environ.get("MARE_JUDGE_BASE_URL") or "https://opencode.ai/zen/v1").rstrip("/")
    key = api_key or os.environ.get("MARE_JUDGE_API_KEY") or os.environ.get("OPENCODE_API_KEY") or ""
    if not key:
        raise ValueError("LLM judge requires MARE_JUDGE_API_KEY or OPENCODE_API_KEY")
    # Strip provider prefix if given in `provider/model` format and endpoint is direct.
    model_id = model.split("/", 1)[1] if "/" in model and os.environ.get("MARE_JUDGE_STRIP_PROVIDER") == "1" else model

    def _call(messages: list[dict[str, str]]) -> str:
        payload = json.dumps({"model": model_id, "messages": messages, "temperature": 0}).encode("utf-8")
        req = urllib.request.Request(
            f"{endpoint}/chat/completions",
            data=payload,
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                body = json.loads(resp.read().decode("utf-8"))
        except urllib.error.URLError as exc:
            raise RuntimeError(f"HTTP error calling judge: {exc}") from exc
        choice = (body.get("choices") or [{}])[0]
        if choice.get("finish_reason") == "length":
            raise RuntimeError("judge reply hit max output tokens")
        return str((choice.get("message") or {}).get("content") or "")

    return _call


def score_record(
    record: dict[str, Any],
    scenario: dict[str, Any],
    *,
    view_lines: list[dict[str, Any]] | None = None,
    judge_fn: Callable[[list[dict[str, str]]], str] | None = None,
    preserve_judge: bool = True,
) -> dict[str, Any]:
    """Score all scenario probes against a saved record (offline-safe for deterministic checks)."""
    budget = scenario.get("budget") or {}
    probes = scenario["probes"]
    saved_results = record.get("probes") if isinstance(record.get("probes"), dict) else {}

    # Check budget exceeded (wall-clock budget stop counts as a failed run, matching Dim).
    run_error = record.get("error")
    max_secs = budget.get("seconds")
    if run_error is None and max_secs is not None and record.get("secs") is not None and record["secs"] > max_secs:
        run_error = f"exceeded wall-clock budget ({record['secs']:.1f}s > {max_secs}s)"
    failed_run = record.get("status") == "ERROR" or run_error is not None

    probe_results: dict[str, dict[str, Any]] = {}
    llm_probes: list[dict[str, Any]] = []
    for probe in probes:
        pid = probe["id"]
        kind = probe["kind"]
        check = probe["check"]
        if check["type"] == "llm_judge":
            llm_probes.append(probe)
            continue
        shown, evidence = eval_deterministic_check(check, record, budget)
        passed = shown if kind == "must_show" else (not shown)
        probe_results[pid] = {
            "kind": kind,
            "source": "deterministic",
            "shown": shown,
            "passed": passed,
            "evidence": evidence,
        }

    unjudged_reason: str | None = None
    if llm_probes and not failed_run:
        if judge_fn is not None:
            view = view_lines if view_lines is not None else build_judge_view(record)
            verdicts, err = run_llm_judge(view, llm_probes, judge_fn)
            if verdicts is None:
                unjudged_reason = err or "judge failed"
                for probe in llm_probes:
                    probe_results[probe["id"]] = {
                        "kind": probe["kind"],
                        "source": "llm_judge",
                        "shown": None,
                        "passed": None,
                        "evidence": unjudged_reason,
                    }
            else:
                for probe in llm_probes:
                    v = verdicts[probe["id"]]
                    shown = v["verdict"] == "present"
                    passed = shown if probe["kind"] == "must_show" else (not shown)
                    probe_results[probe["id"]] = {
                        "kind": probe["kind"],
                        "source": "llm_judge",
                        "verdict": v["verdict"],
                        "shown": shown,
                        "passed": passed,
                        "evidence": v["quote"],
                    }
        elif preserve_judge and all(
            isinstance(saved_results.get(p["id"]), dict) and saved_results[p["id"]].get("shown") is not None
            for p in llm_probes
        ):
            for probe in llm_probes:
                prev = saved_results[probe["id"]]
                shown = bool(prev["shown"])
                passed = shown if probe["kind"] == "must_show" else (not shown)
                probe_results[probe["id"]] = {
                    "kind": probe["kind"],
                    "source": "llm_judge",
                    "verdict": prev.get("verdict"),
                    "shown": shown,
                    "passed": passed,
                    "evidence": prev.get("evidence", ""),
                }
        else:
            unjudged_reason = "llm_judge probe present without --judge or saved judgement"
            for probe in llm_probes:
                probe_results[probe["id"]] = {
                    "kind": probe["kind"],
                    "source": "llm_judge",
                    "shown": None,
                    "passed": None,
                    "evidence": unjudged_reason,
                }

    scored = dict(record)
    scored["failed_run"] = failed_run
    scored["unjudged"] = (not failed_run) and (unjudged_reason is not None)
    scored["unjudged_reason"] = unjudged_reason
    if run_error and not scored.get("error"):
        scored["error"] = run_error
    scored["probes"] = probe_results
    return scored
