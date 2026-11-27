#!/usr/bin/env python3
"""Isolated, offline mare performance probes; `--check` checks validity only."""
import argparse, fcntl, json, math, os, pty, re, select, shutil, socket, statistics, struct, subprocess, sys, tempfile, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
BIN, RUNS = os.path.abspath("target/release/mare"), int(os.environ.get("BENCH_RUNS", "10"))
SESSION_ID, REPLAY_TAIL, INPUT_BOX = "bench-session-100", "BENCH_REPLAY_TAIL", "╰".encode()
ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])")
STARTUP_ROW = re.compile(r"^\s*(\d+\.\d{3})\s+(\d+\.\d{3})  (.*)$")
PLUGIN_LABEL = re.compile(r"^plugin (\S+) \((builtin|package|user|project)\)(?: \[[^\]]+\])?$")
SLOW_CALLBACK = re.compile(r"slow callback: \[[^\]]+\] \S+ ran \d+\.\dms \(budget 2ms\)")
LATE_TICK = re.compile(r"lateness_ms=([0-9.]+).*animation tick late")
def parse_frame_log(path):
    if not os.path.exists(path): return []
    return [(int(p[0]) / 1000.0, int(p[1]) / 1000.0 if len(p) > 1 else None, p[2] == "1" if len(p) > 2 else True)
            for line in Path(path).read_text(errors="ignore").splitlines() if (p := line.split())]
def gap_stats(rows):
    renders, ts = [r[0] for r in rows], [r[1] for r in rows if r[1] is not None]
    gaps = [max(0.0, ts[i] - ts[i - 1]) for i in range(1, len(ts))]
    return {"frames": len(rows), "render_p99_ms": round(percentile(renders, .99), 3) if renders else 0.0,
            "render_max_ms": round(max(renders), 3) if renders else 0.0, "gap_p50_ms": round(percentile(gaps, .5), 2) if gaps else 0.0,
            "gap_p99_ms": round(percentile(gaps, .99), 2) if gaps else 0.0, "gap_max_ms": round(max(gaps), 2) if gaps else 0.0,
            "gaps_over_160ms": sum(g > 160.0 for g in gaps), "gaps_over_1000ms": sum(g > 1000.0 for g in gaps)}
def child_env(home, extra=None):
    env = {k: v for k, v in os.environ.items() if not (k.startswith("MARE_") or k.endswith("_API_KEY") or k.lower() in {"http_proxy", "https_proxy", "all_proxy"})}
    env.update({"HOME": home, "MARE_HOME": home, "MARE_UPDATE_CHECK": "false", "MARE_TEST_OFFLINE": "1", "TERM": "xterm-256color", **(extra or {})})
    return env
def isolate_network():
    if sys.platform == "darwin": return
    uid, gid = os.getuid(), os.getgid(); os.unshare(os.CLONE_NEWUSER | os.CLONE_NEWNET)
    for name, value in (("setgroups", "deny"), ("uid_map", f"{uid} {uid} 1"), ("gid_map", f"{gid} {gid} 1")): Path(f"/proc/self/{name}").write_text(value)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as link:
        name, flags = struct.unpack("16sH14x", fcntl.ioctl(link, 0x8913, struct.pack("16sH14x", b"lo", 0)))
        fcntl.ioctl(link, 0x8914, struct.pack("16sH14x", name, flags | 1))
def temp_root(): return next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), tempfile.gettempdir())
def make_home():
    home = tempfile.mkdtemp(prefix="mare-bench-", dir=temp_root()); os.makedirs(f"{home}/config")
    Path(f"{home}/config/init.lua").write_text("\n".join(('mare.setup { model = "faux/echo", update_check = false, peer = { allow = { "agent.list_messages", "options.get" } } }', 'mare.plugin("onboarding", { plugins = false })', 'mare.plugin("provider-faux", { script_json = "[]" })', 'mare.plugin("llm", { models_dev = false })', 'mare.provider("ollama", { list_models = false })', "")))
    return home
def spawn(args, home, cwd, extra=None):
    pid, fd = pty.fork()
    if pid == 0:
        try: os.chdir(cwd); os.execve(BIN, [BIN, *args], child_env(home, extra))
        finally: os._exit(127)
    import termios; fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    return pid, fd
def screen_text(buf): return ANSI.sub(b"", buf)
def read_until(fd, needle, timeout, plain=False):
    buf, end = b"", time.monotonic() + timeout
    while time.monotonic() < end:
        if select.select([fd], [], [], 0.05)[0]:
            try: data = os.read(fd, 65536)
            except OSError: break
            if not data: break
            for query, reply in ((b"\x1b]11;?", b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"), (b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?62;22c")):
                if query in data: os.write(fd, reply)
            if needle is not None:
                buf += data
                if needle in (screen_text(buf) if plain else buf): return True, buf
    return False, buf
