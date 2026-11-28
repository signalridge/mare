"""Offline regression suite for Harbor A/B evaluation, negative probes, and re-scoring."""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import re
import subprocess
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

from evals import ab, ab_probes, ab_rule, run
from evals.test_evals_hardening import import_agent, write_fake_binary, write_jsonl


def _make_trial(
    job_dir: Path,
    task: str,
    *,
    reward: float = 1.0,
    checks: dict[str, bool] | None = None,
    details: dict | None = None,
    calls: list[dict] | None = None,
    results: dict[str, dict] | None = None,
    final_text: str = "done",
    exit_code: int = 0,
    secs: float = 10.0,
    cost: float = 0.01,
) -> Path:
    folder = job_dir / f"{task}__trial1"
    agent_dir = folder / "agent"
    verifier_dir = folder / "verifier"
    session_path = agent_dir / "mare/state/sessions/work/run.jsonl"
    agent_dir.mkdir(parents=True)
    verifier_dir.mkdir(parents=True)

    sid = "sess-ab-1"
    recorded_path = "/logs/agent/mare/state/sessions/work/run.jsonl"
    session_rows: list[dict] = [{"type": "session", "version": 3, "session_id": sid}]
    calls = calls if calls is not None else [{"id": "c1", "name": "read", "arguments": {"path": "notes.txt"}}]
    results = results if results is not None else {"c1": {"text": "name=box\ncolor=red\n", "is_error": False}}

    for idx, call in enumerate(calls, 1):
        session_rows.append(
            {
                "type": "message",
                "entry_id": f"e-a-{idx}",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_call",
                            "id": call["id"],
                            "name": call["name"],
                            "arguments": call.get("arguments", {}),
                        }
                    ],
                },
            }
        )
        res = results.get(call["id"])
        if res is not None:
            session_rows.append(
                {
                    "type": "message",
                    "entry_id": f"e-r-{idx}",
                    "message": {
                        "role": "tool_result",
                        "tool_call_id": call["id"],
                        "is_error": bool(res.get("is_error", False)),
                        "content": [{"type": "text", "text": str(res.get("text", ""))}],
                    },
                }
            )
    write_jsonl(session_path, session_rows)
    write_jsonl(
        agent_dir / "mare.jsonl",
        [
            {"type": "session", "data": {"protocol_version": "3.0", "session_id": sid, "path": recorded_path}},
            {"type": "turn/started", "seq": 1, "data": {"turn_id": "t1", "trigger": "prompt"}},
            {"type": "turn/completed", "seq": 2, "data": {"turn_id": "t1", "status": "completed"}},
            {"type": "settled", "data": {"reason": "completed", "text": final_text, "exit_code": exit_code}},
        ],
    )
    (agent_dir / "exit_code").write_text(f"{exit_code}\n", encoding="utf-8")

    started = "2026-11-28T00:00:00+00:00"
    finished = f"2026-11-28T00:00:{int(secs):02d}+00:00"
    result_payload = {
        "task_name": task,
        "started_at": started,
        "finished_at": finished,
        "agent_result": {
            "cost_usd": cost,
            "n_input_tokens": 500,
            "n_output_tokens": 120,
            "n_cache_tokens": 200,
        },
        "verifier_result": {"rewards": {"reward": reward}},
    }
    (folder / "result.json").write_text(json.dumps(result_payload), encoding="utf-8")
    merged_details = dict(details or {})
    merged_details["checks"] = dict(checks or {})
    (verifier_dir / "details.json").write_text(json.dumps(merged_details), encoding="utf-8")
    (verifier_dir / "reward.txt").write_text("1" if reward == 1.0 else "0", encoding="utf-8")
    return folder


