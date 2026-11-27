#!/usr/bin/env python3
"""Verify release/debug binary seams, faux exclusion, and release-excluded test modules."""

import argparse
import base64
import http.server
import json
import os
import pathlib
import queue
import re
import subprocess
import sys
import tempfile
import threading
import unittest

SCAN_ROOT = "crates"


def _split_top_level_args(s: str) -> list[str]:
    args: list[str] = []
    depth = 0
    in_str = False
    escape = False
    start = 0
    for i, ch in enumerate(s):
        if escape:
            escape = False
            continue
        if ch == "\\" and in_str:
            escape = True
            continue
        if ch == '"':
            in_str = not in_str
            continue
        if in_str:
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            part = s[start:i].strip()
            if part:
                args.append(part)
            start = i + 1
    tail = s[start:].strip()
    if tail:
        args.append(tail)
    return args


def eval_cfg(
    expr: str,
    *,
    debug_assertions: bool,
    feature_faux: bool | None = None,
    cfg_test: bool | None = False,
) -> bool | None:
    """Evaluate a Rust cfg(...) predicate under given flags. Returns None if unknown."""
    s = expr.strip()
    if not s:
        return None
    if s == "debug_assertions":
        return debug_assertions
    if s == "test":
        return cfg_test
    if re.fullmatch(r'feature\s*=\s*"faux"', s):
        return feature_faux
    for op in ("not", "all", "any"):
        prefix = f"{op}("
        if s.startswith(prefix) and s.endswith(")"):
            # Verify outer parens match each other
            depth = 0
            in_str = False
            escape = False
            matched_end = -1
            for i, ch in enumerate(s):
                if escape:
                    escape = False
                    continue
                if ch == "\\" and in_str:
                    escape = True
                    continue
                if ch == '"':
                    in_str = not in_str
                    continue
                if in_str:
                    continue
                if ch == "(":
                    depth += 1
                elif ch == ")":
                    depth -= 1
                    if depth == 0:
                        matched_end = i
                        break
            if matched_end == len(s) - 1:
                inner = s[len(prefix) : -1]
                args = _split_top_level_args(inner)
                if op == "not":
                    if len(args) != 1:
                        return None
                    v = eval_cfg(
                        args[0],
                        debug_assertions=debug_assertions,
                        feature_faux=feature_faux,
                        cfg_test=cfg_test,
                    )
                    return None if v is None else (not v)
                vals = [
                    eval_cfg(
                        a,
                        debug_assertions=debug_assertions,
                        feature_faux=feature_faux,
                        cfg_test=cfg_test,
                    )
                    for a in args
                ]
                if op == "all":
                    if any(v is False for v in vals):
                        return False
                    if all(v is True for v in vals):
                        return True
                    return None
                if op == "any":
                    if any(v is True for v in vals):
                        return True
                    if all(v is False for v in vals):
                        return False
                    return None
    return None


def _cfg_excludes_release(pred: str, test_feature: str | None = None) -> bool:
    if test_feature:
        pred = re.sub(r'feature\s*=\s*"' + re.escape(test_feature) + r'"', 'test', pred)
    return eval_cfg(pred, debug_assertions=False, cfg_test=False) is False



def list_excluded_test_modules(root: pathlib.Path) -> list[str]:
    """List test modules/files under crates/*/tests/** gated by #[cfg(debug_assertions)]."""
    hits: list[str] = []
    for path in sorted((root / SCAN_ROOT).glob("*/tests/**/*.rs")):
        rel = path.relative_to(root).as_posix()
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        for idx, line in enumerate(lines, start=1):
            m = re.search(r"#\[cfg\(([^]]+)\)\]", line)
            if m and _cfg_excludes_release(m.group(1)):
                next_line = lines[idx].strip() if idx < len(lines) else ""
                hits.append(f"{rel}:{idx}: {next_line}")
    return hits


def rpc_exchange(
    binary: pathlib.Path,
    requests: list[dict],
    env: dict[str, str],
    cwd: pathlib.Path,
    extra_args: list[str] | None = None,
    *,
    no_session: bool = True,
    start_new_session: bool = False,
) -> list[dict]:
    args = [str(binary), "--rpc", "--yolo"]
    if no_session:
        args.append("--no-session")
    if extra_args:
        args.extend(extra_args)
    proc = subprocess.Popen(
        args,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        cwd=str(cwd),
        start_new_session=start_new_session,
    )
    responses: list[dict] = []
    q: queue.Queue[str | None] = queue.Queue()

    def _reader() -> None:
        assert proc.stdout is not None
        try:
            for line in proc.stdout:
                q.put(line)
        finally:
            q.put(None)

    reader = threading.Thread(target=_reader, daemon=True)
    reader.start()
    try:
        assert proc.stdin is not None
        for req in requests:
            req_id = req.get("id")
            proc.stdin.write(json.dumps(req) + "\n")
            proc.stdin.flush()
            if req_id is None:
                continue
            while True:
                try:
                    line = q.get(timeout=30.0)
                except queue.Empty:
                    break
                if line is None:
                    break
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    obj = json.loads(stripped)
                except json.JSONDecodeError:
                    continue
                if "id" in obj:
                    responses.append(obj)
                    if obj.get("id") == req_id:
                        break
        proc.stdin.close()
        proc.stdin = None
        _, err = proc.communicate(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        _, err = proc.communicate()
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        reader.join(timeout=1)
    if not responses and proc.returncode != 0:
        raise RuntimeError(
            f"rpc failed (rc={proc.returncode}): {err.strip()}"
        )
    return responses


def rpc_initialize() -> dict:
    return {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "3.0"}}


