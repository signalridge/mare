#!/usr/bin/env python3
"""Compare built-in provider presets and models_dev ids against models.dev metadata.

Developer utility (`just presets-drift`); requires network unless `--fixture` is given
and is intentionally not run in CI.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
COMPAT_PATH = ROOT / "crates/mare-plugin/preset/plugins/provider-openai-compat.lua"
MODELS_DEV_URL = "https://models.dev/api.json"


def _strip_lua_comments(src: str) -> str:
    return re.sub(r"--[^\n]*", "", src)


def _extract_balanced(src: str, start_brace: int) -> tuple[str, int]:
    depth = 0
    in_str: str | None = None
    i = start_brace
    while i < len(src):
        ch = src[i]
        if in_str is not None:
            if ch == "\\" and i + 1 < len(src):
                i += 2
                continue
            if ch == in_str:
                in_str = None
        else:
            if ch in ('"', "'"):
                in_str = ch
            elif ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    return src[start_brace + 1 : i], i + 1
        i += 1
    raise ValueError("unterminated Lua table literal")


def parse_presets(src: str) -> dict[str, dict[str, Any]]:
    """Parse top-level `local PRESETS = { ... }` entries from provider-openai-compat.lua."""
    clean = _strip_lua_comments(src)
    m = re.search(r"\blocal\s+PRESETS\s*=\s*\{", clean)
    if not m:
        raise ValueError("PRESETS table not found")
    body, _ = _extract_balanced(clean, m.end() - 1)
    presets: dict[str, dict[str, Any]] = {}
    i = 0
    entry_re = re.compile(r'\s*(?:\["([^"]+)"\]|([A-Za-z_][A-Za-z0-9_]*))\s*=\s*\{')
    while i < len(body):
        em = entry_re.search(body, i)
        if not em:
            break
        name = em.group(1) or em.group(2)
        ebody, next_i = _extract_balanced(body, em.end() - 1)
        i = next_i
        info: dict[str, Any] = {"models": []}
        if re.search(r"\bno_key\s*=\s*true\b", ebody):
            info["no_key"] = True
        mdev_str = re.search(r'\bmodels_dev\s*=\s*"([^"]+)"', ebody)
        if mdev_str:
            info["models_dev"] = mdev_str.group(1)
        elif re.search(r"\bmodels_dev\s*=\s*false\b", ebody):
            info["models_dev"] = False
        dm = re.search(r'\bdefault_model\s*=\s*"([^"]+)"', ebody)
        if dm:
            info["default_model"] = dm.group(1)
        mm = re.search(r"\bmodels\s*=\s*\{", ebody)
        if mm:
            mbody, _ = _extract_balanced(ebody, mm.end() - 1)
            j = 0
            while j < len(mbody):
                ob = mbody.find("{", j)
                if ob == -1:
                    break
                item_body, j = _extract_balanced(mbody, ob)
                mid = re.search(r'\bmodel_id\s*=\s*"([^"]+)"', item_body)
                if not mid:
                    continue
                item: dict[str, Any] = {"model_id": mid.group(1)}
                cw = re.search(r"\bcontext_window\s*=\s*(\d+)", item_body)
                mo = re.search(r"\bmax_output\s*=\s*(\d+)", item_body)
                if cw:
                    item["context_window"] = int(cw.group(1))
                if mo:
                    item["max_output"] = int(mo.group(1))
                info["models"].append(item)
        presets[name] = info
    return presets


def check_drift(presets: dict[str, dict[str, Any]], catalog: dict[str, Any]) -> list[str]:
    """Return human-readable drift lines comparing presets against a models.dev JSON payload."""
    issues: list[str] = []
    for name in sorted(presets):
        p = presets[name]
        if p.get("no_key"):
            continue
        mdev = p.get("models_dev")
        if mdev is None:
            issues.append(f"{name}: missing explicit models_dev field")
            continue
        if mdev is False:
            continue
        pv = catalog.get(mdev)
        if not isinstance(pv, dict) or not isinstance(pv.get("models"), dict):
            issues.append(f"{name}: models_dev provider `{mdev}` not found in models.dev")
            continue
        remote_models: dict[str, Any] = pv["models"]
        dm = p.get("default_model")
        if dm and dm not in remote_models:
            issues.append(f"{name}: default_model `{dm}` absent from models.dev/{mdev}")
        for seed in p.get("models", []):
            mid = seed["model_id"]
            rm = remote_models.get(mid)
            if not isinstance(rm, dict):
                issues.append(f"{name}/{mid}: seed model absent from models.dev/{mdev}")
                continue
            lim = rm.get("limit") if isinstance(rm.get("limit"), dict) else {}
            r_ctx = lim.get("context")
            r_out = lim.get("output")
            if isinstance(r_ctx, (int, float)) and "context_window" in seed and int(r_ctx) != seed["context_window"]:
                issues.append(
                    f"{name}/{mid}: context_window seed={seed['context_window']} vs models.dev={int(r_ctx)}"
                )
            if isinstance(r_out, (int, float)) and "max_output" in seed and int(r_out) != seed["max_output"]:
                issues.append(f"{name}/{mid}: max_output seed={seed['max_output']} vs models.dev={int(r_out)}")
    return issues


def _self_test() -> int:
    sample = """
    local PRESETS = {
      alpha = {
        models_dev = "alpha-dev",
        default_model = "a1",
        models = {
          { model_id = "a1", context_window = 1000, max_output = 200 },
        },
      },
      local_only = { no_key = true, models = {} },
    }
    """
    parsed = parse_presets(sample)
    assert set(parsed.keys()) == {"alpha", "local_only"}, parsed
    clean_catalog = {"alpha-dev": {"models": {"a1": {"limit": {"context": 1000, "output": 200}}}}}
    assert check_drift(parsed, clean_catalog) == []
    drifted_catalog = {"alpha-dev": {"models": {"a1": {"limit": {"context": 2000, "output": 200}}}}}
    drifts = check_drift(parsed, drifted_catalog)
    assert len(drifts) == 1 and "context_window" in drifts[0], drifts
    real_presets = parse_presets(COMPAT_PATH.read_text(encoding="utf-8"))
    assert "deepseek" in real_presets and "kimi-code" in real_presets and "lmstudio" in real_presets
    print(f"presets_drift self-test OK ({len(real_presets)} presets parsed)")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Compare built-in provider presets against models.dev.")
    ap.add_argument("--url", default=MODELS_DEV_URL, help="models.dev JSON URL")
    ap.add_argument("--fixture", type=Path, help="Read models.dev JSON from a local file instead of HTTP")
    ap.add_argument("--self-test", action="store_true", help="Run built-in unit tests and exit")
    args = ap.parse_args(argv)

    if args.self_test:
        return _self_test()

    presets = parse_presets(COMPAT_PATH.read_text(encoding="utf-8"))
    if args.fixture is not None:
        catalog = json.loads(args.fixture.read_text(encoding="utf-8"))
    else:
        with urllib.request.urlopen(args.url, timeout=20) as resp:
            catalog = json.loads(resp.read().decode("utf-8"))

    issues = check_drift(presets, catalog)
    if not issues:
        print(f"presets-drift: OK ({len(presets)} presets checked, 0 drift items)")
        return 0
    for line in issues:
        print(f"drift: {line}")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
