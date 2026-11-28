#!/usr/bin/env python3
"""Manual pre-release exploratory runner for a candidate `mare` binary.

Drives a release-candidate `mare` binary through a pseudo-terminal along the
first-time user path:
  1. startup & version check
  2. onboarding state
  3. empty-session common slash commands (/help, /status, /usage, /model)
  4. /side on an empty session
  5. first prompt with tool output
  6. keybindings: Ctrl+O (expand + collapse), Ctrl+T (transcript + Esc), Shift+Tab, Esc
  7. /side after a completed turn
  8. permission approval prompt
  9. subagent task
 10. /new (welcome banner restore)
 11. /resume picker + restore
 12. clean quit (Ctrl+D)

Saves the rendered terminal screen after each step into `--out DIR` along with
`manifest.json` and `review-prompt.md` pointing to
`docs/prerelease-exploratory-checklist.md` for a second-model parity review
against Claude Code, Codex, and Kimi Code.

Not a CI gate. Uses the real low-cost model (`opencode-go/glm-5.3-flash` by
default, reading `OPENCODE_API_KEY` or `MARE_LIVE_OPENCODE_KEY` through a 0600
file deleted before exec — never on argv, never logged). Pass `--faux` with a
faux-enabled debug binary for an offline smoke run of the driver itself.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import termios
import time
from collections.abc import Callable, Iterator
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MARE = ROOT / "target" / "release" / "mare"
DEFAULT_MODEL = os.environ.get("MARE_LIVE_MODEL", "opencode-go/glm-5.3-flash")
CHECKLIST_PATH = ROOT / "docs" / "prerelease-exploratory-checklist.md"

COLS = 110
ROWS = 30

# Offline scripted turns used only when `--faux` is explicitly selected.
FAUX_SCRIPT = [
    {"text": "Empty session side reply ok."},
    {"tool_calls": [{"name": "read", "arguments": {"path": "README.md"}}]},
    {"text": "Read README.md successfully."},
    {"text": "Post-turn side reply ok."},
    {"tool_calls": [{"name": "write", "arguments": {"path": "draft.txt", "content": "hello\n"}}]},
    {"text": "Wrote draft.txt after approval."},
    {
        "tool_calls": [
            {
                "name": "subagent",
                "arguments": {
                    "type": "general",
                    "description": "summarize readme",
                    "prompt": "Reply with one sentence.",
                    "wait": True,
                },
            }
        ]
    },
    {"text": "Subagent completed cleanly."},
]


@dataclasses.dataclass
class StepRecord:
    index: int
    name: str
    description: str
    capture_file: str
    ok: bool
    duration_s: float
    note: str = ""


class SimpleScreen:
    """Minimal stdlib-only VT100 screen tracker (used when `pyte` is not installed).

    Maintains a `rows x cols` grid handling common CSI cursor/erase sequences,
    alternate screen switches, CR/LF, and OSC/APC stripping so saved captures
    contain clean readable terminal frames without external dependencies.
    """

    def __init__(self, cols: int, rows: int) -> None:
        self.cols = cols
        self.rows = rows
        self._main = self._blank()
        self._alt = self._blank()
        self._use_alt = False
        self._buf = self._main
        self.row = 0
        self.col = 0
        self._saved = (0, 0)
        self._state = "text"
        self._esc = ""

    def _blank(self) -> list[list[str]]:
        return [[" "] * self.cols for _ in range(self.rows)]

    def _newline(self) -> None:
        if self.row + 1 < self.rows:
            self.row += 1
        else:
            self._buf.pop(0)
            self._buf.append([" "] * self.cols)

    def _put(self, ch: str) -> None:
        if ch < " " and ch != "\t":
            return
        if ch == "\t":
            nxt = min(self.cols - 1, ((self.col // 8) + 1) * 8)
            self.col = nxt
            return
        if self.col >= self.cols:
            self.col = 0
            self._newline()
        self._buf[self.row][self.col] = ch
        self.col += 1

    def _apply_csi(self, params: str, final: str) -> None:
        private = params.startswith("?")
        raw = params[1:] if private else params
        nums = [int(p) if p.isdigit() else 0 for p in raw.split(";")] if raw else []
        n1 = nums[0] if nums and nums[0] > 0 else 1

        if private:
            if final in ("h", "l") and any(p in (1049, 1047, 47) for p in nums):
                enter = final == "h"
                if enter and not self._use_alt:
                    self._saved = (self.row, self.col)
                    self._alt = self._blank()
                    self._use_alt = True
                    self._buf = self._alt
                    self.row, self.col = 0, 0
                elif not enter and self._use_alt:
                    self._use_alt = False
                    self._buf = self._main
                    self.row, self.col = self._saved
            return

        if final == "A":
            self.row = max(0, self.row - n1)
        elif final == "B":
            self.row = min(self.rows - 1, self.row + n1)
        elif final == "C":
            self.col = min(self.cols - 1, self.col + n1)
        elif final == "D":
            self.col = max(0, self.col - n1)
        elif final == "G":
            self.col = max(0, min(self.cols - 1, n1 - 1))
        elif final in ("H", "f"):
            r = (nums[0] if len(nums) >= 1 and nums[0] > 0 else 1) - 1
            c = (nums[1] if len(nums) >= 2 and nums[1] > 0 else 1) - 1
            self.row = max(0, min(self.rows - 1, r))
            self.col = max(0, min(self.cols - 1, c))
        elif final == "J":
            mode = nums[0] if nums else 0
            if mode in (2, 3):
                for r in range(self.rows):
                    self._buf[r] = [" "] * self.cols
            elif mode == 0:
                for c in range(self.col, self.cols):
                    self._buf[self.row][c] = " "
                for r in range(self.row + 1, self.rows):
                    self._buf[r] = [" "] * self.cols
            elif mode == 1:
                for r in range(0, self.row):
                    self._buf[r] = [" "] * self.cols
                for c in range(0, min(self.cols, self.col + 1)):
                    self._buf[self.row][c] = " "
        elif final == "K":
            mode = nums[0] if nums else 0
            if mode == 0:
                for c in range(self.col, self.cols):
                    self._buf[self.row][c] = " "
            elif mode == 1:
                for c in range(0, min(self.cols, self.col + 1)):
                    self._buf[self.row][c] = " "
            elif mode == 2:
                self._buf[self.row] = [" "] * self.cols

    def feed(self, data: bytes) -> None:
        text = data.decode("utf-8", errors="replace")
        for ch in text:
            if self._state == "text":
                if ch == "\x1b":
                    self._state = "esc"
                    self._esc = ""
                elif ch == "\r":
                    self.col = 0
                elif ch == "\n":
                    self._newline()
                elif ch == "\b":
                    self.col = max(0, self.col - 1)
                else:
                    self._put(ch)
            elif self._state == "esc":
                if ch == "[":
                    self._state = "csi"
                    self._esc = ""
                elif ch == "]":
                    self._state = "osc"
                    self._esc = ""
                elif ch in ("_", "P", "^"):
                    self._state = "st"
                    self._esc = ""
                elif ch == "7":
                    self._saved = (self.row, self.col)
                    self._state = "text"
                elif ch == "8":
                    self.row, self.col = self._saved
                    self._state = "text"
                else:
                    self._state = "text"
            elif self._state == "csi":
                if "@" <= ch <= "~":
                    self._apply_csi(self._esc, ch)
                    self._state = "text"
                else:
                    self._esc += ch
                    if len(self._esc) > 64:
                        self._state = "text"
            elif self._state == "osc":
                if ch == "\x07":
                    self._state = "text"
                elif ch == "\x1b":
                    self._state = "osc_esc"
            elif self._state == "osc_esc":
                self._state = "text"
            elif self._state == "st":
                if ch == "\x1b":
                    self._state = "osc_esc"

    def render(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self._buf).rstrip() + "\n"


def make_screen(cols: int, rows: int):
    """Prefer `pyte` when available, otherwise fall back to `SimpleScreen`."""
    try:
        import pyte  # type: ignore[import-not-found]

        screen = pyte.Screen(cols, rows)
        stream = pyte.ByteStream(screen)

        class _PyteAdapter:
            def feed(self, data: bytes) -> None:
                stream.feed(data)

            def render(self) -> str:
                return "\n".join(line.rstrip() for line in screen.display).rstrip() + "\n"

        return _PyteAdapter()
    except ImportError:
        return SimpleScreen(cols, rows)


def redact(text: str, secret: str) -> str:
    if not secret:
        return text
    return text.replace(secret, "[REDACTED]")


def resolve_api_key() -> str:
    for var in ("MARE_LIVE_OPENCODE_KEY", "OPENCODE_API_KEY"):
        val = os.environ.get(var, "").strip()
        if val:
            return val
    return ""


class PtyDriver:
    """Drives a `mare` process attached to a real Unix PTY."""

    def __init__(self, argv: list[str], cwd: Path, env: dict[str, str], cols: int, rows: int) -> None:
        self.cols = cols
        self.rows = rows
        self.screen = make_screen(cols, rows)
        self.raw_log = bytearray()

        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(cwd)
            os.execvpe(argv[0], argv, env)
        self.pid = pid
        self.fd = fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def alive(self) -> bool:
        try:
            wpid, _ = os.waitpid(self.pid, os.WNOHANG)
            return wpid == 0
        except ChildProcessError:
            return False

    def pump(self, seconds: float) -> None:
        end = time.monotonic() + seconds
        while (left := end - time.monotonic()) > 0:
            ready, _, _ = select.select([self.fd], [], [], min(left, 0.05))
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            if b"\x1b[6n" in data:
                os.write(self.fd, b"\x1b[1;1R")
            if b"\x1b]11;?" in data:
                os.write(self.fd, b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\")
            self.raw_log.extend(data)
            self.screen.feed(data)

    def wait_for(self, pred: Callable[[str], bool], timeout_s: float, what: str) -> str:
        end = time.monotonic() + timeout_s
        while time.monotonic() < end:
            self.pump(0.15)
            txt = self.screen.render()
            if pred(txt):
                return txt
            if not self.alive():
                raise RuntimeError(f"mare exited early while waiting for {what}\n{txt}")
        raise TimeoutError(f"timed out waiting for {what}\n{self.screen.render()}")

    def send_bytes(self, payload: bytes, settle_s: float = 0.25) -> None:
        os.write(self.fd, payload)
        self.pump(settle_s)

    def send_line(self, line: str, settle_s: float = 0.4) -> None:
        for ch in line:
            os.write(self.fd, ch.encode("utf-8"))
            self.pump(0.01)
        os.write(self.fd, b"\r")
        self.pump(settle_s)

    def close(self) -> int | None:
        exit_code: int | None = None
        try:
            if self.alive():
                os.kill(self.pid, signal.SIGTERM)
            end = time.monotonic() + 5.0
            while time.monotonic() < end:
                wpid, status = os.waitpid(self.pid, os.WNOHANG)
                if wpid:
                    exit_code = os.waitstatus_to_exitcode(status)
                    break
                time.sleep(0.05)
            else:
                os.kill(self.pid, signal.SIGKILL)
                _, status = os.waitpid(self.pid, 0)
                exit_code = os.waitstatus_to_exitcode(status)
        except ChildProcessError:
            pass
        finally:
            with contextlib.suppress(OSError):
                os.close(self.fd)
        return exit_code


def build_wrapper_and_env(
    home: Path,
    mare_bin: Path,
    model: str,
    api_key: str,
    faux: bool,
    extra_args: list[str],
) -> tuple[list[str], dict[str, str]]:
    """Create isolated MARE_HOME config and a 0600 keyfile wrapper (never putting keys on argv)."""
    cfg_dir = home / "config"
    cfg_dir.mkdir(parents=True, exist_ok=True)
    init_lines = [
        f'mare.setup {{ model = "{ "faux/echo" if faux else model }", logo = true, update_check = false, news = false }}',
        'mare.plugin("permissions", { preset = "read-only" })',
        'mare.plugin("onboarding", { plugins = true })',
    ]
    if faux:
        init_lines.append(f'mare.plugin("provider-faux", {{ script_json = [==[{json.dumps(FAUX_SCRIPT)}]==] }})')
    (cfg_dir / "init.lua").write_text("\n".join(init_lines) + "\n", encoding="utf-8")

    env = {
        "HOME": str(home),
        "MARE_HOME": str(home),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_STATE_HOME": str(home / ".local" / "state"),
        "XDG_DATA_HOME": str(home / ".local" / "share"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "PATH": os.environ.get("PATH", ""),
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "LANG": "en_US.UTF-8",
    }

    if faux or not api_key:
        return [str(mare_bin), *extra_args], env

    keyfile = home / "api.key"
    fd = os.open(keyfile, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(fd, api_key.encode("utf-8"))
    finally:
        os.close(fd)

    wrapper = home / "run-mare.sh"
    quoted_args = " ".join(f"'{a}'" for a in extra_args)
    wrapper.write_text(
        "#!/bin/sh\n"
        f'OPENCODE_API_KEY="$(cat "{keyfile}")"; export OPENCODE_API_KEY\n'
        f'rm -f "{keyfile}"\n'
        f'exec "{mare_bin}" {quoted_args}\n',
        encoding="utf-8",
    )
    os.chmod(wrapper, stat.S_IRWXU)
    return [str(wrapper)], env


def write_review_prompt(out_dir: Path, metadata: dict[str, object], records: list[StepRecord]) -> Path:
    """Write a ready-to-use review prompt for a second model comparing captures to the checklist."""
    checklist_rel = "docs/prerelease-exploratory-checklist.md"
    lines = [
        "# Pre-Release Exploratory Review Prompt",
        "",
        "Review the terminal screen captures below against the reference-parity checklist in",
        f"`{checklist_rel}` (Claude Code, Codex, and Kimi Code parity).",
        "",
        "## Run Metadata",
        f"- Date (UTC): `{metadata['timestamp_utc']}`",
        f"- Candidate binary version: `{metadata['mare_version']}`",
        f"- Model: `{metadata['model']}`",
        f"- Terminal size: `{metadata['cols']}x{metadata['rows']}`",
        "",
        "## Captures",
        "",
        "| # | Step | Status | Duration | File | Note |",
        "|---|---|---|---|---|---|",
    ]
    for r in records:
        status = "PASS" if r.ok else "FAIL"
        lines.append(
            f"| {r.index:02d} | `{r.name}` | {status} | {r.duration_s:.1f}s | `captures/{r.capture_file}` | {r.note} |"
        )
    lines.extend(
        [
            "",
            "## Instructions for Reviewer Model",
            "",
            "1. Read `docs/prerelease-exploratory-checklist.md` and every file under `captures/`.",
            "2. Check all global invariants (no duplicated prompt boxes, no overflowing borders,",
            "   no Lua tracebacks or raw error codes, no duplicate answer blocks, no occluded modals).",
            "3. Compare each step against Claude Code, Codex, and Kimi Code conventions.",
            "4. Report findings grouped by severity (`blocker`, `major`, `minor`) with capture filename,",
            "   exact quoted evidence, and suggested GitHub issue title.",
            "",
        ]
    )
    prompt_path = out_dir / "review-prompt.md"
    prompt_path.write_text("\n".join(lines), encoding="utf-8")
    return prompt_path


def run_exploratory(args: argparse.Namespace) -> int:
    mare_bin = Path(args.mare).resolve()
    if not mare_bin.is_file():
        print(f"error: mare binary not found at {mare_bin}", file=sys.stderr)
        return 2

    api_key = "" if args.faux else resolve_api_key()
    if not args.faux and not api_key:
        print(
            "error: set OPENCODE_API_KEY or MARE_LIVE_OPENCODE_KEY (or pass --faux for offline smoke test)",
            file=sys.stderr,
        )
        return 2

    ver_proc = subprocess.run([str(mare_bin), "--version"], capture_output=True, text=True, check=False)
    mare_version = (ver_proc.stdout.strip() or ver_proc.stderr.strip() or "unknown").splitlines()[0]

    out_dir = Path(args.out).resolve()
    cap_dir = out_dir / "captures"
    cap_dir.mkdir(parents=True, exist_ok=True)

    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    work_root = Path(tempfile.mkdtemp(prefix="mare-rc-explore-", dir=tmp_root))
    home = work_root / "home"
    project = work_root / "project"
    home.mkdir(parents=True, mode=0o700)
    project.mkdir(parents=True)
    (project / "README.md").write_text(
        "\n".join(f"Line {i}: sample project documentation for exploratory testing." for i in range(1, 31)) + "\n",
        encoding="utf-8",
    )
    subprocess.run(["git", "init", "-q", str(project)], check=False)

    argv, env = build_wrapper_and_env(home, mare_bin, args.model, api_key, args.faux, [])
    timeout_s = float(args.timeout)
    records: list[StepRecord] = []
    driver: PtyDriver | None = None

    def record_step(idx: int, name: str, description: str, fn: Callable[[PtyDriver], str | None]) -> None:
        assert driver is not None
        t0 = time.monotonic()
        ok = True
        note = ""
        try:
            res = fn(driver)
            if res:
                note = res
        except Exception as exc:  # noqa: BLE001 — capture screen and continue exploratory path
            ok = False
            note = re.sub(r"\s+", " ", redact(str(exc), api_key))[:220]
        fname = f"{idx:02d}_{name}.txt"
        screen_txt = redact(driver.screen.render(), api_key)
        (cap_dir / fname).write_text(screen_txt, encoding="utf-8")
        records.append(StepRecord(idx, name, description, fname, ok, time.monotonic() - t0, note))

    try:
        driver = PtyDriver(argv, project, env, args.cols, args.rows)

        record_step(
            1,
            "startup",
            "Cold start in an untrusted project directory",
            lambda d: (d.wait_for(lambda s: "╰" in s or "mare" in s.lower(), 30, "startup screen"), "started")[1],
        )

        def step_onboarding(d: PtyDriver) -> str:
            d.pump(1.0)
            screen = d.screen.render()
            if "Esc" in screen or "onboarding" in screen.lower() or "plugins" in screen.lower():
                d.send_bytes(b"\x1b", 0.5)
                return "onboarding observed and dismissed"
            return "no onboarding modal shown"

        record_step(2, "onboarding", "First-run onboarding view and dismissal", step_onboarding)

        def step_slash_commands(d: PtyDriver) -> str:
            d.send_line("/help", 0.8)
            d.wait_for(lambda s: "Help" in s or "Commands" in s or " of " in s, 15, "/help overlay")
            d.send_bytes(b"\x1b", 0.4)
            d.send_line("/status", 0.8)
            d.send_line("/usage", 0.8)
            return "ran /help, /status, /usage on empty session"

        record_step(3, "empty_session_commands", "Common slash commands on an empty session", step_slash_commands)

        def step_empty_side(d: PtyDriver) -> str:
            d.send_line("/side Quick hello from an empty session", 0.6)
            d.wait_for(lambda s: "SIDE" in s, timeout_s, "/side on empty session")
            d.pump(2.0)
            d.send_bytes(b"\x1b", 0.5)
            return "empty-session /side answered and dismissed"

        record_step(4, "empty_side", "/side on an empty session", step_empty_side)

        def step_first_prompt(d: PtyDriver) -> str:
            d.send_line("Read README.md using the read tool and summarize it in one sentence.", 0.6)
            d.wait_for(lambda s: "Read " in s or "README" in s, timeout_s, "first prompt tool output")
            d.pump(2.0)
            return "completed first tool-using turn"

        record_step(5, "first_prompt", "First prompt reading a project file", step_first_prompt)

        def step_ctrl_o(d: PtyDriver) -> str:
            d.send_bytes(b"\x0f", 0.6)  # Ctrl+O expand
            d.send_bytes(b"\x0f", 0.6)  # Ctrl+O collapse back
            return "toggled Ctrl+O expand -> collapse"

        record_step(6, "keybinding_ctrl_o", "Ctrl+O expand and second press collapse", step_ctrl_o)

        def step_ctrl_t(d: PtyDriver) -> str:
            d.send_bytes(b"\x14", 0.8)  # Ctrl+T transcript
            d.wait_for(lambda s: "Transcript" in s, 15, "Ctrl+T transcript overlay")
            d.send_bytes(b"\x1b", 0.6)  # Esc back
            return "opened Ctrl+T transcript and returned with Esc"

        record_step(7, "keybinding_ctrl_t", "Ctrl+T transcript overlay and Esc return", step_ctrl_t)

        def step_shift_tab(d: PtyDriver) -> str:
            d.send_bytes(b"\x1b[Z", 0.6)  # Shift+Tab
            d.pump(0.5)
            d.send_bytes(b"\x1b", 0.4)  # Esc dismiss picker if opened
            return "tested Shift+Tab in untrusted folder and Esc cancel"

        record_step(8, "keybinding_shift_tab", "Shift+Tab preset picker in untrusted folder", step_shift_tab)

        def step_side_after_turn(d: PtyDriver) -> str:
            d.send_line("/side What file did we just read?", 0.6)
            d.wait_for(lambda s: "SIDE" in s, timeout_s, "/side after turn")
            d.pump(2.0)
            d.send_bytes(b"\x1b", 0.5)
            return "ran /side after completed turn"

        record_step(9, "side_after_turn", "/side after a completed turn", step_side_after_turn)

        def step_permission_prompt(d: PtyDriver) -> str:
            d.send_line("Use the write tool to create draft.txt containing hello.", 0.6)
            d.wait_for(lambda s: "Allow" in s, timeout_s, "permission approval prompt")
            d.send_bytes(b"y", 1.5)
            return "observed approval prompt and allowed once"

        record_step(10, "permission_prompt", "Permission approval dialog on write in read-only mode", step_permission_prompt)

        def step_subagent(d: PtyDriver) -> str:
            d.send_line("Start a general subagent to reply with one sentence and wait for it.", 0.6)
            d.pump(3.0)
            return "exercised subagent flow"

        record_step(11, "subagent", "Subagent delegation turn", step_subagent)

        def step_new(d: PtyDriver) -> str:
            d.send_line("/new", 1.0)
            d.wait_for(lambda s: "/model" in s or "mare v" in s, 15, "welcome banner after /new")
            return "/new restored welcome banner"

        record_step(12, "new_session", "/new clears screen and restores welcome banner", step_new)

        def step_resume(d: PtyDriver) -> str:
            d.send_line("/resume", 1.0)
            d.pump(0.6)
            d.send_bytes(b"\x1b", 0.4)
            return "opened /resume picker and dismissed"

        record_step(13, "resume_session", "/resume session picker", step_resume)

        def step_quit(d: PtyDriver) -> str:
            d.send_bytes(b"\x04", 1.0)
            return "sent Ctrl+D to quit"

        record_step(14, "quit", "Clean quit with Ctrl+D", step_quit)
    finally:
        if driver is not None:
            driver.close()
        shutil.rmtree(work_root, ignore_errors=True)

    metadata: dict[str, object] = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "mare_version": mare_version,
        "model": "faux/echo" if args.faux else args.model,
        "cols": args.cols,
        "rows": args.rows,
        "checklist": str(CHECKLIST_PATH.relative_to(ROOT)),
        "steps": [dataclasses.asdict(r) for r in records],
    }
    (out_dir / "manifest.json").write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    prompt_path = write_review_prompt(out_dir, metadata, records)

    passed = sum(1 for r in records if r.ok)
    print(f"Exploratory run finished: {passed}/{len(records)} steps succeeded")
    print(f"Captures:      {cap_dir}")
    print(f"Review prompt: {prompt_path}")
    return 0 if passed == len(records) else 1


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Drive a candidate mare binary through a PTY along the first-time user exploratory path."
    )
    parser.add_argument("--mare", default=str(DEFAULT_MARE), help="Path to the candidate mare binary")
    parser.add_argument("--model", default=DEFAULT_MODEL, help="Real model ID to use (default: %(default)s)")
    parser.add_argument("--out", default="prerelease-explore-out", help="Directory to store captures and review prompt")
    parser.add_argument("--cols", type=int, default=COLS, help="PTY width in columns")
    parser.add_argument("--rows", type=int, default=ROWS, help="PTY height in rows")
    parser.add_argument("--timeout", type=float, default=90.0, help="Per-step wait timeout in seconds")
    parser.add_argument(
        "--faux",
        action="store_true",
        help="Run against offline provider-faux (requires a faux-enabled binary) for driver self-testing",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    return run_exploratory(parse_args(argv))


if __name__ == "__main__":
    sys.exit(main())
