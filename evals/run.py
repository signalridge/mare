#!/usr/bin/env python3
"""`just eval [group]` — complex real-model evals through Harbor.

Groups (each a fixed list, one try per task):
  own           evals/tasks (qin-shi-huang and pompeii, tool-use cases 1–8); tools-6
                runs `--parallel-tries` times to report how often the three reads share one message
  tbench        the Terminal-Bench 2.0 subset in evals/tbench.txt
  dev           evals/tasks/dev-* (to-do web app, add a feature, multi-file bug, sandbox
                escalation; script-scored)
  swebench      SWE-bench Verified, evals/swebench.txt (5 "<15 min fix" tasks, ~1 GB image each)
  featurebench  FeatureBench-lite, evals/featurebench.txt (3 tasks, ~10 GB image each)
  polyglot      Aider polyglot, evals/polyglot.txt (Rust, Go, JavaScript, Python)
  all           every group (default)
Registry datasets run with permission=full-access and the dangerous-command guard off: the task
container is the sandbox. Own tasks (own, dev) use mare's default `auto` preset.

Needs Docker and `harbor` (`uv tool install harbor`); OPENCODE_API_KEY must be in the environment.
Not part of CI. Prints one row per trial: pass, wall time, tokens, usage.cost, plus task details
(Pompeii precision/recall/F1, the Qin SVG check, the tools-6 batch, the dev tasks' test counts),
and the cost total.
"""

import argparse
import datetime as dt
import hashlib
import json
import math
import mmap
import os
import re
import shutil
import subprocess
import sys
import tomllib
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EVALS = ROOT / "evals"
BIN_DIR = ROOT / "target" / "eval-bin"
JOBS = ROOT / "target" / "eval-jobs"
AGENT = "evals.agent.mare_agent:MareAgent"
PARALLEL_TASK = "tools-6-parallel-reads"
DEV_TASKS = "dev-*"
# registry datasets: group -> (dataset ref pinned to a revision, task list file, mare --timeout)
DATASETS = {
    "tbench": ("terminal-bench@2.0", "tbench.txt", "13m"),
    "swebench": ("swe-bench/swe-bench-verified@2", "swebench.txt", "20m"),
    "featurebench": ("featurebench/featurebench-lite@1", "featurebench.txt", "20m"),
    "polyglot": ("aider/aider-polyglot@1", "polyglot.txt", "20m"),
}
MAX_CONCURRENCY = {"featurebench": 1}
GROUPS = ["own", "tbench", "dev", "swebench", "featurebench", "polyglot"]
LOCAL_TASKS = {
    "own": (
        "qin-shi-huang", "pompeii", "tools-1-read-modify-read", "tools-2-read-before-edit",
        "tools-3-ambiguous-edit", "tools-4-apply-patch", "tools-5-cd-carries-over",
        "tools-6-parallel-reads", "tools-7-background", "tools-8-error-recovery",
    ),
    "dev": ("dev-todo-web", "dev-add-feature", "dev-multifile-bug", "dev-sandbox-escalation"),
}
LIST_HASHES = {
    "tbench": "1dd2738f073e31b5d8bdfefe580734ccbc465f6c50095cdab25aa72eb1e95f6d",
    "swebench": "99ad4abd5253119f8720eab5be3ee72c8896c3456e64c92237dbb1392d1f4888",
    "featurebench": "453df69c17739c70e47db6d46cb066fff5cbe8cebf36b5fd53de200f5b2bdd24",
    "polyglot": "870ff071c9e026cfcf1dde22d431e624e6f58cc835721e0dc39e7e55dd682d39",
}
BINARIES = {"mare-linux-amd64": (62, "x86_64"), "mare-linux-arm64": (183, "aarch64")}

def source_identity():
    """Identify clean runner and build sources; unverifiable archives must not be reused."""
    source = Path(os.environ.get("MARE_EVAL_SRC") or ROOT).resolve()
    env = {k: v for k, v in os.environ.items() if k not in {"GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"}}
    revisions = []
    for tree in (ROOT, source):
        try:
            top = subprocess.run(
                ["git", "-C", str(tree), "rev-parse", "--show-toplevel"],
                env=env, capture_output=True, text=True, check=True,
            ).stdout.strip()
            revision = subprocess.run(
                ["git", "-C", str(tree), "rev-parse", "--verify", "HEAD"],
                env=env, capture_output=True, text=True, check=True,
            ).stdout.strip()
            dirty = subprocess.run(
                ["git", "-C", str(tree), "status", "--porcelain", "--untracked-files=normal"],
                env=env, capture_output=True, text=True, check=True,
            ).stdout
        except (OSError, subprocess.CalledProcessError) as exc:
            raise ValueError("binary source must be an identifiable Git checkout") from exc
        if Path(top).resolve() != Path(tree).resolve():
            raise ValueError("binary source must be the checkout root, not a nested archive")
        if not re.fullmatch(r"[0-9a-f]{40}", revision) or dirty.strip():
            raise ValueError("binary source checkout must be clean and have a full commit SHA")
        revisions.append(revision)
    if revisions[0] != revisions[1]:
        raise ValueError("binary build source and eval runner have different commit SHAs")
    with (source / "Cargo.toml").open("rb") as f:
        version = tomllib.load(f)["workspace"]["package"]["version"]
    return revisions[0], version

