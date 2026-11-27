"""Exercise acceptance temp routing and residue accounting without running Cargo."""

import concurrent.futures
import contextlib
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

WINDOWS = os.name == "nt"

class FixtureTestCase(unittest.TestCase):
    def setUp(self):
        if WINDOWS:
            original = Path.write_text
            def write_lf(path, data, *args, **kwargs):
                kwargs.setdefault("newline", "\n")
                kwargs.setdefault("encoding", "utf-8")
                return original(path, data, *args, **kwargs)
            self.enterContext(mock.patch.object(Path, "write_text", write_lf))

def shell_path(path):
    if WINDOWS:
        return subprocess.check_output(["cygpath", "-u", str(path)], text=True, encoding="utf-8", errors="replace").strip()
    return str(path)

def native_path(path):
    if WINDOWS:
        path = subprocess.check_output(["cygpath", "-w", str(path)], text=True, encoding="utf-8", errors="replace").strip()
    return Path(path)

class AcceptanceTempTests(FixtureTestCase):
    @staticmethod
    @contextlib.contextmanager
    def _registered_tempdir(prefix=None, dir=None):
        with tempfile.TemporaryDirectory(prefix=prefix, dir=dir) as path:
            reg_file = os.environ.get("MARE_ACCEPT_TEST_TEMP_REGISTRY")
            if reg_file:
                with Path(reg_file).open("a", encoding="utf-8") as fh:
                    fh.write(f"{Path(path).resolve()}\n")
            yield path

    def test_temporary_root_below_git_marker_is_rejected_before_steps(self):
        with self._registered_tempdir(prefix="accept-git-root-") as tmp:
            root = Path(tmp)
            parent = root / "parent"
            temp_root = parent / "tmp"
            temp_root.mkdir(parents=True)
            (parent / ".git").write_text("gitdir: elsewhere\n", encoding="utf-8")
            env = dict(
                os.environ,
                HOME=shell_path(root / "home"),
                CARGO_TARGET_DIR=shell_path(root / "target"),
                MARE_ACCEPT_TMPDIR=shell_path(temp_root),
                MARE_ACCEPT_MIN_FREE_GB="0",
            )
            env.pop("XDG_CACHE_HOME", None)
            result = subprocess.run(
                ["sh", shell_path(Path(__file__).with_name("accept.sh")), "--no-git", "--steps", "temporary-file cleanup"],
                cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", check=False, timeout=(120 if WINDOWS else 10),
            )
            self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
            self.assertIn(".git at or above temporary root", result.stderr)
            self.assertFalse((root / "home" / ".cache" / "mare" / "acceptance").exists())

    def test_invalid_explicit_temporary_roots_are_rejected(self):
        with self._registered_tempdir(prefix="accept-invalid-root-") as tmp:
            root = Path(tmp)
            for value, message in (("", "empty"), ("relative", "absolute"),
                                   (shell_path(root / "missing"), "writable directory"),
                                   ("/dev", "temporary root")):
                with self.subTest(value=value):
                    env = dict(
                        os.environ, HOME=shell_path(root / "home"),
                        CARGO_TARGET_DIR=shell_path(root / "target"), MARE_ACCEPT_TMPDIR=value,
                        MARE_ACCEPT_MIN_FREE_GB="0",
                    )
                    env.pop("XDG_CACHE_HOME", None)
                    result = subprocess.run(
                        ["sh", shell_path(Path(__file__).with_name("accept.sh")), "--no-git", "--steps", "temporary-file cleanup"],
                        cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", check=False, timeout=(120 if WINDOWS else 10),
                    )
                    self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                    self.assertIn(message, result.stderr)
                    self.assertFalse((root / "home" / ".cache" / "mare" / "acceptance").exists())

    def test_git_directory_and_symlinked_ancestors_are_rejected(self):
        with self._registered_tempdir(prefix="accept-git-variants-") as tmp:
            root = Path(tmp)
            variants = ("directory",) if WINDOWS else ("directory", "dangling", "physical")
            for variant in variants:
                with self.subTest(variant=variant):
                    parent = root / variant
                    target = parent / "tmp"
                    target.mkdir(parents=True)
                    marker = parent / ".git"
                    if variant == "dangling":
                        marker.symlink_to(parent / "missing")
                    else:
                        marker.mkdir()
                    selected = target
                    if variant == "physical":
                        selected = root / "indirect"
                        selected.symlink_to(target, target_is_directory=True)
                    env = dict(
                        os.environ, HOME=shell_path(root / "home"),
                        CARGO_TARGET_DIR=shell_path(root / "target"), MARE_ACCEPT_TMPDIR=shell_path(selected),
                        MARE_ACCEPT_MIN_FREE_GB="0",
                    )
                    result = subprocess.run(
                        ["sh", shell_path(Path(__file__).with_name("accept.sh")), "--no-git", "--steps", "temporary-file cleanup"],
                        cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", check=False, timeout=(120 if WINDOWS else 10),
                    )
                    self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                    self.assertIn(".git at or above temporary root", result.stderr)
                    self.assertIn(shell_path(marker), result.stderr)

    def test_explicit_temp_root_takes_precedence_and_inherited_root_is_used(self):
        with self._registered_tempdir(prefix="accept-root-choice-") as tmp:
            root = Path(tmp)
            explicit, inherited, commands = (root / "explicit", root / "inherited", root / "bin")
            for directory in (explicit, inherited, commands):
                directory.mkdir()
            (explicit / "sibling").mkdir()
            (inherited / "sibling").mkdir()
            cargo = commands / "cargo"
            cargo.write_text('#!/bin/sh\nprintf "%s|%s|%s\\n" "$TMPDIR" "$TEMP" "$TMP" >> "$LOG"\n')
            cargo.chmod(0o755)
            cases = [("explicit", explicit), ("inherited", inherited)]
            if sys.platform.startswith("linux"):
                cases.append(("default", Path("/tmp")))
            for route, expected in cases:
                with self.subTest(route=route):
                    log = root / "log"
                    log.unlink(missing_ok=True)
                    env = dict(
                        os.environ, HOME=shell_path(root / "home"),
                        PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
                        CARGO_TARGET_DIR=shell_path(root / "target"), TMPDIR=shell_path(inherited),
                        LOG=shell_path(log), MARE_ACCEPT_MIN_FREE_GB="0",
                    )
                    if route == "explicit": env["MARE_ACCEPT_TMPDIR"] = shell_path(explicit)
                    else: env.pop("MARE_ACCEPT_TMPDIR", None)
                    default_reject = None
                    if route == "default":
                        for key in ("TMPDIR", "TEMP", "TMP"): env.pop(key, None)
                        if not (expected.is_dir() and os.access(expected, os.W_OK)):
                            default_reject = "not a writable directory"
                        else:
                            resolved = expected.resolve()
                            if any(os.path.lexists(p / ".git") for p in (resolved, *resolved.parents)):
                                default_reject = ".git at or above temporary root"
                            elif subprocess.check_output(["stat", "-f", "-c", "%T", str(resolved)], text=True, encoding="utf-8", errors="replace").strip() != "tmpfs":
                                default_reject = "temporary root is not tmpfs"
                    result = subprocess.run(
                        ["sh", shell_path(Path(__file__).with_name("accept.sh")), "--no-git", "--steps", "cargo fmt --check"],
                        cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", check=False, timeout=(120 if WINDOWS else 10),
                    )
                    if default_reject is not None:
                        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                        self.assertIn(default_reject, result.stderr)
                        continue
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    test_tmp, temp, tmp_env = log.read_text(encoding="utf-8").strip().split("|")
                    self.assertEqual(native_path(test_tmp).parent.resolve(), expected.resolve())
                    self.assertEqual((temp, tmp_env), (test_tmp, test_tmp))
                    self.assertFalse(native_path(test_tmp).exists())
                    if route != "default": self.assertEqual(sorted(path.name for path in expected.iterdir()), ["sibling"])

    def test_short_disk_tmp_and_only_new_residue_fail(self):
        with (
            self._registered_tempdir(prefix="mare-accept-legacy-") as legacy_tmp,
            tempfile.TemporaryDirectory(
                prefix="mare-accept-preexisting-", dir=legacy_tmp
            ) as old,
        ):
            # "nobinary": cargo reports no mare executable, so the required docs check cannot
            # run, and the stale debug/mare in the shared target directory must not stand in;
            # "triple": CARGO_BUILD_TARGET puts the binary under a target-triple directory
            leaks = ("none", "legacy", "hidden", "askpass", "nobinary", "triple")
            # Creating real dangling symlinks requires optional Windows privileges.
            if not WINDOWS:
                leaks += ("symlink",)
            short_dir = "/tmp" if sys.platform == "darwin" and not os.environ.get("MARE_ACCEPT_TEST_TEMP_REGISTRY") and len(os.fsencode(Path(tempfile.gettempdir()).resolve())) > 32 and Path("/tmp").is_dir() else None
            for leak in leaks:
                with self.subTest(leak=leak), self._registered_tempdir(dir=short_dir) as root:
                    root = Path(root)
                    scripts = root / "scripts"
                    scripts.mkdir()
                    shutil.copyfile(
                        Path(__file__).with_name("accept.sh"), scripts / "accept.sh"
                    )
                    (scripts / "vendor-cordis.sh").write_text("exit 0\n")
                    (scripts / "cordis-suite.sh").write_text("exit 0\n")
                    for py_stub in (
                        "check_seams.py", "check_notices.py",
                    ):
                        (scripts / py_stub).write_text("import sys; sys.exit(0)\n")
                    commands = root / "bin"
                    commands.mkdir()
                    for name in (
                        "cargo",
                        "cargo-nextest",
                        "cargo-deny",
                        "stylua",
                        "lua-language-server",
                        "ast-grep",
                        "lua",
                        "python3",
                    ):
                        command = commands / name
                        command.write_text(
                            "#!/bin/sh\n"
                            'printf "%s\\n" "$*" >> "$COMMAND_LOG"\n'
                            'case "$*" in *--doc*) echo "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured";; esac\n'
                            'case "$1 $MAKE_LEAK" in "build none") mkdir -p "$CARGO_TARGET_DIR/debug"; printf "#!/bin/sh\\nexit 0\\n" > "$CARGO_TARGET_DIR/debug/mare"; chmod +x "$CARGO_TARGET_DIR/debug/mare";; esac\n'
                            'case "$*|$MAKE_LEAK" in *"scripts/test_gen_docs.py"*"|nobinary") ;; *"scripts/test_gen_docs.py"*) [ "$MARE_BIN" = "$CARGO_TARGET_DIR/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug/mare" ] || exit 91;; esac\n'
                            # the build reports where the binary went, as real cargo does (accept.sh made the target absolute)
                            'case "$1 $MAKE_LEAK" in "build nobinary") ;; build*) printf \'{"reason":"compiler-artifact","target":{"kind":["bin"],"crate_types":["bin"],"name":"mare"},"executable":"%s"}\\n\' "$CARGO_TARGET_DIR/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug/mare";; esac\n'
                            'case "$*" in *"--profile gate"*) printf "%s\\n" "${MARE_TEST_EXPECT_BACKEND-unset}" >> "$BACKEND_LOG";; esac\n'
                            'printf "%s|%s|%s|%s|%s|%s\\n" "$CARGO_TARGET_DIR" "$TMPDIR" "$TEMP" "$TMP" "$MBX_DISABLE" "$MARE_TMPDIR" >> "$LOG"\n'
                            'case "$MAKE_LEAK" in legacy) mkdir -p "$RESIDUE";; hidden) mkdir -p "$TMPDIR/.tmp-leak";; askpass) mkdir -p "$TMPDIR/mare-askpass-leak";; symlink) ln -sf missing "$TMPDIR/dangling";; esac\n'
                        )
                        command.chmod(0o755)
                    # Model the installed shim: acceptance must explicitly disable its
                    # target remapping even if the caller enabled it.
                    shim = root / "home" / ".local" / "share" / "mbx" / "bin" / "cargo"
                    shim.parent.mkdir(parents=True)
                    shim.write_text(
                        "#!/bin/sh\n"
                        '[ "$MBX_DISABLE" = 1 ] || exit 90\n'
                        'exec "$REAL_CARGO" "$@"\n'
                    )
                    shim.chmod(0o755)
                    target_relative = "build/" + "/".join(["long-target-component"] * 6)
                    residue = f"{old}-new"
                    env = dict(
                        os.environ,
                        HOME=shell_path(root / "home"),
                        PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
                        CARGO_TARGET_DIR=target_relative,
                        MARE_TMPDIR=shell_path(root / "outside-acceptance"),
                        MBX_DISABLE="0",
                        REAL_CARGO=shell_path(commands / "cargo"),
                        MARE_ACCEPT_MIN_FREE_GB="0",
                        LOG=shell_path(root / "log"),
                        COMMAND_LOG=shell_path(root / "commands"),
                        BACKEND_LOG=shell_path(root / "backend-log"),
                        MARE_TEST_EXPECT_BACKEND="inherited-value",
                        MAKE_LEAK=leak,
                        RESIDUE=shell_path(residue),
                    )
                    env.pop("CORDIS_LUA_REPO", None)
                    if leak == "triple":
                        env["CARGO_BUILD_TARGET"] = "x86_64-unknown-linux-gnu"
                    # a binary at the guessable debug/mare in every case, stale for "triple"
                    # and "nobinary"; the triple-directory binary only where cargo puts one
                    built = [root / target_relative / "debug" / "mare"]
                    if leak == "triple":
                        built.append(root / target_relative / env["CARGO_BUILD_TARGET"] / "debug" / "mare")
                    for binary in ([] if leak == "none" else built):
                        binary.parent.mkdir(parents=True, exist_ok=True)
                        binary.write_text("#!/bin/sh\nexit 0\n")
                        binary.chmod(0o755)
                    expected_bin = shell_path(built[-1].resolve())
                    temp_root = root / "t"
                    sibling = temp_root / "parallel-owned"
                    sibling.mkdir(parents=True)
                    env["MARE_ACCEPT_TMPDIR"] = shell_path(temp_root)
                    (sibling / ".tmp-old").touch()
                    try:
                        result = subprocess.run(
                            [
                                "sh",
                                "scripts/accept.sh",
                                "--no-git",
                                "--allow-no-git",
                                "--selftest-legacy-tmp-dir",
                                shell_path(legacy_tmp),
                            ],
                            cwd=root,
                            check=False,
                            env=env,
                            capture_output=True,
                            text=True, encoding="utf-8", errors="replace",
                            timeout=(120 if WINDOWS else 10),
                        )
                        self.assertEqual(
                            result.returncode,
                            int(leak not in ("none", "triple")),
                            result.stdout + result.stderr,
                        )
                        if leak == "none":
                            self.assertIn(": PARTIAL", result.stdout)
                            self.assertNotIn(": PASS", result.stdout)
                            backend = "windows-acl" if WINDOWS else ("seatbelt" if sys.platform == "darwin" else "bwrap")
                            with self.subTest(sandbox="expected"):
                                self.assertIn(f"- sandbox: expected {backend}", result.stdout)
                                self.assertEqual((root / "backend-log").read_text(encoding="utf-8").splitlines(), [backend])
                        self.assertTrue(Path(old).exists())
                        self.assertTrue((sibling / ".tmp-old").exists())
                        self.assertEqual(
                            "owned temporary residue:" in result.stdout + result.stderr,
                            leak in ("hidden", "askpass", "symlink"),
                        )
                        self.assertIn("acceptance selftests", result.stdout)
                        self.assertIn("test home selftests", result.stdout)
                        self.assertIn("gen_docs selftests", result.stdout)
                        # doctest totals come from the doctest step, not a later one
                        self.assertIn("- doctests: 3 passed, 0 failed, 1 ignored", result.stdout)
                        self.assertEqual(
                            "| generated docs current | FAIL (cargo reported no mare executable)" in result.stdout,
                            leak == "nobinary",
                            result.stdout,
                        )
                        self.assertIn("pending snapshots", result.stdout)
                        self.assertIn("(standard):", result.stdout)
                        invocations = (root / "commands").read_text(encoding="utf-8").splitlines()
                        self.assertIn("scripts/test_accept.py", invocations)
                        self.assertIn("scripts/test_nextest_test_home.py", invocations)
                        docs_selftest = "-m unittest scripts/test_gen_docs.py scripts/test_docs_keymaps.py"
                        self.assertIn(docs_selftest, invocations)
                        self.assertLess(invocations.index("build -p mare-cli --bin mare --message-format=json"), invocations.index(docs_selftest))
                        # the docs check runs with the binary cargo built, never a stale guess
                        docs = [i for i in invocations if i.startswith("scripts/gen_docs.py")]
                        self.assertEqual(
                            docs,
                            [] if leak == "nobinary" else [f"scripts/gen_docs.py --mare {expected_bin} --check"],
                        )
                        if WINDOWS:
                            self.assertNotIn(
                                "scripts/check_lua_public_api.py --self-test", invocations
                            )
                            self.assertNotIn(
                                "scripts/check_lua_public_api.py", invocations
                            )
                        else:
                            self.assertIn("lua public API selftest", result.stdout)
                            self.assertIn("lua public API lint", result.stdout)
                            self.assertIn(
                                "scripts/check_lua_public_api.py --self-test", invocations
                            )
                            self.assertIn(
                                "scripts/check_lua_public_api.py", invocations
                            )
                        self.assertEqual(
                            f"new {shell_path(legacy_tmp)} residue:"
                            in result.stdout + result.stderr,
                            leak == "legacy",
                        )
                        for line in (root / "log").read_text(encoding="utf-8").splitlines():
                            target, tmp, windows_temp, windows_tmp, mbx_disable, mare_tmp = (
                                line.split("|")
                            )
                            self.assertEqual(native_path(target).resolve(), (root / target_relative).resolve())
                            self.assertGreater(len(os.fsencode(target)), 108)
                            self.assertEqual(native_path(tmp).parent.resolve(), temp_root.resolve())
                            self.assertEqual(len(native_path(tmp).name), 6)
                            self.assertFalse(native_path(tmp).exists())
                            self.assertEqual(windows_temp, tmp)
                            self.assertEqual(windows_tmp, tmp)
                            self.assertEqual(mare_tmp, tmp)
                            self.assertEqual(mbx_disable, "1")
                        # Include a representative fixture suffix within Linux's sockaddr_un
                        # budget, even when Cargo's own target path is already over it.
                        socket_path = native_path(tmp) / "mare-peer-01234567" / "peer.sock"
                        if not WINDOWS and hasattr(socket, "AF_UNIX"):
                            self.assertLess(len(os.fsencode(socket_path)), 108)
                            socket_path.parent.mkdir(parents=True)
                            try:
                                with socket.socket(socket.AF_UNIX) as peer:
                                    peer.bind(str(socket_path))
                            finally:
                                shutil.rmtree(native_path(tmp))
                        self.assertFalse((root / "target").exists())
                        self.assertFalse((root / target_relative / "tmp").exists())
                        self.assertFalse(list(temp_root.glob("mare-accept.*")))
                        if leak == "none":
                            degraded = subprocess.run(
                                ["sh", "scripts/accept.sh", "--allow-no-git", "--sandbox-degraded",
                                 "--selftest-legacy-tmp-dir", shell_path(legacy_tmp)],
                                cwd=root, check=False, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 10),
                            )
                            with self.subTest(sandbox="degraded"):
                                self.assertEqual(degraded.returncode, 0, degraded.stdout + degraded.stderr)
                                self.assertIn(": PARTIAL", degraded.stdout)
                                self.assertIn(
                                    "- sandbox: degraded — no expected backend, skips allowed (not a complete acceptance)",
                                    degraded.stdout,
                                )
                                self.assertEqual((root / "backend-log").read_text(encoding="utf-8").splitlines(), [backend, "unset"])
                    finally:
                        shutil.rmtree(residue, ignore_errors=True)

    def test_legacy_tmp_dir_env_var_is_rejected(self):
        accept_sh = Path(__file__).with_name("accept.sh")
        for value in ("/tmp/fake", ""):
            with self.subTest(value=value):
                env = dict(os.environ, MARE_ACCEPT_LEGACY_TMP_DIR=value)
                result = subprocess.run(
                    ["sh", str(accept_sh), "--help"],
                    check=False,
                    env=env,
                    capture_output=True,
                    text=True, encoding="utf-8", errors="replace",
                    timeout=(120 if WINDOWS else 10),
                )
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn(
                    "MARE_ACCEPT_LEGACY_TMP_DIR env var is not permitted",
                    result.stderr,
                )

        bad_flag = subprocess.run(
            ["sh", str(accept_sh), "--selftest-legacy-tmp-dir", ""],
            check=False,
            capture_output=True,
            text=True, encoding="utf-8", errors="replace",
            timeout=(120 if WINDOWS else 10),
        )
        self.assertEqual(bad_flag.returncode, 2, bad_flag.stdout + bad_flag.stderr)


