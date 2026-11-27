#!/usr/bin/env python3
"""Verify that ci.yml ran and succeeded for all required jobs on the release commit."""

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import time
import unittest

ALWAYS_REQUIRED_JOBS = ("changes",)

BUILD_LINUX_JOB = "build tests (ubuntu-latest)"
LINUX_SHARD_JOBS = (
    "test (ubuntu-latest, shard 1/4)",
    "test (ubuntu-latest, shard 2/4)",
    "test (ubuntu-latest, shard 3/4)",
    "test (ubuntu-latest, shard 4/4)",
)

REQUIRED_JOBS = (
    "lint",
    "actionlint",
    "yamllint",
    "cargo-deny",
    "stylua · public-api-lint",
    *LINUX_SHARD_JOBS,
    "test (macos-latest)",
    "test (windows-latest)",
)

ALLOWED_CONCLUSIONS = ("success", "skipped")


def evaluate_jobs(jobs: list[dict]) -> list[str]:
    by_name = {job.get("name"): job for job in jobs if job.get("name")}
    errors: list[str] = []

    # Reject any failing/cancelled/incomplete job anywhere in the run (including build-linux-tests
    # whose failure would cause downstream shard jobs to report conclusion='skipped').
    for job in jobs:
        name = job.get("name") or "<unnamed>"
        conclusion = job.get("conclusion")
        if conclusion not in ALLOWED_CONCLUSIONS:
            errors.append(
                f"CI job {name!r} conclusion is {conclusion!r} (expected 'success' or path-filtered 'skipped')"
            )

    for name in ALWAYS_REQUIRED_JOBS:
        job = by_name.get(name)
        if job is None:
            errors.append(f"missing always-required CI job: {name}")
            continue
        conclusion = job.get("conclusion")
        if conclusion != "success":
            errors.append(
                f"CI job {name!r} conclusion is {conclusion!r} (expected 'success'; skipped does not count)"
            )

    for name in REQUIRED_JOBS:
        job = by_name.get(name)
        if job is None:
            errors.append(f"missing required CI job: {name}")
            continue
        conclusion = job.get("conclusion")
        if conclusion not in ALLOWED_CONCLUSIONS:
            errors.append(
                f"CI job {name!r} conclusion is {conclusion!r} (expected 'success' or 'skipped')"
            )

    build_job = by_name.get(BUILD_LINUX_JOB)
    if build_job is not None and build_job.get("conclusion") == "success":
        for shard in LINUX_SHARD_JOBS:
            shard_job = by_name.get(shard)
            if shard_job is not None and shard_job.get("conclusion") != "success":
                errors.append(
                    f"CI job {shard!r} conclusion is {shard_job.get('conclusion')!r} "
                    f"(must be 'success' when {BUILD_LINUX_JOB!r} succeeded)"
                )

    return errors


def check_runs(runs: list[dict], fetch_jobs) -> tuple[bool, list[str]]:
    if not runs:
        return False, ["no ci.yml workflow runs found for commit"]

    last_errors: list[str] = []
    # Prefer the most recent runs first
    sorted_runs = sorted(runs, key=lambda r: r.get("createdAt", ""), reverse=True)
    for run in sorted_runs:
        run_id = run.get("databaseId")
        if run.get("status") != "completed":
            last_errors = [
                f"ci.yml run {run_id} has status {run.get('status')!r} (not completed)"
            ]
            continue
        jobs = fetch_jobs(run_id)
        errs = evaluate_jobs(jobs)
        if not errs:
            return True, [f"ci.yml run {run_id}: all {len(REQUIRED_JOBS)} required jobs succeeded"]
        last_errors = [f"ci.yml run {run_id}: {e}" for e in errs]

    return False, last_errors


def wait_for_runs(
    list_runs,
    fetch_jobs,
    *,
    wait_minutes: float = 0.0,
    poll_interval: float = 30.0,
    monotonic=time.monotonic,
    sleep=time.sleep,
) -> tuple[bool, list[str]]:
    deadline = monotonic() + max(0.0, wait_minutes) * 60.0
    while True:
        runs = list_runs()
        ok, msgs = check_runs(runs, fetch_jobs)
        if ok:
            return True, msgs

        pending = [r for r in runs if r.get("status") != "completed"]
        if not runs or pending:
            remaining = deadline - monotonic()
            if remaining <= 0:
                if wait_minutes > 0:
                    if pending:
                        ids = ", ".join(str(r.get("databaseId")) for r in pending)
                        return False, [
                            f"timed out after {wait_minutes:g}m waiting for ci.yml run(s) {ids} to complete",
                            *msgs,
                        ]
                    return False, [
                        f"timed out after {wait_minutes:g}m waiting for ci.yml workflow runs to appear",
                        *msgs,
                    ]
                return False, msgs
            sleep(min(max(0.0, poll_interval), remaining))
            continue

        return False, msgs


