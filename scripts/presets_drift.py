#!/usr/bin/env python3
"""Generate and check the bundled models.dev snapshot for built-in provider presets.

Developer utility (`just presets-drift`); requires network unless `--fixture` is given
and is intentionally not run in CI.

Upstream data source: https://models.dev/api.json (https://github.com/anomalyco/models.dev,
MIT License, Copyright (c) 2025 models.dev).
"""

from __future__ import annotations

import argparse
import gzip
import json
import re
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
COMPAT_PATH = ROOT / "crates/mare-plugin/preset/plugins/provider-openai-compat.lua"
SNAPSHOT_PATH = ROOT / "crates/mare-plugin/preset/resources/models_dev.json.gz"
STANDALONE_PROVIDERS: tuple[tuple[str, Path, str], ...] = (
    ("anthropic", ROOT / "crates/mare-plugin/preset/plugins/provider-anthropic.lua", "anthropic"),
    ("google", ROOT / "crates/mare-plugin/preset/plugins/provider-google.lua", "google"),
    ("openai", ROOT / "crates/mare-plugin/preset/plugins/lua/provider-openai/requests.lua", "openai"),
    ("opencode-go", ROOT / "crates/mare-plugin/preset/plugins/lua/provider-opencode-go/provider.lua", "opencode-go"),
)
# Additional models.dev provider ids bundled alongside built-in presets, if any.
EXTRA_SNAPSHOT_PROVIDERS: tuple[str, ...] = ()
MODELS_DEV_URL = "https://models.dev/api.json"
MODELS_DEV_LICENSE = "MIT (Copyright (c) 2025 models.dev; https://github.com/anomalyco/models.dev)"
USER_AGENT = "mare-presets-drift/1.0"

NPM_PROTOCOL: dict[str, str] = {
    "@ai-sdk/openai": "responses",
    "@ai-sdk/anthropic": "messages",
    "@ai-sdk/openai-compatible": "chat-completions",
    "@ai-sdk/google": "generate-content",
}


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


def _parse_models_block(ebody: str) -> list[dict[str, Any]]:
    models: list[dict[str, Any]] = []
    mm = re.search(r"\bmodels\s*=\s*\{", ebody)
    if not mm:
        return models
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
        if re.search(r"\bcontext_window_measured\s*=\s*true\b", item_body):
            item["context_window_measured"] = True
        if mo:
            item["max_output"] = int(mo.group(1))
        models.append(item)
    return models


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
        info: dict[str, Any] = {"models": _parse_models_block(ebody)}
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
        presets[name] = info
    return presets


def parse_registered_provider(src: str, default_models_dev: str) -> dict[str, Any]:
    """Parse a standalone `llm:register_provider({ ... })` declaration."""
    clean = _strip_lua_comments(src)
    m = re.search(r"llm:register_provider\s*\(\s*\{", clean)
    if not m:
        raise ValueError("llm:register_provider({...}) call not found")
    ebody, _ = _extract_balanced(clean, m.end() - 1)
    info: dict[str, Any] = {"models": _parse_models_block(ebody), "models_dev": default_models_dev}
    mdev_str = re.search(r'\bmodels_dev\s*=\s*"([^"]+)"', ebody)
    if mdev_str:
        info["models_dev"] = mdev_str.group(1)
    elif re.search(r"\bmodels_dev\s*=\s*false\b", ebody):
        info["models_dev"] = False
    dm = re.search(r'\bdefault_model\s*=\s*"([^"]+)"', ebody)
    if dm:
        info["default_model"] = dm.group(1)
    return info


def parse_all_builtin_presets() -> dict[str, dict[str, Any]]:
    """Parse all built-in provider preset seeds (openai-compat + standalone providers)."""
    presets = parse_presets(COMPAT_PATH.read_text(encoding="utf-8"))
    for name, path, default_mdev in STANDALONE_PROVIDERS:
        presets[name] = parse_registered_provider(path.read_text(encoding="utf-8"), default_mdev)
    return presets


def tracked_models_dev_ids(presets: dict[str, dict[str, Any]], extra: tuple[str, ...] = EXTRA_SNAPSHOT_PROVIDERS) -> list[str]:
    """Collect sorted unique models.dev provider ids referenced by built-in presets."""
    ids: set[str] = set()
    for p in presets.values():
        if p.get("no_key"):
            continue
        mdev = p.get("models_dev")
        if isinstance(mdev, str) and mdev:
            ids.add(mdev)
    for pid in extra:
        if pid:
            ids.add(pid)
    return sorted(ids)