REPO_ROOT = Path(__file__).resolve().parent.parent

class AcceptanceCounterProofTests(FixtureTestCase):
    def _run_steps(self, steps: str, path_env: str, home: Path) -> subprocess.CompletedProcess[str]:
        env = dict(
            os.environ,
            HOME=shell_path(home),
            PATH=path_env,
            MARE_ACCEPT_MIN_FREE_GB="0",
        )
        return subprocess.run(
            ["sh", "scripts/accept.sh", "--steps", steps],
            cwd=REPO_ROOT,
            check=False,
            env=env,
            capture_output=True,
            text=True, encoding="utf-8", errors="replace",
            timeout=(120 if WINDOWS else 15),
        )

    def test_missing_required_tool_fails(self):
        # Drop any PATH entry that contains stylua (or stylua.exe) so command -v stylua fails,
        # while keeping every other required tool available via shims.
        filtered = []
        for entry in os.environ.get("PATH", "").split(os.pathsep):
            if not entry:
                continue
            p = Path(entry)
            if (p / "stylua").exists() or (p / "stylua.exe").exists():
                continue
            filtered.append(entry)
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            shims = tmp_path / "bin"
            shims.mkdir()
            for name in (
                "cargo-nextest",
                "cargo-deny",
                "lua-language-server",
                "ast-grep",
                "lua",
                "python3",
            ):
                cmd = shims / name
                cmd.write_text("#!/bin/sh\necho '1.0.0'\n")
                cmd.chmod(0o755)
            path_env = os.pathsep.join([str(shims), *filtered])
            result = self._run_steps(
                "required tools,stylua --check",
                path_env,
                tmp_path / "home",
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("missing required tool: stylua", result.stdout + result.stderr)
            self.assertIn("| required tools | FAIL |", result.stdout)
            self.assertIn("| stylua --check | FAIL |", result.stdout)
            self.assertIn(": FAIL", result.stdout)

    def test_shim_failure_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            shims = tmp_path / "bin"
            shims.mkdir()
            bad = shims / "stylua"
            bad.write_text("#!/bin/sh\nexit 1\n")
            bad.chmod(0o755)
            path_env = os.pathsep.join((str(shims), os.environ.get("PATH", "")))
            result = self._run_steps("stylua --check", path_env, tmp_path / "home")
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn(": FAIL", result.stdout)

    def test_steps_subset_reports_partial(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            shims = tmp_path / "bin"
            shims.mkdir()
            for name in (
                "cargo-nextest",
                "cargo-deny",
                "stylua",
                "lua-language-server",
                "ast-grep",
                "lua",
                "python3",
            ):
                cmd = shims / name
                cmd.write_text("#!/bin/sh\necho '1.0.0'\n")
                cmd.chmod(0o755)
            path_env = os.pathsep.join((str(shims), os.environ.get("PATH", "")))
            result = self._run_steps(
                "required tools,stylua --check", path_env, tmp_path / "home"
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn(": PARTIAL", result.stdout)
            self.assertNotIn(": PASS", result.stdout)

    def test_stale_generated_options_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            files = ["scripts/accept.sh"]
            if not WINDOWS:
                files.extend(
                    [
                        "scripts/gen_options.py",
                        "scripts/plugin_options.py",
                        "crates/mare-config/preset/config_api.lua",
                        "crates/mare-config/preset/config_api_validate.lua",
                        "crates/mare-config/src/options_gen.rs",
                        "crates/mare-plugin/preset/types/mare/config.lua",
                        "crates/mare-plugin/preset/types/mare/plugin_options.gen.lua",
                    ]
                )
            for rel in files:
                dst = root / rel
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(REPO_ROOT / rel, dst)
            if not WINDOWS:
                shutil.copytree(
                    REPO_ROOT / "crates/mare-plugin/preset/plugins",
                    root / "crates/mare-plugin/preset/plugins",
                )

            env = dict(
                os.environ,
                HOME=shell_path(root / "home"),
                CARGO_TARGET_DIR=shell_path(root / "target"),
                MARE_ACCEPT_MIN_FREE_GB="0",
            )

            def run_generated_options():
                return subprocess.run(
                    ["sh", "scripts/accept.sh", "--steps", "generated options"],
                    cwd=root,
                    check=False,
                    env=env,
                    capture_output=True,
                    text=True, encoding="utf-8", errors="replace",
                    timeout=(120 if WINDOWS else 15),
                )

            ok = run_generated_options()
            self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
            self.assertIn(": PARTIAL", ok.stdout)
            if WINDOWS:
                self.assertIn("| generated options | n/a (Linux/macOS only) |", ok.stdout)
                return

            self.assertIn("| generated options | pass |", ok.stdout)

            target = root / "crates/mare-config/src/options_gen.rs"
            data = target.read_text(encoding="utf-8")
            target.write_text(data + " ", encoding="utf-8")

            stale = run_generated_options()
            self.assertNotEqual(stale.returncode, 0, stale.stdout + stale.stderr)
            self.assertIn("| generated options | FAIL |", stale.stdout)
            self.assertIn(": FAIL", stale.stdout)


    def _prepare_release_mode_repo(self, root: Path) -> tuple[Path, dict[str, str]]:
        scripts = root / "scripts"
        scripts.mkdir()
        for name in (
            "accept.sh",
            "check_seams.py",
        ):
            shutil.copyfile(Path(__file__).with_name(name), scripts / name)
        if WINDOWS:
            seam = scripts / "check_seams.py"
            # Fixture binaries are Python scripts, so native Windows cannot exec them directly.
            seam.write_text(seam.read_text(encoding="utf-8").replace("[str(binary),", "[sys.executable, str(binary),"), encoding="utf-8")
        (scripts / "vendor-cordis.sh").write_text("exit 0\n")
        (scripts / "cordis-suite.sh").write_text("exit 0\n")
        subprocess.run(["git", "init", "-q", str(root)], check=True)

        (root / "crates" / "a" / "src").mkdir(parents=True)
        (root / "crates" / "a" / "src" / "ok.rs").write_text(
            "fn f() {}\n",
            encoding="utf-8",
        )

        commands = root / "bin"
        commands.mkdir()
        for name in ("cargo-nextest", "cargo-deny", "stylua", "lua-language-server", "ast-grep", "lua"):
            command = commands / name
            command.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$COMMAND_LOG"\n'
            )
            command.chmod(0o755)

        # Real python3 wrapper: skip recursive selftests / docs scripts,
        # but execute the real check_seams.py release/debug binary probes!
        py_cmd = commands / "python3"
        py_cmd.write_text(
            f"#!/bin/sh\n"
            'printf "%s\\n" "$*" >> "$COMMAND_LOG"\n'
            'case "$*" in\n'
            '  scripts/nextest_test_home.py*) echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured"; exit 0 ;;\n'
            '  scripts/test_accept.py*|scripts/test_nextest_test_home.py*|"-m unittest scripts/test_gen_docs.py"*|scripts/test_gen_docs.py*|scripts/check_notices.py*|scripts/gen_options.py*|scripts/gen_docs.py*|scripts/check_lua_public_api.py*) exit 0 ;;\n'
            f'  *) exec "{sys.executable}" "$@" ;;\n'
            "esac\n"
        )
        py_cmd.chmod(0o755)

        mock_bin_template = f"""#!{sys.executable}
# __FAUX_ARTIFACT_MARKER__
import base64, json, os, pathlib, sys, urllib.request
faux_enabled = __FAUX_ENABLED__
debug_enabled = __DEBUG_ENABLED__
if "-p" in sys.argv:
    if faux_enabled:
        print("echo: hi")
        sys.exit(0)
    print("error: unknown model: faux/echo", file=sys.stderr)
    sys.exit(1)
if "--peer" in sys.argv:
    peers = pathlib.Path(os.environ["MARE_HOME"]) / "state" / "peers"
    peers.mkdir(parents=True, exist_ok=True)
    sock = peers / f"{{os.getpid()}}.sock"
    forced = os.environ.get("MARE_TEST_PIPE_FIRST_NAME", "")
    chosen = forced if (debug_enabled and forced) else r"\\\\.\\pipe\\mare-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    sock.write_text(chosen, encoding="utf-8")
    for line in sys.stdin:
        req = json.loads(line)
        if req.get("method") == "shutdown":
            sock.unlink(missing_ok=True)
        print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "result": {{}}}}), flush=True)
    sys.exit(0)
if "--rpc" in sys.argv:
    if "--no-session" not in sys.argv and os.environ.get("MARE_PROBE_OUT"):
        bug = os.environ.get("MARE_ACCEPT_FAKE_SEAM_BUG", "")
        if "MARE_TEST_OFFLINE" in os.environ:
            offline_active = (
                (debug_enabled and bug != "debug-ignores-offline")
                or (not debug_enabled and bug == "release-enforces-offline")
            )
            if offline_active and os.environ.get("MARE_TEST_ALLOW_LIVE") != "1":
                msg = "ok=false;error=offline test guard blocked HTTP request\\n"
            else:
                proxy_url = os.environ.get("HTTP_PROXY", "")
                with urllib.request.urlopen(proxy_url, timeout=5) as resp:
                    body = resp.read().decode("utf-8")
                msg = f"ok=true;body={{body}}\\n"
        elif debug_enabled and os.environ.get("MARE_TEST_CLIPBOARD") == "osc52":
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
            if debug_enabled and os.environ.get("MARE_TEST_FS_FAULT") == f"write-then-eio:{{p.name}}":
                p.write_bytes(raw[: len(raw) // 2])
                print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "error": {{"message": "injected I/O error (MARE_TEST_FS_FAULT)"}}}}), flush=True)
            else:
                p.write_bytes(raw)
                print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "result": {{}}}}), flush=True)
        elif m == "session.load":
            sess = pathlib.Path(req["params"]["path"]).name
            if debug_enabled and os.environ.get("MARE_TEST_UNSTABLE_READ") == sess:
                print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "error": {{"message": "session file keeps changing while it is opened"}}}}), flush=True)
            else:
                print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "result": {{"id": "busy"}}}}), flush=True)
        elif m == "llm.list_models":
            models = [{{"provider": "openai", "ref": "openai/gpt-6.1-terra"}}]
            if faux_enabled:
                models.append({{"provider": "faux", "ref": "faux/echo"}})
            print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "result": {{"data": models}}}}), flush=True)
        else:
            print(json.dumps({{"jsonrpc": "2.0", "id": req["id"], "result": {{}}}}), flush=True)
"""
        cargo_cmd = commands / "cargo"
        cargo_cmd.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            'log = pathlib.Path(os.environ["COMMAND_LOG"])\n'
            'with log.open("a", encoding="utf-8") as fh:\n'
            '    fh.write(" ".join(sys.argv[1:]) + "\\n")\n'
            'if sys.argv[1:3] == ["nextest", "run"] and "gate" in sys.argv:\n'
            '    print("     Summary [   0.001s] 0 tests run: 0 passed, 0 skipped")\n'
            'if len(sys.argv) >= 2 and sys.argv[1] == "build":\n'
            '    is_release = "--release" in sys.argv\n'
            '    profile = "release" if is_release else "debug"\n'
            '    out = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / profile / ("mare.exe" if os.name == "nt" else "mare")\n'
            "    out.parent.mkdir(parents=True, exist_ok=True)\n"
            '    faux = "--features" in sys.argv and "faux" in sys.argv\n'
            f"    tmpl = {mock_bin_template!r}\n"
            '    marker = "MARE_FAUX_SCRIPT / Faux Echo" if faux else ""\n'
            '    rendered = tmpl.replace("__FAUX_ENABLED__", "True" if faux else "False").replace("__DEBUG_ENABLED__", "False" if is_release else "True").replace("__FAUX_ARTIFACT_MARKER__", marker)\n'
            '    out.write_text(rendered, encoding="utf-8")\n'
            "    out.chmod(0o755)\n"
            '    if "--message-format=json" in sys.argv:\n'
            '        print(json.dumps({"reason": "compiler-artifact", "target": {"kind": ["bin"], "crate_types": ["bin"], "name": "mare"}, "executable": str(out)}, separators=(",", ":")))\n'
        )
        cargo_cmd.chmod(0o755)

        (root / "legacy-tmp").mkdir()
        env = dict(
            os.environ,
            HOME=shell_path(root / "home"),
            PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
            CARGO_TARGET_DIR=shell_path(root / "target"),
            MBX_DISABLE="1",
            MARE_ACCEPT_MIN_FREE_GB="0",
            COMMAND_LOG=shell_path(root / "commands"),
        )
        return scripts, env

    def test_release_mode_runs_three_steps_and_labels_report(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts, env = self._prepare_release_mode_repo(root)
            result = subprocess.run(
                [
                    "sh",
                    "scripts/accept.sh",
                    "--release",
                    "--selftest-legacy-tmp-dir",
                    shell_path(root / "legacy-tmp"),
                ],
                cwd=root,
                check=False,
                env=env,
                capture_output=True,
                text=True, encoding="utf-8", errors="replace",
                timeout=(120 if WINDOWS else 90),
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("(release): PASS", result.stdout)
            for step_name in (
                "cordis vendor check", "cordis suite", "release tests compile", "release seams inert",
                "release faux absent", "debug seams active",
            ):
                self.assertIn(f"| {step_name} | pass |", result.stdout)
            invocations = (root / "commands").read_text(encoding="utf-8").splitlines()
            self.assertIn("check --release --workspace --all-targets", invocations)
            self.assertIn("build --release --locked -p mare-cli --bin mare", invocations)
            self.assertIn("build --release --locked -p mare-cli --bin mare --features faux", invocations)
            self.assertIn("build --locked -p mare-cli --bin mare", invocations)
            release_bin = root / "target" / "release" / ("mare.exe" if WINDOWS else "mare")
            self.assertTrue(release_bin.is_file())
            faux_check = subprocess.run(
                [sys.executable, str(scripts / "check_seams.py"), "--verify-faux-absent", str(release_bin)],
                cwd=root, check=False, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 15),
            )
            self.assertEqual(faux_check.returncode, 0, f"final release binary must not be faux: {faux_check.stdout}{faux_check.stderr}")

    def test_standard_mode_runs_and_enforces_cordis_vendor_check(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts, env = self._prepare_release_mode_repo(root)
            tail = ["--steps", "cordis vendor check", "--selftest-legacy-tmp-dir", shell_path(root / "legacy-tmp")]
            ok = subprocess.run(["sh", "scripts/accept.sh"] + tail, cwd=root, check=False, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 30))
            self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
            self.assertIn("| cordis vendor check | pass |", ok.stdout)
            (scripts / "vendor-cordis.sh").write_text("echo 'vendor drift' >&2\nexit 1\n")
            for extra in ([], ["--release"]):
                with self.subTest(extra=extra):
                    fail = subprocess.run(["sh", "scripts/accept.sh"] + extra + tail, cwd=root, check=False, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 30))
                    self.assertNotEqual(fail.returncode, 0, fail.stdout + fail.stderr)
                    self.assertIn("| cordis vendor check | FAIL |", fail.stdout)

    def test_cordis_suite_enforced_in_standard_and_release_modes(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts, env = self._prepare_release_mode_repo(root)
            tail = ["--steps", "build mare,cordis suite", "--selftest-legacy-tmp-dir", shell_path(root / "legacy-tmp")]
            run = lambda args: subprocess.run(args, cwd=root, check=False, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 30))
            ok = run(["sh", "scripts/accept.sh"] + tail)
            self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
            self.assertIn("| cordis suite | pass |", ok.stdout)
            (scripts / "cordis-suite.sh").write_text("echo 'suite failed' >&2\nexit 1\n")
            for extra in ([], ["--release"]):
                fail = run(["sh", "scripts/accept.sh"] + extra + tail)
                self.assertNotEqual(fail.returncode, 0, fail.stdout + fail.stderr)
                self.assertIn("| cordis suite | FAIL |", fail.stdout)
            no_bin = run(["sh", "scripts/accept.sh", "--steps", "cordis suite", "--selftest-legacy-tmp-dir", shell_path(root / "legacy-tmp")])
            self.assertNotEqual(no_bin.returncode, 0, no_bin.stdout + no_bin.stderr)
            self.assertIn("| cordis suite | FAIL (cargo reported no mare executable) |", no_bin.stdout)

    def test_release_mode_fails_when_offline_seam_misbehaves(self):
        for bug, failed_step in (
            ("release-enforces-offline", "release seams inert"),
            ("debug-ignores-offline", "debug seams active"),
        ):
            with self.subTest(bug=bug), tempfile.TemporaryDirectory() as root:
                root = Path(root)
                _, env = self._prepare_release_mode_repo(root)
                env["MARE_ACCEPT_FAKE_SEAM_BUG"] = bug
                result = subprocess.run(
                    ["sh", "scripts/accept.sh", "--release"],
                    cwd=root,
                    check=False,
                    env=env,
                    capture_output=True,
                    text=True, encoding="utf-8", errors="replace",
                    timeout=(120 if WINDOWS else 90),
                )
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"| {failed_step} | FAIL |", result.stdout)


    def test_release_mode_partial_exits_nonzero(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = self._prepare_release_mode_repo(root)
            result = subprocess.run(
                [
                    "sh",
                    "scripts/accept.sh",
                    "--release",
                    "--sandbox-degraded",
                    "--selftest-legacy-tmp-dir",
                    shell_path(root / "legacy-tmp"),
                ],
                cwd=root,
                check=False,
                env=env,
                capture_output=True,
                text=True, encoding="utf-8", errors="replace",
                timeout=(120 if WINDOWS else 90),
            )
            output = result.stdout + result.stderr
            self.assertIn("(release): PARTIAL", output)
            self.assertNotEqual(result.returncode, 0, output)

    def test_release_acceptance_on_git_checkout_uses_merge_base_and_records_full_sha_and_tree(self):
        if WINDOWS:
            return

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            origin = tmp_path / "origin"
            origin.mkdir()
            _, origin_env = self._prepare_release_mode_repo(origin)
            subprocess.run(["git", "-C", str(origin), "config", "user.email", "test@example.com"], check=True)
            subprocess.run(["git", "-C", str(origin), "config", "user.name", "Test"], check=True)
            subprocess.run(["git", "-C", str(origin), "config", "commit.gpgsign", "false"], check=True)
            (origin / "README.md").write_text("c1\n")
            subprocess.run(["git", "-C", str(origin), "add", "."], check=True)
            subprocess.run(["git", "-C", str(origin), "commit", "-q", "-m", "first"], check=True)
            (origin / "README.md").write_text("c2\n")
            subprocess.run(["git", "-C", str(origin), "add", "."], check=True)
            subprocess.run(["git", "-C", str(origin), "commit", "-q", "-m", "second"], check=True)

            cloned = tmp_path / "cloned"
            subprocess.run(
                ["git", "clone", f"file://{origin}", str(cloned)],
                check=True,
                capture_output=True,
            )
            env = dict(
                origin_env,
                HOME=shell_path(tmp_path / "home"),
                PATH=os.pathsep.join((str(cloned / "bin"), os.environ["PATH"])),
                CARGO_TARGET_DIR=shell_path(tmp_path / "target"),
                COMMAND_LOG=shell_path(tmp_path / "commands"),
            )
            result = subprocess.run(
                ["sh", "scripts/accept.sh", "--release"],
                cwd=cloned,
                check=False,
                env=env,
                capture_output=True,
                text=True, encoding="utf-8", errors="replace",
                timeout=90,
            )
            output = result.stdout + result.stderr
            self.assertEqual(result.returncode, 0, output)
            head_sha = subprocess.check_output(["git", "-C", str(cloned), "rev-parse", "HEAD"], text=True, encoding="utf-8", errors="replace").strip()
            tree_sha = subprocess.check_output(["git", "-C", str(cloned), "rev-parse", "HEAD^{tree}"], text=True, encoding="utf-8", errors="replace").strip()
            base_sha = subprocess.check_output(["git", "-C", str(cloned), "rev-parse", "HEAD~1"], text=True, encoding="utf-8", errors="replace").strip()
            self.assertIn(f"scripts/check_lua_public_api.py --base {base_sha}", output)
            self.assertIn(f"- git: `{head_sha}` (tree `{tree_sha}`) (clean checkout)", output)
            self.assertIn("- scope: this platform only (", output)
            self.assertIn("(release): PASS", output)
            self.assertNotIn("PARTIAL", output)

    @staticmethod
    def _win_kernel32():
        import ctypes
        from ctypes import wintypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p]
        kernel32.CreateJobObjectW.restype = wintypes.HANDLE
        kernel32.SetInformationJobObject.argtypes = [
            wintypes.HANDLE,
            ctypes.c_int,
            ctypes.c_void_p,
            wintypes.DWORD,
        ]
        kernel32.SetInformationJobObject.restype = wintypes.BOOL
        kernel32.AssignProcessToJobObject.argtypes = [
            wintypes.HANDLE,
            wintypes.HANDLE,
        ]
        kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
        kernel32.TerminateJobObject.argtypes = [
            wintypes.HANDLE,
            wintypes.UINT,
        ]
        kernel32.TerminateJobObject.restype = wintypes.BOOL
        kernel32.OpenProcess.argtypes = [
            wintypes.DWORD,
            wintypes.BOOL,
            wintypes.DWORD,
        ]
        kernel32.OpenProcess.restype = wintypes.HANDLE
        kernel32.WaitForSingleObject.argtypes = [
            wintypes.HANDLE,
            wintypes.DWORD,
        ]
        kernel32.WaitForSingleObject.restype = wintypes.DWORD
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL
        kernel32.SwitchToThread.argtypes = []
        kernel32.SwitchToThread.restype = wintypes.BOOL
        return kernel32

    @classmethod
    def _assign_win_job(cls, proc: subprocess.Popen[str]) -> int | None:
        import ctypes
        from ctypes import wintypes

        class _IoCounters(ctypes.Structure):
            _fields_ = [
                ("ReadOperationCount", ctypes.c_uint64),
                ("WriteOperationCount", ctypes.c_uint64),
                ("OtherOperationCount", ctypes.c_uint64),
                ("ReadTransferCount", ctypes.c_uint64),
                ("WriteTransferCount", ctypes.c_uint64),
                ("OtherTransferCount", ctypes.c_uint64),
            ]

        class _JobBasicLimit(ctypes.Structure):
            _fields_ = [
                ("PerProcessUserTimeLimit", ctypes.c_int64),
                ("PerJobUserTimeLimit", ctypes.c_int64),
                ("LimitFlags", wintypes.DWORD),
                ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t),
                ("ActiveProcessLimit", wintypes.DWORD),
                ("Affinity", ctypes.c_size_t),
                ("PriorityClass", wintypes.DWORD),
                ("SchedulingClass", wintypes.DWORD),
            ]

        class _JobExtendedLimit(ctypes.Structure):
            _fields_ = [
                ("BasicLimitInformation", _JobBasicLimit),
                ("IoInfo", _IoCounters),
                ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t),
                ("PeakJobMemoryUsed", ctypes.c_size_t),
            ]

        kernel32 = cls._win_kernel32()
        job = kernel32.CreateJobObjectW(None, None)
        if not job:
            return None
        info = _JobExtendedLimit()
        info.BasicLimitInformation.LimitFlags = 0x00002000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not kernel32.SetInformationJobObject(
            job, 9, ctypes.byref(info), ctypes.sizeof(info)
        ):
            kernel32.CloseHandle(job)
            return None
        if not kernel32.AssignProcessToJobObject(job, int(proc._handle)):
            kernel32.CloseHandle(job)
            return None
        return int(job)

    @classmethod
    def _kill_proc_tree(
        cls, proc: subprocess.Popen[str], job_handle: int | None = None
    ) -> None:
        if not WINDOWS:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            return

        terminated = False
        if job_handle is not None:
            terminated = bool(cls._win_kernel32().TerminateJobObject(job_handle, 1))
        if not terminated:
            try:
                subprocess.run(
                    ["taskkill", "/T", "/F", "/PID", str(proc.pid)],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                    timeout=5,
                )
            except (OSError, subprocess.TimeoutExpired):
                pass
        if proc.poll() is None:
            proc.kill()

    @classmethod
    def _is_pid_alive(cls, pid: int) -> bool:
        if not WINDOWS:
            try:
                os.kill(pid, 0)
                return True
            except ProcessLookupError:
                return False
            except PermissionError:
                return True

        import ctypes

        kernel32 = cls._win_kernel32()
        handle = kernel32.OpenProcess(0x00101000, False, pid)
        if not handle:
            return ctypes.get_last_error() == 5
        try:
            return kernel32.WaitForSingleObject(handle, 0) == 0x00000102
        finally:
            kernel32.CloseHandle(handle)

    def _run_concurrent_workers(
        self,
        cmds: list[list[str]],
        cwd: Path,
        envs: list[dict[str, str]],
        timeout: float = 60.0,
    ) -> list[tuple[int, str, str]]:
        procs: list[subprocess.Popen[str]] = []
        jobs: list[int | None] = []
        had_error = True
        popen_kwargs: dict[str, object] = {}
        if WINDOWS:
            popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        else:
            popen_kwargs["start_new_session"] = True
        try:
            for cmd, env in zip(cmds, envs):
                proc = subprocess.Popen(
                    cmd,
                    cwd=cwd,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True, encoding="utf-8", errors="replace",
                    **popen_kwargs,
                )
                procs.append(proc)
                jobs.append(self._assign_win_job(proc) if WINDOWS else None)
            with concurrent.futures.ThreadPoolExecutor(max_workers=len(procs)) as pool:
                futures = [
                    pool.submit(proc.communicate, timeout=timeout) for proc in procs
                ]
                outputs = [future.result() for future in futures]
            had_error = False
            return [
                (proc.returncode, stdout, stderr)
                for proc, (stdout, stderr) in zip(procs, outputs)
            ]
        finally:
            for proc, job_handle in zip(procs, jobs):
                try:
                    if had_error or proc.poll() is None:
                        self._kill_proc_tree(proc, job_handle)
                        try:
                            proc.communicate(timeout=5)
                        except subprocess.TimeoutExpired:
                            self._kill_proc_tree(proc, job_handle)
                            for stream in (proc.stdout, proc.stderr):
                                if stream is not None:
                                    stream.close()
                            try:
                                proc.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                pass
                finally:
                    if job_handle is not None:
                        self._win_kernel32().CloseHandle(job_handle)

    def test_concurrent_runs_do_not_interfere_and_leave_no_tmp_residue(self):
        repo_root = Path(__file__).resolve().parents[1]
        system_tmp = native_path("/tmp")
        cmd = [sys.executable, str(Path(__file__).resolve()),
               "AcceptanceTempTests.test_short_disk_tmp_and_only_new_residue_fail"]
        modes = ["isolated"]
        if not WINDOWS:
            modes.extend(["slash_tmp", "unset"])
        conc_dir = str(system_tmp) if sys.platform == "darwin" and len(os.fsencode(Path(tempfile.gettempdir()).resolve())) > 32 and system_tmp.is_dir() else None
        for mode in modes:
            with (
                self.subTest(mode=mode),
                tempfile.TemporaryDirectory(prefix="ac-", dir=conc_dir) as tmpdir,
                tempfile.TemporaryDirectory(prefix="accept-reg-") as regdir,
            ):
                tmpdir_path = Path(tmpdir)
                regdir_path = Path(regdir)
                worker_envs: list[dict[str, str]] = []
                reg_files: list[Path] = []
                for idx in range(2):
                    worker_env = dict(os.environ)
                    worker_env.pop("MARE_ACCEPT_TMPDIR", None)
                    if mode == "isolated":
                        worker_env["TMPDIR"] = str(tmpdir_path)
                        worker_env["TEMP"] = str(tmpdir_path)
                        worker_env["TMP"] = str(tmpdir_path)
                    elif mode == "slash_tmp":
                        worker_env["TMPDIR"] = "/tmp"
                        worker_env["TEMP"] = "/tmp"
                        worker_env["TMP"] = "/tmp"
                    else:
                        for key in ("TMPDIR", "TEMP", "TMP"):
                            worker_env.pop(key, None)
                    reg_file = regdir_path / f"worker-{idx}.txt"
                    worker_env["MARE_ACCEPT_TEST_TEMP_REGISTRY"] = str(reg_file)
                    worker_envs.append(worker_env)
                    reg_files.append(reg_file)

                results = self._run_concurrent_workers(
                    [cmd, cmd], repo_root, worker_envs, timeout=(300.0 if WINDOWS else 60.0)
                )
                non_tmpfs = (
                    sys.platform.startswith("linux") and mode in ("slash_tmp", "unset")
                    and subprocess.check_output(["stat", "-f", "-c", "%T", str(system_tmp)], text=True, encoding="utf-8", errors="replace").strip() != "tmpfs"
                )
                for idx, (rc, stdout, stderr) in enumerate(results):
                    if non_tmpfs:
                        self.assertNotEqual(rc, 0, f"worker {idx} ({mode}) accepted a non-tmpfs root")
                        self.assertIn("temporary root is not tmpfs", stdout + stderr)
                    else:
                        self.assertEqual(rc, 0, f"concurrent worker {idx} ({mode}) failed (rc={rc}):\n{stdout}\n{stderr}")
                registered_by_worker: list[list[Path]] = []
                for idx, reg_file in enumerate(reg_files):
                    self.assertTrue(reg_file.is_file(), f"worker {idx} ({mode}) did not record its temp dirs")
                    paths = [Path(line) for line in reg_file.read_text(encoding="utf-8").splitlines() if line]
                    self.assertTrue(paths, f"worker {idx} ({mode}) recorded empty temp dir list")
                    for recorded in paths:
                        self.assertFalse(recorded.exists(), f"worker {idx} ({mode}) left registered temp dir: {recorded}")
                    registered_by_worker.append(paths)
                self.assertTrue(
                    set(registered_by_worker[0]).isdisjoint(set(registered_by_worker[1])),
                    f"workers ({mode}) collided on temp dirs: {registered_by_worker}",
                )
                if mode == "isolated":
                    self.assertEqual(list(tmpdir_path.iterdir()), [], f"residue left in TMPDIR ({mode}): {list(tmpdir_path.iterdir())}")
                    expected_parent = tmpdir_path.resolve()
                    for recorded in registered_by_worker[0] + registered_by_worker[1]:
                        self.assertEqual(recorded.parent, expected_parent)
                elif system_tmp.is_dir():
                    expected_parent = system_tmp.resolve()
                    for recorded in registered_by_worker[0] + registered_by_worker[1]:
                        self.assertEqual(recorded.parent, expected_parent)

    @unittest.skipIf(WINDOWS, "/tmp prefix isolation check is Unix-specific")
    def test_concurrent_runs_ignore_unrelated_tmp_mare_accept_dirs(self):
        system_tmp = native_path("/tmp")
        if not system_tmp.is_dir():
            self.skipTest("/tmp does not exist")
        unrelated = Path(
            tempfile.mkdtemp(prefix="mare-accept-unrelated-", dir=str(system_tmp))
        )
        try:
            self.test_concurrent_runs_do_not_interfere_and_leave_no_tmp_residue()
        finally:
            shutil.rmtree(unrelated, ignore_errors=True)

    def test_concurrent_helper_enforces_timeout_on_full_pipe_and_reaps_on_spawn_error(
        self,
    ):
        repo_root = Path(__file__).resolve().parents[1]
        noisy_hang = [
            sys.executable,
            "-c",
            "import sys, time; sys.stdout.write('x' * 200000); sys.stdout.flush(); time.sleep(30)",
        ]
        with self.assertRaises(subprocess.TimeoutExpired):
            self._run_concurrent_workers(
                [noisy_hang, noisy_hang],
                repo_root,
                [dict(os.environ), dict(os.environ)],
                timeout=0.5,
            )
        with self.assertRaises(OSError):
            self._run_concurrent_workers(
                [
                    [sys.executable, "-c", "import time; time.sleep(30)"],
                    ["/nonexistent-mare-binary-for-cleanup-test"],
                ],
                repo_root,
                [dict(os.environ), dict(os.environ)],
                timeout=5.0,
            )

    def test_concurrent_helper_kills_descendants_holding_pipes_in_bounded_time(self):
        repo_root = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory(prefix="accept-desc-") as tmpdir:
            pid_file = Path(tmpdir) / "descendant.pid"
            # Parent spawns a descendant that inherits stdout/stderr pipes and blocks,
            # then parent exits so only the descendant keeps the pipes open.
            parent_script = (
                "import subprocess, sys; "
                "child = subprocess.Popen([sys.executable, '-c', "
                "'import os, pathlib, threading; "
                "pathlib.Path(os.environ[\"MARE_DESC_PID_FILE\"]).write_text(str(os.getpid())); "
                "threading.Event().wait()']); "
                "sys.stdout.write('parent-done\\n'); sys.stdout.flush()"
            )
            desc_env = dict(os.environ, MARE_DESC_PID_FILE=str(pid_file))
            start = time.monotonic()
            with self.assertRaises(subprocess.TimeoutExpired):
                self._run_concurrent_workers(
                    [[sys.executable, "-c", parent_script]],
                    repo_root,
                    [desc_env],
                    timeout=0.8,
                )
            elapsed = time.monotonic() - start
            self.assertLess(
                elapsed,
                5.0,
                f"_run_concurrent_workers waited too long on pipe-holding descendant: {elapsed:.2f}s",
            )
            self.assertTrue(pid_file.is_file(), "descendant did not record its PID")
            descendant_pid = int(pid_file.read_text(encoding="utf-8").strip())
            reap_deadline = time.monotonic() + 2.0
            alive = True
            while time.monotonic() < reap_deadline:
                if not self._is_pid_alive(descendant_pid):
                    alive = False
                    break
                if WINDOWS:
                    self._win_kernel32().SwitchToThread()
                else:
                    os.sched_yield()
            self.assertFalse(
                alive, f"descendant process {descendant_pid} survived timeout cleanup"
            )

    @unittest.skipIf(WINDOWS, "POSIX process-group signals and chmod 0555 are Unix-only")
    def test_release_faux_target_cleaned_on_signal_interruption(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(Path(__file__).with_name("accept.sh"), scripts / "accept.sh")
            (scripts / "check_seams.py").write_text("import sys; sys.exit(0)\n")

            commands = root / "bin"
            commands.mkdir()
            for name in (
                "cargo-nextest",
                "cargo-deny",
                "stylua",
                "lua-language-server",
                "ast-grep",
                "lua",
                "python3",
            ):
                cmd = commands / name
                cmd.write_text("#!/bin/sh\nexit 0\n")
                cmd.chmod(0o755)

            started_flag = root / "faux-started"
            cargo = commands / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '  *"--features faux"*)\n'
                '    mkdir -p "$CARGO_TARGET_DIR/release"\n'
                '    : > "$CARGO_TARGET_DIR/release/mare"\n'
                f'    : > "{shell_path(started_flag)}"\n'
                "    sleep 30\n"
                "    ;;\n"
                "esac\n"
                "exit 0\n"
            )
            cargo.chmod(0o755)

            target_dir = root / "target"
            (target_dir / "release").mkdir(parents=True)
            (target_dir / "release" / "mare").write_text("#!/bin/sh\nexit 0\n")

            env = dict(
                os.environ,
                HOME=shell_path(root / "home"),
                PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
                CARGO_TARGET_DIR=shell_path(target_dir),
                MBX_DISABLE="1",
                MARE_ACCEPT_MIN_FREE_GB="0",
            )
            proc = subprocess.Popen(
                ["sh", "scripts/accept.sh", "--release", "--steps", "release faux absent", "--allow-no-git"],
                cwd=root,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True, encoding="utf-8", errors="replace",
                start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 5.0
                while not started_flag.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)  # sleep-ok: polling subprocess flag file with 5s deadline
                self.assertTrue(started_flag.exists(), "faux cargo build did not start in time")
                os.killpg(proc.pid, signal.SIGTERM)
                stdout, stderr = proc.communicate(timeout=5)
            except Exception:
                proc.kill()
                proc.communicate()
                raise

            self.assertNotEqual(proc.returncode, 0, stdout + stderr)
            leftover = list(target_dir.glob("faux-target.*"))
            self.assertEqual(
                leftover,
                [],
                f"faux-target directory leaked after SIGTERM: {leftover}",
            )

    @unittest.skipIf(WINDOWS, "POSIX process-group signals are Unix-only")
    def test_release_faux_target_cleaned_when_signaled_at_creation(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(Path(__file__).with_name("accept.sh"), scripts / "accept.sh")
            (scripts / "check_seams.py").write_text("import sys; sys.exit(0)\n")

            commands = root / "bin"
            commands.mkdir()
            for name in (
                "cargo",
                "cargo-nextest",
                "cargo-deny",
                "stylua",
                "lua-language-server",
                "ast-grep",
                "lua",
                "python3",
            ):
                cmd = commands / name
                cmd.write_text("#!/bin/sh\nexit 0\n")
                cmd.chmod(0o755)

            created_flag = root / "faux-created"
            for tool_name in ("mktemp", "mkdir"):
                real_tool = shutil.which(tool_name)
                self.assertIsNotNone(real_tool, f"{tool_name} must exist on PATH")
                wrapper = commands / tool_name
                wrapper.write_text(
                    "#!/bin/sh\n"
                    'faux_hit=0\n'
                    'for arg in "$@"; do\n'
                    '  case "$arg" in\n'
                    '    *faux-target.*) faux_hit=1 ;;\n'
                    '  esac\n'
                    'done\n'
                    'if [ "$faux_hit" -eq 1 ]; then\n'
                    f'  "{shell_path(real_tool)}" "$@" || exit $?\n'
                    f'  : > "{shell_path(created_flag)}"\n'
                    "  sleep 30\n"
                    "  exit 0\n"
                    "fi\n"
                    f'exec "{shell_path(real_tool)}" "$@"\n'
                )
                wrapper.chmod(0o755)

            target_dir = root / "target"
            (target_dir / "release").mkdir(parents=True)
            (target_dir / "release" / "mare").write_text("#!/bin/sh\nexit 0\n")

            env = dict(
                os.environ,
                HOME=shell_path(root / "home"),
                PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
                CARGO_TARGET_DIR=shell_path(target_dir),
                MBX_DISABLE="1",
                MARE_ACCEPT_MIN_FREE_GB="0",
            )
            proc = subprocess.Popen(
                ["sh", "scripts/accept.sh", "--release", "--steps", "release faux absent", "--allow-no-git"],
                cwd=root,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True, encoding="utf-8", errors="replace",
                start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 5.0
                while not created_flag.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)  # sleep-ok: polling subprocess flag file with 5s deadline
                self.assertTrue(created_flag.exists(), "faux-target directory was not created in time")
                os.killpg(proc.pid, signal.SIGTERM)
                stdout, stderr = proc.communicate(timeout=5)
            except Exception:
                proc.kill()
                proc.communicate()
                raise

            self.assertNotEqual(proc.returncode, 0, stdout + stderr)
            leftover = list(target_dir.glob("faux-target.*"))
            self.assertEqual(
                leftover,
                [],
                f"faux-target directory leaked when signaled at creation: {leftover}",
            )

    @unittest.skipIf(WINDOWS, "POSIX shell rm wrapper is Unix-only")
    def test_release_faux_target_rm_failure_fails_acceptance(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(Path(__file__).with_name("accept.sh"), scripts / "accept.sh")
            (scripts / "check_seams.py").write_text(
                "import sys\n"
                "if '--verify-faux-absent' in sys.argv and 'faux-target.' in sys.argv[-1]:\n"
                "    sys.exit(1)\n"
                "sys.exit(0)\n"
            )

            commands = root / "bin"
            commands.mkdir()
            for name in (
                "cargo-nextest",
                "cargo-deny",
                "stylua",
                "lua-language-server",
                "ast-grep",
                "lua",
            ):
                cmd = commands / name
                cmd.write_text("#!/bin/sh\nexit 0\n")
                cmd.chmod(0o755)
            py_cmd = commands / "python3"
            py_cmd.write_text(f'#!/bin/sh\nexec "{shell_path(sys.executable)}" "$@"\n')
            py_cmd.chmod(0o755)

            real_rm = shutil.which("rm")
            self.assertIsNotNone(real_rm, "rm must exist on PATH")
            rm_cmd = commands / "rm"
            rm_cmd.write_text(
                "#!/bin/sh\n"
                'for arg in "$@"; do\n'
                '  case "$arg" in\n'
                '    *faux-target.*) exit 1 ;;\n'
                '  esac\n'
                'done\n'
                f'exec "{shell_path(real_rm)}" "$@"\n'
            )
            rm_cmd.chmod(0o755)

            cargo = commands / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '  *"--features faux"*)\n'
                '    mkdir -p "$CARGO_TARGET_DIR/release"\n'
                '    : > "$CARGO_TARGET_DIR/release/mare"\n'
                "    ;;\n"
                "esac\n"
                "exit 0\n"
            )
            cargo.chmod(0o755)

            target_dir = root / "target"
            (target_dir / "release").mkdir(parents=True)
            (target_dir / "release" / "mare").write_text("#!/bin/sh\nexit 0\n")

            env = dict(
                os.environ,
                HOME=shell_path(root / "home"),
                PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
                CARGO_TARGET_DIR=shell_path(target_dir),
                MBX_DISABLE="1",
                MARE_ACCEPT_MIN_FREE_GB="0",
            )
            result = subprocess.run(
                ["sh", "scripts/accept.sh", "--release", "--steps", "release faux absent", "--allow-no-git"],
                cwd=root, check=False, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 15),
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("failed to remove faux target dir", result.stdout + result.stderr)

    @unittest.skipIf(WINDOWS, "POSIX shell mktemp wrapper is Unix-only")
    def test_release_faux_target_collision_does_not_delete_preexisting_dir(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(Path(__file__).with_name("accept.sh"), scripts / "accept.sh")
            (scripts / "check_seams.py").write_text("import sys; sys.exit(0)\n")

            commands = root / "bin"
            commands.mkdir()
            for name in (
                "cargo",
                "cargo-nextest",
                "cargo-deny",
                "stylua",
                "lua-language-server",
                "ast-grep",
                "lua",
            ):
                cmd = commands / name
                cmd.write_text("#!/bin/sh\nexit 0\n")
                cmd.chmod(0o755)
            py_cmd = commands / "python3"
            py_cmd.write_text(f'#!/bin/sh\nexec "{shell_path(sys.executable)}" "$@"\n')
            py_cmd.chmod(0o755)

            real_mktemp = shutil.which("mktemp")
            self.assertIsNotNone(real_mktemp, "mktemp must exist on PATH")
            mktemp_cmd = commands / "mktemp"
            mktemp_cmd.write_text(
                "#!/bin/sh\n"
                'for arg in "$@"; do\n'
                '  case "$arg" in\n'
                '    *mare-accept.*)\n'
                '      dir="${arg%/*}/mare-accept.COLLID"\n'
                '      mkdir -p "$dir" || exit 1\n'
                '      chmod 700 "$dir" || exit 1\n'
                '      printf "%s\\n" "$dir"\n'
                "      exit 0\n"
                "      ;;\n"
                "  esac\n"
                "done\n"
                f'exec "{shell_path(real_mktemp)}" "$@"\n'
            )
            mktemp_cmd.chmod(0o755)

            target_dir = root / "target"
            (target_dir / "release").mkdir(parents=True)
            (target_dir / "release" / "mare").write_text("#!/bin/sh\nexit 0\n")

            colliding_dir = target_dir / "faux-target.COLLID"
            colliding_dir.mkdir(parents=True)
            sentinel = colliding_dir / "keep-me"
            sentinel.write_text("do-not-delete\n")
            temp_root = root / "t"
            temp_root.mkdir()
            env = dict(
                os.environ,
                HOME=shell_path(root / "home"),
                PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
                CARGO_TARGET_DIR=shell_path(target_dir),
                MARE_ACCEPT_TMPDIR=shell_path(temp_root),
                MBX_DISABLE="1",
                MARE_ACCEPT_MIN_FREE_GB="0",
            )
            result = subprocess.run(
                ["sh", "scripts/accept.sh", "--release", "--steps", "release faux absent", "--allow-no-git"],
                cwd=root, check=False, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=(120 if WINDOWS else 15),
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("faux target path already exists:", result.stdout + result.stderr)
            self.assertTrue(colliding_dir.exists(), "pre-existing colliding directory was deleted")
            self.assertTrue(sentinel.exists(), "sentinel file inside pre-existing colliding directory was deleted")
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "do-not-delete\n")

class SnapshotGateTests(FixtureTestCase):
    def _setup_minimal_repo(self, root: Path, *, py_script: str = "#!/bin/sh\nexit 0\n"):
        scripts = root / "scripts"
        scripts.mkdir()
        shutil.copyfile(Path(__file__).with_name("accept.sh"), scripts / "accept.sh")
        commands = root / "bin"
        commands.mkdir()
        for name in ("cargo-nextest", "cargo-deny", "stylua", "lua-language-server", "ast-grep", "lua"):
            cmd = commands / name
            cmd.write_text("#!/bin/sh\nexit 0\n")
            cmd.chmod(0o755)
        cargo = commands / "cargo"
        cargo.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "nextest" ]; then\n'
            '  printf "INSTA=%s MARE=%s\\n" "${INSTA_UPDATE:-}" "${MARE_UPDATE_SNAPSHOTS:-}" >> "$COMMAND_LOG"\n'
            "fi\n"
            "exit 0\n"
        )
        cargo.chmod(0o755)
        py_cmd = commands / "python3"
        py_cmd.write_text(py_script)
        py_cmd.chmod(0o755)
        env = dict(
            os.environ,
            HOME=shell_path(root / "home"),
            PATH=os.pathsep.join((str(commands), os.environ["PATH"])),
            CARGO_TARGET_DIR=shell_path(root / "target"),
            MBX_DISABLE="1",
            MARE_ACCEPT_MIN_FREE_GB="0",
            COMMAND_LOG=shell_path(root / "commands"),
        )
        return env

    def test_nextest_exports_insta_update_no_even_when_outer_always(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            env = self._setup_minimal_repo(root)
            env["INSTA_UPDATE"] = "always"
            env["MARE_UPDATE_SNAPSHOTS"] = "always"
            result = subprocess.run(
                ["sh", "scripts/accept.sh", "--allow-no-git", "--steps", "nextest"],
                cwd=root,
                check=False,
                env=env,
                capture_output=True,
                text=True, encoding="utf-8", errors="replace",
                timeout=(120 if WINDOWS else 15),
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            invocations = (root / "commands").read_text(encoding="utf-8").splitlines()
            self.assertIn("INSTA=no MARE=no", invocations)

    def test_pending_snapshots_blocks_stray_snap_new(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            env = self._setup_minimal_repo(root)
            clean = subprocess.run(
                ["sh", "scripts/accept.sh", "--allow-no-git", "--steps", "pending snapshots"],
                cwd=root,
                check=False,
                env=env,
                capture_output=True,
                text=True, encoding="utf-8", errors="replace",
                timeout=(120 if WINDOWS else 15),
            )
            self.assertEqual(clean.returncode, 0, clean.stdout + clean.stderr)
            self.assertIn("| pending snapshots | pass |", clean.stdout)

            pending_file = root / "crates" / "mare-cli" / "tests" / "suite" / "snapshots" / "demo.snap.new"
            pending_file.parent.mkdir(parents=True)
            pending_file.write_text("pending\n")
            dirty = subprocess.run(
                ["sh", "scripts/accept.sh", "--allow-no-git", "--steps", "pending snapshots"],
                cwd=root,
                check=False,
                env=env,
                capture_output=True,
                text=True, encoding="utf-8", errors="replace",
                timeout=(120 if WINDOWS else 15),
            )
            self.assertNotEqual(dirty.returncode, 0, dirty.stdout + dirty.stderr)
            self.assertIn("pending snapshot files (*.snap.new)", dirty.stdout + dirty.stderr)
            self.assertIn("| pending snapshots | FAIL |", dirty.stdout)



if __name__ == "__main__":
    unittest.main()
