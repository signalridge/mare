#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Render the documentation's TUI screenshots as SVG.

Runs the real `mare` TUI in a pseudo-terminal against the offline scripted provider
(`provider-faux`, debug builds only), replays a fixed list of keystrokes per scene, and
draws the final screen as an SVG. Nothing here talks to a model or the network.

    scripts/screenshots.py --mare target/debug/mare            # all scenes -> docs/assets/screenshots
    scripts/screenshots.py --mare target/debug/mare main-dark  # one scene (approval|commands|startup -> *-dark)

`--record DIR` plays the `main-dark` scene in a pseudo-terminal through local
`asciinema` (against offline `provider-faux`) and writes the home page vector
recordings, `session.cast` (116x26 desktop) and `session-mobile.cast` (72x26
mobile), in asciicast v2 with the Catppuccin Mocha palette to DIR.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import html
import json
import os
import pty
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata
from collections.abc import Iterator
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pyte

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs/assets/screenshots"
COLS, ROWS = 116, 30
MOBILE_COLS = 72

# Terminal default colors per flavor (Catppuccin dark palette for screenshots).
FLAVORS = {
    "dark": {"bg": "#1e1e2e", "fg": "#cdd6f4", "chrome": "#181825", "dot": "#45475a"},
}
ANSI = {
    "black": "#45475a", "red": "#f38ba8", "green": "#a6e3a1", "brown": "#f9e2af", "blue": "#89b4fa",
    "magenta": "#cba6f7", "cyan": "#94e2d5", "white": "#bac2de", "brightblack": "#585b70",
    "brightred": "#f38ba8", "brightgreen": "#a6e3a1", "brightbrown": "#f9e2af", "brightblue": "#89b4fa",
    "brightmagenta": "#cba6f7", "brightcyan": "#94e2d5", "brightwhite": "#a6adc8",
}  # fmt: skip

LIB_RS = """\
pub fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = values.len() / 2;
    Some(values[mid])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_length_averages_the_middle_pair() {
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
"""

FIX_SCRIPT = [
    {
        "tool_calls": [{"name": "read", "arguments": {"path": "src/lib.rs"}}],
    },
    {
        "text": "`median` returns the upper middle element for even lengths; I'll average the middle pair.",
        "tool_calls": [
            {
                "name": "edit",
                "arguments": {
                    "path": "src/lib.rs",
                    "old_string": "    Some(values[mid])",
                    "new_string": "    if values.len() % 2 == 0 {\n        Some((values[mid - 1] + values[mid]) / 2.0)\n    } else {\n        Some(values[mid])\n    }",
                },
            }
        ],
    },
    {
        "text": "Averaged the middle pair for even-length slices in `src/lib.rs` (`cargo test` passes).",
    },
]

APPROVAL_SCRIPT = [
    {
        "text": "`median` should average the middle pair for even lengths. I'll update `src/lib.rs`.",
        "tool_calls": [FIX_SCRIPT[1]["tool_calls"][0]],
    },
    {"text": "Done."},
]

# base scene -> (extra CLI args, faux script, [(keys, seconds to wait)], rows)
BASE_SCENES = {
    "main": (["--permission", "auto"], FIX_SCRIPT, [("the median test is failing, fix it\r", 6.0)], 34),
    "approval": (["--permission", "read-only"], APPROVAL_SCRIPT, [("fix the median function\r", 4.0)], 26),
    "commands": (["--permission", "auto"], [], [("", 6.0), ("/", 1.5)], 17),
    "startup": (["--permission", "auto"], [], [], 20),
}
SCENES = {
    f"{base}-dark": ("dark", *spec)
    for base, spec in BASE_SCENES.items()
}


