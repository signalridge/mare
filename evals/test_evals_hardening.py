"""Offline regressions for the Harbor runner and its 3.0 evidence reader."""

import contextlib
import hashlib
import importlib
import io
import itertools
import json
import os
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

from evals import run
from evals.lib import mare_log

def write_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")

def write_fake_binary(directory, name, machine, arch, revision, version):
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    header[18:20] = machine.to_bytes(2, "little")
    body = bytes(header) + f"{version}\ncommit: {revision[:12]}\ndate: 2026-11-22\ntarget: {arch}-unknown-linux-musl".encode()
    (directory / name).write_bytes(body)
    return body

def add_result(root, task, *, reward=1.0, exit_code="0", error=None, suffix="one"):
    folder = root / f"{task}__{suffix}"
    (folder / "agent").mkdir(parents=True)
    result = {"task_name": task, "agent_result": {"cost_usd": 0.0}}
    if reward is not None:
        result["verifier_result"] = {"rewards": {"reward": reward}}
    if error:
        result["exception_info"] = {"exception_type": error}
    (folder / "result.json").write_text(json.dumps(result), encoding="utf-8")
    if exit_code is not None:
        (folder / "agent" / "exit_code").write_text(exit_code, encoding="utf-8")
    return folder

class EvidenceFixture:
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.old_logs = mare_log.LOGS
        mare_log.LOGS = str(self.root)
        self.addCleanup(setattr, mare_log, "LOGS", self.old_logs)
        self.recorded_path = "/logs/agent/mare/state/sessions/work/run.jsonl"
        self.session_path = self.root / "mare/state/sessions/work/run.jsonl"
        self.event_path = self.root / "mare.jsonl"
        self.header = {"type": "session", "version": 3, "session_id": "session-123"}
        self.start = {"type": "session", "data": {"protocol_version": "3.0", "session_id": "session-123", "path": self.recorded_path}}
        self.finish = {"type": "settled", "data": {"reason": "completed", "text": "done", "exit_code": 0}}
        self.turn_start = {"type": "turn/started", "data": {"turn_id": "turn-1", "trigger": "prompt"}}
        self.turn_end = {"type": "turn/completed", "data": {"turn_id": "turn-1", "status": "completed"}}
        self.assistant = {"type": "message", "entry_id": "entry-a", "message": {
            "role": "assistant", "content": [{"type": "tool_call", "id": "call-1", "name": "read"}]}}
        self.result = {"type": "message", "entry_id": "entry-b", "message": {
            "role": "tool_result", "tool_call_id": "call-1", "content": [{"type": "text", "text": "ok"}]}}
        self.write_valid()

    def write_valid(self, session=None, events=None):
        write_jsonl(self.session_path, session if session is not None else [self.header, self.assistant, self.result])
        rows = events if events is not None else [self.start, self.turn_start, self.turn_end, self.finish]
        rows = [dict(row, seq=row.get("seq", i)) if row["type"] not in ("session", "settled", "turn.failed") else row
                for i, row in enumerate(rows)]
        write_jsonl(self.event_path, rows)
        (self.root / "exit_code").write_text("0\n", encoding="utf-8")