def binary_digest(path, machine, arch, revision, version):
    """Check ELF architecture and the build's embedded version without executing foreign binaries."""
    digest = hashlib.sha256()
    try:
        with path.open("rb") as f:
            header = f.read(20)
            if len(header) < 20 or header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != machine:
                raise ValueError(f"invalid binary architecture: {path.name}")
            f.seek(0)
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                digest.update(chunk)
            f.seek(0)
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as data:
                banner = re.compile(
                    re.escape(f"{version}\ncommit: {revision[:12]}\ndate: ".encode())
                    + rb"\d{4}-\d{2}-\d{2}\ntarget: " + re.escape(f"{arch}-unknown-linux-musl".encode())
                )
                if not banner.search(data):
                    raise ValueError(f"binary version or commit mismatch: {path.name}")
    except OSError as exc:
        raise ValueError(f"missing or unreadable binary: {path.name}") from exc
    return digest.hexdigest()

def write_binary_manifest(directory, revision, version):
    digests = {name: binary_digest(directory / name, machine, arch, revision, version)
               for name, (machine, arch) in BINARIES.items()}
    temporary = directory / "manifest.json.tmp"
    temporary.write_text(json.dumps({"version": version, "commit": revision, "binaries": digests}) + "\n", encoding="utf-8")
    temporary.replace(directory / "manifest.json")


def validate_binaries(directory, revision, version):
    try:
        manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise ValueError("binary manifest is missing or invalid") from exc
    if not isinstance(manifest, dict) or manifest.get("commit") != revision or manifest.get("version") != version:
        raise ValueError("binary manifest version or commit SHA mismatch")
    digests = manifest.get("binaries")
    if not isinstance(digests, dict) or set(digests) != set(BINARIES):
        raise ValueError("binary manifest must contain both architectures")
    for name, (machine, arch) in BINARIES.items():
        if binary_digest(directory / name, machine, arch, revision, version) != digests[name]:
            raise ValueError(f"binary SHA mismatch: {name}")
    return manifest


def sh(cmd, **kw):
    print("+", " ".join(cmd) if isinstance(cmd, list) else cmd, flush=True)
    return subprocess.run(cmd, check=True, **kw)


def build():
    """Static musl binaries for both architectures (Terminal-Bench images are amd64; our own
    tasks build natively). Docker volumes keep the build caches. Parallelism follows
    CARGO_BUILD_JOBS (default 4). MARE_EVAL_SRC must be a clean checkout at the runner's SHA."""
    BIN_DIR.mkdir(parents=True, exist_ok=True)
    common = [
        "docker",
        "run",
        "--rm",
        "-v",
        f"{os.environ.get('MARE_EVAL_SRC') or ROOT}:/src:ro",
        "-v",
        "mare-musl-target:/target",
        "-v",
        "mare-cargo-registry:/usr/local/cargo/registry",
        "-v",
        "mare-tools:/opt/tools",
        "-v",
        f"{BIN_DIR}:/out",
        "-e",
        "CARGO_TARGET_DIR=/target",
        "-e",
        "CARGO_INCREMENTAL=0",
        "-e",
        "GIT_CONFIG_COUNT=1",
        "-e",
        "GIT_CONFIG_KEY_0=safe.directory",
        "-e",
        "GIT_CONFIG_VALUE_0=/src",
        "-e",
        f"CARGO_BUILD_JOBS={os.environ.get('CARGO_BUILD_JOBS', '4')}",
        "-w",
        "/src",
        "rust:latest",
        "bash",
        "-c",
    ]
    script = (
        "set -e; apt-get update -qq >/dev/null; apt-get install -y -qq musl-tools python3-pip >/dev/null 2>&1; "
        "pip install --break-system-packages -q ziglang >/dev/null 2>&1; export PATH=/opt/tools/bin:$PATH; "
        "command -v cargo-zigbuild >/dev/null || cargo install cargo-zigbuild --locked -q --root /opt/tools; "
        "rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl >/dev/null 2>&1; "
        "cargo zigbuild --release --locked -p mare-cli --target aarch64-unknown-linux-musl; "
        "cargo zigbuild --release --locked -p mare-cli --target x86_64-unknown-linux-musl; "
        "cp /target/aarch64-unknown-linux-musl/release/mare /out/mare-linux-arm64; "
        "cp /target/x86_64-unknown-linux-musl/release/mare /out/mare-linux-amd64"
    )
    sh(common + [script])