def _probe_pipe_first_name(
    binary: pathlib.Path,
    mare_home: pathlib.Path,
    ws: pathlib.Path,
    base_env: dict[str, str],
    *,
    expect_active: bool,
) -> None:
    forced_name = r"\\.\pipe\mare-0123456789abcdef0123456789abcdef"
    pipe_env = dict(base_env, MARE_TEST_PIPE_FIRST_NAME=forced_name)
    proc = subprocess.Popen(
        [str(binary), "--rpc", "--peer", "--yolo", "--no-session"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=pipe_env,
        cwd=str(ws),
    )
    try:
        assert proc.stdin is not None
        proc.stdin.write(json.dumps(rpc_initialize()) + "\n")
        proc.stdin.flush()
        sock_path = mare_home / "state" / "peers" / f"{proc.pid}.sock"
        import time

        deadline = time.monotonic() + 30.0
        pipe_name = ""
        while time.monotonic() < deadline:
            if sock_path.is_file():
                pipe_name = sock_path.read_text(encoding="utf-8").strip()
                if pipe_name:
                    break
            if proc.poll() is not None:
                break
            time.sleep(0.05)
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": {}}) + "\n")
        proc.stdin.close()
        proc.stdin = None
        out, err = proc.communicate(timeout=30)
        if proc.returncode != 0:
            raise RuntimeError(
                f"MARE_TEST_PIPE_FIRST_NAME probe failed (rc={proc.returncode}): {out}{err}"
            )
        if not pipe_name:
            raise RuntimeError(
                f"MARE_TEST_PIPE_FIRST_NAME probe did not write peer socket file while running: {sock_path}"
            )
        if not pipe_name.startswith(r"\\.\pipe\mare-"):
            raise RuntimeError(
                f"MARE_TEST_PIPE_FIRST_NAME probe got unexpected endpoint: {pipe_name!r}"
            )
        if expect_active:
            if pipe_name != forced_name:
                raise RuntimeError(
                    f"MARE_TEST_PIPE_FIRST_NAME was not active in debug: got {pipe_name!r}, expected {forced_name!r}"
                )
        else:
            if pipe_name == forced_name:
                raise RuntimeError(
                    f"MARE_TEST_PIPE_FIRST_NAME was not inert in release: got forced name {pipe_name!r}"
                )
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def _run_seams_probe(binary: pathlib.Path, *, expect_active: bool) -> None:
    if not binary.is_file():
        raise RuntimeError(f"binary not found: {binary}")
    mode_label = "debug" if expect_active else "release"

    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    with tempfile.TemporaryDirectory(prefix=f"mare-seam-{mode_label}-", dir=tmp_root) as tmp:
        root = pathlib.Path(tmp)
        mare_home = root / "mare_home"
        ws = root / "ws"
        mare_home.mkdir()
        ws.mkdir()

        base_env = dict(os.environ, MARE_HOME=str(mare_home), MARE_TMPDIR=tmp, TMPDIR=tmp, TEMP=tmp, TMP=tmp)

        # 1. MARE_TEST_FS_FAULT=write-then-eio:fault.txt
        fs_env = dict(base_env, MARE_TEST_FS_FAULT="write-then-eio:fault.txt")
        content = b"hello-release-seam"
        resps = rpc_exchange(
            binary,
            [
                rpc_initialize(),
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "fs.write",
                    "params": {
                        "path": "fault.txt",
                        "data_base64": base64.b64encode(content).decode("ascii"),
                        "atomic": False,
                    },
                },
                {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {}},
            ],
            env=fs_env,
            cwd=ws,
        )
        write_resp = next((r for r in resps if r.get("id") == 2), None)
        if not write_resp:
            raise RuntimeError(f"MARE_TEST_FS_FAULT ({mode_label}): missing fs.write reply")
        written = (ws / "fault.txt").read_bytes() if (ws / "fault.txt").is_file() else b""
        if expect_active:
            err_msg = str(write_resp.get("error", ""))
            if "error" not in write_resp or "MARE_TEST_FS_FAULT" not in err_msg:
                raise RuntimeError(
                    f"MARE_TEST_FS_FAULT was not active in debug: {write_resp}"
                )
            if written != content[: len(content) // 2]:
                raise RuntimeError(
                    f"MARE_TEST_FS_FAULT debug partial write mismatch: got {written!r}"
                )
        else:
            if "error" in write_resp or "result" not in write_resp:
                raise RuntimeError(f"MARE_TEST_FS_FAULT was not inert: {write_resp}")
            if written != content:
                raise RuntimeError(
                    f"MARE_TEST_FS_FAULT corrupted write in release: got {written!r}"
                )

        # 2. MARE_TEST_UNSTABLE_READ=2026-01-01T00-00-00_busy.jsonl
        busy_name = "2026-01-01T00-00-00_busy.jsonl"
        busy_path = ws / busy_name
        header = {
            "type": "session",
            "version": 3,
            "session_id": "2026-01-01T00-00-00_busy",
            "cwd": str(ws),
            "created_at_ms": 1,
        }
        busy_path.write_text(json.dumps(header) + "\n", encoding="utf-8")
        unstable_env = dict(base_env, MARE_TEST_UNSTABLE_READ=busy_name)
        resps = rpc_exchange(
            binary,
            [
                rpc_initialize(),
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.load",
                    "params": {"path": str(busy_path)},
                },
                {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {}},
            ],
            env=unstable_env,
            cwd=ws,
            no_session=False,
        )
        resume_resp = next((r for r in resps if r.get("id") == 2), None)
        if not resume_resp:
            raise RuntimeError(f"MARE_TEST_UNSTABLE_READ ({mode_label}): missing session.load reply")
        if expect_active:
            err_msg = str(resume_resp.get("error", ""))
            if "error" not in resume_resp or "changed" not in err_msg.lower() and "changing" not in err_msg.lower():
                raise RuntimeError(
                    f"MARE_TEST_UNSTABLE_READ was not active in debug: {resume_resp}"
                )
        else:
            if "error" in resume_resp or "result" not in resume_resp:
                raise RuntimeError(
                    f"MARE_TEST_UNSTABLE_READ did not succeed cleanly in release: {resume_resp}"
                )

        # 3. MARE_TEST_CLIPBOARD=osc52: run via config/init.lua session/start hook
        # (not plugin top-level, which is forbidden by the plugin loader) and verify
        # the probe actually executed and wrote MARE_PROBE_OUT.
        config_dir = mare_home / "config"
        config_dir.mkdir(parents=True, exist_ok=True)
        probe_out = root / "clipboard_probe.txt"
        (config_dir / "init.lua").write_text(
            'mare.on("session/started", function()\n'
            '  local r = mare.clipboard.copy("probe")\n'
            '  local line = string.format("ok=%s;method=%s;error=%s\\n",\n'
            '    tostring(r.ok), tostring(r.method or ""), tostring(r.error or ""))\n'
            '  local f = assert(io.open(os.getenv("MARE_PROBE_OUT"), "w"))\n'
            "  f:write(line)\n"
            "  f:close()\n"
            "end)\n",
            encoding="utf-8",
        )
        clip_env = dict(base_env, MARE_TEST_CLIPBOARD="osc52", MARE_PROBE_OUT=str(probe_out))
        for var in ("DISPLAY", "WAYLAND_DISPLAY", "SSH_TTY", "SSH_CONNECTION", "TMUX", "WSL_DISTRO_NAME"):
            clip_env[var] = ""
        resps = rpc_exchange(
            binary,
            [
                rpc_initialize(),
                {"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": {}},
            ],
            env=clip_env,
            cwd=ws,
            no_session=False,
            start_new_session=(os.name != "nt"),
        )
        init_resp = next((r for r in resps if r.get("id") == 1), None)
        if not init_resp or "error" in init_resp:
            raise RuntimeError(f"MARE_TEST_CLIPBOARD ({mode_label}) rpc init failed: {init_resp}")
        if not probe_out.is_file():
            raise RuntimeError(
                f"MARE_TEST_CLIPBOARD ({mode_label}) probe did not execute (missing {probe_out})"
            )
        probe_text = probe_out.read_text(encoding="utf-8")
        if expect_active:
            if "native clipboard:" in probe_text:
                raise RuntimeError(
                    f"MARE_TEST_CLIPBOARD was not active in debug (native ran): {probe_text!r}"
                )
            if "ok=true;method=osc52" not in probe_text and "OSC 52 clipboard copy failed over SSH:" not in probe_text:
                raise RuntimeError(
                    f"MARE_TEST_CLIPBOARD debug probe unexpected result: {probe_text!r}"
                )
        else:
            if "MARE_TEST_CLIPBOARD" in probe_text:
                raise RuntimeError(
                    f"MARE_TEST_CLIPBOARD was not inert in release: {probe_text!r}"
                )
            if "ok=true;method=native" not in probe_text and "native clipboard:" not in probe_text:
                raise RuntimeError(
                    f"MARE_TEST_CLIPBOARD release probe did not reach native clipboard path: {probe_text!r}"
                )

        # 4. MARE_TEST_OFFLINE and MARE_TEST_ALLOW_LIVE via local mock HTTP proxy
        _probe_offline_seams(
            binary,
            mare_home,
            ws,
            root,
            base_env,
            expect_active=expect_active,
            mode_label=mode_label,
        )

        # 5. MARE_TEST_PIPE_FIRST_NAME (Windows named pipe endpoint)
        if sys.platform == "win32":
            _probe_pipe_first_name(
                binary, mare_home, ws, base_env, expect_active=expect_active
            )