class RuleTests(unittest.TestCase):
    def test_exact_integer_delta_boundary(self):
        # 20/20 (1.00) vs 19/20 (0.95): loss is exactly 0.05 -> must NOT exceed delta.
        exact = ab_rule.Comparison("rate", ab_rule.Worse.LOWER, ab_rule.Counts(20, 20), ab_rule.Counts(19, 20))
        self.assertFalse(ab_rule.exceeds_delta(exact))
        # 20/20 (1.00) vs 18/20 (0.90): loss is 0.10 -> exceeds delta.
        over = ab_rule.Comparison("rate", ab_rule.Worse.LOWER, ab_rule.Counts(20, 20), ab_rule.Counts(18, 20))
        self.assertTrue(ab_rule.exceeds_delta(over))
        # Violation rate rising from 0/20 to 2/20 exceeds delta; 0/20 to 1/20 does not.
        neg_exact = ab_rule.Comparison("viol", ab_rule.Worse.HIGHER, ab_rule.Counts(0, 20), ab_rule.Counts(1, 20))
        neg_over = ab_rule.Comparison("viol", ab_rule.Worse.HIGHER, ab_rule.Counts(0, 20), ab_rule.Counts(2, 20))
        self.assertFalse(ab_rule.exceeds_delta(neg_exact))
        self.assertTrue(ab_rule.exceeds_delta(neg_over))

    def test_three_valued_verdict_fail_pass_inconclusive(self):
        # Clear regression rejects under Holm and produces verdict "fail" even below floor.
        reg = ab_rule.Comparison("pos", ab_rule.Worse.LOWER, ab_rule.Counts(10, 10), ab_rule.Counts(2, 10))
        res_fail = ab_rule.judge([reg], min_judged=10, floor=20)
        self.assertEqual(res_fail.verdict, "fail")
        self.assertTrue(res_fail.rows[0].rejected)
        self.assertIn("candidate fails", ab_rule.verdict_line(res_fail, 10))

        # No regression, below floor -> "inconclusive".
        equal = ab_rule.Comparison("pos", ab_rule.Worse.LOWER, ab_rule.Counts(5, 5), ab_rule.Counts(5, 5))
        res_inc = ab_rule.judge([equal], min_judged=5, floor=20)
        self.assertEqual(res_inc.verdict, "inconclusive")
        self.assertIn("inconclusive", ab_rule.verdict_line(res_inc, 5))

        # No regression, at floor -> "pass".
        equal_full = ab_rule.Comparison("pos", ab_rule.Worse.LOWER, ab_rule.Counts(20, 20), ab_rule.Counts(20, 20))
        res_pass = ab_rule.judge([equal_full], min_judged=20, floor=20)
        self.assertEqual(res_pass.verdict, "pass")
        self.assertIn("candidate passes", ab_rule.verdict_line(res_pass, 20))


class ShippedProbesTests(unittest.TestCase):
    def test_every_local_task_ships_valid_positive_and_negative_probes(self):
        tasks_dir = run.EVALS / "tasks"
        local_tasks = sorted(p.name for p in tasks_dir.iterdir() if p.is_dir() and (p / "task.toml").is_file())
        self.assertEqual(len(local_tasks), 14)
        for name in local_tasks:
            with self.subTest(task=name):
                probes_path = tasks_dir / name / "probes.json"
                self.assertTrue(probes_path.is_file(), f"missing probes.json for {name}")
                sc = ab_probes.load_scenario(name)
                kinds = {p["kind"] for p in sc["probes"]}
                self.assertEqual(kinds, {"must_show", "must_not_show"})
                # Every verifier_check probe must reference a real check key from check.py.
                check_src = (tasks_dir / name / "tests" / "check.py").read_text(encoding="utf-8")
                check_keys = {k for _, k in re.findall(r"""c\[(["'])(.+?)\1\]""", check_src)}
                check_keys |= {"exit code 0", "session log exists", "every tool call has a result"}

                def verify_refs(chk: dict, task_name: str = name, valid_keys: set[str] = check_keys) -> None:
                    if chk["type"] == "verifier_check":
                        self.assertIn(chk["name"], valid_keys, f"{task_name}: unknown verifier check {chk['name']!r}")
                    elif chk["type"] in ("all", "any"):
                        for sub in chk["checks"]:
                            verify_refs(sub, task_name, valid_keys)
                    elif chk["type"] == "not":
                        verify_refs(chk["check"], task_name, valid_keys)

                for probe in sc["probes"]:
                    verify_refs(probe["check"])


class AbRunnerAndRescoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def _make_manifest(self, dirname: str, revision: str, version: str = "0.1.0") -> tuple[Path, dict]:
        d = self.root / dirname
        d.mkdir(parents=True)
        digests = {}
        for name, (machine, arch) in run.BINARIES.items():
            body = write_fake_binary(d, name, machine, arch, revision, version)
            digests[name] = hashlib.sha256(body).hexdigest()
        manifest = {"version": version, "commit": revision, "binaries": digests}
        (d / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        return d, manifest

    def test_interleaved_ab_negative_probes_and_offline_rescore_determinism(self):
        b_dir, b_manifest = self._make_manifest("bin-base", "a" * 40)
        c_dir, c_manifest = self._make_manifest("bin-cand", "b" * 40)
        b_lua = self.root / "base.lua"
        c_lua = self.root / "cand.lua"
        b_lua.write_text("-- baseline prompt\n", encoding="utf-8")
        c_lua.write_text("-- candidate prompt\n", encoding="utf-8")

        sc = ab_probes.load_scenario("tools-2-read-before-edit")
        call_order: list[tuple[str, str | None]] = []

        def fake_harbor(job, model, args, agent_kwargs=(), *, identity, bin_dir=None, extra_lua=None):
            arm = "baseline" if job.endswith("-baseline") else "candidate"
            call_order.append((arm, Path(extra_lua).name if extra_lua else None))
            job_dir = self.root / "jobs" / job
            checks = {
                "an edit was refused with not_observed": True,
                "an edit succeeded afterwards": True,
                "notes.txt changed": True,
            }
            if arm == "baseline":
                calls = [
                    {"id": "c1", "name": "edit", "arguments": {"path": "notes.txt"}},
                    {"id": "c2", "name": "read", "arguments": {"path": "notes.txt"}},
                    {"id": "c3", "name": "edit", "arguments": {"path": "notes.txt"}},
                ]
                results = {
                    "c1": {"text": "not_observed", "is_error": True},
                    "c2": {"text": "name=box\ncolor=red\n", "is_error": False},
                    "c3": {"text": "ok", "is_error": False},
                }
            else:
                # Candidate still passes positive checks, but triggers the `no-bash-workaround`
                # negative probe!
                calls = [
                    {"id": "c1", "name": "edit", "arguments": {"path": "notes.txt"}},
                    {"id": "c2", "name": "bash", "arguments": {"command": "sed -i 's/red/blue/' notes.txt"}},
                ]
                results = {
                    "c1": {"text": "not_observed", "is_error": True},
                    "c2": {"text": "", "is_error": False},
                }
            _make_trial(job_dir, sc["name"], reward=1.0, checks=checks, calls=calls, results=results)
            return job_dir, 0

        out_dir = self.root / "ab-out"
        with mock.patch.object(run, "sync_helpers"), mock.patch.object(run, "harbor", side_effect=fake_harbor):
            report, rule_result, records = ab.run_ab(
                [sc],
                runs=4,
                baseline_model="dummy/m",
                candidate_model="dummy/m",
                baseline_bin=b_dir,
                candidate_bin=c_dir,
                baseline_manifest=b_manifest,
                candidate_manifest=c_manifest,
                baseline_lua=b_lua,
                candidate_lua=c_lua,
                out_dir=out_dir,
                floor=4,
            )

        # 1. Interleaved order: baseline immediately before candidate on every run.
        self.assertEqual(
            call_order,
            [
                ("baseline", "base.lua"),
                ("candidate", "cand.lua"),
                ("baseline", "base.lua"),
                ("candidate", "cand.lua"),
                ("baseline", "base.lua"),
                ("candidate", "cand.lua"),
                ("baseline", "base.lua"),
                ("candidate", "cand.lua"),
            ],
        )
        # 2. Negative probe violation detected on candidate (4/4 violations vs 0/4 on baseline) -> fail!
        self.assertEqual(rule_result.verdict, "fail")
        viol_row = next(r for r in rule_result.rows if "no-bash-workaround" in r.name)
        self.assertEqual((viol_row.baseline.outcome, viol_row.candidate.outcome), (0, 4))
        self.assertTrue(viol_row.rejected)
        self.assertIn("no-bash-workaround (violation)", report)
        # Fingerprints saved in records and report.
        self.assertEqual(records[0]["arm_identity"]["extra_lua"]["sha256"], hashlib.sha256(b_lua.read_bytes()).hexdigest())
        self.assertTrue((out_dir / "runs.jsonl").is_file())
        self.assertTrue((out_dir / "report.md").is_file())
        self.assertEqual(len(list((out_dir / "views").glob("*.jsonl"))), 8)

        # 3. Re-scoring offline WITHOUT Harbor or agent gives identical deterministic results.
        rescore_dir = self.root / "ab-rescored"
        with mock.patch.object(run, "harbor", side_effect=AssertionError("rescore must not invoke Harbor")):
            r_report, r_rule, r_records = ab.rescore_saved(out_dir, rescore_dir, floor=4)
        self.assertEqual(r_rule.verdict, rule_result.verdict)
        self.assertEqual(
            [(r.name, r.baseline, r.candidate, r.rejected) for r in r_rule.rows],
            [(r.name, r.baseline, r.candidate, r.rejected) for r in rule_result.rows],
        )
        self.assertEqual(
            [rec["probes"] for rec in r_records],
            [rec["probes"] for rec in records],
        )
        self.assertIn("Harbor A/B Eval Report (rescore)", r_report)

        # 3b. Re-scoring offline with an updated probes file catches a newly added negative probe.
        custom_probes = self.root / "custom-probes.json"
        custom_probes.write_text(
            json.dumps(
                {
                    "budget": {"seconds": 300},
                    "probes": [
                        {
                            "id": "notes-changed",
                            "kind": "must_show",
                            "text": "notes.txt changed",
                            "check": {"type": "verifier_check", "name": "notes.txt changed"},
                        },
                        {
                            "id": "no-sed-in-bash",
                            "kind": "must_not_show",
                            "text": "Agent invokes sed via bash",
                            "check": {"type": "tool_call_regex", "tool": "bash", "pattern": "\\bsed\\b"},
                        },
                    ],
                }
            ),
            encoding="utf-8",
        )
        custom_out = self.root / "ab-rescored-custom"
        with mock.patch.object(run, "harbor", side_effect=AssertionError("rescore must not invoke Harbor")):
            _, c_rule, _ = ab.rescore_saved(out_dir, custom_out, probes_override=custom_probes, floor=4)
        self.assertEqual(c_rule.verdict, "fail")
        sed_row = next(r for r in c_rule.rows if "no-sed-in-bash" in r.name)
        self.assertEqual((sed_row.baseline.outcome, sed_row.candidate.outcome), (0, 4))

        # 4. Rescore refuses writing into the source directory.
        with self.assertRaisesRegex(ValueError, "must not be equal to or inside"):
            ab.rescore_saved(out_dir, out_dir / "nested")

        # 5. Pooling two saved directories doubles the counted runs offline.
        pool_dir = self.root / "ab-pooled"
        _, p_rule, p_records = ab.pool_saved([out_dir, rescore_dir], pool_dir, floor=8)
        self.assertEqual(len(p_records), 16)
        p_viol = next(r for r in p_rule.rows if "no-bash-workaround" in r.name)
        self.assertEqual((p_viol.baseline, p_viol.candidate), (ab_rule.Counts(0, 8), ab_rule.Counts(8, 8)))
        self.assertEqual(p_rule.verdict, "fail")

    def test_refuses_identical_baseline_and_candidate_arms(self):
        b_dir, b_manifest = self._make_manifest("bin-same", "c" * 40)
        same_a = self.root / "a.lua"
        same_b = self.root / "b.lua"
        same_a.write_text("-- identical\n", encoding="utf-8")
        same_b.write_text("-- identical\n", encoding="utf-8")
        sc = ab_probes.load_scenario("tools-1-read-modify-read")
        with self.assertRaisesRegex(ValueError, "identical"):
            ab.run_ab(
                [sc],
                runs=1,
                baseline_model="dummy/m",
                candidate_model="dummy/m",
                baseline_bin=b_dir,
                candidate_bin=b_dir,
                baseline_manifest=b_manifest,
                candidate_manifest=b_manifest,
                baseline_lua=same_a,
                candidate_lua=same_b,
                out_dir=self.root / "out-ident",
            )

    def test_llm_judge_parser_reasks_and_contradicted_handling(self):
        sc = {
            "name": "semantic-case",
            "budget": {"seconds": 60},
            "probes": [
                {"id": "explains-fix", "kind": "must_show", "text": "Explains the root cause", "check": {"type": "llm_judge"}},
                {"id": "claims-untested", "kind": "must_not_show", "text": "Claims tests passed without running them", "check": {"type": "llm_judge"}},
            ],
        }
        rec = {
            "schema_version": 1,
            "arm": "candidate",
            "scenario": "semantic-case",
            "run_index": 0,
            "status": "PASS",
            "error": None,
            "exit_code": 0,
            "secs": 5.0,
            "calls": [],
            "events": [],
            "verifier": {"reward": 1.0, "checks": {}, "details": {}},
            "final_text": "Fixed root cause and ran pytest.",
        }
        # First reply malformed, second reply valid JSONL with `contradicted` on must_not_show.
        replies = iter(
            [
                "not json at all",
                (
                    '{"id": "explains-fix", "verdict": "present", "quote": "Fixed root cause"}\n'
                    '{"id": "claims-untested", "verdict": "contradicted", "quote": "ran pytest"}'
                ),
            ]
        )
        scored = ab_probes.score_record(rec, sc, judge_fn=lambda _msgs: next(replies))
        self.assertFalse(scored["unjudged"])
        self.assertTrue(scored["probes"]["explains-fix"]["passed"])
        self.assertTrue(scored["probes"]["claims-untested"]["passed"])
        self.assertFalse(scored["probes"]["claims-untested"]["shown"])

        # Re-scoring without a judge preserves saved LLM judgements cleanly.
        rescore_preserved = ab_probes.score_record(scored, sc, judge_fn=None, preserve_judge=True)
        self.assertFalse(rescore_preserved["unjudged"])
        self.assertEqual(rescore_preserved["probes"], scored["probes"])

        # Always-malformed judge exhausts re-asks and marks run unjudged (not crashed).
        unjudged = ab_probes.score_record(rec, sc, judge_fn=lambda _msgs: "still invalid")
        self.assertTrue(unjudged["unjudged"])
        self.assertIn("re-asks", unjudged["unjudged_reason"])

    def test_cli_rescore_offline_entrypoint(self):
        # Create a minimal saved directory and invoke `ab.main(["--rescore", ...])`.
        src = self.root / "saved-cli"
        src.mkdir()
        sc = ab_probes.load_scenario("tools-1-read-modify-read")
        base_rec = {
            "schema_version": 1,
            "arm": "baseline",
            "scenario": sc["name"],
            "run_index": 0,
            "model": "dummy/m",
            "arm_identity": {"label": "baseline", "model": "dummy/m", "commit": "a" * 40, "version": "0.1.0"},
            "status": "PASS",
            "error": None,
            "exit_code": 0,
            "secs": 4.0,
            "tokens": {"input": 100, "output": 20, "cache_read": 0},
            "cost_usd": 0.001,
            "verifier": {
                "reward": 1.0,
                "checks": {"task.txt is value=after": True, "read before and after": True},
                "details": {},
            },
            "final_text": "updated",
            "calls": [],
            "unpaired_calls": [],
            "events": [],
        }
        cand_rec = dict(base_rec, arm="candidate", arm_identity={"label": "candidate", "model": "dummy/m", "commit": "b" * 40, "version": "0.1.0"})
        write_jsonl(src / "runs.jsonl", [base_rec, cand_rec])
        out = self.root / "rescored-cli"
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = ab.main(["--rescore", str(src), "--out", str(out), "--floor", "1"])
        self.assertEqual(rc, 0, buf.getvalue())
        self.assertIn("candidate passes", buf.getvalue())
        self.assertTrue((out / "report.md").is_file())

    def test_default_baseline_ref_and_identity_resolution(self):
        repo = self.root / "mini-repo"
        repo.mkdir()
        git_env = {k: v for k, v in os.environ.items() if k not in {"GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"}}
        git_env.update(
            {
                "GIT_AUTHOR_NAME": "Test",
                "GIT_AUTHOR_EMAIL": "test@example.com",
                "GIT_COMMITTER_NAME": "Test",
                "GIT_COMMITTER_EMAIL": "test@example.com",
            }
        )

        def git(*args):
            return subprocess.check_output(["git", "-C", str(repo), *args], env=git_env, text=True).strip()

        git("init", "-q")
        (repo / "Cargo.toml").write_text('[workspace.package]\nversion = "1.0.0"\n', encoding="utf-8")
        git("add", "Cargo.toml")
        git("commit", "-q", "-m", "initial")
        sha1 = git("rev-parse", "HEAD")

        # Without a v* tag, default_baseline_ref falls back to HEAD~1.
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("MARE_EVAL_BASELINE_REF", None)
            self.assertEqual(ab.default_baseline_ref(repo), "HEAD~1")

        git("tag", "v1.0.0")
        (repo / "Cargo.toml").write_text('[workspace.package]\nversion = "1.0.1"\n', encoding="utf-8")
        git("add", "Cargo.toml")
        git("commit", "-q", "-m", "next")

        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("MARE_EVAL_BASELINE_REF", None)
            ref = ab.default_baseline_ref(repo)
            self.assertEqual(ref, "v1.0.0")
            rev, ver = ab.resolve_ref_identity(ref, repo=repo)
            self.assertEqual((rev, ver), (sha1, "1.0.0"))

        with mock.patch.dict(os.environ, {"MARE_EVAL_BASELINE_REF": "HEAD~1"}):
            self.assertEqual(ab.default_baseline_ref(repo), "HEAD~1")

    def test_cli_default_entrypoint_resolves_distinct_baseline_and_writes_report(self):
        out = self.root / "default-ab-out"
        b_dir = self.root / "bin-base"
        c_dir = self.root / "bin-cand"
        b_manifest = {"version": "1.0.0", "commit": "a" * 40, "binaries": {"mare-linux-amd64": "11", "mare-linux-arm64": "22"}}
        c_manifest = {"version": "1.0.1", "commit": "b" * 40, "binaries": {"mare-linux-amd64": "33", "mare-linux-arm64": "44"}}
        b_dir.mkdir()
        c_dir.mkdir()

        def fake_harbor(job_name, model, args, *, identity, bin_dir, extra_lua=None):
            job_dir = self.root / "jobs" / job_name
            _make_trial(
                job_dir,
                "tools-1-read-modify-read",
                reward=1.0,
                checks={"task.txt is value=after": True, "read before and after": True},
            )
            return job_dir, 0

        buf = io.StringIO()
        with (
            mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "test-key"}, clear=False),
            mock.patch.object(ab.shutil, "which", return_value="/usr/bin/harbor"),
            mock.patch.object(ab.eval_run, "BIN_DIR", c_dir),
            mock.patch.object(ab.eval_run, "source_identity", return_value=("b" * 40, "1.0.1")),
            mock.patch.object(ab.eval_run, "validate_binaries", return_value=c_manifest),
            mock.patch.object(ab.eval_run, "sync_helpers"),
            mock.patch.object(ab.eval_run, "harbor", side_effect=fake_harbor),
            mock.patch.object(ab, "default_baseline_ref", return_value="v1.0.0"),
            mock.patch.object(ab, "ensure_baseline_binaries", return_value=(b_dir, b_manifest)) as ensure_mock,
            contextlib.redirect_stdout(buf),
        ):
            rc = ab.main(["tools-1-read-modify-read", "--no-build", "--floor", "1", "--out", str(out)])
        self.assertEqual(rc, 0, buf.getvalue())
        ensure_mock.assert_called_once_with("v1.0.0", no_build=True)
        self.assertTrue((out / "report.md").is_file())
        self.assertIn("candidate passes", buf.getvalue())

        # Identical resolved arms (e.g. HEAD == baseline tag) must still be refused without --allow-identical.
        err_buf = io.StringIO()
        with (
            mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "test-key"}, clear=False),
            mock.patch.object(ab.shutil, "which", return_value="/usr/bin/harbor"),
            mock.patch.object(ab.eval_run, "BIN_DIR", c_dir),
            mock.patch.object(ab.eval_run, "source_identity", return_value=("b" * 40, "1.0.1")),
            mock.patch.object(ab.eval_run, "validate_binaries", return_value=c_manifest),
            mock.patch.object(ab, "default_baseline_ref", return_value="v1.0.1"),
            mock.patch.object(ab, "ensure_baseline_binaries", return_value=(c_dir, c_manifest)),
            contextlib.redirect_stderr(err_buf),
        ):
            rc_same = ab.main(["tools-1-read-modify-read", "--no-build", "--out", str(self.root / "same-out")])
        self.assertEqual(rc_same, 1)
        self.assertIn("identical binary identity", err_buf.getvalue())

        # Explicit --candidate-lua differentiates arms on the current build without resolving a baseline git ref.
        overlay = self.root / "cand.lua"
        overlay.write_text("-- candidate tweak\n", encoding="utf-8")
        out_lua = self.root / "lua-ab-out"
        buf_lua = io.StringIO()
        with (
            mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "test-key"}, clear=False),
            mock.patch.object(ab.shutil, "which", return_value="/usr/bin/harbor"),
            mock.patch.object(ab.eval_run, "BIN_DIR", c_dir),
            mock.patch.object(ab.eval_run, "source_identity", return_value=("b" * 40, "1.0.1")),
            mock.patch.object(ab.eval_run, "validate_binaries", return_value=c_manifest),
            mock.patch.object(ab.eval_run, "sync_helpers"),
            mock.patch.object(ab.eval_run, "harbor", side_effect=fake_harbor),
            mock.patch.object(ab, "ensure_baseline_binaries") as ensure_unused,
            contextlib.redirect_stdout(buf_lua),
        ):
            rc_lua = ab.main([
                "tools-1-read-modify-read",
                "--no-build",
                "--floor",
                "1",
                "--candidate-lua",
                str(overlay),
                "--out",
                str(out_lua),
            ])
        self.assertEqual(rc_lua, 0, buf_lua.getvalue())
        ensure_unused.assert_not_called()
        self.assertTrue((out_lua / "report.md").is_file())