def drain(fd, seconds): read_until(fd, None, seconds)
def kill(pid, fd):
    _DARWIN_PEAK_RSS.pop(pid, None)
    try: os.kill(pid, 9)
    except ProcessLookupError: pass
    end = time.monotonic() + 10
    while time.monotonic() < end:
        try:
            if os.waitpid(pid, os.WNOHANG)[0]: break
        except ChildProcessError: break
        drain(fd, 0.02)
    os.close(fd)
_DARWIN_PEAK_RSS = {}
def darwin_ps(pid):
    out = subprocess.check_output(["ps", "-o", "rss=,time=", "-p", str(pid)], text=True).strip()
    if not out: raise RuntimeError(f"missing process {pid}")
    rss_s, cpu_s = out.split(None, 1); rss = int(rss_s) / 1024; _DARWIN_PEAK_RSS[pid] = max(_DARWIN_PEAK_RSS.get(pid, 0.0), rss)
    days, _, rest = cpu_s.rpartition("-")
    secs = sum(float(p) * m for p, m in zip(reversed(rest.split(":")), (1, 60, 3600))) + (int(days) * 86400 if days else 0)
    return rss, _DARWIN_PEAK_RSS[pid], secs * 1000
def proc_status(pid, key):
    if sys.platform == "darwin":
        rss, hwm, _ = darwin_ps(pid)
        if key == "VmRSS": return rss
        if key == "VmHWM": return hwm
        raise RuntimeError(f"unsupported Darwin status key {key}")
    line = next((s for s in Path(f"/proc/{pid}/status").read_text().splitlines() if s.startswith(key + ":")), None)
    if line is None: raise RuntimeError(f"missing {key} for process {pid}")
    return int(line.split()[1]) / 1024
def parse_proc_stat(line, hz): fields = line.rsplit(")", 1)[1].split(); return (int(fields[11]) + int(fields[12])) * 1000 / hz
def cpu_ms(pid): return darwin_ps(pid)[2] if sys.platform == "darwin" else parse_proc_stat(Path(f"/proc/{pid}/stat").read_text(), os.sysconf("SC_CLK_TCK"))
class TurnCPU:
    def __init__(self): self.started = self.completed = None
    def feed(self, event, cpu):
        kind, data = event.get("type"), event.get("data", {})
        if event.get("turn_id") not in (None, data.get("turn_id")): raise RuntimeError("event and payload turn_id differ")
        if kind == "turn/started":
            if self.started or not data.get("turn_id") or not math.isfinite(cpu): raise RuntimeError("invalid or repeated turn/started")
            self.started = (data["turn_id"], cpu)
        elif kind == "turn/completed":
            if not self.started or self.completed is not None or data.get("turn_id") != self.started[0] or data.get("status") != "completed":
                raise RuntimeError("mismatched or unsuccessful turn/completed")
            self.completed = cpu
    def result(self):
        if not self.started or self.completed is None or not math.isfinite(self.completed) or self.completed < self.started[1]:
            raise RuntimeError("incomplete or invalid turn CPU sample")
        return self.completed - self.started[1]
def plugin_loads(path):
    lines = Path(path).read_text().splitlines()
    if not lines or lines[0].split() != ["clock", "self", "step"]: raise RuntimeError(f"{path}: invalid --startuptime header")
    loads = {}
    for line in lines[1:]:
        row = STARTUP_ROW.match(line)
        if not row: raise RuntimeError(f"{path}: unrecognised --startuptime row {line!r}")
        label = PLUGIN_LABEL.match(row.group(3))
        if label: loads[f"{label.group(1)} ({label.group(2)})"] = float(row.group(2))
    if not loads: raise RuntimeError(f"{path}: no plugin load rows")
    return loads
def percentile(values, p): return sorted(values)[min(len(values) - 1, math.ceil(len(values) * p) - 1)]
def rpc(home, pid, method, params):
    path = f"{home}/state/peers/{pid}.sock"
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(3)
        client.connect(path)
        with client.makefile("rwb") as stream:
            def call(number, name, arguments):
                stream.write(json.dumps({"jsonrpc": "2.0", "id": number, "method": name, "params": arguments}).encode() + b"\n"); stream.flush()
                for _ in range(100):
                    line = stream.readline()
                    if not line: raise RuntimeError("peer closed before its reply")
                    obj = json.loads(line)
                    if obj.get("method") in ("stream.failed", "lagged", "dropped"): raise RuntimeError("peer event stream incomplete")
                    if obj.get("id") == number:
                        if "error" in obj: raise RuntimeError(f"{name}: {obj['error']}")
                        return obj["result"]
                raise RuntimeError("too many unrelated peer messages")
            init = call(1, "initialize", {"protocol_version": "3.0", "capabilities": {}})
            if init.get("protocol_version") != "3.0": raise RuntimeError("peer did not negotiate protocol 3.0")
            return call(2, method, params)