@contextlib.contextmanager
def scene_home(name: str) -> Iterator[tuple[Path, dict[str, str]]]:
    """Create the throwaway home and project for a scene; yield the project and its environment."""
    flavor, _args, script, _steps, _rows = SCENES[name]
    tmp_root = next((v for k in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if (v := os.environ.get(k))), None)
    tmp = Path(tempfile.mkdtemp(prefix="mare-shot-", dir=tmp_root))
    try:
        home = tmp / "home"
        project = home / "median"
        (home / ".config/mare").mkdir(parents=True)
        (project / "src").mkdir(parents=True)
        (project / "src/lib.rs").write_text(LIB_RS)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "median"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        init = (
            f'mare.setup {{ model = "faux/echo", flavor = "{flavor}", update_check = false, news = false }}\n'
            'mare.plugin("onboarding", { plugins = false })\n'
        )
        if script:
            init += f'mare.plugin("provider-faux", {{ script_json = [==[{json.dumps(script)}]==] }})\n'
        (home / ".config/mare/init.lua").write_text(init)
        env = {
            "HOME": str(home),
            "MARE_TMPDIR": str(tmp),
            "TMPDIR": str(tmp),
            "TEMP": str(tmp),
            "TMP": str(tmp),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_STATE_HOME": str(home / ".local/state"),
            "XDG_DATA_HOME": str(home / ".local/share"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "PATH": os.environ["PATH"],
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LANG": "en_US.UTF-8",
            "USER": "you",
        }
        yield project, env
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def run_scene(mare: str, name: str) -> pyte.Screen:
    import pyte

    flavor, args, _script, steps, rows = SCENES[name]
    with scene_home(name) as (project, env):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(project)
            os.execvpe(mare, [mare, "--trust", *args], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, COLS, 0, 0))
        screen = pyte.Screen(COLS, rows)
        stream = pyte.ByteStream(screen)

        def check_alive() -> None:
            wpid, status = os.waitpid(pid, os.WNOHANG)
            if wpid:
                code = os.waitstatus_to_exitcode(status)
                raise RuntimeError(f"{name}: mare exited early (status {code})")

        def pump(seconds: float) -> None:
            end = time.monotonic() + seconds
            while (left := end - time.monotonic()) > 0:
                ready, _, _ = select.select([fd], [], [], min(left, 0.1))
                if ready:
                    try:
                        data = os.read(fd, 65536)
                    except OSError:
                        check_alive()
                        return
                    if not data:
                        check_alive()
                        return
                    # answer the queries a real terminal would (cursor position, background color)
                    if b"\x1b[6n" in data:
                        os.write(fd, b"\x1b[1;1R")
                    if b"\x1b]11;?" in data:
                        bg = FLAVORS[flavor]["bg"]
                        rgb = "/".join(bg[i : i + 2] * 2 for i in (1, 3, 5))
                        os.write(fd, f"\x1b]11;rgb:{rgb}\x1b\\".encode())
                    stream.feed(data)

        pump(3.0)
        for keys, wait in steps:
            for ch in keys:
                os.write(fd, ch.encode())
                pump(0.02)
            pump(wait)
        check_alive()
        os.kill(pid, 9)
        os.waitpid(pid, 0)
        return screen


def color(value: str, flavor: str, default: str) -> str:
    if value == "default":
        return FLAVORS[flavor][default]
    if value in ANSI:
        return ANSI[value]
    return "#" + value if len(value) == 6 else FLAVORS[flavor][default]


def to_svg(screen: pyte.Screen, flavor: str, title: str) -> str:
    cw, ch, pad, bar = 8.4, 18, 16, 34
    last = max(
        (y for y in range(screen.lines) if any(c.data.strip() for c in screen.buffer[y].values())),
        default=0,
    )
    rows = last + 2
    width, height = COLS * cw + pad * 2, rows * ch + pad * 2 + bar
    f = FLAVORS[flavor]
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0f} {height:.0f}" role="img" aria-label="{html.escape(title)}">',
        f"<title>{html.escape(title)}</title>",
        f'<rect width="100%" height="100%" rx="10" fill="{f["bg"]}"/>',
        f'<path d="M0 10a10 10 0 0 1 10-10h{width - 20:.0f}a10 10 0 0 1 10 10v{bar - 10}H0z" fill="{f["chrome"]}"/>',
        *(f'<circle cx="{18 + i * 18}" cy="{bar / 2}" r="5.5" fill="{f["dot"]}"/>' for i in range(3)),
        "<g font-family=\"'JetBrains Mono','SF Mono',Menlo,Consolas,'DejaVu Sans Mono',monospace\" font-size=\"14\" xml:space=\"preserve\">",
    ]
    for y in range(rows):
        line = screen.buffer[y]
        x = 0
        while x < COLS:
            c = line[x]
            fg, bg = color(c.fg, flavor, "fg"), color(c.bg, flavor, "bg")
            if c.reverse:
                fg, bg = bg, fg
            wide = unicodedata.east_asian_width(c.data[:1] or " ") in ("W", "F")
            span = 2 if wide else 1
            px, py = pad + x * cw, bar + pad + y * ch
            if bg != f["bg"]:
                out.append(f'<rect x="{px:.1f}" y="{py}" width="{cw * span + 0.3:.1f}" height="{ch}" fill="{bg}"/>')
            if c.data.strip():
                attrs = f'fill="{fg}"'
                if c.bold:
                    attrs += ' font-weight="700"'
                if c.italics:
                    attrs += ' font-style="italic"'
                if c.underscore:
                    attrs += ' text-decoration="underline"'
                out.append(
                    f'<text x="{px:.1f}" y="{py + 13.5}" textLength="{cw * span:.1f}" lengthAdjust="spacingAndGlyphs" {attrs}>{html.escape(c.data)}</text>'
                )
            x += span
    out += ["</g>", "</svg>"]
    return "\n".join(out) + "\n"


