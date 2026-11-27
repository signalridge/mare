#!/usr/bin/env python3
"""`just tui-e2e`: drive the real mare TUI in a real terminal through herdr (TUI e2e,
local layer). Not in CI; skipped unless running inside a herdr pane (HERDR_ENV=1) with `herdr`
on PATH. Model: the same real model as `just live` (`OPENCODE_API_KEY` from the environment;
MARE_LIVE_MODEL overrides). The key reaches mare through a 0600 file read by a
wrapper script — never on a command line, never logged.

Scenarios: startup, mare's state reported to herdr (via examples/herdr/init.lua), a
prompt and its answer, long output, an approval dialog (herdr sees `blocked`), the /tree
overlay, a resize, quitting (herdr sees the agent released). Prints one line per scenario.
"""

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "mare")
MODEL = os.environ.get("MARE_LIVE_MODEL", "opencode-go/glm-5.3-flash")
TIMEOUT_MS = 180_000


def herdr(*args, check=True):
    p = subprocess.run(["herdr", *args], capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"herdr {' '.join(args)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return p.stdout


def herdr_json(*args):
    return json.loads(herdr(*args))


def wait_output(pane, text, timeout_ms=TIMEOUT_MS, source="recent"):
    herdr("pane", "wait-output", "--match", text, "--source", source, "--timeout", str(timeout_ms), pane)


def read(pane, source="visible"):
    return herdr("pane", "read", "--source", source, pane)


def pane_info(pane):
    return herdr_json("pane", "get", pane)["result"]["pane"]


def wait_state(pane, states, timeout=TIMEOUT_MS / 1000):
    end = time.monotonic() + timeout
    seen = None
    while time.monotonic() < end:
        info = pane_info(pane)
        seen = (info.get("agent"), info.get("agent_status"))
        if info.get("agent") == "mare" and info.get("agent_status") in states:
            return info
        time.sleep(0.2)
    raise AssertionError(f"herdr never saw mare in {states}; last: {seen}")


def send_line(pane, text):
    herdr("pane", "send-text", pane, text)
    herdr("pane", "send-keys", pane, "enter")


def tiny_png():
    """A valid 2x2 RGB PNG (stdlib only)."""
    import struct
    import zlib

    def chunk(kind, data):
        c = struct.pack(">I", len(data)) + kind + data
        return c + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + b"\xff\x40\x40" * 2 for _ in range(2))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def assert_read_image(messages):
    """The PNG is recorded either in the read tool_result itself or a following user image message."""
    calls = [
        part
        for message in messages
        if message.get("role") == "assistant"
        for part in message.get("content", [])
        if part.get("type") == "tool_call"
    ]
    read_ids = {
        part.get("id")
        for part in calls
        if isinstance(part.get("id"), str)
        and part.get("id")
        and part.get("name") == "read"
        and os.path.basename(part.get("arguments", {}).get("path", "")) == "dot.png"
    }
    observed = [(part.get("name"), part.get("arguments", {}).get("path")) for part in calls]
    assert read_ids, f"the model never called read on dot.png (messages={len(messages)}, calls={observed})"
    results = [
        i
        for i, message in enumerate(messages)
        if message.get("role") == "tool_result"
        and message.get("tool_call_id") in read_ids
        and not message.get("is_error")
    ]
    assert results, "the read call did not return a successful tool result"
    has_png = lambda m: any(p.get("type") == "image" and p.get("mime") == "image/png" for p in m.get("content", []))
    images = [i for i, m in enumerate(messages) if (i in results and has_png(m)) or (i > results[-1] and m.get("role") == "user" and has_png(m))]
    assert images, "the read result did not record a PNG image block"
    assert any(
        "seen" in part.get("text", "").lower()
        for message in messages[images[-1] + 1 :]
        if message.get("role") == "assistant"
        for part in message.get("content", [])
        if part.get("type") == "text"
    ), f"no assistant reply after the image result (tail={messages[images[-1] + 1 :]!r})"


