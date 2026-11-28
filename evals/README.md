# evals — complex real-model tests (Harbor)

`just eval [own|tbench|dev|swebench|featurebench|polyglot|all]` (default `all`, one try per task). Needs Docker and `uv tool install harbor`; requires `OPENCODE_API_KEY` in the environment. Not part of CI.

- `agent/mare_agent.py` — the one Harbor adapter (`BaseInstalledAgent`): uploads a static Linux
  mare matching the container (`uname -m`), writes a minimal `init.lua` (plus a task's
  `/etc/mare-eval/extra.lua`), runs `mare -p --json --trust --permission <p> --model <m>` on the
  instruction with `MARE_HOME=/logs/agent/mare`, and reports tokens from the last `usage/changed`
  event. The key reaches only that one `exec` as an environment variable.
- `tasks/` — mare's own Harbor tasks (`instruction.md`, `task.toml`, `environment/`, `tests/`):
  `qin-shi-huang`, `pompeii`, and `tools-1` … `tools-8`
  (tool-use cases; fixture provenance in `SOURCE.md`). Verifiers run in the agent's container (shared mode) and read the files
  and mare's session log; they write `/logs/verifier/reward.txt` and `details.json`.
- `lib/mare_log.py` — the verifier helper; `run.py` copies it into every task's `tests/` (edit it
  here).
- `tasks/dev-*` — development tasks (group `dev`, scored by scripts only): `dev-todo-web` (a
  to-do page plus a CRUD API from scratch; the verifier starts `/app/start.sh` and tests the HTTP
  contract and page ids), `dev-add-feature` (coupons in a shipped `shopcart` repo; hidden tests,
  the pristine original suite, and the agent's own tests must pass on the new code and fail on
  the original commit), `dev-multifile-bug` (an overnight-shift bug spanning validation, parsing
  and invoicing in a shipped `timesheet` repo; hidden regression tests), `dev-sandbox-escalation`
  (a write outside the workspace: the model must obtain approved narrow write permissions —
  proactively via `request_permissions` or after a sandbox refusal via `request_permissions` /
  `sandbox_permissions` with justification; its `eval-approver` plugin plays the user). Each has a
  `solution/solve.sh` reference used to validate the verifier.
- `tbench.txt`, `swebench.txt`, `featurebench.txt`, `polyglot.txt` — the fixed registry subsets
  (`terminal-bench@2.0`, `swe-bench/swe-bench-verified@2`, `featurebench/featurebench-lite@1`,
  `aider/aider-polyglot@1`; selection reasons in the comments), run with
  `permission=full-access` and the dangerous-command guard off (the container is the sandbox).
- A task can ship Lua for the run: `/etc/mare-eval/extra.lua` (appended to `init.lua`) and
  `/etc/mare-eval/plugins/<name>/` (user plugins).
- `run.py` — builds `target/eval-bin/mare-linux-{arm64,amd64}` (static musl, in Docker), runs the
  groups with Harbor into `target/eval-jobs/`, prints pass / time / tokens per trial.
- `ab.py`, `ab_probes.py`, `ab_rule.py` — same-scenario interleaved A/B evaluation on Harbor
  (`just eval-ab <scenario>` defaults to comparing the latest reachable `v*` release tag baseline,
  cached under `target/eval-bin-baseline/`, against the current build; override with
  `--baseline-ref`, `--baseline-bin`/`--candidate-bin`, `--baseline-lua`/`--candidate-lua`, or
  `--baseline-model`/`--candidate-model`). Each local task ships `probes.json` with a budget
  plus paired `must_show` and `must_not_show` checks (deterministic from logs by default; an LLM
  judge is invoked only for probes explicitly marked `llm_judge`). Every run saves its structured
  record (`runs.jsonl`) and judge view (`views/*.jsonl`) under `target/eval-ab/`, so saved runs can
  be re-scored offline without re-running the agent (`just eval-rescore <dir> --out <new-dir>`) or
  pooled (`python3 evals/ab.py --pool <dir1> --pool <dir2> --out <out>`). Side-by-side reports
  apply a Holm-adjusted one-sided mid-p Fisher exact regression rule (`δ = 0.05`, `α = 0.10`).

The same tasks can be run with Harbor's other built-in agent adapters for comparison
(run `just eval-sync` first so each task's `tests/mare_log.py` is populated, then
`harbor run -p evals/tasks -a claude-code -m …`); not done by default.