class _MockProxyHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self.server.hits += 1  # type: ignore[attr-defined]
        body = b"from-mock-proxy"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        pass


def _probe_offline_seams(
    binary: pathlib.Path,
    mare_home: pathlib.Path,
    ws: pathlib.Path,
    root: pathlib.Path,
    base_env: dict[str, str],
    *,
    expect_active: bool,
    mode_label: str,
) -> None:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _MockProxyHandler)
    server.hits = 0  # type: ignore[attr-defined]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        proxy_url = f"http://127.0.0.1:{server.server_port}"
        config_dir = mare_home / "config"
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "init.lua").write_text(
            'mare.on("session/started", function()\n'
            "  local ok, res = pcall(function()\n"
            '    return mare.http.request({ url = "http://example.com/", timeout_ms = 5000 })\n'
            "  end)\n"
            "  local line\n"
            "  if ok then\n"
            '    line = "ok=true;body=" .. tostring(res:text()) .. "\\n"\n'
            "  else\n"
            '    line = "ok=false;error=" .. tostring(res) .. "\\n"\n'
            "  end\n"
            '  local f = assert(io.open(os.getenv("MARE_PROBE_OUT"), "w"))\n'
            "  f:write(line)\n"
            "  f:close()\n"
            "end)\n",
            encoding="utf-8",
        )
        proxy_base = dict(base_env)
        for var in (
            "NO_PROXY",
            "no_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "MARE_TEST_OFFLINE",
            "MARE_TEST_ALLOW_LIVE",
        ):
            proxy_base.pop(var, None)
        proxy_base["HTTP_PROXY"] = proxy_url
        proxy_base["http_proxy"] = proxy_url

        def _run_case(
            label: str, extra_env: dict[str, str], probe_name: str
        ) -> tuple[str, int]:
            server.hits = 0  # type: ignore[attr-defined]
            probe_out = root / probe_name
            env = dict(proxy_base, MARE_PROBE_OUT=str(probe_out), **extra_env)
            resps = rpc_exchange(
                binary,
                [
                    rpc_initialize(),
                    {"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": {}},
                ],
                env=env,
                cwd=ws,
                no_session=False,
            )
            init_resp = next((r for r in resps if r.get("id") == 1), None)
            if not init_resp or "error" in init_resp:
                raise RuntimeError(
                    f"{label} ({mode_label}) rpc init failed: {init_resp}"
                )
            if not probe_out.is_file():
                raise RuntimeError(
                    f"{label} ({mode_label}) probe did not execute (missing {probe_out})"
                )
            return probe_out.read_text(encoding="utf-8"), int(server.hits)  # type: ignore[attr-defined]

        text_off, hits_off = _run_case(
            "MARE_TEST_OFFLINE",
            {"MARE_TEST_OFFLINE": "1"},
            "http_offline_probe.txt",
        )
        if expect_active:
            if "ok=false;" not in text_off or "offline test guard" not in text_off:
                raise RuntimeError(
                    f"MARE_TEST_OFFLINE was not active in debug: {text_off!r}"
                )
            if hits_off != 0:
                raise RuntimeError(
                    f"MARE_TEST_OFFLINE debug probe contacted mock proxy ({hits_off} hits): {text_off!r}"
                )
        else:
            if (
                "offline test guard" in text_off
                or "ok=true;body=from-mock-proxy" not in text_off
            ):
                raise RuntimeError(
                    f"MARE_TEST_OFFLINE was not inert in release: {text_off!r}"
                )
            if hits_off < 1:
                raise RuntimeError(
                    f"MARE_TEST_OFFLINE release probe did not reach mock proxy ({hits_off} hits): {text_off!r}"
                )

        text_live, hits_live = _run_case(
            "MARE_TEST_ALLOW_LIVE",
            {"MARE_TEST_OFFLINE": "1", "MARE_TEST_ALLOW_LIVE": "1"},
            "http_live_probe.txt",
        )
        if expect_active:
            if "ok=true;body=from-mock-proxy" not in text_live or hits_live < 1:
                raise RuntimeError(
                    f"MARE_TEST_ALLOW_LIVE did not override MARE_TEST_OFFLINE in debug (hits={hits_live}): {text_live!r}"
                )
        else:
            if "ok=true;body=from-mock-proxy" not in text_live or hits_live < 1:
                raise RuntimeError(
                    f"MARE_TEST_ALLOW_LIVE changed request behavior in release (hits={hits_live}): {text_live!r}"
                )
    finally:
        server.shutdown()
        server.server_close()