def _trim_cost(raw: Any) -> dict[str, float] | None:
    if not isinstance(raw, dict):
        return None
    out: dict[str, float] = {}
    for k in ("input", "output", "cache_read", "cache_write"):
        v = raw.get(k)
        if isinstance(v, (int, float)) and not isinstance(v, bool):
            out[k] = v
    return out or None


def _trim_limit(raw: Any) -> dict[str, int] | None:
    if not isinstance(raw, dict):
        return None
    out: dict[str, int] = {}
    for k in ("context", "input", "output"):
        v = raw.get(k)
        if isinstance(v, (int, float)) and not isinstance(v, bool) and int(v) > 0:
            out[k] = int(v)
    return out or None


def _trim_reasoning_options(raw: Any) -> list[dict[str, Any]] | None:
    if not isinstance(raw, list):
        return None
    out: list[dict[str, Any]] = []
    for opt in raw:
        if not isinstance(opt, dict) or not isinstance(opt.get("type"), str) or not opt["type"]:
            continue
        item: dict[str, Any] = {"type": opt["type"]}
        vals = opt.get("values")
        if isinstance(vals, list):
            clean = [v for v in vals if isinstance(v, str) and v and v != "null"]
            if clean:
                item["values"] = clean
        for k in ("min", "max"):
            v = opt.get(k)
            if isinstance(v, (int, float)) and not isinstance(v, bool):
                item[k] = int(v)
        out.append(item)
    return out or None


def trim_catalog(catalog: dict[str, Any], provider_ids: list[str]) -> dict[str, dict[str, dict[str, Any]]]:
    """Trim a full models.dev payload down to tracked providers and fields used by mare."""
    providers: dict[str, dict[str, dict[str, Any]]] = {}
    for pid in sorted(provider_ids):
        pv = catalog.get(pid)
        if not isinstance(pv, dict) or not isinstance(pv.get("models"), dict):
            continue
        pv_npm = pv.get("npm") if isinstance(pv.get("npm"), str) else ""
        models_out: dict[str, dict[str, Any]] = {}
        for mid in sorted(pv["models"]):
            e = pv["models"][mid]
            if not isinstance(e, dict):
                continue
            entry: dict[str, Any] = {}
            e_prov = e.get("provider")
            npm = e_prov.get("npm") if isinstance(e_prov, dict) and isinstance(e_prov.get("npm"), str) else pv_npm
            proto = NPM_PROTOCOL.get(npm or "")
            if proto:
                entry["protocol"] = proto
            if isinstance(e.get("name"), str) and e["name"]:
                entry["name"] = e["name"]
            if isinstance(e.get("family"), str) and e["family"]:
                entry["family"] = e["family"]
            if isinstance(e.get("release_date"), str) and e["release_date"]:
                entry["release_date"] = e["release_date"]
            if isinstance(e.get("status"), str) and e["status"]:
                entry["status"] = e["status"]
            if isinstance(e.get("type"), str) and e["type"]:
                entry["type"] = e["type"]
            if isinstance(e.get("tool_call"), bool):
                entry["tool_call"] = e["tool_call"]
            if isinstance(e.get("reasoning"), bool):
                entry["reasoning"] = e["reasoning"]
            ro = _trim_reasoning_options(e.get("reasoning_options"))
            if ro:
                entry["reasoning_options"] = ro
            il = e.get("interleaved")
            if isinstance(il, (bool, str, dict)):
                entry["interleaved"] = il
            if isinstance(e.get("dynamically_loaded_tools"), bool):
                entry["dynamically_loaded_tools"] = e["dynamically_loaded_tools"]
            lim = _trim_limit(e.get("limit"))
            if lim:
                entry["limit"] = lim
            mods = e.get("modalities")
            if isinstance(mods, dict) and isinstance(mods.get("input"), list):
                inp = [x for x in mods["input"] if x in ("text", "image")]
                if inp:
                    entry["input"] = inp
            cost = _trim_cost(e.get("cost"))
            if cost:
                entry["cost"] = cost
            if entry:
                models_out[mid] = entry
        providers[pid] = models_out
    return providers