class EvidenceReaderTests(EvidenceFixture, unittest.TestCase):
    # Match real protocol records from the headless JSONL event stream.
    def test_pathless_session_binds_by_id_not_newest_file(self):
        start = dict(self.start, data={"protocol_version": "3.0", "session_id": "session-123"})
        self.write_valid(events=[start, self.turn_start, self.turn_end, self.finish])
        other = self.root / "mare/state/sessions/other/newer.jsonl"
        write_jsonl(other, [{"type": "session", "version": 3, "session_id": "other"}])
        os.utime(other, (2_000_000_000, 2_000_000_000))
        self.assertEqual(mare_log.session_file(), self.session_path)
        self.assertEqual([c["id"] for c in mare_log.Log().calls], ["call-1"])
    def test_pathless_session_requires_exactly_one_matching_header(self):
        start = dict(self.start, data={"protocol_version": "3.0", "session_id": "session-123"})
        self.write_valid(events=[start, self.turn_start, self.turn_end, self.finish])
        duplicate = self.root / "mare/state/sessions/other/duplicate.jsonl"
        write_jsonl(duplicate, [self.header])
        with self.assertRaisesRegex(ValueError, "exactly one.*session"):
            mare_log.session_file()
        duplicate.unlink()
        self.session_path.unlink()
        with self.assertRaisesRegex(ValueError, "exactly one.*session"):
            mare_log.session_file()

    def test_pathless_scan_rejects_an_outside_symlink_and_invalid_paths(self):
        outside = self.root / "outside.jsonl"
        write_jsonl(outside, [self.header])
        self.session_path.unlink()
        (self.session_path.parent / "escape.jsonl").symlink_to(outside)
        start = dict(self.start, data={"protocol_version": "3.0", "session_id": "session-123", "path": None})
        write_jsonl(self.event_path, [start, dict(self.turn_start, seq=1), dict(self.turn_end, seq=2), self.finish])
        with self.assertRaisesRegex(ValueError, "exactly one.*session"):
            mare_log.session_file()
        for invalid in ("", 1, False):
            with self.subTest(path=invalid):
                bad_start = dict(start, data=dict(start["data"], path=invalid))
                write_jsonl(self.event_path, [bad_start, dict(self.turn_start, seq=1), dict(self.turn_end, seq=2), self.finish])
                with self.assertRaisesRegex(ValueError, "session id or path"):
                    mare_log.events()

    def test_bound_session_wins_over_newer_unrelated_session(self):
        other = self.root / "mare/state/sessions/other/unrelated.jsonl"
        write_jsonl(other, [{"type": "session", "version": 3, "session_id": "not-this-run"}])
        os.utime(other, (2_000_000_000, 2_000_000_000))
        self.assertEqual(Path(mare_log.session_file()), self.session_path)
        parsed = mare_log.Log()
        self.assertEqual([c["id"] for c in parsed.calls], ["call-1"])
        self.assertEqual(parsed.result(parsed.calls[0])["text"], "ok")

    def test_rejects_wrong_session_header_and_path_escape(self):
        self.write_valid(session=[dict(self.header, session_id="other"), self.assistant, self.result])
        with self.assertRaisesRegex(ValueError, "session"):
            mare_log.Log()
        with self.assertRaisesRegex(ValueError, "session"):
            mare_log.session_file()
        escaped = dict(self.start, data=dict(self.start["data"], path="/logs/agent/mare/state/sessions/../../other.jsonl"))
        self.write_valid(events=[escaped, self.turn_start, self.turn_end, self.finish])
        with self.assertRaisesRegex(ValueError, "path|session"):
            mare_log.Log()

    def test_rejects_malformed_truncated_and_lost_event_stream(self):
        bad_streams = [
            json.dumps(self.start) + "\n{broken\n" + json.dumps(self.finish) + "\n",
            json.dumps(self.start) + "\n" + json.dumps(self.finish),
            "\n".join(json.dumps(x) for x in [self.start, {"type": "dropped", "data": {"count": 1}}, self.finish]) + "\n",
            "\n".join(json.dumps(x) for x in [self.start, {"type": "lagged", "data": {}}, self.finish]) + "\n",
            json.dumps(self.start) + "\n",
        ]
        for data in bad_streams:
            with self.subTest(stream=data[:45]):
                self.event_path.write_text(data, encoding="utf-8")
                with self.assertRaises(ValueError):
                    mare_log.events()

    def test_allow_incomplete_tolerates_loss_markers_and_rejects_cross_session_data_id(self):
        settled = {"type": "settled", "data": {"reason": "timeout", "text": "", "exit_code": 4}}
        for marker in (
            {"type": "dropped", "data": {"session_id": "session-123", "session_epoch": "e1", "count": 1}},
            {"type": "lagged", "data": {"from_seq": 2, "to_seq": 4}},
            {"type": "stream.failed", "data": {"error": "slow consumer"}},
        ):
            with self.subTest(marker=marker["type"]):
                rows = [self.start, dict(self.turn_start, seq=1),
                        {"type": "item/started", "seq": 2, "data": {"turn_id": "turn-1", "item_id": "call-1"}}, marker,
                        {"type": "usage/changed", "seq": 3, "data": {"input": 10, "cache_read": 2, "output": 5, "cost": 0.01}},
                        {"type": "turn.failed", "data": {"error": "timeout"}}, settled]
                write_jsonl(self.event_path, rows)
                with self.assertRaises(ValueError):
                    mare_log.events()
                self.assertEqual(len(mare_log.events(allow_incomplete=True)), len(rows))
        # A dropped marker carrying another session's data.session_id must still be rejected.
        write_jsonl(self.event_path, [self.start, dict(self.turn_start, seq=1),
            {"type": "dropped", "data": {"session_id": "other-session", "session_epoch": "e1", "count": 1}}, settled])
        with self.assertRaisesRegex(ValueError, "another session"):
            mare_log.events(allow_incomplete=True)

    def test_unicode_line_separator_inside_json_text_is_not_a_record_break(self):
        finish = dict(self.finish, data=dict(self.finish["data"], text="first\u2028second"))
        rows = [self.start, dict(self.turn_start, seq=1), dict(self.turn_end, seq=2), finish]
        self.event_path.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")
        try:
            text = mare_log.events()[-1]["data"]["text"]
        except ValueError as exc:
            self.fail(f"valid Unicode was mistaken for a line boundary: {exc}")
        self.assertEqual(text, "first\u2028second")

    def test_rejects_duplicate_json_keys_and_carriage_return_truncation(self):
        start = json.dumps(self.start).replace('"session_id": "session-123"', '"session_id": "session-123", "session_id": "session-123"')
        valid_rest = "".join(json.dumps(row) + "\n" for row in
                             [dict(self.turn_start, seq=1), dict(self.turn_end, seq=2), self.finish])
        self.event_path.write_text(start + "\n" + valid_rest, encoding="utf-8")
        with self.assertRaises(ValueError):
            mare_log.events()
        self.write_valid()
        self.event_path.write_bytes(self.event_path.read_bytes()[:-1] + b"\r")
        with self.assertRaises(ValueError):
            mare_log.events()

    def test_unpaired_or_duplicate_turn_and_item_ids_are_errors(self):
        item_start = {"type": "item/started", "data": {"turn_id": "turn-1", "item_id": "item-1"}}
        item_end = {"type": "item/completed", "data": {"turn_id": "turn-1", "item_id": "item-1"}}
        for rows in ([self.start, self.turn_start, self.finish], [self.start, self.turn_end, self.turn_start, self.finish],
                     [self.start, self.turn_start, self.turn_start, self.turn_end, self.finish],
                     [self.start, self.turn_start, item_start, self.turn_end, self.finish],
                     [self.start, self.turn_start, item_end, self.turn_end, self.finish],
                     [self.start, self.turn_start, item_start, item_start, item_end, self.turn_end, self.finish]):
            with self.subTest(rows=rows):
                self.write_valid(events=rows)
                with self.assertRaises(ValueError):
                    mare_log.events()

    def test_parallel_items_can_finish_out_of_order(self):
        def event(phase, ident):
            return {"type": f"item/{phase}", "data": {"turn_id": "turn-1", "item_id": ident}}
        rows = [self.start, self.turn_start, event("started", "a"), event("started", "b"),
                event("completed", "b"), event("completed", "a"), self.turn_end, self.finish]
        self.write_valid(events=rows)
        self.assertEqual(len(mare_log.events()), len(rows))

    def test_protocol_three_tool_turn_with_step_and_items(self):
        def event(kind, **data):
            return {"type": kind, "session_id": "session-123", "session_epoch": "epoch-1", "data": data}
        user, call = {"id": "user-1", "type": "user_message"}, {"id": "call-1", "type": "tool_call"}
        rows = [self.start, event("turn/started", turn_id="turn-1", trigger="prompt"),
                event("item/started", turn_id="turn-1", item_id="user-1", item=user),
                event("item/completed", turn_id="turn-1", item_id="user-1", status="completed", item=user),
                event("step/started", turn_id="turn-1", step_id="step-1"),
                event("item/started", turn_id="turn-1", item_id="call-1", item=call),
                event("item/completed", turn_id="turn-1", item_id="call-1", status="completed", item=call),
                event("step/completed", turn_id="turn-1", step_id="step-1", status="completed"),
                event("turn/completed", turn_id="turn-1", status="completed"), self.finish]
        self.write_valid(events=rows)
        self.assertEqual([c["id"] for c in mare_log.Log().calls], ["call-1"])

    def test_step_can_restart_only_after_a_retryable_completion(self):
        def step(phase, **extra):
            return {"type": f"step/{phase}", "data": {"turn_id": "turn-1", "step_id": "step-1", **extra}}
        start = step("started", attempt=0)
        retry = step("completed", status="failed", will_retry=True, attempt=1)
        restart = step("started", attempt=1)
        fallback = step("completed", status="failed", will_retry=True, attempt=2)
        end = step("completed", status="completed")
        rows = [self.start, self.turn_start, start, retry, restart, fallback, step("started", attempt=2),
                end, self.turn_end, self.finish]
        self.write_valid(events=rows)
        self.assertEqual(len(mare_log.events()), len(rows))
        for invalid in ([start, end, restart, end], [start, step("completed", will_retry=1), restart, end],
                        [start, start, end],
                        [start, retry, restart, restart, end]):
            with self.subTest(invalid=invalid):
                self.write_valid(events=[self.start, self.turn_start, *invalid, self.turn_end, self.finish])
                with self.assertRaisesRegex(ValueError, "duplicate step id"):
                    mare_log.events()

    def test_duplicate_and_decreasing_event_sequences_are_errors(self):
        for seq in [1, 0]:
            with self.subTest(seq=seq):
                rows = [self.start, dict(self.turn_start, seq=1), dict(self.turn_end, seq=seq), self.finish]
                self.write_valid(events=rows)
                with self.assertRaises(ValueError):
                    mare_log.events()

    def test_session_epoch_cannot_change_mid_run(self):
        rows = [self.start, dict(self.turn_start, seq=1, session_epoch="first"),
                dict(self.turn_end, seq=1, session_epoch="second"), self.finish]
        self.write_valid(events=rows)
        with self.assertRaises(ValueError):
            mare_log.events()

    def test_rejects_bad_protocol_duplicate_session_and_cross_session_event(self):
        wrong = dict(self.start, data=dict(self.start["data"], protocol_version="2.3"))
        for rows in ([wrong, self.finish], [self.start, self.start, self.finish],
                     [self.start, {"type": "turn/started", "session_id": "other", "data": {}}, self.finish]):
            with self.subTest(rows=rows):
                self.write_valid(events=rows)
                with self.assertRaises(ValueError):
                    mare_log.events()

    def test_rejects_bad_session_json_and_incomplete_tool_pairs(self):
        for rows in ([self.header, self.assistant], [self.header, self.result],
                     [self.header, self.assistant, self.assistant, self.result],
                     [self.header, self.assistant, self.result, self.result]):
            with self.subTest(rows=rows):
                self.write_valid(session=rows)
                with self.assertRaises(ValueError):
                    mare_log.Log()
        self.write_valid()
        with self.session_path.open("a", encoding="utf-8") as f:
            f.write('{"type":')
        with self.assertRaises(ValueError):
            mare_log.Log()

    def test_rejects_malformed_message_shapes_and_a_second_header(self):
        bad_assistant = {"type": "message", "entry_id": "bad-a", "message": {"role": "assistant", "content": "not-an-array"}}
        bad_result = {"type": "message", "entry_id": "bad-b", "message": {"role": "tool_result", "tool_call_id": "call-1", "content": "not-an-array"}}
        for rows in ([self.header, self.assistant, self.result, bad_assistant], [self.header, self.assistant, bad_result],
                     [self.header, self.assistant, self.result, {"type": "session", "session_id": "another"}]):
            with self.subTest(rows=rows):
                self.write_valid(session=rows)
                with self.assertRaises(ValueError):
                    mare_log.Log()

    def test_legacy_camel_case_is_not_accepted(self):
        legacy = {"type": "message", "entry_id": "legacy", "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "call-1", "name": "read"}]}}
        self.write_valid(session=[self.header, legacy])
        with self.assertRaises(ValueError):
            mare_log.Log()
        old_result = {"type": "message", "entry_id": "legacy", "message": {"role": "toolResult", "toolCallId": "call-1", "content": []}}
        self.write_valid(session=[self.header, self.assistant, old_result])
        with self.assertRaises(ValueError):
            mare_log.Log()

    def test_no_tool_calls_do_not_pass_pairing_check(self):
        self.write_valid(session=[self.header])
        parsed = mare_log.Log()
        checks = mare_log.common_checks(parsed)
        self.assertFalse(checks["every tool call has a result"])

class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def collect(self, expected, code=0):
        try:
            return run.trials(self.root, expected, code)
        except TypeError as e:
            self.fail(f"runner does not reconcile expected tasks and Harbor status: {e}")

    def test_full_plan_has_exactly_the_real_34_tasks(self):
        self.assertTrue(callable(getattr(run, "task_plan", None)), "runner must expose the planned task inventory")
        plans = run.task_plan()
        plan = [task for group in run.GROUPS for task in plans[group]]
        self.assertEqual((len(plan), len(set(plan))), (34, 34))
        for task in ("qin-shi-huang", "pompeii"):
            self.assertTrue(task in plans["own"] and (run.EVALS / "tasks" / task / "task.toml").is_file(), task)
        self.assertTrue({"dev-sandbox-escalation", "swe-bench/django__django-11179", "aider/polyglot_rust_wordy"} <= set(plan))

    def test_missing_crashed_or_unverified_tasks_are_errors(self):
        add_result(self.root, "pass")
        add_result(self.root, "no-verifier", reward=None)
        add_result(self.root, "bad-exit", exit_code="5")
        add_result(self.root, "timeout", error="TimeoutError")
        (self.root / "crashed__one").mkdir()
        expected = ["pass", "missing", "crashed", "no-verifier", "bad-exit", "timeout"]
        rows = self.collect(expected)
        self.assertEqual(len(rows), len(expected))
        statuses = {row["task"]: row["status"] for row in rows}
        self.assertEqual(statuses["pass"], "PASS")
        self.assertEqual({statuses[name] for name in expected[1:]}, {"ERROR"})
        known = {row["task"]: row["cost"] for row in rows}
        self.assertEqual(known["no-verifier"], 0.0)
        self.assertEqual(known["bad-exit"], 0.0)
        self.assertIsNone(known["missing"])

    def test_unexplained_nonzero_harbor_exit_cannot_be_reported_as_pass(self):
        add_result(self.root, "pass")
        rows = self.collect(["pass"], code=7)
        self.assertEqual(rows[0]["status"], "ERROR")
        self.assertEqual(rows[0]["cost"], 0.0)
        identity = {"commit": "a" * 40, "version": "0.1.0", "binaries": {}}
        with mock.patch.object(run.subprocess, "run", return_value=types.SimpleNamespace(returncode=7)) as process:
            try:
                result = run.harbor("case", "dummy/model", [], identity=identity)
            except TypeError as exc:
                self.fail(f"validated binary identity was not passed to Harbor: {exc}")
        self.assertIsInstance(result, tuple, "the process status must reach the reconciler")
        self.assertEqual(result[1], 7)
        self.assertEqual(json.loads(process.call_args.kwargs["env"]["MARE_EVAL_IDENTITY"]), identity)
        self.assertEqual(process.call_args.kwargs["env"].get("MARE_EVAL_VERSION"), "0.1.0+aaaaaaaaaaaa")

    def test_failed_trial_does_not_mask_other_completed_trials(self):
        add_result(self.root, "pompeii", exit_code="4", error="TimeoutError")
        add_result(self.root, "tools-1")
        add_result(self.root, "qin", reward=0.0)
        rows = self.collect(["pompeii", "tools-1", "qin", "missing"], code=1)
        self.assertEqual([row["status"] for row in rows], ["ERROR", "PASS", "FAIL", "ERROR"])
        rows = self.collect(["tools-1", "pompeii", "qin", "not-started"], code=-1)
        self.assertEqual([row["status"] for row in rows], ["PASS", "ERROR", "FAIL", "ERROR"])

    def test_harbor_timeout_marks_every_expected_task_error(self):
        identity = {"commit": "a" * 40, "version": "0.1.0", "binaries": {}}
        failure = run.subprocess.TimeoutExpired(["harbor"], 1)
        with mock.patch.object(run.subprocess, "run", side_effect=failure), \
             mock.patch.object(run, "JOBS", self.root), contextlib.redirect_stderr(io.StringIO()):
            job, code = run.harbor("timed-out", "dummy/model", [], identity=identity)
        self.assertNotEqual(code, 0)
        self.assertEqual([r["status"] for r in run.trials(job, ["one", "two"], code)], ["ERROR", "ERROR"])

    def test_duplicate_results_do_not_increase_denominator(self):
        add_result(self.root, "dup", suffix="first")
        add_result(self.root, "dup", suffix="second")
        rows = self.collect(["dup"])
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["status"], "ERROR")

    def test_result_without_matching_task_identity_is_an_error(self):
        folder = add_result(self.root, "planned")
        result_path = folder / "result.json"
        result = json.loads(result_path.read_text(encoding="utf-8"))
        for identity in [None, "different-task"]:
            with self.subTest(identity=identity):
                if identity is None:
                    result.pop("task_name", None)
                else:
                    result["task_name"] = identity
                result_path.write_text(json.dumps(result), encoding="utf-8")
                self.assertEqual(self.collect(["planned"])[0]["status"], "ERROR")

    def test_unreadable_job_directory_yields_error_rows(self):
        with mock.patch.object(Path, "iterdir", side_effect=PermissionError("not readable")):
            try:
                rows = self.collect(["first", "second"])
            except OSError as exc:
                self.fail(f"job directory error lost the planned tasks: {exc}")
        self.assertEqual([row["status"] for row in rows], ["ERROR", "ERROR"])

    def test_nonzero_harbor_exit_is_preserved_even_if_job_is_unreadable(self):
        with mock.patch.object(Path, "iterdir", side_effect=PermissionError("not readable")):
            rows = self.collect(["first", "second"], code=7)
        self.assertTrue(all("7" in row["error"] for row in rows), rows)

    def test_full_run_reports_all_34_missing_tasks_even_with_retries(self):
        (self.root / "manifest.json").write_text("{}", encoding="utf-8")
        output = io.StringIO()
        with mock.patch.object(run, "BIN_DIR", self.root), mock.patch.object(run.shutil, "which", return_value="harbor"), \
             mock.patch.object(run, "source_identity", return_value=("a" * 40, "0.1.0")), \
             mock.patch.object(run, "validate_binaries"), mock.patch.object(run, "sync_helpers"), \
             mock.patch.object(run, "harbor", return_value=(self.root / "absent", 0)) as invoke, \
             mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "offline-dummy"}), \
             mock.patch.object(sys, "argv", ["run.py", "all", "--no-build", "--parallel-tries", "3"]), \
             contextlib.redirect_stdout(output):
            code = run.main()
        self.assertEqual(code, 1)
        self.assertEqual(invoke.call_count, 7)
        self.assertIn("0/34 tasks passed (36 trials)", output.getvalue())
        self.assertEqual(output.getvalue().count(" ERROR "), 36)

    def test_task_plan_does_not_change_during_a_harbor_run(self):
        evals = self.root / "evals"
        evals.mkdir()
        for _, listing, _ in run.DATASETS.values():
            (evals / listing).write_bytes((run.EVALS / listing).read_bytes())
        (self.root / "manifest.json").write_text("{}", encoding="utf-8")
        invocations = []
        def launch(job, model, args, *extra, **kwargs):
            invocations.append((job, list(args)))
            if len(invocations) == 1:
                (evals / "tbench.txt").write_text("changed-after-preflight\n", encoding="utf-8")
            return self.root / "absent", 0
        with mock.patch.object(run, "EVALS", evals), mock.patch.object(run, "BIN_DIR", self.root), \
             mock.patch.object(run.shutil, "which", return_value="harbor"), \
             mock.patch.object(run, "source_identity", return_value=("a" * 40, "0.1.0")), \
             mock.patch.object(run, "validate_binaries", return_value={}), mock.patch.object(run, "sync_helpers"), \
             mock.patch.object(run, "harbor", side_effect=launch), \
             mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "offline-dummy"}), \
             mock.patch.object(sys, "argv", ["run.py", "all", "--no-build"]), contextlib.redirect_stdout(io.StringIO()):
            try:
                result = run.main()
            except (OSError, ValueError) as exc:
                self.fail(f"task inventory was reread after Harbor started: {exc}")
        self.assertEqual(result, 1)
        tbench_args = next(args for name, args in invocations if name.startswith("tbench-"))
        self.assertIn("build-pmars", tbench_args)
        self.assertNotIn("changed-after-preflight", tbench_args)

    def test_unknown_cost_is_distinct_from_zero(self):
        base = {"task": "case", "pass": True, "status": "PASS", "reward": 1.0, "secs": None,
                "tok_in": 0, "tok_out": 0, "exit": "0", "error": None, "details": {}, "failed": []}
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            run.table([dict(base, task="free", cost=0.0), dict(base, task="missing-price", cost=None)])
        text = out.getvalue().lower()
        self.assertIn("1 unknown", text)
        self.assertIn("0.0000", text)
        self.assertIn("known subtotal", text)

    def test_no_build_refuses_missing_manifest_before_launching_harbor(self):
        output = io.StringIO()
        with mock.patch.object(run, "BIN_DIR", self.root), mock.patch.object(run.shutil, "which", return_value="harbor"), \
             mock.patch.object(run, "sync_helpers"), mock.patch.object(run, "harbor", side_effect=AssertionError("Harbor launched")) as invoke, \
             mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "offline-dummy"}), \
             mock.patch.object(sys, "argv", ["run.py", "own", "--no-build"]), contextlib.redirect_stdout(output):
            try:
                code = run.main()
            except (ValueError, RuntimeError, SystemExit) as exc:
                self.fail(f"preflight aborted without accounting for the planned tasks: {exc}")
        self.assertEqual(code, 1)
        invoke.assert_not_called()
        self.assertIn("0/10 tasks passed", output.getvalue())
        self.assertEqual(output.getvalue().count(" ERROR "), 10)

    def test_build_failure_reports_every_planned_task_as_error(self):
        output = io.StringIO()
        failure = run.subprocess.CalledProcessError(1, ["docker"])
        with mock.patch.object(run, "BIN_DIR", self.root), mock.patch.object(run.shutil, "which", return_value="harbor"), \
             mock.patch.object(run, "source_identity", return_value=("a" * 40, "0.1.0")), \
             mock.patch.object(run, "build", side_effect=failure), mock.patch.object(run, "harbor") as invoke, \
             mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "offline-dummy"}), \
             mock.patch.object(sys, "argv", ["run.py", "all"]), contextlib.redirect_stdout(output), \
             contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(run.main(), 1)
        invoke.assert_not_called()
        self.assertIn("0/34 tasks passed", output.getvalue())
        self.assertEqual(output.getvalue().count(" ERROR "), 34)

    def test_no_build_rejects_stale_manifest_without_launching_harbor(self):
        (self.root / "manifest.json").write_text(
            json.dumps({"commit": "b" * 40, "version": "0.1.0", "binaries": {}}), encoding="utf-8"
        )
        output = io.StringIO()
        errors = io.StringIO()
        with mock.patch.object(run, "BIN_DIR", self.root), mock.patch.object(run.shutil, "which", return_value="harbor"), \
             mock.patch.object(run, "source_identity", return_value=("a" * 40, "0.1.0")), \
             mock.patch.object(run, "sync_helpers"), mock.patch.object(run, "harbor") as invoke, \
             mock.patch.dict(os.environ, {"OPENCODE_API_KEY": "offline-dummy"}), \
             mock.patch.object(sys, "argv", ["run.py", "all", "--no-build"]), \
             contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            self.assertEqual(run.main(), 1)
        invoke.assert_not_called()
        self.assertIn("0/34 tasks passed", output.getvalue())
        self.assertEqual(output.getvalue().count(" ERROR "), 34)
        self.assertIn("binary validation: ValueError: binary manifest version or commit SHA mismatch", errors.getvalue())

    def test_container_build_can_stamp_the_mounted_checkout(self):
        with mock.patch.object(run, "BIN_DIR", self.root), mock.patch.object(run, "sh") as execute:
            run.build()
        command = execute.call_args.args[0]
        pairs = list(itertools.pairwise(command))
        self.assertIn(("-e", "GIT_CONFIG_COUNT=1"), pairs)
        self.assertIn(("-e", "GIT_CONFIG_KEY_0=safe.directory"), pairs)
        self.assertIn(("-e", "GIT_CONFIG_VALUE_0=/src"), pairs)

    def test_dataset_task_identity_preserves_double_underscore(self):
        add_result(self.root, "django__django-11179")
        rows = self.collect(["swe-bench/django__django-11179"])
        self.assertEqual(rows[0]["task"], "swe-bench/django__django-11179")
        self.assertEqual(rows[0]["status"], "PASS")

    def test_archive_nested_inside_checkout_is_not_mistaken_for_checkout(self):
        nested = self.root / "archive"
        nested.mkdir()
        (nested / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.0"\n', encoding="utf-8")
        revision = "a" * 40
        def fake_git(cmd, **kwargs):
            if "--show-toplevel" in cmd:
                output = str(self.root)
            elif "rev-parse" in cmd:
                output = revision
            else:
                output = ""
            return types.SimpleNamespace(stdout=output, returncode=0)
        with (mock.patch.object(run, "ROOT", self.root), mock.patch.object(run.subprocess, "run", side_effect=fake_git),
              mock.patch.dict(os.environ, {"MARE_EVAL_SRC": str(nested)}),
              self.assertRaisesRegex(ValueError, "(?i)source|checkout")):
            run.source_identity()

    def test_binary_manifest_checks_version_commit_and_bytes(self):
        validate = getattr(run, "validate_binaries", None)
        self.assertTrue(callable(validate), "binary identity verification must exist")
        revision = "1234567890abcdef1234567890abcdef12345678"
        names = {"mare-linux-amd64": (62, "x86_64"), "mare-linux-arm64": (183, "aarch64")}
        digests = {}
        for name, (machine, arch) in names.items():
            content = write_fake_binary(self.root, name, machine, arch, revision, "0.1.0")
            digests[name] = hashlib.sha256(content).hexdigest()
        manifest = {"version": "0.1.0", "commit": revision, "binaries": digests}
        (self.root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        validate(self.root, revision, "0.1.0")
        with self.assertRaisesRegex(ValueError, "(?i)version|commit|sha"):
            validate(self.root, revision, "0.2.0")
        with self.assertRaisesRegex(ValueError, "(?i)commit|sha"):
            validate(self.root, "f" * 40, "0.1.0")
        old = (self.root / "mare-linux-arm64").read_bytes()
        wrong = old.replace(b"0.1.0\ncommit:", b"0.2.0\ncommit:")
        (self.root / "mare-linux-arm64").write_bytes(wrong)
        manifest["binaries"]["mare-linux-arm64"] = hashlib.sha256(wrong).hexdigest()
        (self.root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "(?i)version|binary"):
            validate(self.root, revision, "0.1.0")
        (self.root / "mare-linux-arm64").write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "(?i)sha|binary"):
            validate(self.root, revision, "0.1.0")

def import_agent():
    packages = ["harbor", "harbor.agents", "harbor.agents.installed", "harbor.agents.installed.base",
                "harbor.environments", "harbor.environments.base", "harbor.models", "harbor.models.agent",
                "harbor.models.agent.context"]
    modules = {}
    for name in packages:
        module = types.ModuleType(name)
        module.__path__ = []
        modules[name] = module
    modules["harbor.agents.installed.base"].BaseInstalledAgent = object
    modules["harbor.agents.installed.base"].with_prompt_template = lambda f: f
    modules["harbor.environments.base"].BaseEnvironment = object
    modules["harbor.models.agent.context"].AgentContext = object
    spec = importlib.util.spec_from_file_location("offline_mare_agent", Path(__file__).parent / "agent/mare_agent.py")
    module = importlib.util.module_from_spec(spec)
    with mock.patch.dict(sys.modules, modules):
        spec.loader.exec_module(module)
    return module

class AgentUsageTests(EvidenceFixture, unittest.TestCase):
    def setUp(self):
        super().setUp()
        cls = import_agent().MareAgent
        self.agent = cls.__new__(cls)
        self.agent.logs_dir = self.root

    def context_for(self, usage):
        self.write_valid(events=[self.start, self.turn_start, {"type": "usage/changed", "data": usage}, self.turn_end, self.finish])
        context = types.SimpleNamespace(n_input_tokens=None, n_cache_tokens=None, n_output_tokens=None, cost_usd=None)
        self.agent.populate_context_post_run(context)
        return context

    def test_cost_unknown_is_not_a_free_request(self):
        ctx = self.context_for({"input": 3, "output": 2, "cache_read": 1, "cost": 0.0, "cost_unknown": True})
        self.assertIsNone(ctx.cost_usd)
        self.assertEqual(ctx.n_input_tokens, 4)
        self.assertEqual(ctx.n_output_tokens, 2)

    def test_unknown_cost_in_final_settled_record_overrides_earlier_zero(self):
        usage = {"input": 3, "output": 2, "cache_read": 1, "cost": 0.0, "cost_unknown": False}
        finish = dict(self.finish, data=dict(self.finish["data"], usage=dict(usage, cost_unknown=True)))
        self.write_valid(events=[self.start, self.turn_start, {"type": "usage/changed", "data": usage}, self.turn_end, finish])
        context = types.SimpleNamespace(n_input_tokens=None, n_cache_tokens=None, n_output_tokens=None, cost_usd=None)
        self.agent.populate_context_post_run(context)
        self.assertIsNone(context.cost_usd)

    def test_zero_and_negative_cost(self):
        for cost, expected in ((0.0, 0.0), (-1, None)):
            with self.subTest(cost=cost):
                ctx = self.context_for({"input": 3, "output": 2, "cache_read": 1, "cost": cost, "cost_unknown": False})
                self.assertEqual(ctx.cost_usd, expected)

    def test_missing_usage_does_not_reuse_an_old_cost(self):
        self.write_valid()
        context = types.SimpleNamespace(n_input_tokens=5, n_cache_tokens=2, n_output_tokens=1, cost_usd=9.0)
        self.agent.populate_context_post_run(context)
        self.assertIsNone(context.cost_usd)
        self.assertIsNone(context.n_input_tokens)

    def test_bad_event_stream_fails_agent_post_processing(self):
        self.event_path.write_text('{"type": "session"}\n{bad\n', encoding="utf-8")
        with self.assertRaises(ValueError):
            self.agent.populate_context_post_run(types.SimpleNamespace())

    def test_failed_run_preserves_usage_from_complete_records(self):
        usage = {"input": 3, "cache_read": 1, "output": 2, "cost": 0.25}
        usage_event = {"type": "usage/changed", "data": usage}
        cases = [
            ([self.header, self.assistant, self.result], [self.start, self.turn_start, usage_event]),
            ([self.header, self.assistant], [self.start, self.turn_start, usage_event, self.turn_end, self.finish]),
        ]
        for session, events in cases:
            with self.subTest(events=events):
                self.write_valid(session=session, events=events)
                (self.root / "exit_code").write_text("4\n", encoding="utf-8")
                context = types.SimpleNamespace()
                self.agent.populate_context_post_run(context)
                self.assertEqual((context.n_input_tokens, context.n_cache_tokens, context.n_output_tokens), (4, 1, 2))
                self.assertEqual(context.cost_usd, 0.25)

    def test_incomplete_evidence_cannot_be_accepted_for_success_or_malformed_stream(self):
        self.write_valid(events=[self.start, self.turn_start, {"type": "usage/changed", "data": {"cost": 0.25}}])
        with self.assertRaises(ValueError):
            self.agent.populate_context_post_run(types.SimpleNamespace())
        (self.root / "exit_code").write_text("4\n", encoding="utf-8")
        with self.event_path.open("a", encoding="utf-8") as f:
            f.write('{"type":')
        with self.assertRaises(ValueError):
            self.agent.populate_context_post_run(types.SimpleNamespace())

    def test_failed_run_with_dropped_event_preserves_usage_instead_of_crashing(self):
        rows = [self.start, dict(self.turn_start, seq=1),
                {"type": "item/started", "seq": 2, "data": {"turn_id": "turn-1", "item_id": "call-1"}},
                {"type": "dropped", "data": {"session_id": "session-123", "session_epoch": "e1", "count": 1}},
                {"type": "usage/changed", "seq": 3, "data": {"input": 100, "cache_read": 20, "output": 30, "cost": 0.05}},
                {"type": "turn.failed", "data": {"error": "timeout"}},
                {"type": "settled", "data": {"reason": "timeout", "text": "", "exit_code": 4}}]
        write_jsonl(self.event_path, rows)
        (self.root / "exit_code").write_text("4\n", encoding="utf-8")
        context = types.SimpleNamespace()
        self.agent.populate_context_post_run(context)
        self.assertEqual((context.n_input_tokens, context.n_cache_tokens, context.n_output_tokens), (120, 20, 30))
        self.assertEqual(context.cost_usd, 0.05)

class AgentInstallTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        cls = import_agent().MareAgent
        self.agent = cls.__new__(cls)
        self.agent.exec_as_root = mock.AsyncMock()

    def make_binary(self, revision):
        body = write_fake_binary(self.directory, "mare-linux-amd64", 62, "x86_64", revision, "0.1.0")
        manifest = {"commit": revision, "version": "0.1.0", "binaries": {"mare-linux-amd64": hashlib.sha256(body).hexdigest()}}
        (self.directory / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        return body, manifest

    def install_env(self, identity):
        return mock.patch.dict(os.environ, {"MARE_EVAL_BIN_DIR": str(self.directory), "MARE_EVAL_IDENTITY": json.dumps(identity)})

    def environment(self, protocol=None, upload=None):
        responses = [types.SimpleNamespace(stdout="x86_64\n")]
        if protocol is not None:
            responses.append(types.SimpleNamespace(stdout=json.dumps({"protocol_version": protocol})))
        return types.SimpleNamespace(exec=mock.AsyncMock(side_effect=responses), upload_file=mock.AsyncMock(side_effect=upload))

    async def test_minimal_container_ca_setup_is_bounded_and_best_effort(self):
        root = self.directory
        bins, logs, certs = root / "bin", root / "logs", root / "certs"
        for path in (bins, logs, certs):
            path.mkdir()
        commands = {"id": 'echo "$FAKE_UID"', "timeout": 'shift; exec "$@"',
                    "apt-get": 'echo "$*" >> "$APT_LOG"; exit "$APT_RC"', "mare": 'exit 0'}
        for name, script in commands.items():
            exe = bins / name
            exe.write_text("#!/bin/sh\n" + script + "\n")
            exe.chmod(0o755)
        agent = self.agent
        agent._permission, agent._timeout, agent._dangerous_guard, agent.model_name = "auto", "1m", True, "dummy"
        async def upload(path, destination):
            (logs / "instruction.md").write_bytes(path.read_bytes())
        environment = types.SimpleNamespace(upload_file=upload)
        async def execute(_environment, command, env):
            command = command.replace("/etc/ssl/certs", str(certs)).replace("/logs/agent", str(logs))
            command = command.replace("/installed-agent/mare", str(bins / "mare"))
            result = subprocess.run(["sh", "-c", command], env=dict(os.environ, **env), capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
        agent.exec_as_agent = execute
        apt_log = root / "apt.log"
        env = {"OPENCODE_API_KEY": "test", "PATH": f"{bins}{os.pathsep}{os.environ['PATH']}",
               "FAKE_UID": "0", "APT_LOG": str(apt_log), "APT_RC": "0", "SSL_CERT_FILE": "", "SSL_CERT_DIR": ""}
        with mock.patch.dict(os.environ, env):
            await agent.run("test", environment, types.SimpleNamespace())
            self.assertTrue(apt_log.exists(), "root minimal container must attempt CA installation")
            self.assertIn("install", apt_log.read_text())
            apt_log.unlink()
            (certs / "existing.crt").write_text("certificate")
            await agent.run("test", environment, types.SimpleNamespace())
            self.assertFalse(apt_log.exists())
            (certs / "existing.crt").unlink()
            os.environ["FAKE_UID"] = "1000"
            await agent.run("test", environment, types.SimpleNamespace())
            self.assertFalse(apt_log.exists())
            os.environ.update(FAKE_UID="0", APT_RC="1")
            await agent.run("test", environment, types.SimpleNamespace())
            self.assertEqual((logs / "exit_code").read_text().strip(), "0")

    async def test_matching_binary_is_uploaded(self):
        body, manifest = self.make_binary("b" * 40)
        received = {}
        async def upload(path, destination):
            (self.directory / "mare-linux-amd64").write_bytes(b"changed after validation")
            received.update(bytes=path.read_bytes(), destination=destination)
        environment = self.environment("3.0", upload)
        with self.install_env(manifest):
            await self.agent.install(environment)
        environment.upload_file.assert_awaited_once()
        self.assertEqual(received["destination"], "/installed-agent/mare")
        self.assertEqual(received["bytes"], body, "upload must use the validated snapshot")

    async def test_installed_binary_must_speak_protocol_three(self):
        _, manifest = self.make_binary("e" * 40)
        with self.install_env(manifest), self.assertRaisesRegex(ValueError, "(?i)protocol"):
            await self.agent.install(self.environment("2.3"))

    async def test_changed_binary_or_manifest_is_rejected_before_upload(self):
        for mutation in ("binary", "identity"):
            with self.subTest(mutation=mutation):
                _, manifest = self.make_binary("a" * 40)
                if mutation == "binary":
                    (self.directory / "mare-linux-amd64").write_bytes(b"modified after runner validation")
                else:
                    manifest = dict(manifest, commit="d" * 40)
                environment = self.environment()
                with self.install_env(manifest), self.assertRaisesRegex(ValueError, "(?i)binary|sha|identity"):
                    await self.agent.install(environment)
                environment.upload_file.assert_not_awaited()

    def test_evals_readme_matches_restored_tasks_and_registries(self):
        evals_dir = Path(__file__).resolve().parent
        readme = (evals_dir / "README.md").read_text(encoding="utf-8")
        task_names = sorted(p.name for p in (evals_dir / "tasks").iterdir() if p.is_dir() and (p / "task.toml").is_file())
        for name in ("qin-shi-huang", "pompeii"):
            self.assertIn(name, task_names)
        for token in ("qin-shi-huang", "pompeii", "tools-1", "tools-8", "SOURCE.md"):
            self.assertIn(token, readme)
        for name in task_names:
            if name.startswith("dev-"):
                self.assertIn(name, readme, f"README missing dev task {name}")
        for subset in ("tbench.txt", "swebench.txt", "featurebench.txt", "polyglot.txt"):
            self.assertTrue((evals_dir / subset).is_file(), f"missing subset {subset}")
            self.assertIn(subset, readme, f"README missing subset {subset}")
        self.assertTrue((evals_dir / "SOURCE.md").is_file(), "README cites SOURCE.md")

if __name__ == "__main__":
    unittest.main()
