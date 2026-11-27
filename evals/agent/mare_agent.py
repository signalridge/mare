"""Harbor installed-agent adapter for mare.

Uploads the matching static binary from ``MARE_EVAL_BIN_DIR`` to ``/installed-agent/mare``.
Harbor's model defaults to ``opencode-go/glm-5.3-flash``; ``OPENCODE_API_KEY`` is passed
only in the process environment. ``MARE_HOME=/logs/agent/mare`` keeps the session alongside
``mare.jsonl``, ``mare.stderr`` and ``exit_code`` for verification and collection.
Tasks may supply ``/etc/mare-eval/extra.lua`` and ``/etc/mare-eval/plugins/<name>/init.lua``.
Agent kwargs: ``permission`` (default ``auto``), ``dangerous_guard`` (default ``on``), and
``mare_timeout`` (default ``20m``). Registry tasks run fully permissive inside the container.
"""

from __future__ import annotations

import json
import os
import shlex
import shutil
import tempfile
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from evals.lib import mare_log
from evals.run import BINARIES, binary_digest, finite_cost

DEFAULT_MODEL = "opencode-go/glm-5.3-flash"
KEY_ENV = "OPENCODE_API_KEY"
REMOTE_BIN = "/installed-agent/mare"
LOGS = "/logs/agent"