def check_idle(home, pid):
    st = rpc(home, pid, "status.get", {})
    if st.get("state") != "idle": raise RuntimeError(f"tui not idle: {st.get('state')} {st.get('message', '')}")
    return st
def wait_ready(action, fd, timeout=20):
    deadline, last = time.monotonic() + timeout, None
    while time.monotonic() < deadline:
        try:
            return action()
        except (OSError, RuntimeError, TypeError, ValueError, KeyError) as exc:
            last = exc
            drain(fd, 0.02)
    raise RuntimeError(f"readiness check timed out: {last}")
def write_long_session(home, cwd, turns=100):
    directory = f"{home}/state/sessions/--" + "".join(c if c.isalnum() or c in "._" else "-" for c in cwd.lstrip("/")) + "--"
    os.makedirs(directory, exist_ok=True); path = f"{directory}/2026-01-01T00-00-00_bench.jsonl"
    with open(path, "w") as f:
        f.write(json.dumps({"type": "session", "version": 3, "session_id": SESSION_ID, "cwd": cwd, "created_at_ms": 1}) + "\n")
        parent = None
        for i in range(turns):
            for role, prefix, text in (("user", "u", f"question {i}: explain step {i}"), ("assistant", "a", f"## Step {i}\n\n" + ("Explanation with `code` and **bold**. " * 25) + (f"\n\n{REPLAY_TAIL}" if i == turns - 1 else ""))):
                ident = f"{prefix}{i}"
                f.write(json.dumps({"type": "message", "entry_id": ident, "parent_entry_id": parent, "message": {"role": role, "content": [{"type": "text", "text": text}]}}) + "\n"); parent = ident
    return path
def validate_replay(session_id, messages):
    if session_id != SESSION_ID or not isinstance(messages, list) or len(messages) != 200 or any(not isinstance(m, dict) or m.get("role") != ("user" if i % 2 == 0 else "assistant") for i, m in enumerate(messages)) or REPLAY_TAIL not in json.dumps(messages[-1]):
        raise RuntimeError("resumed session id, message count, order or tail is incorrect")
def verify_replay(home, pid):
    session_id = rpc(home, pid, "status.get", {}).get("session", {}).get("session_id")
    if session_id != SESSION_ID: raise RuntimeError("unexpected active session")
    rows, seen, cursor = [], set(), None
    while True:
        page = rpc(home, pid, "agent.list_messages", {"session_id": session_id, "limit": 100, **({"cursor": cursor} if cursor else {})})
        if not isinstance(page.get("data"), list): raise TypeError("agent.list_messages returned no data array")
        rows.extend(page["data"]); cursor = page.get("next_cursor")
        if not cursor: break
        if cursor in seen or len(rows) > 200 or len(seen) >= 200: raise RuntimeError("invalid message pagination")
        seen.add(cursor)
    validate_replay(session_id, rows)
def bench_startup(home, cwd):
    times, samples = [], {}
    for i in range(-1, RUNS):
        path, start = f"{home}/startuptime-{i}.txt", time.monotonic(); pid, fd = spawn(["--startuptime", path], home, cwd)
        try:
            ready, _ = read_until(fd, INPUT_BOX, 10); elapsed = (time.monotonic() - start) * 1000
            if not ready: raise RuntimeError(f"startup sample {i} did not draw the input box")
            loads = wait_ready(lambda: plugin_loads(path), fd, 2)
        finally: kill(pid, fd)
        if i < 0: continue
        times.append(elapsed)
        for name, ms in loads.items(): samples.setdefault(name, []).append(ms)
    if not times or any(k not in samples for k in ("agent (builtin)", "llm (builtin)", "provider-faux (builtin)", "session (builtin)", "tools (builtin)")) or any(len(v) != RUNS for v in samples.values()): raise RuntimeError("missing startup or per-plugin timing samples")
    return statistics.median(times), samples
def bench_idle(home, cwd):
    pid, fd = spawn([], home, cwd)
    try:
        if not read_until(fd, INPUT_BOX, 10)[0]: raise RuntimeError("idle benchmark did not draw the input box")
        drain(fd, 5); rss, before = proc_status(pid, "VmRSS"), cpu_ms(pid); drain(fd, 10)
        return rss, cpu_ms(pid) - before
    finally: kill(pid, fd)