def sync_helpers():
    """Each task's verifier is self-contained: copy the shared helper into every tests/ dir."""
    src = EVALS / "lib" / "mare_log.py"
    for t in sorted((EVALS / "tasks").iterdir()):
        if (t / "task.toml").is_file():
            shutil.copyfile(src, t / "tests" / "mare_log.py")


def harbor(job, model, args, agent_kwargs=(), *, identity):
    cmd = [
        "harbor",
        "run",
        "-a",
        AGENT,
        "-m",
        model,
        "-o",
        str(JOBS),
        "--job-name",
        job,
        *args,
    ]
    for kv in agent_kwargs:
        cmd += ["--ak", kv]
    env = dict(
        os.environ,
        PYTHONPATH=f"{ROOT}{os.pathsep}{os.environ.get('PYTHONPATH', '')}",
        MARE_EVAL_BIN_DIR=str(BIN_DIR),
        MARE_EVAL_IDENTITY=json.dumps(identity, sort_keys=True),
        MARE_EVAL_VERSION=f"{identity['version']}+{identity['commit'][:12]}",
    )
    print("+", " ".join(cmd), flush=True)
    try:
        result = subprocess.run(cmd, env=env, timeout=6 * 60 * 60, check=False)
        return JOBS / job, result.returncode
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"harbor could not finish: {type(exc).__name__}", file=sys.stderr)
        return JOBS / job, -1


def task_list(name):
    return [
        l.split("#", 1)[0].strip()
        for l in (EVALS / name).read_text().splitlines()
        if l.split("#", 1)[0].strip()
    ]


def task_plan():
    """The curated inventory is fixed: detect a changed or duplicated dataset selection."""
    plans = {}
    for g in GROUPS:
        if g in LOCAL_TASKS:
            plans[g] = list(LOCAL_TASKS[g])
        else:
            names = task_list(DATASETS[g][1])
            digest = hashlib.sha256(("\n".join(names) + "\n").encode()).hexdigest()
            if digest != LIST_HASHES[g]:
                raise ValueError(f"curated eval task list changed: {g}")
            plans[g] = names
    planned = [task for group in GROUPS for task in plans[group]]
    if len(set(planned)) != len(planned) or len(planned) != 34:
        raise ValueError("eval task inventory must contain 34 distinct tasks")
    return plans