def verify_release_seams_inert(binary: pathlib.Path) -> None:
    """Counter-examples proving registered seams are inert in a release binary."""
    _run_seams_probe(binary, expect_active=False)


def verify_debug_seams_active(binary: pathlib.Path) -> None:
    """Positive examples proving registered seams are active in a debug binary."""
    _run_seams_probe(binary, expect_active=True)


FAUX_ARTIFACT_MARKERS = (
    b"MARE_FAUX_SCRIPT",
    b"Faux Echo",
    b"provider-faux: script table configuration is not supported",
)


def verify_faux_absent(binary: pathlib.Path) -> None:
    if not binary.is_file():
        raise RuntimeError(f"release binary not found: {binary}")
    raw = binary.read_bytes()
    found_markers = [m.decode("ascii") for m in FAUX_ARTIFACT_MARKERS if m in raw]
    if found_markers:
        raise RuntimeError(f"release binary unexpectedly contains packaged faux artifact markers: {found_markers}")
    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    with tempfile.TemporaryDirectory(prefix="mare-faux-absent-", dir=tmp_root) as tmp:
        env = dict(os.environ, MARE_HOME=tmp, MARE_TMPDIR=tmp, TMPDIR=tmp, TEMP=tmp, TMP=tmp)
        resps = rpc_exchange(
            binary,
            [
                rpc_initialize(),
                {"jsonrpc": "2.0", "id": 2, "method": "llm.list_models", "params": {}},
                {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {}},
            ],
            env=env,
            cwd=pathlib.Path(tmp),
        )
        models_resp = next((r for r in resps if r.get("id") == 2), None)
        if not models_resp or "error" in models_resp or "result" not in models_resp:
            raise RuntimeError(f"llm.list_models failed on release binary: {models_resp}")
        models = models_resp["result"].get("data")
        if not isinstance(models, list) or not models:
            raise RuntimeError(f"llm.list_models returned empty/invalid models: {models_resp}")
        faux_models = [
            m for m in models
            if m.get("provider") == "faux" or str(m.get("ref", "")).startswith("faux/")
        ]
        if faux_models:
            raise RuntimeError(
                f"release binary unexpectedly registered faux models: {faux_models}"
            )