def bench_replay(home, cwd):
    path, start = write_long_session(home, os.path.realpath(cwd)), time.monotonic(); pid, fd = spawn(["--resume", path], home, cwd)
    try:
        ready, screen = read_until(fd, INPUT_BOX, 20)
        if not ready: raise RuntimeError("replay did not draw the input box")
        resume_ms = (time.monotonic() - start) * 1000
        if REPLAY_TAIL.encode() not in screen_text(screen) and not read_until(fd, REPLAY_TAIL.encode(), 20, plain=True)[0]: raise RuntimeError("replay tail did not render")
        wait_ready(lambda: verify_replay(home, pid), fd)
        return proc_status(pid, "VmRSS"), proc_status(pid, "VmHWM"), resume_ms
    finally: kill(pid, fd)
def bench_frames(home, cwd):
    start, end = "MARE_BENCH_STREAM_STARTED", "MARE_BENCH_STREAM_FINISHED"
    text = start + "\n\n" + "\n\n".join(f"### Section {i}\n\nSome **bold**, `code`, and a table:\n\n| a | b |\n|---|---|\n| {i} | {i * 2} |\n\n```python\nfor x in range(10):\n    print(x)\n```" for i in range(120)) + "\n\n" + end
    script, log = f"{home}/stream.json", f"{home}/frames.log"; Path(script).write_text(json.dumps([{"text": text, "delay_ms": 3}]))
    pid, fd = spawn([], home, cwd, {"MARE_FAUX_SCRIPT": script, "MARE_FRAME_LOG": log, "MARE_LOG": "debug"})
    try:
        if not read_until(fd, INPUT_BOX, 10)[0]: raise RuntimeError("stream benchmark did not draw the input box")
        wait_ready(lambda: check_idle(home, pid), fd); os.write(fd, b"bench-go")
        if not read_until(fd, b"bench-go", 5, plain=True)[0]: raise RuntimeError("stream prompt was not echoed")
        os.write(fd, b"\r")
        if not read_until(fd, start.encode(), 10, plain=True)[0]: raise RuntimeError("stream did not start")
        initial = len(parse_frame_log(log)); completed, _ = read_until(fd, end.encode(), 30, plain=True); drain(fd, 0.25)
    finally: kill(pid, fd)
    values = [r[0] for r in parse_frame_log(log)[initial:]]
    if not completed or len(values) < 100: raise RuntimeError(f"incomplete stream: completed={completed}, frames={len(values)}")
    logpath = f"{home}/state/log/mare.log"
    slow = sum(bool(SLOW_CALLBACK.search(line)) for line in Path(logpath).read_text(errors="ignore").splitlines()) if os.path.exists(logpath) else 0
    return percentile(values, 0.99), slow, len(values)

def _run_anim(home, cwd, marker, *, steps=None, plugin_lua=None, timeout=60, args=None, focus_out_after=None, focus_in_after=None, full_access=False):
    with open(f"{home}/config/init.lua", "a") as f:
        f.write('mare.plugin("onboarding", { enabled = false, plugins = false })\n')
        if full_access: f.write('mare.plugin("permissions", { preset = "full-access" })\n')
    log, extra, initial, fi_idx = f"{home}/frames.log", {"MARE_FRAME_LOG": f"{home}/frames.log", "MARE_LOG": "debug", "MARE_NO_TERM_PROBE": "1"}, 0, None
    if steps is not None:
        script = f"{home}/anim.json"; Path(script).write_text(json.dumps(steps)); extra["MARE_FAUX_SCRIPT"] = script
    if plugin_lua is not None:
        pdir = Path(home) / "config/plugins"; pdir.mkdir(parents=True, exist_ok=True); (pdir / "anim_probe.lua").write_text(plugin_lua)
    pid, fd = spawn(["--trust" if full_access else "--no-trust", *(args or [])], home, cwd, extra)
    try:
        if not read_until(fd, INPUT_BOX, 15)[0]: raise RuntimeError("anim bench did not draw input box")
        if steps is not None:
            wait_ready(lambda: check_idle(home, pid), fd); initial = len(parse_frame_log(log)); os.write(fd, b"anim-go")
            if not read_until(fd, b"anim-go", 5, plain=True)[0]: raise RuntimeError("anim prompt was not echoed")
            os.write(fd, b"\r")
        if focus_out_after is not None:
            drain(fd, focus_out_after); os.write(fd, b"\x1b[O")
            if focus_in_after is not None:
                drain(fd, focus_in_after); fi_idx = len([r for r in parse_frame_log(log)[initial:] if r[2]]); os.write(fd, b"\x1b[I")
        completed, _ = read_until(fd, marker.encode(), timeout, plain=True); drain(fd, 0.25)
    finally: kill(pid, fd)
    if not completed: raise RuntimeError(f"anim bench did not reach marker {marker!r}")
    rows = [r for r in parse_frame_log(log)[initial:] if r[2]]
    lp = f"{home}/state/log/mare.log"
    ll = Path(lp).read_text(errors="ignore").splitlines() if os.path.exists(lp) else []
    lv = [float(m.group(1)) for line in ll if (m := LATE_TICK.search(line))]
    return rows, len(lv), round(max(lv), 2) if lv else 0.0, sum(bool(SLOW_CALLBACK.search(line)) for line in ll), fi_idx