def build_snapshot(catalog: dict[str, Any], provider_ids: list[str], fetched_at: int | None = None) -> dict[str, Any]:
    return {
        "source": MODELS_DEV_URL,
        "license": MODELS_DEV_LICENSE,
        "fetched_at": int(time.time() if fetched_at is None else fetched_at),
        "providers": trim_catalog(catalog, provider_ids),
    }


def write_snapshot(path: Path, snapshot: dict[str, Any]) -> bytes:
    payload = json.dumps(snapshot, separators=(",", ":"), sort_keys=True, ensure_ascii=False).encode("utf-8")
    gz = gzip.compress(payload, compresslevel=9, mtime=0)
    path.write_bytes(gz)
    return gz


def read_snapshot(path: Path) -> dict[str, Any] | None:
    if not path.is_file():
        return None
    raw = gzip.decompress(path.read_bytes())
    data = json.loads(raw.decode("utf-8"))
    return data if isinstance(data, dict) else None


def check_drift(
    presets: dict[str, dict[str, Any]],
    catalog: dict[str, Any],
    snapshot: dict[str, Any] | None,
    extra_providers: tuple[str, ...] = EXTRA_SNAPSHOT_PROVIDERS,
) -> list[str]:
    """Check built-in preset references and whether the bundled models.dev snapshot needs refreshing."""
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
            if not isinstance(remote_models.get(mid), dict):
                issues.append(f"{name}/{mid}: seed model absent from models.dev/{mdev}")

    pids = tracked_models_dev_ids(presets, extra_providers)
    fresh_providers = trim_catalog(catalog, pids)
    if not isinstance(snapshot, dict) or not isinstance(snapshot.get("providers"), dict):
        issues.append("bundled snapshot missing or unreadable — run `python3 scripts/presets_drift.py --update`")
        return issues

    bundled_providers: dict[str, Any] = snapshot["providers"]
    for pid in sorted(set(bundled_providers) | set(fresh_providers)):
        old_p = bundled_providers.get(pid)
        new_p = fresh_providers.get(pid)
        if old_p is None:
            issues.append(f"snapshot stale: provider `{pid}` missing from bundled snapshot ({len(new_p)} models)")
            continue
        if new_p is None:
            issues.append(f"snapshot stale: provider `{pid}` no longer tracked or absent from models.dev")
            continue
        added = sorted(set(new_p) - set(old_p))
        removed = sorted(set(old_p) - set(new_p))
        changed = sorted(k for k in (set(new_p) & set(old_p)) if new_p[k] != old_p[k])
        if added or removed or changed:
            parts = []
            if added:
                parts.append(f"+{len(added)} added ({', '.join(added[:3])}{'…' if len(added) > 3 else ''})")
            if removed:
                parts.append(f"-{len(removed)} removed ({', '.join(removed[:3])}{'…' if len(removed) > 3 else ''})")
            if changed:
                parts.append(f"~{len(changed)} updated ({', '.join(changed[:3])}{'…' if len(changed) > 3 else ''})")
            issues.append(f"snapshot stale for `{pid}`: {'; '.join(parts)}")
    return issues