class MareAgent(BaseInstalledAgent):
    def __init__(self, *args, permission: str = "auto", mare_timeout: str = "20m", dangerous_guard: str = "on", **kwargs):
        super().__init__(*args, **kwargs)
        self._permission = permission
        self._timeout = mare_timeout
        self._dangerous_guard = dangerous_guard != "off"

    @staticmethod
    def name() -> str:
        return "mare"

    def version(self) -> str | None:
        return os.environ.get("MARE_EVAL_VERSION") or None

    async def install(self, environment: BaseEnvironment) -> None:
        bin_dir = Path(os.environ.get("MARE_EVAL_BIN_DIR", ""))
        arch = (await environment.exec(command="uname -m")).stdout.strip()
        name = {
            "x86_64": "mare-linux-amd64",
            "amd64": "mare-linux-amd64",
            "aarch64": "mare-linux-arm64",
            "arm64": "mare-linux-arm64",
        }.get(arch)
        if name is None:
            raise RuntimeError(f"mare: unsupported container architecture {arch!r}")
        local = bin_dir / name
        if not local.is_file():
            raise RuntimeError(
                f"mare: {local} not found (MARE_EVAL_BIN_DIR; build with `just eval`)"
            )
        try:
            manifest = json.loads((bin_dir / "manifest.json").read_text(encoding="utf-8"))
            if json.loads(os.environ.get("MARE_EVAL_IDENTITY", "")) != manifest:
                raise ValueError("binary identity changed after runner validation")
            revision, version, expected = manifest["commit"], manifest["version"], manifest["binaries"][name]
            if not isinstance(revision, str) or len(revision) != 40 or not isinstance(version, str):
                raise ValueError("invalid binary identity")
            machine, target_arch = BINARIES[name]
        except (OSError, KeyError, TypeError, ValueError) as exc:
            raise ValueError(f"mare binary validation failed: {name}") from exc
        with tempfile.TemporaryDirectory(prefix="mare-eval-bin-") as tmp:
            snapshot = Path(tmp) / name
            try:
                shutil.copyfile(local, snapshot)
                if binary_digest(snapshot, machine, target_arch, revision, version) != expected:
                    raise ValueError("binary SHA mismatch")
            except (OSError, ValueError) as exc:
                raise ValueError(f"mare binary validation failed: {name}") from exc
            await self.exec_as_root(environment, command="mkdir -p /installed-agent")
            await environment.upload_file(snapshot, REMOTE_BIN)
        await self.exec_as_root(
            environment, command=f"chmod 755 {REMOTE_BIN} && {REMOTE_BIN} --version"
        )
        try:
            schema = json.loads((await environment.exec(command=f"{REMOTE_BIN} rpc schema")).stdout)
            if not isinstance(schema, dict) or schema.get("protocol_version") != "3.0":
                raise ValueError("protocol version mismatch")
        except (AttributeError, TypeError, ValueError) as exc:
            raise ValueError("mare binary does not expose protocol 3.0") from exc

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Validate this run's evidence and preserve unknown token cost as unknown."""
        context.n_input_tokens = context.n_cache_tokens = context.n_output_tokens = context.cost_usd = None
        usage = None
        unknown_cost = False
        try:
            events = mare_log.Log(self.logs_dir).events
        except ValueError:
            try:
                failed = int((Path(self.logs_dir) / "exit_code").read_text(encoding="utf-8").strip()) != 0
            except (OSError, ValueError):
                failed = False
            if not failed:
                raise
            # Failed trials remain errors; retain usage only from well-formed event records.
            events = mare_log.events(self.logs_dir, allow_incomplete=True)
        for ev in events:
            if ev["type"] == "usage/changed":
                usage = ev["data"]
                unknown_cost |= usage.get("cost_unknown") is True
            elif ev["type"] == "settled":
                final_usage = ev["data"].get("usage")
                if isinstance(final_usage, dict):
                    unknown_cost |= final_usage.get("cost_unknown") is True
                    if usage is None:
                        usage = final_usage
        if isinstance(usage, dict):
            if type(usage.get("input")) is int and type(usage.get("cache_read")) is int:
                context.n_input_tokens = usage["input"] + usage["cache_read"]
            if type(usage.get("cache_read")) is int:
                context.n_cache_tokens = usage["cache_read"]
            if type(usage.get("output")) is int:
                context.n_output_tokens = usage["output"]
            cost = usage.get("cost")
            if not unknown_cost and not usage.get("cost_unknown") and finite_cost(cost):
                context.cost_usd = float(cost)

    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        key = os.environ.get(KEY_ENV, "")
        if not key:
            raise RuntimeError(
                f"mare: {KEY_ENV} is not set in the Harbor process environment"
            )
        model = self.model_name or os.environ.get("MARE_LIVE_MODEL") or DEFAULT_MODEL
        # the instruction travels as a file: no length limits, no quoting surprises
        with tempfile.TemporaryDirectory(prefix="mare-eval-") as tmp:
            p = Path(tmp) / "instruction.md"
            p.write_text(instruction)
            await environment.upload_file(p, f"{LOGS}/instruction.md")
        init = f"mare.setup {{ model = {json.dumps(model)} }}\n"
        if not self._dangerous_guard:
            # throwaway task container: the dangerous-command reminder (headless: refuse) only
            # measures the guard, not the agent; Harbor's own agents run fully permissive here
            init += 'mare.plugin("permissions", { dangerous = { enabled = false } })\n'
        script = (
            'ca_present=; for cert in "${SSL_CERT_FILE:-/dev/null}" '
            '"${SSL_CERT_DIR:-/etc/ssl/certs}"/*.pem "${SSL_CERT_DIR:-/etc/ssl/certs}"/*.crt; do '
            '[ -s "$cert" ] && ca_present=1 && break; done; '
            'if [ -z "$ca_present" ] && [ "$(id -u)" = 0 ] && command -v apt-get >/dev/null 2>&1 '
            '&& command -v timeout >/dev/null 2>&1; then '
            '(export DEBIAN_FRONTEND=noninteractive; timeout 45s apt-get -o Acquire::Retries=0 '
            '-o Acquire::http::Timeout=5 -o Acquire::https::Timeout=5 update -qq && '
            'timeout 45s apt-get install -y -qq ca-certificates) >/dev/null 2>&1 || '
            'echo "mare: could not install CA certificates" >&2; fi; '
            f"mkdir -p {LOGS}/mare/config && "
            f"printf '%s' {shlex.quote(init)} > {LOGS}/mare/config/init.lua && "
            f"{{ cat /etc/mare-eval/extra.lua >> {LOGS}/mare/config/init.lua 2>/dev/null || true; }}; "
            f"{{ cp -r /etc/mare-eval/plugins {LOGS}/mare/config/ 2>/dev/null || true; }}; "
            f"MARE_HOME={LOGS}/mare {REMOTE_BIN} -p --json --trust "
            f"--permission {shlex.quote(self._permission)} --timeout {shlex.quote(self._timeout)} "
            f'--model {shlex.quote(model)} "$(cat {LOGS}/instruction.md)" '
            f"> {LOGS}/mare.jsonl 2> {LOGS}/mare.stderr; "
            f"echo $? > {LOGS}/exit_code; exit 0"
        )
        await self.exec_as_agent(environment, command=script, env={KEY_ENV: key})