CATPPUCCIN_MOCHA_PALETTE = ":".join(
    [
        ANSI["black"],
        ANSI["red"],
        ANSI["green"],
        ANSI["brown"],
        ANSI["blue"],
        ANSI["magenta"],
        ANSI["cyan"],
        ANSI["white"],
        ANSI["brightblack"],
        ANSI["brightred"],
        ANSI["brightgreen"],
        ANSI["brightbrown"],
        ANSI["brightblue"],
        ANSI["brightmagenta"],
        ANSI["brightcyan"],
        ANSI["brightwhite"],
    ]
)


def _trim_cast(cast_path: Path, flavor: str, cols: int, rows: int) -> None:
    """Normalize asciicast v2 header (Catppuccin theme) and trim boot/teardown edges."""
    lines = cast_path.read_text(encoding="utf-8").splitlines()
    if not lines:
        raise RuntimeError(f"empty recording: {cast_path}")
    header = json.loads(lines[0])
    events = [json.loads(line) for line in lines[1:] if line.strip()]
    if not events:
        raise RuntimeError(f"recording has no events: {cast_path}")

    # Drop any final teardown events emitted after Ctrl-D (e.g. cursor restore / goodbye / clear).
    # Keep up through the last frame where the post-turn idle prompt is rendered.
    prompt_idx = None
    for idx, (_t, code, data) in enumerate(events):
        if code == "o" and "worked " in data:
            prompt_idx = idx
    if prompt_idx is not None:
        # Keep a short settling window after the summary line (status bar + editor redraw),
        # then stop before exit teardown.
        settled_t = events[prompt_idx][0]
        last_keep = prompt_idx
        for idx in range(prompt_idx, len(events)):
            if events[idx][0] - settled_t <= 0.6:
                last_keep = idx
            else:
                break
        events = events[: last_keep + 1]

    # Shift timestamps so the first visible TUI draw starts cleanly at t=0.0,
    # and compress the initial startup toast wait to a crisp 1.0s welcome dwell
    # before typing begins.
    first_draw_t = 0.0
    for t, code, data in events:
        if code == "o" and ("Ask mare anything" in data or "mare" in data):
            first_draw_t = t
            break

    # Find when the first character of the prompt is echoed (so we can cap pre-typing dwell)
    first_type_t = None
    for t, code, data in events:
        if code == "o" and t > first_draw_t + 1.5 and ("the median" in data or ("> t" in data)):
            first_type_t = t
            break

    normalized: list[list[object]] = []
    time_shift = first_draw_t
    compressed = False
    for t, code, data in events:
        if code != "o":
            continue
        if not compressed and first_type_t is not None and t >= first_type_t:
            raw_gap = first_type_t - first_draw_t
            if raw_gap > 1.2:
                time_shift += raw_gap - 1.2
            compressed = True
        nt = max(0.0, round(t - time_shift, 4))
        if normalized and nt < float(normalized[-1][0]):
            nt = float(normalized[-1][0])
        normalized.append([nt, "o", data])

    # Append a gentle hold at the end so looping pauses on the final diff frame
    if normalized:
        end_t = float(normalized[-1][0])
        normalized.append([round(end_t + 3.0, 4), "o", ""])

    header["version"] = 2
    header["width"] = cols
    header["height"] = rows
    header["title"] = "mare ~/median"
    header["command"] = "mare"
    header["idle_time_limit"] = 2.0
    header["theme"] = {
        "fg": FLAVORS[flavor]["fg"],
        "bg": FLAVORS[flavor]["bg"],
        "palette": CATPPUCCIN_MOCHA_PALETTE,
    }
    header.pop("env", None)
    header.pop("timestamp", None)

    out_lines = [json.dumps(header, separators=(",", ":"))]
    out_lines.extend(json.dumps(ev, separators=(",", ":")) for ev in normalized)
    cast_path.write_text("\n".join(out_lines) + "\n", encoding="utf-8")