def _silent_plugin(duration_ms, marker):
    return ('return { name = "anim_probe", inject = { "task" }, apply = function(ctx) ctx:on("ui/ready", function() ctx:spawn(function()\n'
            f'mare.task.sleep(300); mare.ui.working("Waiting on silent tool"); mare.task.sleep({duration_ms}); mare.ui.working(nil)\n'
            f'mare.ui.push("info", {{ markdown = "{marker}" }})\nend) end) end }}\n')
def _anim_meta(late, late_max, slow): return {"late_ticks": late, "late_max_ms": late_max, "slow_callbacks": slow}
def bench_anim_tab_switch(home, cwd):
    rows, late, lm, slow, fi = _run_anim(home, cwd, "ANIM_TAB_DONE", plugin_lua=_silent_plugin(7000, "ANIM_TAB_DONE"), timeout=25, focus_out_after=1.0, focus_in_after=1.5)
    return {"total": gap_stats(rows), "after_focus_in": gap_stats(rows[fi:] if fi is not None else []), **_anim_meta(late, lm, slow)}
def bench_anim_silent(home, cwd):
    rows, late, lm, slow, _ = _run_anim(home, cwd, "ANIM_SILENT_DONE", plugin_lua=_silent_plugin(35000, "ANIM_SILENT_DONE"), timeout=60)
    t0 = rows[0][1] if rows and rows[0][1] is not None else 0.0
    f30, a30 = [r for r in rows if r[1] is not None and (r[1] - t0) < 30000.0], [r for r in rows if r[1] is not None and (r[1] - t0) >= 30000.0]
    return {"total": gap_stats(rows), "first_30s": gap_stats(f30), "after_30s": gap_stats(([f30[-1]] if f30 else []) + a30), **_anim_meta(late, lm, slow)}
def bench_anim_unfocused(home, cwd):
    rows, late, lm, slow, _ = _run_anim(home, cwd, "ANIM_UNFOCUSED_DONE", plugin_lua=_silent_plugin(5500, "ANIM_UNFOCUSED_DONE"), timeout=20, focus_out_after=1.0)
    t0 = rows[0][1] if rows and rows[0][1] is not None else 0.0
    return {"total": gap_stats(rows), "unfocused_window": gap_stats([r for r in rows if r[1] is not None and (r[1] - t0) >= 1200.0]), **_anim_meta(late, lm, slow)}
def bench_anim_heavy_output(home, cwd):
    with open(f"{home}/config/init.lua", "a") as f: f.write('mare.plugin("tools", { live_tail_bytes = 5000000 })\n')
    cmd = "python3 -c 'import sys,time; c=\"0123456789abcdef\"*4+\"\\n\"; block=c*256; [(sys.stdout.write(block), sys.stdout.flush(), time.sleep(0.02)) for _ in range(120)]'"
    steps = [{"tool_calls": [{"name": "bash", "arguments": {"command": cmd, "description": "heavy live output"}}]}, {"text": "ANIM_HEAVY_DONE"}]
    rows, late, lm, slow, _ = _run_anim(home, cwd, "ANIM_HEAVY_DONE", steps=steps, timeout=60, full_access=True)
    return {**gap_stats(rows), **_anim_meta(late, lm, slow)}
def bench_anim_session_fsync(home, cwd):
    steps = [{"tool_calls": [{"name": "bash", "arguments": {"command": f"echo step-{i}", "description": f"step {i}"}}]} for i in range(15)] + [{"text": "ANIM_FSYNC_DONE"}]
    rows, late, lm, slow, _ = _run_anim(home, cwd, "ANIM_FSYNC_DONE", steps=steps, timeout=60, full_access=True)
    return {**gap_stats(rows), **_anim_meta(late, lm, slow)}