def _extract_job_block(ci_text: str, job_key: str) -> str:
    lines = ci_text.splitlines()
    marker = f"  {job_key}:"
    start = None
    for idx, line in enumerate(lines):
        if line == marker:
            start = idx
            break
    if start is None:
        return ""
    end = len(lines)
    for idx in range(start + 1, len(lines)):
        line = lines[idx]
        if line and not line.startswith("    ") and not line.lstrip().startswith("#"):
            end = idx
            break
    return "\n".join(lines[start:end])


def check_ci_workflow_accept_tools(ci_text: str) -> list[str]:
    accept_block = _extract_job_block(ci_text, "accept")
    if not accept_block:
        return ["accept-nightly.yml is missing 'accept' job"]
    errors: list[str] = []
    for tool in ("stylua", "ast-grep", "lua-language-server"):
        if tool not in accept_block:
            errors.append(f"accept-nightly.yml accept job must install {tool} so accept.sh --release never skips it")
    if "--allow-missing-tools" in accept_block:
        errors.append("accept-nightly.yml accept job must not pass --allow-missing-tools")
    if "scripts/accept.sh --release" not in accept_block:
        errors.append("accept-nightly.yml accept job must run scripts/accept.sh --release")
    return errors


def gh_json(args: list[str]) -> object:
    proc = subprocess.run(
        ["gh", *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args)} failed (rc={proc.returncode}): {proc.stderr.strip()}")
    return json.loads(proc.stdout)