def _record_cast(mare: str, name: str, cast_path: Path, cols: int, rows: int) -> Path:
    """Play a scene in a PTY through local asciinema at cols x rows and save asciicast v2."""
    asciinema = shutil.which("asciinema")
    if not asciinema:
        raise RuntimeError("asciinema not found on PATH")
    flavor, args, _script, steps, _rows = SCENES[name]
    cast_path.parent.mkdir(parents=True, exist_ok=True)

    with scene_home(name) as (project, env):
        # Pass the isolated scene env to `mare` itself so `asciinema` (which may be
        # launched through a shim manager like aqua/mise/asdf) keeps its host env.
        rec_env = {**os.environ, "TERM": env["TERM"], "COLORTERM": env["COLORTERM"]}
        env_args = [f"{k}={v}" for k, v in env.items()]
        cmd_str = shlex.join(["env", "-i", *env_args, mare, "--trust", *args])
        rec_argv = [
            asciinema,
            "rec",
            "--output-format",
            "asciicast-v2",
            "--overwrite",
            "--quiet",
            "--window-size",
            f"{cols}x{rows}",
            "--idle-time-limit",
            "2.0",
            "--command",
            cmd_str,
            str(cast_path),
        ]
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(project)
            os.execvpe(asciinema, rec_argv, rec_env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

        seen = bytearray()

        def pump(seconds: float, stop_on: bytes | None = None) -> bool:
            end = time.monotonic() + seconds
            while (left := end - time.monotonic()) > 0:
                ready, _, _ = select.select([fd], [], [], min(left, 0.05))
                if not ready:
                    continue
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return False
                if not data:
                    return False
                seen.extend(data)
                if b"\x1b[6n" in data:
                    os.write(fd, b"\x1b[1;1R")
                if b"\x1b]11;?" in data:
                    bg = FLAVORS[flavor]["bg"]
                    rgb = "/".join(bg[i : i + 2] * 2 for i in (1, 3, 5))
                    os.write(fd, f"\x1b]11;rgb:{rgb}\x1b\\".encode())
                if stop_on and stop_on in seen:
                    return True
            return False

        # Wait until mare's TUI prompt is drawn, and let the transient startup Info
        # toast auto-dismiss (~5s) so the finished diff fills the frame cleanly.
        pump(6.0, stop_on=b"Ask mare anything")
        pump(5.2)

        for keys, wait in steps:
            text = keys.rstrip("\r")
            for ch in text:
                os.write(fd, ch.encode())
                pump(0.045)
            if text:
                pump(0.45)
            if keys.endswith("\r"):
                os.write(fd, b"\r")
            pump(wait)

        # Clean exit via Ctrl-D on empty input so asciinema flushes the cast file
        try:
            os.write(fd, b"\x04")
        except OSError:
            pass
        deadline = time.monotonic() + 5.0
        exited = False
        while time.monotonic() < deadline:
            pump(0.1)
            wpid, _status = os.waitpid(pid, os.WNOHANG)
            if wpid == pid:
                exited = True
                break
        if not exited:
            os.kill(pid, 15)
            os.waitpid(pid, 0)
        os.close(fd)

    _trim_cast(cast_path, flavor, cols, rows)
    return cast_path


def record_scene(mare: str, name: str, out_dir: Path) -> list[Path]:
    """Record desktop (116x26) and mobile (72x26) asciicast v2 replays into out_dir."""
    out_dir = out_dir.resolve()
    rows = 26  # shorter than the still, so the finished turn fills the frame
    return [
        _record_cast(mare, name, out_dir / "session.cast", COLS, rows),
        _record_cast(mare, name, out_dir / "session-mobile.cast", MOBILE_COLS, rows),
    ]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mare", default=str(ROOT / "target/debug/mare"))
    ap.add_argument("--text", action="store_true", help="print the captured screen as text instead of writing SVG")
    ap.add_argument("--record", metavar="DIR", type=Path, help="record the main-dark scene with asciinema into DIR/session.cast and DIR/session-mobile.cast")
    ap.add_argument("scenes", nargs="*", default=list(SCENES))
    args = ap.parse_args()
    mare = str(Path(args.mare).expanduser().resolve()) if (os.sep in args.mare or "/" in args.mare or Path(args.mare).exists()) else args.mare
    if args.record:
        for path in record_scene(mare, "main-dark", args.record):
            print(f"wrote {path}")
        return 0
    OUT.mkdir(parents=True, exist_ok=True)
    for name in args.scenes:
        screen = run_scene(mare, name)
        if args.text:
            print(f"--- {name}", *(line.rstrip() for line in screen.display), sep="\n")
            continue
        svg = to_svg(screen, SCENES[name][0], f"mare: {name}")
        (OUT / f"{name}.svg").write_text(svg, encoding="utf-8")
        print(f"wrote docs/assets/screenshots/{name}.svg")
    return 0


if __name__ == "__main__":
    sys.exit(main())