def finite_cost(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def seconds(a, b):
    try:
        return (
            dt.datetime.fromisoformat(b)
            - dt.datetime.fromisoformat(a)
        ).total_seconds()
    except (TypeError, ValueError, OverflowError):
        return None


def error_row(task, error):
    return {"task": task, "status": "ERROR", "pass": False, "reward": None, "secs": None,
            "tok_in": None, "tok_out": None, "cost": None, "exit": "-", "error": error,
            "details": {}, "failed": []}


def expected_task(raw, expected, directory=False):
    """Resolve Harbor's task name or directory against the known inventory only."""
    if not isinstance(raw, str):
        return None
    matches = []
    for task in expected:
        leaf = task.rsplit("/", 1)[-1]
        if raw in (task, leaf) or (directory and raw.startswith((task + "__", leaf + "__"))):
            matches.append(task)
    return matches[0] if len(matches) == 1 else None


def trial_result(folder, task, result):
    row = error_row(task, "missing or invalid result")
    try:
        if not isinstance(result, dict):
            return row
        agent = result.get("agent_result")
        if not isinstance(agent, dict):
            row["error"] = "missing agent result"
            return row
        cost = agent.get("cost_usd")
        row.update(
            secs=seconds(result.get("started_at", ""), result.get("finished_at", "")),
            tok_in=agent.get("n_input_tokens"), tok_out=agent.get("n_output_tokens"),
            cost=float(cost) if finite_cost(cost) else None,
        )
        verifier = result.get("verifier_result")
        reward = verifier.get("rewards", {}).get("reward") if isinstance(verifier, dict) else None
        if type(reward) not in (int, float) or not math.isfinite(reward):
            row["error"] = "missing verifier result"
            return row
        exit_code = (folder / "agent" / "exit_code").read_text(encoding="utf-8").strip()
        exit_number = int(exit_code)
        row["exit"] = str(exit_number)
        if exit_number != 0:
            row["error"] = f"agent exit {exit_number}"
            return row
        if result.get("exception_info"):
            row["error"] = "trial crashed or timed out"
            return row
        details_path = folder / "verifier" / "details.json"
        details = json.loads(details_path.read_text(encoding="utf-8")) if details_path.exists() else {}
        if not isinstance(details, dict) or not isinstance(details.get("checks", {}), dict):
            row["error"] = "invalid verifier details"
            return row
        status = "PASS" if reward == 1.0 else "FAIL"
        row.update({
            "status": status, "pass": status == "PASS", "reward": reward, "error": None,
            "details": {k: v for k, v in details.items() if k != "checks"},
            "failed": [k for k, v in details.get("checks", {}).items() if not v],
        })
    except (OSError, ValueError, TypeError, AttributeError):
        return row
    return row


def trials(job_dir, expected, returncode):
    """Produce exactly the planned task/attempt rows, including absent or corrupt trials."""
    counts = Counter(expected)
    if not expected or any(not isinstance(name, str) or not name for name in expected):
        raise ValueError("expected eval tasks must be a nonempty list")
    found = defaultdict(list)
    unexpected = False
    try:
        folders = sorted(p for p in job_dir.iterdir() if p.is_dir()) if job_dir.is_dir() else []
    except OSError:
        reason = f"harbor exited with status {returncode}; cannot read job" if returncode != 0 else "cannot read Harbor job directory"
        return [error_row(task, reason) for task in expected]
    if folders:
        for folder in folders:
            by_dir = expected_task(folder.name, counts, directory=True)
            result_path = folder / "result.json"
            if not result_path.is_file():
                if by_dir:
                    found[by_dir].append(error_row(by_dir, "missing result"))
                continue
            try:
                result = json.loads(result_path.read_text(encoding="utf-8"))
                by_result = expected_task(result.get("task_name"), counts) if isinstance(result, dict) else None
            except (OSError, ValueError):
                by_result = None
            if not by_result:
                if by_dir:
                    found[by_dir].append(error_row(by_dir, "missing or mismatched result task identity"))
                else:
                    unexpected = True
                continue
            task = by_result
            if by_result and by_dir and by_result != by_dir:
                found[by_dir].append(error_row(by_dir, "result task identity mismatch"))
                unexpected = True
            else:
                found[task].append(trial_result(folder, task, result))
    rows = []
    offsets = Counter()
    for task in expected:
        matches = found[task]
        if unexpected or len(matches) > counts[task]:
            row = error_row(task, "unexpected or duplicate task result")
        elif offsets[task] >= len(matches):
            row = error_row(task, "missing task result")
        else:
            row = matches[offsets[task]]
        offsets[task] += 1
        rows.append(row)
    # Harbor also exits nonzero when one trial fails. Preserve other independently
    # verified results; an unexplained nonzero exit must still fail the batch.
    if returncode != 0 and all(row["status"] == "PASS" for row in rows):
        for row in rows:
            row.update({"status": "ERROR", "pass": False, "error": f"harbor exited with status {returncode}"})
    return rows


def table(rows):
    print()
    print(
        f"{'task':44} {'result':6} {'time':>7} {'tok in':>8} {'tok out':>8} {'cost $':>8} {'exit':>4}  notes"
    )
    for r in rows:
        notes = []
        d = r["details"]
        for k in (
            "precision",
            "recall",
            "f1",
            "svg root",
            "max reads in one message",
            "api_checks_passed",
            "hidden",
            "original",
            "own",
        ):
            if k in d:
                notes.append(f"{k}={d[k]}")
        if r["failed"]:
            notes.append("failed: " + "; ".join(r["failed"]))
        if r["error"]:
            notes.append(f"error: {r['error']}")
        secs = f"{r['secs']:.0f}s" if r["secs"] is not None else "-"
        res = r["status"]
        cost = f"{r['cost']:.4f}" if r.get("cost") is not None else "unknown"
        tok_in = r["tok_in"] if r["tok_in"] is not None else "-"
        tok_out = r["tok_out"] if r["tok_out"] is not None else "-"
        print(
            f"{r['task']:44} {res:6} {secs:>7} {tok_in!s:>8} {tok_out!s:>8} {cost:>8} {r['exit']:>4}  {', '.join(notes)}"
        )
    tasks = defaultdict(list)
    for row in rows:
        tasks[row["task"]].append(row)
    passed = sum(all(row["status"] == "PASS" for row in attempts) for attempts in tasks.values())
    known = [row["cost"] for row in rows if row["cost"] is not None]
    unknown = len(rows) - len(known)
    print(f"\n{passed}/{len(tasks)} tasks passed ({len(rows)} trials) · known subtotal ${sum(known):.4f} · {unknown} unknown")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("group", nargs="?", default="all", choices=GROUPS + ["all"])
    ap.add_argument(
        "--model",
        default=os.environ.get("MARE_LIVE_MODEL", "opencode-go/glm-5.3-flash"),
    )
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--concurrency", "-n", type=int, default=2)
    # the model is billed (opencode Go glm-5.3-flash): one try per task; only tools-6 can repeat
    ap.add_argument("--parallel-tries", type=int, default=1)
    a = ap.parse_args()
    if a.concurrency < 1 or a.parallel_tries < 1:
        ap.error("--concurrency and --parallel-tries must be positive")
    plans = task_plan()
    groups = GROUPS if a.group == "all" else [a.group]
    planned = [task for group in groups for task in plans[group]]
    if a.group in ("all", "own"):
        planned += [PARALLEL_TASK] * (a.parallel_tries - 1)

    def preflight_error(stage):
        table([error_row(task, f"preflight failed: {stage}") for task in planned])
        return 1

    if not os.environ.get("OPENCODE_API_KEY"):
        return preflight_error("OPENCODE_API_KEY is not set")
    if not shutil.which("harbor"):
        return preflight_error("harbor not found")
    manifest = BIN_DIR / "manifest.json"
    if a.no_build and not manifest.is_file():
        return preflight_error("binary manifest is missing")
    stage = "binary source identity"
    try:
        revision, version = source_identity()
        if not a.no_build:
            stage = "binary build"
            manifest.unlink(missing_ok=True)
            build()
            if source_identity() != (revision, version):
                raise ValueError("binary source changed during build")
            write_binary_manifest(BIN_DIR, revision, version)
        stage = "binary validation"
        identity = validate_binaries(BIN_DIR, revision, version)
        stage = "verifier helper setup"
        sync_helpers()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"{stage}: {type(exc).__name__}: {exc}", file=sys.stderr)
        return preflight_error(stage)
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    rows = []
    if "own" in groups:
        job, code = harbor(
            f"own-{stamp}",
            a.model,
            ["-p", str(EVALS / "tasks"), "-n", str(a.concurrency), "-x", PARALLEL_TASK, "-x", DEV_TASKS],
            identity=identity,
        )
        rows += trials(job, [name for name in plans["own"] if name != PARALLEL_TASK], code)
        job, code = harbor(
            f"parallel-{stamp}",
            a.model,
            [
                "-p",
                str(EVALS / "tasks" / PARALLEL_TASK),
                "-k",
                str(a.parallel_tries),
                "-n",
                str(a.concurrency),
            ],
            identity=identity,
        )
        prow = trials(job, [PARALLEL_TASK] * a.parallel_tries, code)
        rows += prow
        par = sum(1 for r in prow if r["details"].get("parallel"))
        print(
            f"\ntools-6: the three reads were issued in one assistant message in {par}/{len(prow)} runs"
        )
    if "dev" in groups:
        job, code = harbor(
            f"dev-{stamp}",
            a.model,
            ["-p", str(EVALS / "tasks"), "-i", DEV_TASKS, "-n", str(a.concurrency), "-k", "1"],
            identity=identity,
        )
        rows += trials(job, plans["dev"], code)
    for g in groups:
        if g not in DATASETS:
            continue
        ref, _, timeout = DATASETS[g]
        # FeatureBench tasks ask for 8 GB each: one at a time on a 16 GB Docker VM
        n = min(a.concurrency, MAX_CONCURRENCY.get(g, 99))
        args = ["-d", ref, "-n", str(n), "-k", "1"]
        for task in plans[g]:
            args += ["-i", task]
        job, code = harbor(
            f"{g}-{stamp}",
            a.model,
            args,
            ["permission=full-access", "dangerous_guard=off", f"mare_timeout={timeout}"],
            identity=identity,
        )
        rows += trials(job, plans[g], code)
    table(rows)
    return 0 if rows and all(row["status"] == "PASS" for row in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