class ReleaseGateSelfTests(unittest.TestCase):
    def _all_ok_jobs(self) -> list[dict]:
        return [
            {"name": n, "conclusion": "success"}
            for n in (*ALWAYS_REQUIRED_JOBS, BUILD_LINUX_JOB, *REQUIRED_JOBS)
        ]

    def test_no_runs_fails(self):
        ok, errs = check_runs([], lambda _id: [])
        self.assertFalse(ok)
        self.assertTrue(any("no ci.yml workflow runs" in e for e in errs), errs)

    def test_required_jobs_allow_skipped_when_changes_succeeded(self):
        jobs = self._all_ok_jobs()
        for job in jobs:
            if job["name"] in ("actionlint", "yamllint", "cargo-deny", "stylua · public-api-lint"):
                job["conclusion"] = "skipped"
        runs = [{"databaseId": 101, "status": "completed", "createdAt": "2026-11-15T10:00:00Z"}]
        ok, msgs = check_runs(runs, lambda _id: jobs)
        self.assertTrue(ok, msgs)

        # Docs-only commit where all conditional jobs (including build-linux-tests) were skipped.
        docs_only = [
            {"name": n, "conclusion": "success" if n in ALWAYS_REQUIRED_JOBS else "skipped"}
            for n in (*ALWAYS_REQUIRED_JOBS, BUILD_LINUX_JOB, *REQUIRED_JOBS)
        ]
        ok, msgs = check_runs(runs, lambda _id: docs_only)
        self.assertTrue(ok, msgs)

    def test_ci_failures_missing_jobs_or_skipped_changes_fail(self):
        skipped_changes = self._all_ok_jobs()
        skipped_changes[0]["conclusion"] = "skipped"

        missing_changes = [j for j in self._all_ok_jobs() if j["name"] != "changes"]
        missing_last = self._all_ok_jobs()[:-1]
        missing_deny = [j for j in self._all_ok_jobs() if j["name"] != "cargo-deny"]

        failed_build_skipped_shards = self._all_ok_jobs()
        for job in failed_build_skipped_shards:
            if job["name"] == BUILD_LINUX_JOB:
                job["conclusion"] = "failure"
            elif job["name"] in LINUX_SHARD_JOBS:
                job["conclusion"] = "skipped"

        succeeded_build_skipped_shard = self._all_ok_jobs()
        for job in succeeded_build_skipped_shard:
            if job["name"] == LINUX_SHARD_JOBS[0]:
                job["conclusion"] = "skipped"

        cases = [
            ("skipped_changes", 102, skipped_changes, lambda e: "changes" in e and "skipped" in e),
            ("missing_changes", 103, missing_changes, lambda e: "missing always-required CI job: changes" in e),
            ("missing_last_job", 104, missing_last, lambda e: "missing required CI job" in e),
            ("missing_deny", 105, missing_deny, lambda e: "missing required CI job: cargo-deny" in e),
            ("failed_build_skipped_shards", 106, failed_build_skipped_shards, lambda e: BUILD_LINUX_JOB in e and "failure" in e),
            ("succeeded_build_skipped_shard", 107, succeeded_build_skipped_shard, lambda e: LINUX_SHARD_JOBS[0] in e and "must be 'success'" in e),
        ]
        for label, run_id, jobs, predicate in cases:
            with self.subTest(case=label):
                runs = [{"databaseId": run_id, "status": "completed", "createdAt": "2026-11-15T10:00:00Z"}]
                ok, errs = check_runs(runs, lambda _id, j=jobs: j)
                self.assertFalse(ok)
                self.assertTrue(any(predicate(e) for e in errs), errs)

    def test_required_jobs_match_ci_workflow(self):
        ci_path = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows" / "ci.yml"
        if not ci_path.is_file():
            self.skipTest("ci.yml not present")
        ci_text = ci_path.read_text(encoding="utf-8")
        for job in (*ALWAYS_REQUIRED_JOBS, BUILD_LINUX_JOB, *REQUIRED_JOBS):
            if job.startswith("test (ubuntu-latest, shard "):
                self.assertIn("test (ubuntu-latest, shard ${{ matrix.shard }}/4)", ci_text)
                self.assertIn("shard: [1, 2, 3, 4]", ci_text)
            elif job == BUILD_LINUX_JOB:
                self.assertIn(f"name: {BUILD_LINUX_JOB}", ci_text)
            elif job.endswith(")"):
                prefix, os_part = job.rsplit(" (", 1)
                os_name = os_part[:-1]
                self.assertIn(f"{prefix} (${{{{ matrix.os }}}})", ci_text)
                self.assertIn(os_name, ci_text)
            else:
                self.assertIn(f"name: {job}", ci_text)

    def test_changes_classifier_contract(self):
        if os.name == "nt":
            self.skipTest("git shell contract runs on Unix")
        ci_path = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows" / "ci.yml"
        if not ci_path.is_file():
            self.skipTest("ci.yml not present")
        changes_block = _extract_job_block(ci_path.read_text(encoding="utf-8"), "changes")
        m = re.search(r"python3 - <<'PY'\n(.*?)\n\s+PY", changes_block, re.S)
        self.assertIsNotNone(m, "could not extract changes classifier python script from ci.yml")
        raw_lines = m.group(1).splitlines()
        indents = [len(line) - len(line.lstrip(" ")) for line in raw_lines if line.strip()]
        min_indent = min(indents)
        py_script = "\n".join(line[min_indent:] if len(line) >= min_indent else "" for line in raw_lines) + "\n"

        def run_classifier(repo: pathlib.Path, extra_env: dict[str, str]) -> dict[str, str]:
            out = repo / ".gh_out"
            out.write_text("", encoding="utf-8")
            env = dict(os.environ, GITHUB_OUTPUT=str(out), **extra_env)
            res = subprocess.run(
                [sys.executable, "-c", py_script],
                cwd=repo,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            parsed: dict[str, str] = {}
            for line in out.read_text(encoding="utf-8").splitlines():
                if "=" in line:
                    k, v = line.split("=", 1)
                    parsed[k] = v
            return parsed

        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            git_env = dict(
                os.environ,
                GIT_AUTHOR_NAME="Test",
                GIT_AUTHOR_EMAIL="test@example.invalid",
                GIT_COMMITTER_NAME="Test",
                GIT_COMMITTER_EMAIL="test@example.invalid",
            )
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            (repo / "README.md").write_text("init\n", encoding="utf-8")
            subprocess.run(["git", "add", "README.md"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "initial"], cwd=repo, env=git_env, check=True)
            c1 = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

            # 1. Orphan/initial commit on push must run ALL categories.
            orphan = run_classifier(repo, {"EVENT_NAME": "push", "PUSH_BEFORE_SHA": "0" * 40})
            self.assertTrue(all(v == "true" for v in orphan.values()), orphan)

            # 2. Pure docs/README/image change skips all heavy groups.
            (repo / "docs").mkdir()
            (repo / "docs" / "guide.md").write_text("hi\n", encoding="utf-8")
            (repo / "docs" / "shot.png").write_bytes(b"png")
            subprocess.run(["git", "add", "docs"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "docs"], cwd=repo, env=git_env, check=True)
            c2 = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            docs_out = run_classifier(repo, {"EVENT_NAME": "push", "PUSH_BEFORE_SHA": c1})
            self.assertEqual(docs_out["docs"], "true", docs_out)
            for heavy in ("yaml", "workflows", "rust", "lua", "deny", "scripts"):
                self.assertEqual(docs_out[heavy], "false", f"{heavy} should be false for docs-only: {docs_out}")

            # 3. Rust-only change enables rust only (not yaml/workflows/lua/deny).
            (repo / "crates" / "mare-tui" / "src").mkdir(parents=True)
            (repo / "crates" / "mare-tui" / "src" / "app.rs").write_text("// rs\n", encoding="utf-8")
            subprocess.run(["git", "add", "crates"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "tui"], cwd=repo, env=git_env, check=True)
            c3 = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            rust_out = run_classifier(repo, {"EVENT_NAME": "push", "PUSH_BEFORE_SHA": c2})
            self.assertEqual(rust_out["rust"], "true", rust_out)
            for skipped in ("yaml", "workflows", "lua", "deny", "scripts", "docs"):
                self.assertEqual(rust_out[skipped], "false", rust_out)

            # 4. Lua preset change enables rust + lua only.
            (repo / "crates" / "mare-plugin" / "preset" / "plugins").mkdir(parents=True)
            (repo / "crates" / "mare-plugin" / "preset" / "plugins" / "p.lua").write_text("return {}\n", encoding="utf-8")
            subprocess.run(["git", "add", "crates"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "lua"], cwd=repo, env=git_env, check=True)
            c4 = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            lua_out = run_classifier(repo, {"EVENT_NAME": "push", "PUSH_BEFORE_SHA": c3})
            self.assertEqual((lua_out["rust"], lua_out["lua"]), ("true", "true"), lua_out)
            self.assertEqual((lua_out["yaml"], lua_out["workflows"], lua_out["deny"]), ("false", "false", "false"), lua_out)

            # 5. Unknown file triggers fail-safe full CI.
            (repo / "unknown-root-file.bin").write_bytes(b"x")
            subprocess.run(["git", "add", "unknown-root-file.bin"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "unknown"], cwd=repo, env=git_env, check=True)
            unk_out = run_classifier(repo, {"EVENT_NAME": "push", "PUSH_BEFORE_SHA": c4})
            self.assertTrue(all(v == "true" for v in unk_out.values()), unk_out)

            # 6. workflow_dispatch always runs full CI.
            disp_out = run_classifier(repo, {"EVENT_NAME": "workflow_dispatch"})
            self.assertTrue(all(v == "true" for v in disp_out.values()), disp_out)

    def test_accept_job_requires_lua_tools_and_no_allow_missing_tools(self):
        accept_path = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows" / "accept-nightly.yml"
        if not accept_path.is_file():
            self.skipTest("accept-nightly.yml not present")
        ci_text = accept_path.read_text(encoding="utf-8")
        self.assertEqual(check_ci_workflow_accept_tools(ci_text), [])
        for tool in ("stylua", "ast-grep", "lua-language-server"):
            broken = ci_text.replace(tool, "removed-tool")
            errs = check_ci_workflow_accept_tools(broken)
            self.assertTrue(
                any(tool in e for e in errs),
                f"expected error mentioning missing {tool}, got: {errs}",
            )
        with_allow = ci_text.replace(
            "sh scripts/accept.sh --release",
            "sh scripts/accept.sh --release --allow-missing-tools",
        )
        errs = check_ci_workflow_accept_tools(with_allow)
        self.assertTrue(
            any("--allow-missing-tools" in e for e in errs),
            f"expected error mentioning --allow-missing-tools, got: {errs}",
        )

    def test_required_jobs_includes_workflow_linters(self):
        self.assertIn("actionlint", REQUIRED_JOBS)
        self.assertIn("yamllint", REQUIRED_JOBS)

    def test_wait_for_runs_times_out_when_run_never_completes(self):
        clock = [0.0]

        def fake_sleep(dt: float) -> None:
            clock[0] += dt

        pending = [{"databaseId": 201, "status": "in_progress", "createdAt": "2026-11-25T10:00:00Z"}]
        ok, errs = wait_for_runs(
            lambda: pending,
            lambda _id: [],
            wait_minutes=1.0,
            poll_interval=20.0,
            monotonic=lambda: clock[0],
            sleep=fake_sleep,
        )
        self.assertFalse(ok)
        self.assertTrue(any("timed out" in e and "201" in e for e in errs), errs)
        self.assertGreaterEqual(clock[0], 60.0)

    def test_wait_for_runs_passes_when_pending_run_completes_with_success(self):
        clock = [0.0]
        calls = [0]

        def fake_sleep(dt: float) -> None:
            clock[0] += dt

        def fake_list() -> list[dict]:
            calls[0] += 1
            if calls[0] < 3:
                return [{"databaseId": 202, "status": "in_progress", "createdAt": "2026-11-25T10:00:00Z"}]
            return [{"databaseId": 202, "status": "completed", "createdAt": "2026-11-25T10:00:00Z"}]

        ok, msgs = wait_for_runs(
            fake_list,
            lambda _id: self._all_ok_jobs(),
            wait_minutes=2.0,
            poll_interval=15.0,
            monotonic=lambda: clock[0],
            sleep=fake_sleep,
        )
        self.assertTrue(ok, msgs)
        self.assertEqual(calls[0], 3)

    def test_wait_for_runs_fails_immediately_when_completed_run_fails(self):
        clock = [0.0]
        sleeps = [0]

        def fake_sleep(dt: float) -> None:
            sleeps[0] += 1
            clock[0] += dt

        jobs = self._all_ok_jobs()
        jobs[0]["conclusion"] = "failure"
        completed = [{"databaseId": 203, "status": "completed", "createdAt": "2026-11-25T10:00:00Z"}]
        ok, errs = wait_for_runs(
            lambda: completed,
            lambda _id: jobs,
            wait_minutes=5.0,
            poll_interval=15.0,
            monotonic=lambda: clock[0],
            sleep=fake_sleep,
        )
        self.assertFalse(ok)
        self.assertEqual(sleeps[0], 0)
        self.assertTrue(any("failure" in e for e in errs), errs)

    def test_wait_for_runs_waits_for_delayed_run_creation(self):
        clock = [0.0]
        calls = [0]

        def fake_sleep(dt: float) -> None:
            clock[0] += dt

        def fake_list() -> list[dict]:
            calls[0] += 1
            if calls[0] == 1:
                return []
            return [{"databaseId": 204, "status": "completed", "createdAt": "2026-11-25T10:00:00Z"}]

        ok, msgs = wait_for_runs(
            fake_list,
            lambda _id: self._all_ok_jobs(),
            wait_minutes=1.0,
            poll_interval=10.0,
            monotonic=lambda: clock[0],
            sleep=fake_sleep,
        )
        self.assertTrue(ok, msgs)
        self.assertEqual(calls[0], 2)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--commit", help="Commit SHA to verify")
    parser.add_argument("--repo", help="GitHub repository (owner/name)")
    parser.add_argument(
        "--wait-minutes",
        type=float,
        default=0.0,
        help="Maximum minutes to poll while ci.yml runs are pending or not yet created (default: 0)",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=30.0,
        help="Seconds between polls when --wait-minutes > 0 (default: 30)",
    )
    parser.add_argument("--self-test", action="store_true", help="Run internal unit tests")
    args = parser.parse_args()

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReleaseGateSelfTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

    if not args.commit:
        print("--commit is required", file=sys.stderr)
        return 2

    accept_path = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows" / "accept-nightly.yml"
    if accept_path.is_file():
        wf_errors = check_ci_workflow_accept_tools(accept_path.read_text(encoding="utf-8"))
        if wf_errors:
            for err in wf_errors:
                print(f"release gate error: {err}", file=sys.stderr)
            return 1

    repo_args = ["--repo", args.repo] if args.repo else []
    try:
        ok, msgs = wait_for_runs(
            lambda: gh_json([
                "run", "list",
                "--workflow", "ci.yml",
                "--commit", args.commit,
                "--json", "databaseId,status,conclusion,createdAt",
                *repo_args,
            ]),
            lambda run_id: gh_json([
                "run", "view", str(run_id),
                "--json", "jobs",
                *repo_args,
            ]).get("jobs", []),
            wait_minutes=args.wait_minutes,
            poll_interval=args.poll_interval,
        )
    except RuntimeError as exc:
        print(f"release gate error: {exc}", file=sys.stderr)
        return 1

    for m in msgs:
        print(m, file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