def _self_test() -> int:
    sample = """
    local PRESETS = {
      alpha = {
        models_dev = "alpha-dev",
        default_model = "a1",
        models = {
          { model_id = "a1" },
        },
      },
      local_only = { no_key = true, models = {} },
    }
    """
    parsed = parse_presets(sample)
    assert set(parsed.keys()) == {"alpha", "local_only"}, parsed
    catalog = {
        "alpha-dev": {
            "npm": "@ai-sdk/openai",
            "models": {
                "a1": {
                    "name": "Alpha One",
                    "family": "alpha",
                    "release_date": "2026-10-01",
                    "status": "active",
                    "type": "chat",
                    "tool_call": True,
                    "reasoning": True,
                    "reasoning_options": [
                        {"type": "toggle"},
                        {"type": "effort", "values": ["none", "low", "high", "null"]},
                    ],
                    "limit": {"context": 1050000, "input": 922000, "output": 128000},
                    "modalities": {"input": ["text", "image", "pdf"]},
                    "cost": {"input": 1.0, "output": 5.0, "cache_read": 0.1},
                }
            },
        }
    }
    snap = build_snapshot(catalog, ["alpha-dev"], fetched_at=1700000000)
    a1 = snap["providers"]["alpha-dev"]["a1"]
    assert a1["protocol"] == "responses"
    assert a1["input"] == ["text", "image"]
    assert a1["family"] == "alpha" and a1["release_date"] == "2026-10-01"
    assert a1["status"] == "active" and a1["type"] == "chat" and a1["tool_call"] is True
    assert a1["reasoning_options"] == [
        {"type": "toggle"},
        {"type": "effort", "values": ["none", "low", "high"]},
    ], a1["reasoning_options"]
    assert check_drift(parsed, catalog, snap, extra_providers=()) == []

    drifted_catalog = {
        "alpha-dev": {
            "npm": "@ai-sdk/openai",
            "models": {
                "a1": {
                    "name": "Alpha One",
                    "reasoning": True,
                    "limit": {"context": 1050000, "input": 922000, "output": 256000},
                },
                "a2": {"name": "Alpha Two", "limit": {"context": 128000, "output": 16384}},
            },
        }
    }
    drifts = check_drift(parsed, drifted_catalog, snap, extra_providers=())
    assert len(drifts) == 1 and "snapshot stale for `alpha-dev`" in drifts[0], drifts

    with tempfile.TemporaryDirectory() as td:
        gz_path = Path(td) / "models_dev.json.gz"
        write_snapshot(gz_path, snap)
        roundtrip = read_snapshot(gz_path)
        assert roundtrip == snap

    real_presets = parse_all_builtin_presets()
    for expected in (
        "deepseek",
        "kimi",
        "kimi-cn",
        "kimi-code",
        "kimi-code-global",
        "minimax",
        "minimax-cn",
        "zai",
        "zhipu-cn",
        "openrouter",
        "lmstudio",
        "anthropic",
        "google",
        "openai",
        "opencode-go",
    ):
        assert expected in real_presets, f"missing {expected} in {sorted(real_presets)}"
    bundled = read_snapshot(SNAPSHOT_PATH)
    assert bundled is not None and isinstance(bundled.get("providers"), dict), f"missing {SNAPSHOT_PATH}"
    for pid in tracked_models_dev_ids(real_presets):
        assert pid in bundled["providers"], f"bundled snapshot missing provider `{pid}`"
    print(f"presets_drift self-test OK ({len(real_presets)} presets, {len(bundled['providers'])} bundled providers)")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Generate or check the bundled models.dev snapshot.")
    ap.add_argument("--url", default=MODELS_DEV_URL, help="models.dev JSON URL")
    ap.add_argument("--fixture", type=Path, help="Read models.dev JSON from a local file instead of HTTP")
    ap.add_argument("--snapshot", type=Path, default=SNAPSHOT_PATH, help="Path to bundled models_dev.json.gz")
    ap.add_argument("--update", action="store_true", help="Refresh the bundled models_dev.json.gz snapshot in place")
    ap.add_argument("--self-test", action="store_true", help="Run built-in unit tests and exit")
    args = ap.parse_args(argv)

    if args.self_test:
        return _self_test()

    presets = parse_all_builtin_presets()
    if args.fixture is not None:
        catalog = json.loads(args.fixture.read_text(encoding="utf-8"))
    else:
        req = urllib.request.Request(
            args.url,
            headers={"User-Agent": USER_AGENT, "Accept": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=20) as resp:
            catalog = json.loads(resp.read().decode("utf-8"))

    if args.update:
        pids = tracked_models_dev_ids(presets)
        snap = build_snapshot(catalog, pids)
        gz = write_snapshot(args.snapshot, snap)
        total_models = sum(len(ms) for ms in snap["providers"].values())
        print(
            f"presets-drift: wrote {args.snapshot.relative_to(ROOT) if args.snapshot.is_relative_to(ROOT) else args.snapshot} "
            f"({len(snap['providers'])} providers, {total_models} models, {len(gz)} bytes gz)"
        )
        return 0

    snapshot = read_snapshot(args.snapshot)
    issues = check_drift(presets, catalog, snapshot)
    if not issues:
        print(f"presets-drift: OK ({len(presets)} presets checked, bundled snapshot up to date)")
        return 0
    for line in issues:
        print(f"drift: {line}")
    print("Run `python3 scripts/presets_drift.py --update` to refresh the bundled snapshot.")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