def bench_anim_large_stream(home, cwd):
    path = write_long_session(home, os.path.realpath(cwd), turns=100)
    text = "ANIM_LARGE_START\n\n" + "\n\n".join(f"### Section {i}\n\nSome **bold**, `code`, and a table:\n\n| a | b |\n|---|---|\n| {i} | {i * 2} |\n\n```python\nfor x in range(10):\n    print(x)\n```" for i in range(120)) + "\n\nANIM_LARGE_DONE"
    rows, late, lm, slow, _ = _run_anim(home, cwd, "ANIM_LARGE_DONE", steps=[{"text": text, "delay_ms": 3}], timeout=45, args=["--resume", path], full_access=False)
    return {**gap_stats(rows), **_anim_meta(late, lm, slow)}
def validate_request(body):
    messages = body.get("messages") if isinstance(body, dict) else None
    if not isinstance(messages, list) or body.get("model") != "echo" or not isinstance(body.get("tools"), list) or not body["tools"]:
        raise RuntimeError("first request lacks messages or tools")
    if not all(isinstance(m, dict) for m in messages): raise RuntimeError("invalid first-request messages")
    tools = {t.get("function", {}).get("name") for t in body["tools"] if isinstance(t, dict) and isinstance(t.get("function"), dict)}
    if not {"read", "write", "bash"} <= tools: raise RuntimeError("first request lacks required tools")
    system = [m for m in messages if m.get("role") in ("system", "developer")]
    users = [m for m in messages if m.get("role") == "user"]
    if not system or "You are a coding agent" not in json.dumps(system) or "BENCH_SKILL_MARKER" not in json.dumps(system) or "BENCH_REQUEST" not in json.dumps(users):
        raise RuntimeError("first request lacks system prompt, skill or prompt marker")
class ProbeHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args): pass
    def do_POST(self):
        arrived = time.monotonic()
        data = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.requests.append((arrived, self.path, json.loads(data)))
        if not self.server.proceed.wait(30): self.send_error(504); return
        response = (b'data: {"id":"bench","choices":[{"index":0,"delta":{"content":"BENCH_REPLY"}}]}\n\n'
                    b'data: {"id":"bench","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n')
        self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.send_header("Content-Length", str(len(response))); self.end_headers(); self.wfile.write(response)
def bench_headless(home, cwd):
    server = HTTPServer(("127.0.0.1", 0), ProbeHandler); server.requests, server.proceed = [], threading.Event()
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True); thread.start()
    process, stderr, stderr_thread = None, bytearray(), None
    try:
        skill = f"{home}/.agents/skills/bench-probe"; os.makedirs(skill, exist_ok=True)
        Path(f"{skill}/SKILL.md").write_text("---\nname: bench-probe\ndescription: BENCH_SKILL_MARKER\n---\nBenchmark only.\n")
        with open(f"{home}/config/init.lua", "a") as f:
            f.write('mare.setup { model = "bench/echo" }\n')
            f.write('mare.provider("bench", { base_url = "http://127.0.0.1:' + str(server.server_port) + '/v1", no_key = true, list_models = false, models = { { model_id = "echo", context_window = 128000 } } })\n')
        start, tracker, token, settled, session_seen = time.monotonic(), TurnCPU(), None, None, False
        process = subprocess.Popen([BIN, "-p", "--json", "--stream-deltas", "--no-session", "--no-trust", "BENCH_REQUEST"], cwd=cwd, env=child_env(home), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def capture_stderr():
            while chunk := process.stderr.read(4096): stderr[:] = (stderr + chunk)[-4096:]
        stderr_thread = threading.Thread(target=capture_stderr, daemon=True); stderr_thread.start()
        buf, deadline = b"", time.monotonic() + 40
        while time.monotonic() < deadline and settled is None:
            if not select.select([process.stdout], [], [], 0.1)[0]: continue
            chunk = os.read(process.stdout.fileno(), 65536)
            if not chunk: break
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1); event = json.loads(line)
                kind, data = event.get("type"), event.get("data", {})
                if kind in ("turn/started", "turn/completed"):
                    tracker.feed(event, cpu_ms(process.pid))
                    if kind == "turn/started": server.proceed.set()
                if kind == "item/delta" and data.get("kind") == "text" and data.get("delta") and token is None:
                    if not tracker.started or data.get("turn_id") != tracker.started[0]: raise RuntimeError("text delta belongs to another turn")
                    token = time.monotonic()
                if kind in ("turn.failed", "stream.failed", "lagged", "dropped"): raise RuntimeError(f"incomplete headless event stream: {kind}")
                if kind == "session":
                    if session_seen or data.get("protocol_version") != "3.0": raise RuntimeError("headless output is not protocol 3.0")
                    session_seen = True
                if kind == "settled":
                    if data.get("exit_code") != 0 or data.get("text") != "BENCH_REPLY":
                        raise RuntimeError("headless result did not complete with the expected answer")
                    settled = time.monotonic()
        if process.wait(timeout=5) != 0 or not session_seen or token is None or settled is None:
            raise RuntimeError("incomplete headless benchmark")
        if len(server.requests) != 1: raise RuntimeError(f"expected exactly one request, got {len(server.requests)}")
        arrived, path, body = server.requests[0]
        if not path.endswith("/chat/completions") or not start <= arrived <= token <= settled:
            raise RuntimeError("invalid first-request timing or endpoint")
        validate_request(body)
        return (arrived - start) * 1000, (token - start) * 1000, (settled - start) * 1000, tracker.result()
    except Exception as exc:
        if process and process.poll() is None: process.kill()
        if process: process.wait()
        if stderr_thread: stderr_thread.join()
        raise RuntimeError(f"{exc}; stderr: {stderr.decode(errors='replace')}") from exc
    finally:
        server.proceed.set()
        if process and process.poll() is None: process.kill(); process.wait()
        if process and process.stdout: process.stdout.close()
        if stderr_thread: stderr_thread.join()
        if process and process.stderr: process.stderr.close()
        server.shutdown(); thread.join(); server.server_close()