def verify_faux_present(binary: pathlib.Path) -> None:
    if not binary.is_file():
        raise RuntimeError(f"faux release binary not found: {binary}")
    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    with tempfile.TemporaryDirectory(prefix="mare-faux-present-", dir=tmp_root) as tmp:
        env = dict(os.environ, MARE_HOME=tmp, MARE_TMPDIR=tmp, TMPDIR=tmp, TEMP=tmp, TMP=tmp)
        proc = subprocess.run(
            [str(binary), "-p", "--model", "faux/echo", "--no-session", "hi"],
            input="",
            text=True,
            capture_output=True,
            env=env,
            cwd=tmp,
            timeout=30,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"faux release binary failed with faux/echo (rc={proc.returncode}): "
                f"{proc.stdout}{proc.stderr}"
            )
        if "echo: hi" not in proc.stdout:
            raise RuntimeError(
                f"faux release binary did not produce expected echo output: {proc.stdout!r}"
            )


class SeamSelfTests(unittest.TestCase):
    def _write_mock_bin(self, path: pathlib.Path, script_body: str) -> pathlib.Path:
        path.write_text(f"#!{sys.executable}\n{script_body}", encoding="utf-8")
        path.chmod(0o755)
        return path

    def test_verify_release_rejects_empty_passes(self):
        if os.name == "nt":
            self.skipTest("executable python shebang mock is POSIX-only")
        common_imports = "import base64, json, os, pathlib, sys\n"
        # 1. Missing session.load reply
        with tempfile.TemporaryDirectory() as tmp:
            mock = self._write_mock_bin(
                pathlib.Path(tmp) / "mare-mock",
                common_imports
                + """
if "--rpc" in sys.argv:
    for line in sys.stdin:
        req = json.loads(line)
        m = req.get("method")
        if m == "initialize":
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
        elif m == "fs.write":
            p = pathlib.Path(req["params"]["path"])
            p.write_bytes(base64.b64decode(req["params"]["data_base64"]))
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
        elif m == "session.load":
            pass  # drop reply!
        elif m == "shutdown":
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
""",
            )
            with self.assertRaisesRegex(RuntimeError, "missing session.load reply"):
                verify_release_seams_inert(mock)

        # 2. Unrelated error on session.load
        with tempfile.TemporaryDirectory() as tmp:
            mock = self._write_mock_bin(
                pathlib.Path(tmp) / "mare-mock",
                common_imports
                + """
if "--rpc" in sys.argv:
    for line in sys.stdin:
        req = json.loads(line)
        m = req.get("method")
        if m == "initialize":
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
        elif m == "fs.write":
            p = pathlib.Path(req["params"]["path"])
            p.write_bytes(base64.b64decode(req["params"]["data_base64"]))
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
        elif m == "session.load":
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": {"message": "bad params"}}), flush=True)
        elif m == "shutdown":
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
""",
            )
            with self.assertRaisesRegex(RuntimeError, "did not succeed cleanly"):
                verify_release_seams_inert(mock)

        # 3. Missing clipboard probe execution (MARE_PROBE_OUT not written)
        with tempfile.TemporaryDirectory() as tmp:
            mock = self._write_mock_bin(
                pathlib.Path(tmp) / "mare-mock",
                common_imports
                + """
if "--rpc" in sys.argv:
    for line in sys.stdin:
        req = json.loads(line)
        m = req.get("method")
        if m == "fs.write":
            p = pathlib.Path(req["params"]["path"])
            p.write_bytes(base64.b64decode(req["params"]["data_base64"]))
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
""",
            )
            with self.assertRaisesRegex(RuntimeError, "probe did not execute"):
                verify_release_seams_inert(mock)

    def test_verify_faux_probes_reject_unrelated_exit_and_empty_echo(self):
        if os.name == "nt":
            self.skipTest("executable python shebang mock is POSIX-only")
        with tempfile.TemporaryDirectory() as tmp:
            bad_absent = self._write_mock_bin(
                pathlib.Path(tmp) / "mare-bad-absent",
                'import json, sys\n'
                'for line in sys.stdin:\n'
                '    req = json.loads(line)\n'
                '    assert req.get("method") != "initialize" or req.get("params") == {"protocol_version": "3.0"}, req\n'
                '    if req.get("method") == "llm.list_models":\n'
                '        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"data": [{"provider": "faux", "ref": "faux/echo"}]}}), flush=True)\n'
                '    else:\n'
                '        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)\n',
            )
            with self.assertRaisesRegex(RuntimeError, "unexpectedly registered faux models"):
                verify_faux_absent(bad_absent)

            bad_present = self._write_mock_bin(
                pathlib.Path(tmp) / "mare-bad-present",
                "import sys; sys.exit(0)\n",
            )
            with self.assertRaisesRegex(RuntimeError, "expected echo output"):
                verify_faux_present(bad_present)

    def test_verify_faux_absent_rejects_packaged_but_unregistered_faux(self):
        if os.name == "nt":
            self.skipTest("executable python shebang mock is POSIX-only")
        with tempfile.TemporaryDirectory() as tmp:
            # Simulates faux bytecode packaged in the binary even when llm.list_models hides it.
            packaged_hidden = self._write_mock_bin(
                pathlib.Path(tmp) / "mare-packaged-hidden",
                'import json, sys\n'
                '# Embedded faux artifact marker: MARE_FAUX_SCRIPT / Faux Echo\n'
                'for line in sys.stdin:\n'
                '    req = json.loads(line)\n'
                '    if req.get("method") == "llm.list_models":\n'
                '        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"data": [{"provider": "openai", "ref": "openai/gpt-6.1-terra"}]}}), flush=True)\n'
                '    else:\n'
                '        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)\n',
            )
            with self.assertRaisesRegex(RuntimeError, "faux"):
                verify_faux_absent(packaged_hidden)

    def test_verify_release_and_debug_positive_and_windows_pipe_mock(self):
        if os.name == "nt":
            self.skipTest("executable python shebang mock is POSIX-only")
        mock_body = """
import base64, json, os, pathlib, sys, urllib.parse, urllib.request
mode = os.environ["MOCK_MODE"]
if "--peer" in sys.argv:
    peers = pathlib.Path(os.environ["MARE_HOME"]) / "state" / "peers"
    peers.mkdir(parents=True, exist_ok=True)
    sock = peers / f"{os.getpid()}.sock"
    forced = os.environ.get("MARE_TEST_PIPE_FIRST_NAME", "")
    chosen = forced if (mode == "debug" and forced) else r"\\\\.\\pipe\\mare-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    sock.write_text(chosen, encoding="utf-8")
    for line in sys.stdin:
        req = json.loads(line)
        assert req.get("method") != "initialize" or req.get("params") == {"protocol_version": "3.0"}, req
        if req.get("method") == "shutdown":
            sock.unlink(missing_ok=True)
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
    sys.exit(0)

if "--rpc" in sys.argv:
    if "--no-session" not in sys.argv and os.environ.get("MARE_PROBE_OUT"):
        if os.environ.get("HTTP_PROXY"):
            offline = os.environ.get("MARE_TEST_OFFLINE") == "1"
            allow_live = os.environ.get("MARE_TEST_ALLOW_LIVE") == "1"
            if mode == "debug" and offline and not allow_live:
                msg = "ok=false;error=offline test guard: example.com is not a loopback address\\n"
            else:
                proxy = urllib.parse.urlparse(os.environ["HTTP_PROXY"])
                opener = urllib.request.build_opener(
                    urllib.request.ProxyHandler({"http": f"http://{proxy.hostname}:{proxy.port}"})
                )
                with opener.open("http://example.com/", timeout=5) as resp:
                    body = resp.read().decode("utf-8")
                msg = f"ok=true;body={body}\\n"
        elif mode == "debug" and os.environ.get("MARE_TEST_CLIPBOARD") == "osc52":
            msg = "ok=false;method=;error=OSC 52 clipboard copy failed over SSH: no terminal\\n"
        else:
            msg = "ok=false;method=;error=native clipboard: unavailable; OSC 52 fallback: no terminal\\n"
        pathlib.Path(os.environ["MARE_PROBE_OUT"]).write_text(msg, encoding="utf-8")
    for line in sys.stdin:
        req = json.loads(line)
        m = req.get("method")
        assert m != "initialize" or req.get("params") == {"protocol_version": "3.0"}, req
        assert m != "session.load" or set(req.get("params", {})) == {"path"}, req
        if m == "fs.write":
            raw = base64.b64decode(req["params"]["data_base64"])
            p = pathlib.Path(req["params"]["path"])
            if mode == "debug" and os.environ.get("MARE_TEST_FS_FAULT") == f"write-then-eio:{p.name}":
                p.write_bytes(raw[: len(raw) // 2])
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": {"message": "injected I/O error (MARE_TEST_FS_FAULT)"}}), flush=True)
            else:
                p.write_bytes(raw)
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
        elif m == "session.load":
            sess = pathlib.Path(req["params"]["path"]).name
            if mode == "debug" and os.environ.get("MARE_TEST_UNSTABLE_READ") == sess:
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": {"message": "session file keeps changing while it is opened"}}), flush=True)
            else:
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"id": "busy"}}), flush=True)
        else:
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
"""
        with tempfile.TemporaryDirectory() as tmp:
            mock = self._write_mock_bin(pathlib.Path(tmp) / "mare-mock", mock_body)
            orig_mode = os.environ.get("MOCK_MODE")
            orig_plat = sys.platform
            try:
                sys.platform = "win32"
                os.environ["MOCK_MODE"] = "release"
                verify_release_seams_inert(mock)
                with self.assertRaises(RuntimeError):
                    verify_debug_seams_active(mock)

                os.environ["MOCK_MODE"] = "debug"
                verify_debug_seams_active(mock)
                with self.assertRaises(RuntimeError):
                    verify_release_seams_inert(mock)
            finally:
                sys.platform = orig_plat
                if orig_mode is None:
                    os.environ.pop("MOCK_MODE", None)
                else:
                    os.environ["MOCK_MODE"] = orig_mode

    def test_verify_release_rejects_active_offline_guard_and_debug_requires_allow_live(self):
        if os.name == "nt":
            self.skipTest("executable python shebang mock is POSIX-only")
        mock_body = """
import base64, json, os, pathlib, sys, urllib.parse, urllib.request
mode = os.environ["MOCK_MODE"]
if "--rpc" in sys.argv:
    if "--no-session" not in sys.argv and os.environ.get("MARE_PROBE_OUT"):
        if os.environ.get("HTTP_PROXY"):
            offline = os.environ.get("MARE_TEST_OFFLINE") == "1"
            allow_live = os.environ.get("MARE_TEST_ALLOW_LIVE") == "1"
            if mode == "release_unguarded":
                blocked = offline and not allow_live
            elif mode == "debug_no_offline":
                blocked = False
            elif mode == "debug_no_allow_live":
                blocked = offline
            else:
                blocked = False
            if blocked:
                msg = "ok=false;error=offline test guard: example.com is not a loopback address\\n"
            else:
                proxy = urllib.parse.urlparse(os.environ["HTTP_PROXY"])
                opener = urllib.request.build_opener(
                    urllib.request.ProxyHandler({"http": f"http://{proxy.hostname}:{proxy.port}"})
                )
                with opener.open("http://example.com/", timeout=5) as resp:
                    body = resp.read().decode("utf-8")
                msg = f"ok=true;body={body}\\n"
            pathlib.Path(os.environ["MARE_PROBE_OUT"]).write_text(msg, encoding="utf-8")
        else:
            if mode.startswith("debug") and os.environ.get("MARE_TEST_CLIPBOARD") == "osc52":
                msg = "ok=false;method=;error=OSC 52 clipboard copy failed over SSH: no terminal\\n"
            else:
                msg = "ok=false;method=;error=native clipboard: unavailable; OSC 52 fallback: no terminal\\n"
            pathlib.Path(os.environ["MARE_PROBE_OUT"]).write_text(msg, encoding="utf-8")
    for line in sys.stdin:
        req = json.loads(line)
        m = req.get("method")
        if m == "fs.write":
            raw = base64.b64decode(req["params"]["data_base64"])
            p = pathlib.Path(req["params"]["path"])
            if mode.startswith("debug") and os.environ.get("MARE_TEST_FS_FAULT") == f"write-then-eio:{p.name}":
                p.write_bytes(raw[: len(raw) // 2])
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": {"message": "injected I/O error (MARE_TEST_FS_FAULT)"}}), flush=True)
            else:
                p.write_bytes(raw)
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
        elif m == "session.load":
            sess = pathlib.Path(req["params"]["path"]).name
            if mode.startswith("debug") and os.environ.get("MARE_TEST_UNSTABLE_READ") == sess:
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": {"message": "session file keeps changing while it is opened"}}), flush=True)
            else:
                print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"id": "busy"}}), flush=True)
        else:
            print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {}}), flush=True)
"""
        with tempfile.TemporaryDirectory() as tmp:
            mock = self._write_mock_bin(pathlib.Path(tmp) / "mare-mock", mock_body)
            orig_mode = os.environ.get("MOCK_MODE")
            try:
                os.environ["MOCK_MODE"] = "release_unguarded"
                with self.assertRaisesRegex(RuntimeError, "MARE_TEST_OFFLINE"):
                    verify_release_seams_inert(mock)

                os.environ["MOCK_MODE"] = "debug_no_offline"
                with self.assertRaisesRegex(RuntimeError, "MARE_TEST_OFFLINE"):
                    verify_debug_seams_active(mock)

                os.environ["MOCK_MODE"] = "debug_no_allow_live"
                with self.assertRaisesRegex(RuntimeError, "MARE_TEST_ALLOW_LIVE"):
                    verify_debug_seams_active(mock)
            finally:
                if orig_mode is None:
                    os.environ.pop("MOCK_MODE", None)
                else:
                    os.environ["MOCK_MODE"] = orig_mode

    def test_rpc_exchange_and_probe_pipe_first_name_close_stdin_before_communicate(self):
        if os.name == "nt":
            self.skipTest("executable python shebang mock is POSIX-only")
        stub_body = (
            "import json, os, pathlib, sys\n"
            'if "--peer" in sys.argv:\n'
            '    peers = pathlib.Path(os.environ["MARE_HOME"]) / "state" / "peers"\n'
            "    peers.mkdir(parents=True, exist_ok=True)\n"
            '    sock = peers / f"{os.getpid()}.sock"\n'
            '    sock.write_text(os.environ["MARE_TEST_PIPE_FIRST_NAME"], encoding="utf-8")\n'
            "for line in sys.stdin:\n"
            "    req = json.loads(line)\n"
            '    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            stub = self._write_mock_bin(tmp_path / "rpc-stub", stub_body)
            mare_home = tmp_path / "mare-home"
            mare_home.mkdir()
            env = dict(os.environ, MARE_HOME=str(mare_home))
            resps = rpc_exchange(
                stub,
                [rpc_initialize()],
                env=env,
                cwd=tmp_path,
            )
            self.assertEqual(len(resps), 1)
            self.assertEqual(resps[0]["id"], 1)
            _probe_pipe_first_name(stub, mare_home, tmp_path, env, expect_active=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="Repository root")
    parser.add_argument("--self-test", action="store_true", help="Run internal unit tests")
    parser.add_argument(
        "--list-excluded-test-modules",
        action="store_true",
        help="List test modules excluded when not(debug_assertions)",
    )
    parser.add_argument("--verify-release", type=pathlib.Path, help="Verify seams inert on release binary")
    parser.add_argument("--verify-debug", type=pathlib.Path, help="Verify seams active on debug binary")
    parser.add_argument("--verify-faux-absent", type=pathlib.Path, help="Verify faux/echo absent on release binary")
    parser.add_argument("--verify-faux-present", type=pathlib.Path, help="Verify faux/echo works on faux binary")
    args = parser.parse_args()

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SeamSelfTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

    root = pathlib.Path(args.root).resolve()

    if args.list_excluded_test_modules:
        for item in list_excluded_test_modules(root):
            print(item)
        return 0

    if args.verify_release or args.verify_debug or args.verify_faux_absent or args.verify_faux_present:
        try:
            if args.verify_release:
                verify_release_seams_inert(args.verify_release.resolve())
                print(f"release seams inert verified: {args.verify_release}")
            if args.verify_debug:
                verify_debug_seams_active(args.verify_debug.resolve())
                print(f"debug seams active verified: {args.verify_debug}")
            if args.verify_faux_absent:
                verify_faux_absent(args.verify_faux_absent.resolve())
                print(f"release faux absent verified: {args.verify_faux_absent}")
            if args.verify_faux_present:
                verify_faux_present(args.verify_faux_present.resolve())
                print(f"release faux present verified: {args.verify_faux_present}")
        except RuntimeError as exc:
            print(f"release verification failed: {exc}", file=sys.stderr)
            return 1
        return 0

    parser.error("specify one of --self-test, --list-excluded-test-modules, --verify-release, --verify-debug, --verify-faux-absent, or --verify-faux-present")


if __name__ == "__main__":
    raise SystemExit(main())