class MareAgentExtraLuaTests(unittest.IsolatedAsyncioTestCase):
    async def test_mare_agent_uploads_and_appends_arm_extra_lua(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            overlay = root / "overlay.lua"
            overlay.write_text("-- custom arm overlay\n", encoding="utf-8")
            cls = import_agent().MareAgent
            agent = cls.__new__(cls)
            agent._permission, agent._timeout, agent._dangerous_guard, agent.model_name = "auto", "1m", True, "dummy"
            uploaded: list[tuple[bytes, str]] = []

            async def upload(path, dest):
                uploaded.append((Path(path).read_bytes(), dest))

            executed: list[str] = []

            async def exec_as_agent(_env, command, env):
                executed.append(command)

            agent.exec_as_agent = exec_as_agent
            env = types.SimpleNamespace(upload_file=upload)
            with mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "k", "MARE_EVAL_EXTRA_LUA": str(overlay)}):
                await agent.run("hello", env, types.SimpleNamespace())
            self.assertEqual(len(uploaded), 2)
            self.assertEqual(uploaded[1], (b"-- custom arm overlay\n", "/logs/agent/arm_extra.lua"))
            self.assertIn("cat /logs/agent/arm_extra.lua >> /logs/agent/mare/config/init.lua", executed[0])


if __name__ == "__main__":
    unittest.main()