def bench_reload(home, cwd):
    init, log, plugins = Path(home) / "config/init.lua", Path(home) / "reload.log", Path(home) / "config/plugins"
    config = init.read_text(); log.unlink(missing_ok=True); plugins.mkdir(parents=True, exist_ok=True)
    (plugins / "bench-reload.lua").write_text('return { name = "bench-reload", apply = function(ctx) ctx:on("reload/completed", function(r)\n' + f'assert(mare.fs.write({json.dumps(str(log), ensure_ascii=False)}, tostring(#(r.reloaded or {{}})) .. "\\n", {{ append = true }}))\nend) end }}\n')
    pid, fd = spawn([], home, cwd); times, cpus, failures = [], [], []
    try:
        if not read_until(fd, INPUT_BOX, 10)[0]: raise RuntimeError("reload benchmark did not draw the input box")
        session_id = wait_ready(lambda: check_idle(home, pid), fd).get("session", {}).get("session_id")
        if not session_id: raise RuntimeError("reload benchmark did not report active session_id")
        current = wait_ready(lambda: rpc(home, pid, "options.get", {"session_id": session_id, "keys": ["reduce_motion"]})["reduce_motion"], fd)
        if not isinstance(current, bool): raise TypeError("reload benchmark did not reach a readable configuration")
        for i in range(RUNS):
            desired = not current; Path(init).write_text(config + f"mare.setup {{ reduce_motion = {str(desired).lower()} }}\n")
            try:
                os.write(fd, b"/reload")
                if not read_until(fd, b"/reload", 5, plain=True)[0]: raise RuntimeError("reload command did not appear")
                before, start = cpu_ms(pid), time.monotonic(); os.write(fd, b"\r"); deadline, lines = time.monotonic() + 30, []
                while time.monotonic() < deadline:
                    drain(fd, 0.02); lines = log.read_text().splitlines(keepends=True) if log.exists() else []
                    if len(lines) > i and lines[i].endswith("\n"): break
                if len(lines) <= i or not lines[i].endswith("\n") or int(lines[i]) <= 0: raise RuntimeError("reload completion barrier was not observed")
                actual = rpc(home, pid, "options.get", {"session_id": session_id, "keys": ["reduce_motion"]})["reduce_motion"]
                if actual is not desired: raise RuntimeError("reload did not apply the changed configuration")
                times.append((time.monotonic() - start) * 1000); cpus.append(cpu_ms(pid) - before); current = actual
            except (OSError, RuntimeError, KeyError, ValueError) as exc: failures.append(str(exc)); break
    finally: kill(pid, fd)
    return times, cpus, failures
def bench_many_empty(home, cwd):
    n_plugins = 50
    def run_for(count):
        h = make_home()
        try:
            if count > 0:
                pdir = Path(h) / "config/plugins"; pdir.mkdir(parents=True, exist_ok=True)
                for i in range(count): (pdir / f"e{i:03d}.lua").write_text(f'return {{ name = "e{i:03d}", inject = {{ "commands" }}, apply = function() end }}\n')
            times = []
            for i in range(-1, RUNS):
                start = time.monotonic(); pid, fd = spawn([], h, cwd)
                try:
                    ready, _ = read_until(fd, INPUT_BOX, 15); elapsed = (time.monotonic() - start) * 1000
                    if not ready: raise RuntimeError(f"many-empty N={count} sample {i} did not draw input box")
                finally: kill(pid, fd)
                if i >= 0: times.append(elapsed)
            return statistics.median(times)
        finally: shutil.rmtree(h, ignore_errors=True)
    base_ms, many_ms = run_for(0), run_for(n_plugins)
    return base_ms, many_ms, max(0.0, (many_ms - base_ms) / n_plugins)