def main():
    if os.environ.get("HERDR_ENV") != "1" or not shutil.which("herdr"):
        print("skipped: not inside a herdr pane (HERDR_ENV=1) or herdr not installed")
        return 0
    key = os.environ.get("OPENCODE_API_KEY", "")
    if not key:
        print("skipped: no OPENCODE_API_KEY (run through `just tui-e2e`)")
        return 0
    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} not found")

    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    home = tempfile.mkdtemp(prefix="mare-tui-e2e-", dir=tmp_root)
    work = tempfile.mkdtemp(prefix="mare-tui-e2e-cwd-", dir=tmp_root)
    os.chmod(home, 0o700)
    cfg = os.path.join(home, "config")
    os.makedirs(cfg)
    with open(os.path.join(REPO, "examples", "herdr", "init.lua")) as f:
        herdr_init = f.read()
    with open(os.path.join(cfg, "init.lua"), "w") as f:
        f.write(f'mare.setup {{ model = "{MODEL}" }}\n')
        # read-only: a file write asks (the approval scenario); disable first-open plugin picker
        f.write('mare.plugin("permissions", { preset = "read-only" })\n')
        f.write('mare.plugin("onboarding", { plugins = false })\n')
        f.write(herdr_init)
    keyfile = os.path.join(home, "key")
    fd = os.open(keyfile, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(fd, key.encode())
    os.close(fd)
    wrapper = os.path.join(home, "run-mare")
    with open(wrapper, "w") as f:
        f.write(
            "#!/bin/sh\n"
            f'export MARE_HOME="{home}"\n'
            f'OPENCODE_API_KEY="$(cat "{keyfile}")"; export OPENCODE_API_KEY\n'
            f'rm -f "{keyfile}"\n'
            f'exec "{BIN}"\n'
        )
    os.chmod(wrapper, stat.S_IRWXU)

    pane = None
    results = []

    def scenario(name, fn):
        t0 = time.monotonic()
        try:
            fn()
            results.append((name, True, time.monotonic() - t0, ""))
        except Exception as e:  # noqa: BLE001 — report and continue with the next scenario
            results.append((name, False, time.monotonic() - t0, str(e).replace(key, "[redacted]")[:300]))

    try:
        split = herdr_json("pane", "split", "--current", "--direction", "right", "--cwd", work, "--no-focus")
        pane = split["result"]["pane"]["pane_id"]
        herdr("pane", "run", pane, wrapper)

        scenario("startup: input box", lambda: wait_output(pane, "╰", 30_000))
        scenario("status reported to herdr (idle)", lambda: wait_state(pane, {"idle", "done"}, 30))

        def prompt():
            # the expected answer must not appear in the prompt (wait-output also sees the input)
            send_line(pane, "What is 17 plus 25? Reply with only the number.")
            wait_state(pane, {"working"}, 30)
            wait_output(pane, "42")
            wait_state(pane, {"idle", "done"})

        scenario("prompt → working → answer → idle", prompt)

        def long_output():
            send_line(pane, "Do not use any tools. Reply directly with the integers from 1 to 60 in digits, one per line, no other text.")
            wait_state(pane, {"working"}, 30)
            wait_state(pane, {"idle", "done"})
            out = read(pane, source="recent-unwrapped")
            assert "59" in out, f"expected 59 in output; tail={out[-300:]!r}"
            vis = read(pane)
            assert "╰" in vis, "input box still drawn after a long answer"

        scenario("long output scrolls, input box stays", long_output)

        def approval():
            send_line(pane, "Use the write tool to create hello.txt containing the text hi.")
            wait_output(pane, "Allow once")
            wait_state(pane, {"blocked"}, 30)
            herdr("pane", "send-keys", pane, "enter")
            end = time.monotonic() + TIMEOUT_MS / 1000
            while not os.path.exists(os.path.join(work, "hello.txt")):
                assert time.monotonic() < end, "hello.txt never written after approval"
                time.sleep(0.3)
            wait_state(pane, {"idle", "done"})

        scenario("approval dialog → herdr sees blocked → allowed", approval)

        def tree():
            send_line(pane, "/tree")
            wait_output(pane, "Session tree", 15_000, source="visible")
            herdr("pane", "send-keys", pane, "esc")
            end = time.monotonic() + 15
            while "Session tree" in read(pane, source="visible"):
                assert time.monotonic() < end, "the tree overlay did not close"
                time.sleep(0.05)

        scenario("/tree overlay", tree)

        def image():
            # herdr speaks the Kitty graphics protocol: a read image is drawn, not replaced by the
            # `[image: …]` text placeholder (pixels aren't visible to `pane read`)
            png = os.path.join(work, "dot.png")
            with open(png, "wb") as f:
                f.write(tiny_png())
            send_line(pane, 'Use the read tool to read dot.png, then reply with the literal word "seen".')
            # "seen" appears in the prompt too, so waiting for that text alone can match the
            # input before the model or tool has run. Observe the turn and its saved records.
            wait_state(pane, {"working"}, 30)
            wait_state(pane, {"idle", "done"})
            messages = []
            for root, _, files in os.walk(os.path.join(home, "state", "sessions")):
                for name in files:
                    if name.endswith(".jsonl"):
                        with open(os.path.join(root, name), encoding="utf-8") as f:
                            for line in f:
                                entry = json.loads(line)
                                if entry.get("type") == "message":
                                    messages.append(entry.get("message", {}))
            assert_read_image(messages)
            vis = read(pane, source="recent")
            assert "[image: dot.png]" not in vis, "text placeholder shown: no image protocol detected"

        scenario("image read draws through a protocol (no placeholder)", image)

        def resize():
            before = read(pane)
            herdr("pane", "resize", "--pane", pane, "--direction", "left", "--amount", "0.15")
            time.sleep(0.5)  # the resize debounce (75ms) plus a repaint
            herdr("pane", "resize", "--pane", pane, "--direction", "right", "--amount", "0.15")
            wait_output(pane, "╰", 10_000, source="visible")
            after = read(pane)
            assert after.count("╰") >= 1 and before, "input box redrawn after resizes"

        scenario("resize keeps the screen consistent", resize)

        def quit_():
            herdr("pane", "send-keys", pane, "ctrl+d")
            end = time.monotonic() + 20
            while time.monotonic() < end:
                if pane_info(pane).get("agent") != "mare":
                    return
                time.sleep(0.3)
            raise AssertionError("herdr still lists mare as the pane's agent after quitting")

        scenario("quit releases the agent in herdr", quit_)
    finally:
        if pane:
            herdr("pane", "close", pane, check=False)
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)

    width = max(len(n) for n, *_ in results)
    for name, ok, secs, err in results:
        print(f"{'PASS' if ok else 'FAIL'}  {name:<{width}}  {secs:6.1f}s  {err}")
    failed = sum(1 for _, ok, *_ in results if not ok)
    print(f"{len(results) - failed}/{len(results)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
