#!/usr/bin/env python3
"""`just eval-ab <scenario>` — interleaved same-scenario A/B runs, negative probes, and offline re-scoring.

Builds on Mare's Harbor runner (`evals/run.py`) and follows Dim's `xtask scenario-eval`
methodology:
  - Interleaved execution: baseline and candidate run alternately on the same scenario
    and setup (`baseline[0], candidate[0], baseline[1], candidate[1], ...`), so provider
    drift lands on both arms alike.
  - Arm fingerprints: binary manifest identity, model, and any Lua config/prompt overlay
    SHA-256 + length are recorded in every run record; identical arms are refused.
  - Negative probes: every scenario pairs `must_show` checks with `must_not_show` probes.
    Anything assertable from logs stays deterministic; an LLM judge is invoked only for
    probes explicitly marked `llm_judge`.
  - Saved records & offline re-scoring: every trial saves its structured record
    (`runs.jsonl`) and ordered judge view (`views/*.jsonl`). `--rescore <dir>` re-evaluates
    saved records offline without Docker, Harbor, or re-running the agent.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import tomllib
from collections import defaultdict
from collections.abc import Callable
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from evals import ab_probes, ab_rule
from evals import run as eval_run

AB_OUT_ROOT = ROOT / "target" / "eval-ab"
BASELINE_BIN_ROOT = ROOT / "target" / "eval-bin-baseline"


def _clean_git_env() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if k not in {"GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"}}


def default_baseline_ref(repo: Path = ROOT) -> str:
    """Resolve the default baseline ref for `just eval-ab <scenario>`: env override, latest
    reachable `v*` release tag, or `HEAD~1` when no release tag is reachable."""
    override = os.environ.get("MARE_EVAL_BASELINE_REF", "").strip()
    if override:
        return override
    env = _clean_git_env()
    res = subprocess.run(
        ["git", "-C", str(repo), "describe", "--tags", "--abbrev=0", "--match", "v*"],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    tag = res.stdout.strip()
    if res.returncode == 0 and tag:
        return tag
    return "HEAD~1"


def resolve_ref_identity(ref: str, *, repo: Path = ROOT) -> tuple[str, str]:
    """Return `(commit_sha, workspace_version)` for a Git revision."""
    env = _clean_git_env()
    try:
        revision = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--verify", f"{ref}^{{commit}}"],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        cargo_toml = subprocess.run(
            ["git", "-C", str(repo), "show", f"{revision}:Cargo.toml"],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        raise ValueError(f"cannot resolve baseline ref {ref!r} in {repo}") from exc
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError(f"baseline ref {ref!r} did not resolve to a full commit SHA")
    version = tomllib.loads(cargo_toml)["workspace"]["package"]["version"]
    return revision, version


def ensure_baseline_binaries(
    ref: str,
    *,
    no_build: bool = False,
    repo: Path = ROOT,
    cache_root: Path = BASELINE_BIN_ROOT,
) -> tuple[Path, dict[str, Any]]:
    """Return a validated static binary directory + manifest for `ref`, building once into a
    revision-keyed cache via a temporary detached worktree when needed."""
    revision, version = resolve_ref_identity(ref, repo=repo)
    bin_dir = cache_root / revision[:12]
    try:
        manifest = eval_run.validate_binaries(bin_dir, revision, version)
        return bin_dir, manifest
    except ValueError:
        if no_build:
            raise ValueError(
                f"baseline binaries for {ref} ({revision[:12]}) are missing or invalid in {bin_dir}; "
                "omit --no-build or pass --baseline-bin"
            )

    env = _clean_git_env()
    bin_dir.mkdir(parents=True, exist_ok=True)
    (bin_dir / "manifest.json").unlink(missing_ok=True)
    with tempfile.TemporaryDirectory(prefix=f"mare-eval-baseline-{revision[:12]}-") as tmp:
        wt = Path(tmp) / "src"
        subprocess.run(
            ["git", "-C", str(repo), "worktree", "add", "--detach", str(wt), revision],
            env=env,
            check=True,
        )
        try:
            eval_run.build(src_dir=wt, out_dir=bin_dir)
            eval_run.write_binary_manifest(bin_dir, revision, version)
        finally:
            subprocess.run(
                ["git", "-C", str(repo), "worktree", "remove", "--force", str(wt)],
                env=env,
                check=False,
            )
    manifest = eval_run.validate_binaries(bin_dir, revision, version)
    return bin_dir, manifest


def _file_fingerprint(path: Path) -> dict[str, Any]:
    raw = path.read_bytes()
    return {
        "path": str(path.name),
        "sha256": hashlib.sha256(raw).hexdigest(),
        "bytes": len(raw),
        "approx_tokens": max(1, len(raw) // 4) if raw else 0,
    }


def _arm_identity(
    label: str,
    *,
    model: str,
    bin_dir: Path,
    manifest: dict[str, Any],
    extra_lua: Path | None,
) -> dict[str, Any]:
    ident: dict[str, Any] = {
        "label": label,
        "model": model,
        "version": manifest.get("version"),
        "commit": manifest.get("commit"),
        "binaries": dict(sorted((manifest.get("binaries") or {}).items())),
        "bin_dir": str(bin_dir),
    }
    if extra_lua is not None:
        ident["extra_lua"] = _file_fingerprint(extra_lua)
    else:
        ident["extra_lua"] = None
    return ident


def _arms_identical(baseline: dict[str, Any], candidate: dict[str, Any]) -> bool:
    keys = ("model", "version", "commit", "binaries")
    if any(baseline.get(k) != candidate.get(k) for k in keys):
        return False
    b_lua = baseline.get("extra_lua")
    c_lua = candidate.get("extra_lua")
    if (b_lua is None) != (c_lua is None):
        return False
    if b_lua is not None and c_lua is not None:
        return b_lua.get("sha256") == c_lua.get("sha256")
    return True


def _load_arm_manifest(bin_dir: Path) -> dict[str, Any]:
    manifest_path = bin_dir / "manifest.json"
    if not manifest_path.is_file():
        raise ValueError(f"binary manifest is missing in {bin_dir}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise TypeError(f"invalid binary manifest in {bin_dir}")
    revision = manifest.get("commit")
    version = manifest.get("version")
    if not isinstance(revision, str) or not isinstance(version, str):
        raise TypeError(f"binary manifest missing commit or version in {bin_dir}")
    return eval_run.validate_binaries(bin_dir, revision, version)


def _expand_scenarios(
    names: list[str],
    *,
    evals_dir: Path | None = None,
    probes_override: Path | None = None,
) -> list[dict[str, Any]]:
    expanded: list[str] = []
    for name in names:
        if name in eval_run.LOCAL_TASKS:
            expanded.extend(eval_run.LOCAL_TASKS[name])
        else:
            expanded.append(name)
    if probes_override is not None and len(expanded) != 1:
        raise ValueError("--probes override can only be used with a single scenario")
    seen: set[str] = set()
    scenarios: list[dict[str, Any]] = []
    for name in expanded:
        sc = ab_probes.load_scenario(name, evals_dir=evals_dir, probes_override=probes_override)
        if sc["name"] in seen:
            raise ValueError(f"duplicate scenario: {sc['name']}")
        seen.add(sc["name"])
        scenarios.append(sc)
    return scenarios


def _find_trial_folder(job_dir: Path, task_name: str) -> Path | None:
    if not job_dir.is_dir():
        return None
    matches = sorted(
        p for p in job_dir.iterdir()
        if p.is_dir() and (p.name == task_name or p.name.startswith(task_name + "__"))
    )
    return matches[0] if len(matches) == 1 else None


def interleaved_schedule(scenarios: list[dict[str, Any]], runs: int) -> list[tuple[int, dict[str, Any], str]]:
    """Build Dim-style interleaved schedule: baseline immediately before candidate per (run, scenario)."""
    schedule: list[tuple[int, dict[str, Any], str]] = []
    for run_index in range(runs):
        for scenario in scenarios:
            schedule.append((run_index, scenario, "baseline"))
            schedule.append((run_index, scenario, "candidate"))
    return schedule


def build_comparisons(
    records: list[dict[str, Any]],
    scenarios: list[dict[str, Any]],
    *,
    floor: int = ab_rule.FLOOR,
) -> tuple[ab_rule.RuleResult, int]:
    """Compute the fixed comparison family and three-valued verdict across baseline vs candidate."""
    by_arm_scenario: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    by_arm: dict[str, list[dict[str, Any]]] = {"baseline": [], "candidate": []}
    for rec in records:
        arm = rec["arm"]
        if arm in by_arm:
            by_arm[arm].append(rec)
            by_arm_scenario[(arm, rec["scenario"])].append(rec)

    comparisons: list[ab_rule.Comparison] = []
    pooled_pos = {"baseline": [0, 0], "candidate": [0, 0]}
    pooled_neg = {"baseline": [0, 0], "candidate": [0, 0]}
    min_judged: int | None = None

    for sc in scenarios:
        sname = sc["name"]
        for arm in ("baseline", "candidate"):
            judged_count = sum(
                1 for r in by_arm_scenario[(arm, sname)] if not r.get("failed_run") and not r.get("unjudged")
            )
            min_judged = judged_count if min_judged is None else min(min_judged, judged_count)

        for probe in sc["probes"]:
            pid = probe["id"]
            kind = probe["kind"]
            arm_counts: dict[str, ab_rule.Counts] = {}
            for arm in ("baseline", "candidate"):
                counted = [
                    r for r in by_arm_scenario[(arm, sname)] if not r.get("failed_run") and not r.get("unjudged")
                ]
                if kind == "must_show":
                    outcome = sum(1 for r in counted if (r.get("probes", {}).get(pid) or {}).get("passed") is True)
                    pooled_pos[arm][0] += outcome
                    pooled_pos[arm][1] += len(counted)
                else:
                    # Violation rate: how many counted runs showed the forbidden behavior.
                    outcome = sum(1 for r in counted if (r.get("probes", {}).get(pid) or {}).get("shown") is True)
                    pooled_neg[arm][0] += outcome
                    pooled_neg[arm][1] += len(counted)
                arm_counts[arm] = ab_rule.Counts(outcome=outcome, total=len(counted))
            comparisons.append(
                ab_rule.Comparison(
                    name=f"{sname}/{pid} ({kind})",
                    worse=ab_rule.Worse.LOWER if kind == "must_show" else ab_rule.Worse.HIGHER,
                    baseline=arm_counts["baseline"],
                    candidate=arm_counts["candidate"],
                )
            )

    comparisons.append(
        ab_rule.Comparison(
            name="pooled must_show pass rate",
            worse=ab_rule.Worse.LOWER,
            baseline=ab_rule.Counts(*pooled_pos["baseline"]),
            candidate=ab_rule.Counts(*pooled_pos["candidate"]),
        )
    )
    comparisons.append(
        ab_rule.Comparison(
            name="pooled must_not_show violation rate",
            worse=ab_rule.Worse.HIGHER,
            baseline=ab_rule.Counts(*pooled_neg["baseline"]),
            candidate=ab_rule.Counts(*pooled_neg["candidate"]),
        )
    )
    comparisons.append(
        ab_rule.Comparison(
            name="failed-run rate",
            worse=ab_rule.Worse.HIGHER,
            baseline=ab_rule.Counts(
                outcome=sum(1 for r in by_arm["baseline"] if r.get("failed_run")),
                total=len(by_arm["baseline"]),
            ),
            candidate=ab_rule.Counts(
                outcome=sum(1 for r in by_arm["candidate"] if r.get("failed_run")),
                total=len(by_arm["candidate"]),
            ),
        )
    )
    comparisons.append(
        ab_rule.Comparison(
            name="unjudged-run rate",
            worse=ab_rule.Worse.HIGHER,
            baseline=ab_rule.Counts(
                outcome=sum(1 for r in by_arm["baseline"] if r.get("unjudged")),
                total=len(by_arm["baseline"]),
            ),
            candidate=ab_rule.Counts(
                outcome=sum(1 for r in by_arm["candidate"] if r.get("unjudged")),
                total=len(by_arm["candidate"]),
            ),
        )
    )

    min_judged_val = min_judged if min_judged is not None else 0
    return ab_rule.judge(comparisons, min_judged_val, floor=floor), min_judged_val


def _arm_summary(records: list[dict[str, Any]], arm: str) -> dict[str, Any]:
    mine = [r for r in records if r["arm"] == arm]
    secs = [r["secs"] for r in mine if r.get("secs") is not None]
    costs = [r["cost_usd"] for r in mine if r.get("cost_usd") is not None]
    tok_in = [r["tokens"]["input"] for r in mine if (r.get("tokens") or {}).get("input") is not None]
    tok_out = [r["tokens"]["output"] for r in mine if (r.get("tokens") or {}).get("output") is not None]
    return {
        "total": len(mine),
        "pass": sum(1 for r in mine if r.get("status") == "PASS" and not r.get("failed_run")),
        "fail": sum(1 for r in mine if r.get("status") == "FAIL" and not r.get("failed_run")),
        "error": sum(1 for r in mine if r.get("failed_run")),
        "unjudged": sum(1 for r in mine if r.get("unjudged")),
        "median_secs": statistics.median(secs) if secs else None,
        "tok_in": sum(tok_in) if tok_in else None,
        "tok_out": sum(tok_out) if tok_out else None,
        "known_cost": sum(costs),
        "unknown_cost": len(mine) - len(costs),
    }


def _describe_identity(ident: dict[str, Any]) -> str:
    parts = [f"model={ident.get('model') or '-'}"]
    if ident.get("commit"):
        parts.append(f"commit={str(ident['commit'])[:12]}")
    if ident.get("version"):
        parts.append(f"version={ident['version']}")
    extra = ident.get("extra_lua")
    if isinstance(extra, dict):
        parts.append(
            f"extra_lua={extra.get('path')} (sha256={str(extra.get('sha256', ''))[:12]}, "
            f"{extra.get('bytes')}B/~{extra.get('approx_tokens')}tok)"
        )
    return ", ".join(parts)


def render_report(
    records: list[dict[str, Any]],
    scenarios: list[dict[str, Any]],
    rule_result: ab_rule.RuleResult,
    min_judged: int,
    *,
    mode: str = "ab",
    source_note: str | None = None,
) -> str:
    """Render a side-by-side Markdown A/B report."""
    lines: list[str] = [f"# Harbor A/B Eval Report ({mode})", ""]
    if source_note:
        lines.append(f"- Source: {source_note}")
    lines.append(f"- Scenarios: {', '.join(sc['name'] for sc in scenarios)}")
    for arm in ("baseline", "candidate"):
        sample = next((r.get("arm_identity") for r in records if r["arm"] == arm and r.get("arm_identity")), None)
        if sample:
            lines.append(f"- {arm}: {_describe_identity(sample)}")
    lines.append("")
    lines.append(f"**Verdict:** {ab_rule.verdict_line(rule_result, min_judged)}")
    lines.append("")
    lines.append("## Side-by-side probe comparisons")
    lines.append("")
    lines.append(ab_rule.format_table(rule_result.rows))
    lines.append("")
    lines.append("## Arm summary")
    lines.append("")
    lines.append("| arm | runs | pass | fail | error | unjudged | median time | tokens in/out | known cost |")
    lines.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for arm in ("baseline", "candidate"):
        s = _arm_summary(records, arm)
        med = f"{s['median_secs']:.1f}s" if s["median_secs"] is not None else "-"
        toks = f"{s['tok_in']}/{s['tok_out']}" if s["tok_in"] is not None and s["tok_out"] is not None else "-"
        cost = f"${s['known_cost']:.4f}" + (f" ({s['unknown_cost']} unk)" if s["unknown_cost"] else "")
        lines.append(
            f"| {arm} | {s['total']} | {s['pass']} | {s['fail']} | {s['error']} | {s['unjudged']} "
            f"| {med} | {toks} | {cost} |"
        )
    lines.append("")
    lines.append("## Per-run details")
    lines.append("")
    lines.append("| run | scenario | arm | status | time | failed probes / violations | error |")
    lines.append("| ---: | --- | --- | --- | ---: | --- | --- |")
    for rec in records:
        secs = f"{rec['secs']:.1f}s" if rec.get("secs") is not None else "-"
        bad = []
        for pid, pres in (rec.get("probes") or {}).items():
            if pres.get("passed") is False:
                tag = "violation" if pres.get("kind") == "must_not_show" else "missing"
                bad.append(f"{pid} ({tag})")
            elif pres.get("passed") is None:
                bad.append(f"{pid} (unjudged)")
        err = rec.get("error") or rec.get("unjudged_reason") or ""
        lines.append(
            f"| {rec['run_index']} | {rec['scenario']} | {rec['arm']} | {rec.get('status', 'ERROR')} "
            f"| {secs} | {', '.join(bad) or '-'} | {err} |"
        )
    lines.append("")
    return "\n".join(lines)


def _write_outputs(
    out_dir: Path,
    records: list[dict[str, Any]],
    views: dict[str, list[dict[str, Any]]],
    report: str,
    meta: dict[str, Any],
) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    views_dir = out_dir / "views"
    views_dir.mkdir(parents=True, exist_ok=True)
    for rel, view_lines in views.items():
        target = out_dir / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            "".join(json.dumps(line, ensure_ascii=False) + "\n" for line in view_lines),
            encoding="utf-8",
        )
    (out_dir / "runs.jsonl").write_text(
        "".join(json.dumps(rec, ensure_ascii=False) + "\n" for rec in records),
        encoding="utf-8",
    )
    (out_dir / "meta.json").write_text(json.dumps(meta, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    (out_dir / "report.md").write_text(report, encoding="utf-8")


def _ensure_not_inside(out_dir: Path, source_dir: Path) -> None:
    out_resolved = out_dir.resolve()
    src_resolved = source_dir.resolve()
    if out_resolved == src_resolved or out_resolved.is_relative_to(src_resolved):
        raise ValueError(f"--out ({out_dir}) must not be equal to or inside source directory ({source_dir})")


def _read_saved_runs(source_dir: Path) -> tuple[list[dict[str, Any]], dict[str, list[dict[str, Any]]]]:
    runs_path = source_dir / "runs.jsonl"
    if not runs_path.is_file():
        raise ValueError(f"saved runs.jsonl not found in {source_dir}")
    records: list[dict[str, Any]] = []
    views: dict[str, list[dict[str, Any]]] = {}
    for line_no, raw in enumerate(runs_path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw.strip():
            continue
        rec = json.loads(raw)
        if not isinstance(rec, dict) or "arm" not in rec or "scenario" not in rec:
            raise ValueError(f"invalid record at {runs_path}:{line_no}")
        records.append(rec)
        rel = rec.get("judge_view_file")
        if isinstance(rel, str):
            view_path = source_dir / rel
            if view_path.is_file():
                views[rel] = [
                    json.loads(v) for v in view_path.read_text(encoding="utf-8").splitlines() if v.strip()
                ]
    if not records:
        raise ValueError(f"no saved records in {runs_path}")
    return records, views


def rescore_saved(
    source_dir: Path,
    out_dir: Path,
    *,
    evals_dir: Path | None = None,
    probes_override: Path | None = None,
    judge_fn: Callable[[list[dict[str, str]]], str] | None = None,
    floor: int = ab_rule.FLOOR,
) -> tuple[str, ab_rule.RuleResult, list[dict[str, Any]]]:
    """Offline re-score a saved A/B directory without running Harbor or the agent."""
    _ensure_not_inside(out_dir, source_dir)
    saved_records, saved_views = _read_saved_runs(source_dir)
    scenario_names = list(dict.fromkeys(r["scenario"] for r in saved_records))
    scenarios = _expand_scenarios(scenario_names, evals_dir=evals_dir, probes_override=probes_override)
    by_name = {sc["name"]: sc for sc in scenarios}

    rescored: list[dict[str, Any]] = []
    out_views: dict[str, list[dict[str, Any]]] = {}
    for rec in saved_records:
        sc = by_name[rec["scenario"]]
        rel = rec.get("judge_view_file") or f"views/{rec['arm']}__{rec['scenario']}__run-{rec['run_index']:03d}.jsonl"
        view_lines = saved_views.get(rel) or ab_probes.build_judge_view(rec)
        scored = ab_probes.score_record(
            rec,
            sc,
            view_lines=view_lines,
            judge_fn=judge_fn,
            preserve_judge=(judge_fn is None),
        )
        scored["judge_view_file"] = rel
        rescored.append(scored)
        out_views[rel] = view_lines

    rule_result, min_judged = build_comparisons(rescored, scenarios, floor=floor)
    report = render_report(
        rescored,
        scenarios,
        rule_result,
        min_judged,
        mode="rescore",
        source_note=str(source_dir),
    )
    _write_outputs(
        out_dir,
        rescored,
        out_views,
        report,
        {
            "mode": "rescore",
            "source": str(source_dir),
            "verdict": rule_result.verdict,
            "min_judged": min_judged,
            "floor": floor,
        },
    )
    return report, rule_result, rescored


def pool_saved(
    source_dirs: list[Path],
    out_dir: Path,
    *,
    evals_dir: Path | None = None,
    floor: int = ab_rule.FLOOR,
) -> tuple[str, ab_rule.RuleResult, list[dict[str, Any]]]:
    """Pool multiple saved A/B run directories into one combined report offline."""
    if not source_dirs:
        raise ValueError("pool requires at least one source directory")
    for src in source_dirs:
        _ensure_not_inside(out_dir, src)
    all_records: list[dict[str, Any]] = []
    all_views: dict[str, list[dict[str, Any]]] = {}
    for idx, src in enumerate(source_dirs):
        recs, views = _read_saved_runs(src)
        for rec in recs:
            rel = rec.get("judge_view_file")
            pooled_rel = f"views/pool-{idx:02d}__{Path(rel).name}" if rel else None
            copy = dict(rec)
            if pooled_rel and rel in views:
                copy["judge_view_file"] = pooled_rel
                all_views[pooled_rel] = views[rel]
            all_records.append(copy)
    scenario_names = list(dict.fromkeys(r["scenario"] for r in all_records))
    scenarios = _expand_scenarios(scenario_names, evals_dir=evals_dir)
    rule_result, min_judged = build_comparisons(all_records, scenarios, floor=floor)
    report = render_report(
        all_records,
        scenarios,
        rule_result,
        min_judged,
        mode="pool",
        source_note=", ".join(str(s) for s in source_dirs),
    )
    _write_outputs(
        out_dir,
        all_records,
        all_views,
        report,
        {
            "mode": "pool",
            "sources": [str(s) for s in source_dirs],
            "verdict": rule_result.verdict,
            "min_judged": min_judged,
            "floor": floor,
        },
    )
    return report, rule_result, all_records


def run_ab(
    scenarios: list[dict[str, Any]],
    *,
    runs: int,
    baseline_model: str,
    candidate_model: str,
    baseline_bin: Path,
    candidate_bin: Path,
    baseline_manifest: dict[str, Any],
    candidate_manifest: dict[str, Any],
    baseline_lua: Path | None,
    candidate_lua: Path | None,
    out_dir: Path,
    floor: int = ab_rule.FLOOR,
    allow_identical: bool = False,
    judge_fn: Callable[[list[dict[str, str]]], str] | None = None,
) -> tuple[str, ab_rule.RuleResult, list[dict[str, Any]]]:
    """Execute interleaved baseline vs candidate Harbor trials and save re-scorable records."""
    if runs < 1:
        raise ValueError("--runs must be >= 1")
    arm_cfg = {
        "baseline": {
            "model": baseline_model,
            "bin_dir": baseline_bin,
            "manifest": baseline_manifest,
            "extra_lua": baseline_lua,
            "identity": _arm_identity(
                "baseline",
                model=baseline_model,
                bin_dir=baseline_bin,
                manifest=baseline_manifest,
                extra_lua=baseline_lua,
            ),
        },
        "candidate": {
            "model": candidate_model,
            "bin_dir": candidate_bin,
            "manifest": candidate_manifest,
            "extra_lua": candidate_lua,
            "identity": _arm_identity(
                "candidate",
                model=candidate_model,
                bin_dir=candidate_bin,
                manifest=candidate_manifest,
                extra_lua=candidate_lua,
            ),
        },
    }
    if not allow_identical and _arms_identical(arm_cfg["baseline"]["identity"], arm_cfg["candidate"]["identity"]):
        raise ValueError(
            "baseline and candidate have identical binary identity, model, and extra_lua; "
            "pass different builds/overlays/models or --allow-identical"
        )

    eval_run.sync_helpers()
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    records: list[dict[str, Any]] = []
    views: dict[str, list[dict[str, Any]]] = {}

    for run_index, sc, arm in interleaved_schedule(scenarios, runs):
        cfg = arm_cfg[arm]
        job_name = f"ab-{stamp}-r{run_index:03d}-{sc['name']}-{arm}"
        job_dir, returncode = eval_run.harbor(
            job_name,
            cfg["model"],
            ["-p", str(sc["task_dir"]), "-n", "1", "-k", "1"],
            identity=cfg["manifest"],
            bin_dir=cfg["bin_dir"],
            extra_lua=cfg["extra_lua"],
        )
        folder = _find_trial_folder(job_dir, sc["name"])
        err_override = None
        if folder is None:
            err_override = (
                f"harbor exited with status {returncode}; missing trial"
                if returncode != 0
                else "missing trial folder"
            )
        rec = ab_probes.build_run_record(
            folder,
            arm=arm,
            scenario=sc["name"],
            run_index=run_index,
            model=cfg["model"],
            arm_identity=cfg["identity"],
            error_override=err_override,
        )
        if returncode != 0 and rec["status"] == "PASS":
            rec["status"] = "ERROR"
            rec["error"] = f"harbor exited with status {returncode}"
        rel = f"views/{arm}__{sc['name']}__run-{run_index:03d}.jsonl"
        view_lines = ab_probes.build_judge_view(rec)
        scored = ab_probes.score_record(rec, sc, view_lines=view_lines, judge_fn=judge_fn, preserve_judge=False)
        scored["judge_view_file"] = rel
        records.append(scored)
        views[rel] = view_lines

    rule_result, min_judged = build_comparisons(records, scenarios, floor=floor)
    report = render_report(records, scenarios, rule_result, min_judged, mode="ab")
    _write_outputs(
        out_dir,
        records,
        views,
        report,
        {
            "mode": "ab",
            "runs": runs,
            "scenarios": [sc["name"] for sc in scenarios],
            "baseline": arm_cfg["baseline"]["identity"],
            "candidate": arm_cfg["candidate"]["identity"],
            "verdict": rule_result.verdict,
            "min_judged": min_judged,
            "floor": floor,
        },
    )
    return report, rule_result, records


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(description="Same-scenario A/B runs, negative probes, and offline re-scoring on Harbor.")
    ap.add_argument("scenarios", nargs="*", help="Scenario task name(s), group name (`own`/`dev`), or scenario dir(s)")
    ap.add_argument("--rescore", type=Path, help="Re-score a saved A/B output directory offline without running the agent")
    ap.add_argument("--pool", action="append", type=Path, default=[], help="Pool saved A/B output directories offline")
    ap.add_argument("--runs", type=int, default=1, help="Interleaved runs per arm per scenario (default: 1)")
    ap.add_argument("--model", default=os.environ.get("MARE_LIVE_MODEL", "opencode-go/glm-5.3-flash"))
    ap.add_argument("--baseline-model", default=None)
    ap.add_argument("--candidate-model", default=None)
    ap.add_argument("--baseline-bin", type=Path, default=None, help="Directory holding baseline static binaries + manifest.json")
    ap.add_argument(
        "--baseline-ref",
        default=None,
        help="Git ref for the baseline arm (default when arms would otherwise match: MARE_EVAL_BASELINE_REF, latest reachable v* tag, or HEAD~1)",
    )
    ap.add_argument("--candidate-bin", type=Path, default=None, help="Directory holding candidate static binaries + manifest.json")
    ap.add_argument("--baseline-lua", type=Path, default=None, help="Optional Lua config/prompt overlay for baseline arm")
    ap.add_argument("--candidate-lua", type=Path, default=None, help="Optional Lua config/prompt overlay for candidate arm")
    ap.add_argument("--probes", type=Path, default=None, help="Override probes.json path (single-scenario runs or rescore)")
    ap.add_argument("--judge", default=None, help="Optional LLM judge model (only used for llm_judge probes)")
    ap.add_argument("--out", type=Path, default=None, help="Output directory (default: target/eval-ab/<timestamp>)")
    ap.add_argument("--floor", type=int, default=ab_rule.FLOOR, help=f"Minimum judged runs per arm for pass (default: {ab_rule.FLOOR})")
    ap.add_argument("--no-build", action="store_true", help="Reuse existing target/eval-bin without rebuilding")
    ap.add_argument("--allow-identical", action="store_true", help="Allow identical baseline and candidate arms")
    ap.add_argument("--strict", action="store_true", help="Exit non-zero unless verdict is pass")
    return ap.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    a = parse_args(argv)
    if a.rescore and a.pool:
        print("error: --rescore and --pool are mutually exclusive", file=sys.stderr)
        return 2
    if a.floor < 1:
        print("error: --floor must be positive", file=sys.stderr)
        return 2

    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    out_dir = a.out or (AB_OUT_ROOT / f"ab-{stamp}")
    judge_fn = ab_probes.make_openai_judge(a.judge) if a.judge else None

    try:
        if a.rescore is not None:
            report, rule_result, _ = rescore_saved(
                a.rescore, out_dir, probes_override=a.probes, judge_fn=judge_fn, floor=a.floor
            )
        elif a.pool:
            report, rule_result, _ = pool_saved(a.pool, out_dir, floor=a.floor)
        else:
            if not a.scenarios:
                print("error: specify at least one scenario (or --rescore / --pool)", file=sys.stderr)
                return 2
            if a.baseline_bin is not None and a.baseline_ref is not None:
                print("error: --baseline-bin and --baseline-ref are mutually exclusive", file=sys.stderr)
                return 2
            scenarios = _expand_scenarios(a.scenarios, probes_override=a.probes)
            if not os.environ.get("OPENCODE_API_KEY"):
                raise ValueError("OPENCODE_API_KEY is not set")
            if not shutil.which("harbor"):
                raise ValueError("harbor not found")
            for label, path in (("baseline", a.baseline_lua), ("candidate", a.candidate_lua)):
                if path is not None and not path.is_file():
                    raise ValueError(f"{label} extra_lua file not found: {path}")

            b_model = a.baseline_model or a.model
            c_model = a.candidate_model or a.model
            env_baseline_bin = os.environ.get("MARE_EVAL_BASELINE_BIN", "").strip()
            baseline_bin_arg = a.baseline_bin or (Path(env_baseline_bin) if env_baseline_bin and a.baseline_ref is None else None)
            arms_differ_without_bin = (
                b_model != c_model
                or a.baseline_lua != a.candidate_lua
                or a.candidate_bin is not None
            )
            use_default_baseline_ref = (
                a.baseline_ref is not None
                or (baseline_bin_arg is None and not arms_differ_without_bin and not a.allow_identical)
            )

            need_current_bin = a.candidate_bin is None or (baseline_bin_arg is None and not use_default_baseline_ref)
            current_manifest = None
            if need_current_bin:
                revision, version = eval_run.source_identity()
                manifest_path = eval_run.BIN_DIR / "manifest.json"
                if not a.no_build:
                    manifest_path.unlink(missing_ok=True)
                    eval_run.build()
                    if eval_run.source_identity() != (revision, version):
                        raise ValueError("binary source changed during build")
                    eval_run.write_binary_manifest(eval_run.BIN_DIR, revision, version)
                current_manifest = eval_run.validate_binaries(eval_run.BIN_DIR, revision, version)

            if use_default_baseline_ref:
                ref = a.baseline_ref or default_baseline_ref()
                b_bin, b_manifest = ensure_baseline_binaries(ref, no_build=a.no_build)
            elif baseline_bin_arg is not None:
                b_bin = baseline_bin_arg
                b_manifest = _load_arm_manifest(b_bin)
            else:
                b_bin = eval_run.BIN_DIR
                b_manifest = current_manifest

            if a.candidate_bin is not None:
                c_bin = a.candidate_bin
                c_manifest = _load_arm_manifest(c_bin)
            else:
                c_bin = eval_run.BIN_DIR
                c_manifest = current_manifest
            assert b_manifest is not None and c_manifest is not None

            report, rule_result, _ = run_ab(
                scenarios,
                runs=a.runs,
                baseline_model=b_model,
                candidate_model=c_model,
                baseline_bin=b_bin,
                candidate_bin=c_bin,
                baseline_manifest=b_manifest,
                candidate_manifest=c_manifest,
                baseline_lua=a.baseline_lua,
                candidate_lua=a.candidate_lua,
                out_dir=out_dir,
                floor=a.floor,
                allow_identical=a.allow_identical,
                judge_fn=judge_fn,
            )
    except (OSError, ValueError, TypeError, RuntimeError) as exc:
        print(f"eval-ab failed: {exc}", file=sys.stderr)
        return 1

    print(report)
    print(f"wrote {out_dir / 'report.md'}")
    if rule_result.verdict == "fail":
        return 1
    if a.strict and rule_result.verdict != "pass":
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
