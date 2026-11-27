"""End-to-end LuaLS isolation and concurrency self-tests for check_lua_public_api."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lua_api_require_state import _self_test as self_test_require_state
from lua_api_rules import module_violations, self_test_cordis_underscore
from lua_api_runner import (
    BOUNDARY_DIAGNOSTICS,
    PRESET,
    ROOT,
    _is_pid_alive,
    _win_kernel32,
    diagnose,
    isolated_temp_dir,
    run_concurrent_workers,
    workspace_at,
)
from lua_api_selftest_rules import (
    self_test_ast_grep_failures,
    self_test_boot_prims,
    self_test_builtin_apply_no_yield,
    self_test_legacy_api,
    self_test_preset_private_api,
    self_test_public_events,
    self_test_scanner_entry_points,
    self_test_third_party_requires,
)


def builtins_typecheck_with_public_types_only(base_dir: Path | None = None) -> None:
    """Built-in plugins must typecheck with only `types/` (no `types-internal/`)."""
    with isolated_temp_dir("mare-public-only-", base_dir=base_dir) as directory:
        ws = Path(directory) / "workspace"
        ws.mkdir(parents=True)
        shutil.copytree(PRESET / "types", ws / "types")
        shutil.copy2(PRESET / "plugins/session.lua", ws / "session.lua")
        shutil.copytree(PRESET / "plugins/lua/session", ws / "lua/session")
        (ws / ".luarc.json").write_text('{"runtime.version":"Lua 5.5","runtime.path":["lua/?.lua","lua/?/init.lua"],"workspace.library":["types"],"workspace.checkThirdParty":false,"diagnostics.globals":["mare","ui"]}')
        rep = ws / "diagnostics.json"
        child_env = dict(os.environ, MARE_TMPDIR=str(directory), TMPDIR=str(directory), TEMP=str(directory), TMP=str(directory))
        res = subprocess.run(["lua-language-server", f"--check={ws}", "--check_format=json", f"--check_out_path={rep}", f"--logpath={directory / 'logs'}", f"--metapath={directory / 'meta'}"], env=child_env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=300, check=False)
        data = __import__("json").loads(rep.read_text()) if rep.is_file() and rep.read_text().strip() else {}
        bad = [d for ds in data.values() for d in ds if d.get("code") in BOUNDARY_DIAGNOSTICS]
        if res.returncode not in (0, 1) or bad:
            raise RuntimeError(f"builtin typecheck with public types only failed ({len(bad)}): {bad[:3]}")


def self_test(base_dir: Path | None = None, entry_script: Path | None = None) -> None:
    """Prove that aliases cannot infer undocumented API from implementation files."""
    script_path = (entry_script or Path(__file__).resolve().with_name("check_lua_public_api.py")).resolve()
    builtins_typecheck_with_public_types_only(base_dir)
    self_test_preset_private_api(base_dir)
    self_test_ast_grep_failures()
    self_test_scanner_entry_points(base_dir)
    self_test_public_events(base_dir)
    self_test_legacy_api(base_dir)
    self_test_builtin_apply_no_yield(base_dir)
    self_test_boot_prims(base_dir)
    self_test_cordis_underscore(base_dir)
    self_test_third_party_requires(base_dir)
    self_test_require_state()
    with isolated_temp_dir("mare-public-api-test-", base_dir=base_dir) as directory:
        workspace = Path(directory) / "workspace"
        workspace_at(workspace)
        probe_lines = [
            "local own = {_private = true}; print(own._private)",  # 1: ok
            "mare.plugins.failures()",  # 2: ok
            "local keys = mare.keymap; keys._remove({})",  # 3: undefined-field
            "---@param req mare.LlmRequest",  # 4: ok
            "local function run(req) req._rebuild_tool_declarations() end",  # 5: undefined-field
            "mare.caller_site(1)",  # 6: ok (declared in types/mare.lua)
            "---@type mare.PluginDef",  # 7: ok
            'local plugin = {name="probe",apply=function(ctx)',  # 8: ok
            "  ctx.agent:not_in_contract()",  # 9: undefined-field
            '  local a = ctx:get("agent"); if a then a:queued(); a:not_in_contract() end',  # 10: undefined-field
            '  local s = ctx:get("session"); if s then s:append_prune({}); s:not_in_contract() end',  # 11: undefined-field
            '  local u = ctx:get("undeclared"); if u then u:secret() end',  # 12: undefined-field
            '  local dyn = tostring(1); local d = ctx:get(dyn); if d then d:secret() end',  # 13: undefined-field
            "end}",  # 14: ok
            'local own_module = require("probe.private"); own_module.ok(); own_module.missing()',  # 15: undefined-field
            "local public_module = require [[mare_lib]]; local _, _ = public_module.parse_args('{}'); public_module.undeclared()",  # 16: undefined-field
            'local rev = require("mare_lib.table_revision"); rev.snapshot({}); rev.undeclared()',  # 17: undefined-field
            'local forbidden = require "agent.queue_shape"',  # 18: import
            'local name = "agent.queue_shape"; require(name)',  # 19: import
            'local concat_req = require("mare_" .. "lib")',  # 20: import
            'local loader = require; loader("agent.queue_shape")',  # 21: import
            "local saved = {loader = require}",  # 22: import
            'local g_idx = _G["require"]("agent.queue_shape")',  # 23: import (_G)
            'local e_idx = _ENV["require"]("mare_lib")',  # 24: import (_ENV)
            'local g_dot = (_G).require("mare_lib")',  # 25: import (_G + require)
            'local r_req = rawget(_G, "require")("agent.queue_shape")',  # 26: import (rawget + _G)
            'local r_priv = rawget(mare, "_stream_buffer")()',  # 27: import (rawget)
            'rawset(mare, "_injected", true)',  # 28: import (rawset)
            'local ls = loadstring("return 1")',  # 29: import (loadstring)
            'local gl = load("return 1")',  # 30: import (global load before local shadow)
            "local p_load = package.loaders",  # 31: import (package.loaders)
            "local p_search = package.searchers",  # 32: import (package.searchers)
            'local p_idx = package["searchers"]',  # 33: import (package bracket index)
            "local function load() return {} end; assert(load())",  # 34: ok (local load)
            "local own = {require = true, rawget = true, rawset = true, load = true, loadstring = true, package = true}; assert(own.require and own.rawget and own.load and own.package)",  # 35: ok
            "local object = {}; function object:require() return true end; assert(object:require())",  # 36: ok
            "local function rooted(ctx) return ctx.root end",  # 37: import (ctx.root)
            'local function rooted2(ctx) return ctx["root"] end',  # 38: import (ctx["root"])
            "local opts = {root = '/tmp'}; assert(opts.root)",  # 39: ok (not a ctx)
            "---@param ctx mare.Ctx",  # 40
            "local function public_service_types(ctx)",  # 41
            '  local b = ctx:get("bus"); if b then b:emit("turn/started", {}) end',  # 42: ok
            '  local k = ctx:get("kernel"); if k then k:plugins() end',  # 43: ok
            '  local p = ctx:get("policy"); if p then p:has_voters("request/ready") end',  # 44: ok
            '  local f = ctx:inject({"bus"}, function(sub) end); f:await(); f:dispose()',  # 45: ok
            "end",  # 46
            "---@param svc cordis.Service",  # 47
            "local function caller_service_types(svc)",  # 48
            '  svc.ctx.kernel:plugins(); svc.ctx.policy:has_voters("request/ready")',  # 49: ok
            '  local k = svc.ctx:get("kernel"); if k then k:plugins() end',  # 50: ok
            '  local p = svc.ctx:get("policy"); if p then p:has_voters("request/ready") end',  # 51: ok
            '  local u = svc.ctx:get("undeclared"); if u then u:secret() end',  # 52: undefined-field
            "end",  # 53
            "mare._where(1)",  # 54: undefined-field (the old alias is no longer public)
        ]
        (workspace / "probe.lua").write_text("\n".join(probe_lines) + "\n")
        owned = workspace / "lua/probe/private.lua"
        owned.parent.mkdir(parents=True)
        owned.write_text(
            "---@class probe.Private\nlocal M = {}\nfunction M.ok() return true end\nreturn M\n"
        )
        bad_helper = workspace / "lua/probe/bad_helper.lua"
        bad_helper.write_text(
            'local function bad() return rawget(mare, "_secret"), mare._undeclared end\nreturn bad\n'
        )
        imports = module_violations(workspace, "probe")
        def import_lines(rel: str) -> set[int]:
            return {error["line"] for error in imports if error["file"] == Path(rel)}
        probe_import_lines = import_lines("probe.lua")
        expected_probe_imports = {18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 37, 38}
        if probe_import_lines != expected_probe_imports:
            raise RuntimeError(f"import boundary regression: expected {sorted(expected_probe_imports)}, got {sorted(probe_import_lines)} ({imports})")
        if import_lines("lua/probe/bad_helper.lua") != {1}:
            raise RuntimeError(f"submodule import boundary regression: {imports}")
        reports = diagnose(workspace)
        malformed_docs = [(uri, d) for uri, ds in reports.items() for d in ds if d["code"] == "luadoc-miss-symbol"]
        if malformed_docs:
            raise RuntimeError(f"public declarations contain malformed LuaDoc: {malformed_docs}")
        def undef_lines(suffix: str) -> set[int]:
            return {
                d["range"]["start"]["line"] + 1
                for uri, ds in reports.items()
                if uri.endswith(suffix)
                for d in ds
                if d["code"] == "undefined-field"
            }
        fields = undef_lines("/probe.lua")
        expected_fields = {3, 5, 9, 10, 11, 12, 13, 15, 16, 17, 52, 54}
        if fields != expected_fields:
            raise RuntimeError(f"boundary checker regression: expected lines {sorted(expected_fields)}, got {sorted(fields)}")
        helper_fields = undef_lines("/lua/probe/bad_helper.lua")
        if helper_fields != {1}:
            raise RuntimeError(f"submodule boundary checker regression: expected {{1}}, got {sorted(helper_fields)}")
        console_ws = Path(directory) / "lua_console"
        workspace_at(console_ws)
        (console_ws / "lua-console.lua").write_text(
            "\n".join(
                [
                    'local env = setmetatable({}, { __index = _ENV })',  # 1: ok for lua-console
                    'local f = load("return 1", "=lua", "t", env)',  # 2: ok for lua-console
                    'local bad_req = require("agent.queue_shape")',  # 3: private import
                    'local bad_env = _ENV["require"]("mare_lib")',  # 4: _ENV bracket index
                    'local bad_ls = loadstring("return 1")',  # 5: loadstring still forbidden
                    'local bad_g = _G',  # 6: _G still forbidden
                ]
            )
            + "\n"
        )
        console_imports = {
            error["line"] for error in module_violations(console_ws, "lua-console")
        }
        if console_imports != {3, 4, 5, 6}:
            raise RuntimeError(
                f"lua-console boundary regression: expected {{3, 4, 5, 6}}, got {sorted(console_imports)}"
            )
    with isolated_temp_dir("mare-public-api-err-", base_dir=base_dir) as err_root:
        leaked_path: Path | None = None
        try:
            with isolated_temp_dir("mare-public-api-fail-", base_dir=err_root) as d:
                leaked_path = d
                (d / "marker").write_text("x")
                raise RuntimeError("simulated failure")
        except RuntimeError as error:
            if str(error) != "simulated failure":
                raise
        if leaked_path is None or leaked_path.exists() or list(err_root.iterdir()):
            raise RuntimeError(
                f"isolated_temp_dir leaked on failure: {list(err_root.iterdir())}"
            )
    with isolated_temp_dir("mare-public-api-rmtree-", base_dir=base_dir) as rmtree_root:
        orig_rmtree = shutil.rmtree
        created_dir: Path | None = None

        def failing_rmtree(target: str | os.PathLike[str], *args: object, **kwargs: object) -> None:
            raise PermissionError(f"simulated rmtree failure for {target}")

        try:
            shutil.rmtree = failing_rmtree  # type: ignore[assignment]
            try:
                with isolated_temp_dir("mare-public-api-perm-", base_dir=rmtree_root) as d:
                    created_dir = d
                    (d / "marker").write_text("x")
            except PermissionError:
                pass
            else:
                raise RuntimeError("isolated_temp_dir silently ignored cleanup failure")
        finally:
            shutil.rmtree = orig_rmtree
        if created_dir is not None and created_dir.exists():
            shutil.rmtree(created_dir)

    with isolated_temp_dir("mare-public-api-empty-", base_dir=base_dir) as empty_root:
        shim_bin = empty_root / "bin"
        shim_bin.mkdir()
        luals_empty = shim_bin / "lua-language-server"
        luals_empty.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            "from pathlib import Path\n"
            "for arg in sys.argv[1:]:\n"
            "    if arg.startswith('--check_out_path='):\n"
            "        Path(arg.split('=', 1)[1]).write_text('')\n"
            "sys.exit(0)\n"
        )
        luals_empty.chmod(0o755)
        ws_empty = empty_root / "ws"
        workspace_at(ws_empty)
        orig_path = os.environ.get("PATH", "")
        try:
            os.environ["PATH"] = f"{shim_bin}{os.pathsep}{orig_path}"
            try:
                diagnose(ws_empty)
            except RuntimeError as error:
                if "empty report" not in str(error):
                    raise
            else:
                raise RuntimeError("diagnose accepted empty report with exit 0")
        finally:
            os.environ["PATH"] = orig_path

    # Deterministic cross-process barrier + failure isolation test.
    with isolated_temp_dir("mare-public-api-barrier-", base_dir=base_dir) as bar_root:
        shim_bin = bar_root / "bin"
        shim_bin.mkdir()
        barrier_file = bar_root / "barrier.txt"
        conc_tmp = bar_root / "tmp"
        conc_tmp.mkdir()
        luals_shim = shim_bin / "lua-language-server"
        luals_shim.write_text(
            "#!/usr/bin/env python3\n"
            "import fcntl, os, sys, time\n"
            "from pathlib import Path\n"
            "args = dict(a.split('=', 1) for a in sys.argv[1:] if '=' in a)\n"
            "meta = Path(args['--metapath'])\n"
            "ws = Path(args['--check'])\n"
            "report = Path(args['--check_out_path'])\n"
            "meta.mkdir(parents=True, exist_ok=True)\n"
            "(meta / 'marker.txt').write_text(os.environ['MARE_WORKER_ID'])\n"
            "barrier = Path(os.environ['MARE_BARRIER_FILE'])\n"
            "with barrier.open('a+') as fh:\n"
            "    fcntl.flock(fh, fcntl.LOCK_EX)\n"
            "    fh.write(f\"{os.environ['MARE_WORKER_ID']}|{meta}|{ws}\\n\")\n"
            "    fh.flush()\n"
            "deadline = time.monotonic() + 15\n"
            "lines = []\n"
            "while time.monotonic() < deadline:\n"
            "    lines = [l for l in barrier.read_text().splitlines() if l]\n"
            "    if len(lines) >= 2:\n"
            "        break\n"
            "    time.sleep(0.02)\n"
            "if len(lines) < 2:\n"
            "    raise SystemExit('barrier timed out')\n"
            "metas = {l.split('|')[1] for l in lines}\n"
            "if len(metas) != 2:\n"
            "    raise SystemExit(f'expected 2 distinct metapaths, got {metas}')\n"
            "if os.environ.get('MARE_LUALS_FAIL') == '1':\n"
            "    raise SystemExit(2)\n"
            "peer_meta = Path([l.split('|')[1] for l in lines if not l.startswith(os.environ['MARE_WORKER_ID'] + '|')][0])\n"
            "while time.monotonic() < deadline and peer_meta.exists():\n"
            "    time.sleep(0.02)\n"
            "if peer_meta.exists():\n"
            "    raise SystemExit('failing peer metapath was not cleaned up')\n"
            "if not (meta / 'marker.txt').is_file() or not ws.is_dir():\n"
            "    raise SystemExit('sibling cleanup deleted active worker files')\n"
            "report.write_text('{}')\n"
        )
        luals_shim.chmod(0o755)
        cmd = [sys.executable, str(script_path), "--plugin", "status"]
        barrier_envs = [
            dict(
                os.environ,
                PATH=f"{shim_bin}{os.pathsep}{os.environ.get('PATH', '')}",
                MARE_TMPDIR=str(conc_tmp),
                TMPDIR=str(conc_tmp),
                TEMP=str(conc_tmp),
                TMP=str(conc_tmp),
                MARE_BARRIER_FILE=str(barrier_file),
                MARE_WORKER_ID=f"w{idx}",
                MARE_LUALS_FAIL="1" if idx == 0 else "0",
            )
            for idx in range(2)
        ]
        results = run_concurrent_workers([cmd, cmd], ROOT, barrier_envs, timeout=60.0)
        if results[0][0] == 0:
            raise RuntimeError("expected failing barrier worker 0 to exit non-zero")
        if results[1][0] != 0:
            raise RuntimeError(
                f"surviving barrier worker 1 failed ({results[1][0]}): {results[1][1]}{results[1][2]}"
            )
        if list(conc_tmp.iterdir()):
            raise RuntimeError(
                f"barrier test left TMPDIR residue: {list(conc_tmp.iterdir())}"
            )

    # Verify descendant holding stdout/stderr pipes is killed with its process tree in bounded time.
    with isolated_temp_dir("mare-public-api-desc-", base_dir=base_dir) as desc_dir:
        pid_file = desc_dir / "descendant.pid"
        # Parent spawns a descendant that inherits stdout/stderr pipes and blocks,
        # then parent exits so only the descendant keeps the pipes open.
        parent_script = (
            "import subprocess, sys; "
            "subprocess.Popen([sys.executable, '-c', "
            "'import os, pathlib, threading; "
            "pathlib.Path(os.environ[\"MARE_DESC_PID_FILE\"]).write_text(str(os.getpid())); "
            "threading.Event().wait()']); "
            "sys.stdout.write('parent-done\\n'); sys.stdout.flush()"
        )
        desc_env = dict(os.environ, MARE_DESC_PID_FILE=str(pid_file))
        start = time.monotonic()
        try:
            run_concurrent_workers(
                [[sys.executable, "-c", parent_script]],
                ROOT,
                [desc_env],
                timeout=0.8,
            )
        except subprocess.TimeoutExpired:
            pass
        else:
            raise RuntimeError("run_concurrent_workers did not time out on pipe-holding descendant")
        elapsed = time.monotonic() - start
        if elapsed >= 5.0:
            raise RuntimeError(f"run_concurrent_workers took too long ({elapsed:.2f}s) to clean up descendant")
        if not pid_file.is_file():
            raise RuntimeError("descendant did not record its PID")
        descendant_pid = int(pid_file.read_text(encoding="utf-8").strip())
        reap_deadline = time.monotonic() + 2.0
        alive = True
        while time.monotonic() < reap_deadline:
            if not _is_pid_alive(descendant_pid):
                alive = False
                break
            if os.name == "nt":
                _win_kernel32().SwitchToThread()
            else:
                os.sched_yield()
        if alive:
            raise RuntimeError(f"descendant process {descendant_pid} survived timeout cleanup")

    # Verify second-Popen failure terminates and reaps the first started process.
    spawned: list[subprocess.Popen[str]] = []
    try:
        spawned.append(
            subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
        )
        try:
            spawned.append(
                subprocess.Popen(
                    ["/nonexistent-mare-binary-for-cleanup-test"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
            )
        except OSError:
            pass
    finally:
        for proc in spawned:
            if proc.poll() is None:
                proc.kill()
            proc.communicate(timeout=5)
    if spawned[0].returncode is None:
        raise RuntimeError("first process was not terminated after second Popen failure")

    with isolated_temp_dir("mare-public-api-conc-", base_dir=base_dir) as conc_tmp:
        env = dict(
            os.environ,
            MARE_TMPDIR=str(conc_tmp),
            TMPDIR=str(conc_tmp),
            TEMP=str(conc_tmp),
            TMP=str(conc_tmp),
        )
        cmd = [sys.executable, str(script_path), "--plugin", "status"]
        results = run_concurrent_workers([cmd, cmd], ROOT, [env, env], timeout=120.0)
        for idx, (rc, stdout, stderr) in enumerate(results):
            if rc != 0:
                raise RuntimeError(
                    f"concurrent check_lua_public_api worker {idx} failed ({rc}): {stdout}{stderr}"
                )
        if list(conc_tmp.iterdir()):
            raise RuntimeError(
                f"concurrent check_lua_public_api left TMPDIR residue: {list(conc_tmp.iterdir())}"
            )

    # Verify MARE_TMPDIR overrides default system temp dir even when TMPDIR/TEMP/TMP are unset,
    # propagates TMPDIR into lua-language-server, and cleans up on SIGTERM.
    with isolated_temp_dir("mare-public-api-env-", base_dir=base_dir) as env_root:
        shim_bin = env_root / "bin"
        shim_bin.mkdir()
        mare_tmp = env_root / "mare-tmp"
        mare_tmp.mkdir()
        record_file = env_root / "recorded.txt"
        luals_rec = shim_bin / "lua-language-server"
        luals_rec.write_text(
            "#!/usr/bin/env python3\n"
            "import os, sys, threading\n"
            "from pathlib import Path\n"
            "args = dict(a.split('=', 1) for a in sys.argv[1:] if '=' in a)\n"
            "rec = Path(os.environ['MARE_RECORD_FILE'])\n"
            "rec.write_text(f\"{args['--metapath']}|{os.environ.get('TMPDIR', '')}\\n\")\n"
            "if os.environ.get('MARE_BLOCK_FOR_SIGTERM') == '1':\n"
            "    threading.Event().wait()\n"
            "Path(args['--check_out_path']).write_text('{}')\n"
        )
        luals_rec.chmod(0o755)
        clean_env = {k: v for k, v in os.environ.items() if k not in ("TMPDIR", "TEMP", "TMP", "MARE_TMPDIR")}
        clean_env["PATH"] = f"{shim_bin}{os.pathsep}{os.environ.get('PATH', '')}"
        clean_env["MARE_TMPDIR"] = str(mare_tmp)
        clean_env["MARE_RECORD_FILE"] = str(record_file)
        res = subprocess.run(
            [sys.executable, str(script_path), "--plugin", "status"],
            cwd=ROOT,
            env=clean_env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        if res.returncode != 0:
            raise RuntimeError(f"MARE_TMPDIR check failed ({res.returncode}): {res.stdout}{res.stderr}")
        recorded_meta, recorded_tmpdir = record_file.read_text().strip().split("|", 1)
        if not Path(recorded_meta).resolve().is_relative_to(mare_tmp.resolve()):
            raise RuntimeError(f"metapath escaped MARE_TMPDIR: {recorded_meta} not under {mare_tmp}")
        if not recorded_tmpdir or not Path(recorded_tmpdir).resolve().is_relative_to(mare_tmp.resolve()):
            raise RuntimeError(f"child lua-language-server TMPDIR escaped MARE_TMPDIR: {recorded_tmpdir!r}")
        if list(mare_tmp.iterdir()):
            raise RuntimeError(f"MARE_TMPDIR run left residue: {list(mare_tmp.iterdir())}")

        if os.name != "nt":
            term_tmp = env_root / "term-tmp"
            term_tmp.mkdir()
            term_record = env_root / "term-ready.txt"
            term_env = dict(clean_env, MARE_TMPDIR=str(term_tmp), MARE_RECORD_FILE=str(term_record), MARE_BLOCK_FOR_SIGTERM="1")
            proc = subprocess.Popen(
                [sys.executable, str(script_path), "--plugin", "status"],
                cwd=ROOT,
                env=term_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 15.0
                while time.monotonic() < deadline and not term_record.is_file():
                    if proc.poll() is not None:
                        break
                    os.sched_yield()
                if not term_record.is_file():
                    raise RuntimeError("SIGTERM test shim never became ready")
                proc.terminate()
                proc.communicate(timeout=10)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate(timeout=5)
            if list(term_tmp.iterdir()):
                raise RuntimeError(f"SIGTERM left temp residue in MARE_TMPDIR: {list(term_tmp.iterdir())}")

        tp_dir = env_root / "third-party"
        (tp_dir / "ext").mkdir(parents=True)
        (tp_dir / "ext/init.lua").write_text('require("mare_kernel.boot")\n')
        tp_res = subprocess.run(
            [sys.executable, str(script_path), "--third-party-dir", str(tp_dir)],
            cwd=ROOT,
            env=dict(os.environ, MARE_TMPDIR=str(mare_tmp)),
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        if tp_res.returncode != 0 or "warning:" not in tp_res.stdout or "mare_kernel.boot" not in tp_res.stdout:
            raise RuntimeError(f"--third-party-dir expected warning and exit 0, got rc={tp_res.returncode}: {tp_res.stdout}{tp_res.stderr}")
    print("public Lua type isolation: positive and negative fixtures passed")