def valid_numbers(values): return bool(values) and all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0 for v in values)
def main():
    global BIN
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default=BIN)
    for flag in ("check", "keep", "anim"): parser.add_argument("--" + flag, action="store_true")
    parser.add_argument("--only", action="append", default=[], help="run only named probe(s)")
    args = parser.parse_args(); BIN = os.path.abspath(args.binary)
    if not os.path.isfile(BIN) or RUNS < 1: raise SystemExit("benchmark requires a built binary and BENCH_RUNS >= 1")
    directory = Path(temp_root()).resolve()
    for parent in (directory, *directory.parents):
        if os.path.lexists(parent / ".git"): raise SystemExit(f"benchmark temp directory has a .git ancestor: {parent}/.git")
    try: isolate_network()
    except (OSError, AttributeError) as exc: raise SystemExit(f"cannot isolate benchmark network: {exc}")
    results, errors = {}, []
    probes = (
        (("anim_tab_switch", bench_anim_tab_switch, None), ("anim_silent", bench_anim_silent, None),
         ("anim_unfocused", bench_anim_unfocused, None), ("anim_heavy_output", bench_anim_heavy_output, None),
         ("anim_session_fsync", bench_anim_session_fsync, None), ("anim_large_stream", bench_anim_large_stream, None))
        if args.anim else
        (("startup", bench_startup, ("input_median_ms", "plugins")), ("many_empty", bench_many_empty, ("n0_median_ms", "n50_median_ms", "marginal_ms_per_plugin")),
         ("idle", bench_idle, ("rss_mb", "cpu_ms_10s")), ("replay", bench_replay, ("rss_mb", "peak_vmhwm_mb", "input_ready_ms")),
         ("frames", bench_frames, ("p99_ms", "slow_callbacks", "frame_count")), ("headless", bench_headless, ("first_request_ms", "first_text_ms", "settled_ms", "turn_cpu_ms")),
         ("reload", bench_reload, ("median_ms", "p95_ms", "samples_ms", "cpu_ms", "failures")))
    )
    for name, probe, labels in probes:
        if args.only and name not in args.only: continue
        home, cwd = make_home(), tempfile.mkdtemp(prefix="mare-bench-cwd-", dir=temp_root())
        try:
            result = probe(home, cwd)
            if labels is None: results[name] = result
            else:
                if name == "startup":
                    median, plugins = result
                    if median <= 0 or not valid_numbers([median, *[v for values in plugins.values() for v in values]]): raise RuntimeError("invalid startup timing")
                    result = (median, {p: {"median_ms": statistics.median(v), "p95_ms": percentile(v, .95), "samples_ms": v} for p, v in plugins.items()})
                elif name == "reload":
                    times, cpus, failures = result
                    if failures or len(times) != RUNS or len(cpus) != RUNS or not valid_numbers(times) or min(times, default=0) <= 0 or not valid_numbers(cpus):
                        raise RuntimeError(f"{len(failures)} failed; {len(times)}/{RUNS} samples: {failures}")
                    result = (statistics.median(times), percentile(times, .95), times, cpus, 0)
                elif not valid_numbers(result) or result[0] <= 0 or (name == "replay" and result[1] < result[0]) or (name == "frames" and result[2] < 100):
                    raise RuntimeError("invalid measurement")
                if len(result) != len(labels): raise RuntimeError("missing measurement fields")
                results[name] = dict(zip(labels, result))
        except (OSError, RuntimeError, TypeError, ValueError, KeyError, subprocess.SubprocessError) as exc: errors.append(f"{name}: {exc}")
        finally:
            if args.keep: print(f"{name} artifacts: home={home} cwd={cwd}")
            else: shutil.rmtree(home, ignore_errors=True); shutil.rmtree(cwd, ignore_errors=True)
    print(json.dumps(results, indent=2, sort_keys=True))
    if not args.anim:
        print("Loopback-only network; response waits for turn/started. CPU/VmHWM exclude children; RSS is not heap. References: startup ≤80 ms (target 60 ms), marginal ≤0.6 ms/plugin, idle ≤50 MB, replay ≤120 MB, frame p99 ≤16 ms, plugin sync apply ≤5 ms.")
        print("Plugin times are --startuptime self samples, not a framework-cost A/B; references are not pass/fail.")
    for error in errors: print(f"incomplete: {error}")
    return int((args.anim or args.check) and bool(errors))
if __name__ == "__main__": raise SystemExit(main())
