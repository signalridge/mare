#!/usr/bin/env python3
"""Exercise the nextest home wrapper without running Cargo or touching the user's home."""

import json
import os
import select
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import nextest_test_home

WRAPPER = Path(__file__).with_name("nextest_test_home.py")

class TestHomeWrapper(unittest.TestCase):
    def setUp(self):
        if os.name == "nt":
            base = os.environ.get("MARE_TMPDIR")
        else:
            exclusions = nextest_test_home._home_exclusions(os.environ)
            raw_base = os.environ.get("MARE_TMPDIR")
            base = None
            if raw_base and os.path.isabs(raw_base):
                try:
                    candidate = Path(raw_base).resolve(strict=True)
                    if candidate.is_dir() and not any(
                        nextest_test_home._within(candidate, root) for root in exclusions
                    ):
                        base = str(candidate)
                except OSError:
                    base = None
            if base is None:
                base = str(nextest_test_home._home_parent(os.environ, exclusions))
        self.temp = tempfile.TemporaryDirectory(prefix="mare-wrapper-tests-", dir=base)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.parent = self.root / "tmp"
        self.parent.mkdir()
        self.fake_home = self.root / "fake-user"
        self.fake_home.mkdir()
        self.env = os.environ.copy()
        self.env.update(
            {
                "HOME": str(self.fake_home),
                "USERPROFILE": str(self.fake_home),
                "MARE_HOME": str(self.fake_home / "mare"),
                "MARE_TMPDIR": str(self.parent),
                "TMPDIR": str(self.parent),
                "CARGO_HOME": str(self.root / "cargo"),
                "RUSTUP_HOME": str(self.root / "rustup"),
            }
        )

    def run_wrapper(self, *args, env=None, timeout=15):
        return subprocess.run(
            [sys.executable, str(WRAPPER), *args],
            env=self.env if env is None else env,
            capture_output=True,
            check=False,
            timeout=timeout,
        )

    def private_runtime(self):
        runtime = tempfile.TemporaryDirectory(prefix="mare-runtime-", dir="/var/tmp")
        self.addCleanup(runtime.cleanup)
        return Path(runtime.name).resolve()

    def test_arguments_environment_output_and_exit_are_forwarded(self):
        code = """
import json, os, pathlib, sys
p = pathlib.Path(os.environ['MARE_HOME'])
keys = ['HOME', 'USERPROFILE', 'MARE_HOME', 'XDG_STATE_HOME', 'XDG_CONFIG_HOME',
        'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'CARGO_HOME', 'RUSTUP_HOME', 'TMPDIR',
        'TEMP', 'TMP', 'APPDATA', 'LOCALAPPDATA']
print(json.dumps({'args':sys.argv[1:], 'env':{k:os.environ.get(k) for k in keys},
                  'mode':p.parent.stat().st_mode & 0o777}), flush=True)
sys.stderr.buffer.write(b'stderr marker\\n')
sys.exit(17)
"""
        result = self.run_wrapper(sys.executable, "-c", code, "spaces and quotes", "µ")
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertEqual(result.stderr, b"stderr marker\n")
        obj = json.loads(result.stdout)
        self.assertEqual(obj["args"], ["spaces and quotes", "µ"])
        actual = obj["env"]
        root = Path(actual["MARE_HOME"]).parent
        self.assertEqual(root.parent, self.parent)
        for name, directory in [
            ("MARE_HOME", "mare"),
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("XDG_STATE_HOME", "xdg-state"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
            ("TMPDIR", "tmp"),
            ("TEMP", "tmp"),
            ("TMP", "tmp"),
            ("APPDATA", "appdata"),
            ("LOCALAPPDATA", "local-appdata"),
        ]:
            self.assertEqual(Path(actual[name]), root / directory)
        home, temp = Path(actual["HOME"]), Path(actual["TMPDIR"])
        self.assertFalse(home.is_relative_to(temp))
        self.assertFalse(temp.is_relative_to(home))
        for name in ("CARGO_HOME", "RUSTUP_HOME"):
            self.assertEqual(actual[name], self.env[name])
        if os.name != "nt":
            self.assertEqual(obj["mode"], 0o700)
        self.assertFalse(root.exists(), "wrapper must remove its own root")

    def test_fake_user_state_is_not_read_or_overwritten(self):
        fake = self.fake_home / "mare"
        names = [
            "state/state.json",
            "state/trust.json",
            "config/auth.json",
            "state/host/token",
        ]
        before = {}
        for name in names:
            path = fake / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fake user sentinel")
            info = path.stat()
            before[name] = (path.read_bytes(), info.st_ino, info.st_mtime_ns)
        code = """
import os, pathlib
root = pathlib.Path(os.environ['MARE_HOME'])
for name in ['state/state.json', 'state/trust.json', 'config/auth.json', 'state/host/token']:
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('test state')
"""
        result = self.run_wrapper(sys.executable, "-c", code)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in names:
            path = fake / name
            info = path.stat()
            self.assertEqual(
                (path.read_bytes(), info.st_ino, info.st_mtime_ns), before[name], name
            )

    def test_rejects_missing_command_and_nonabsolute_or_sensitive_temp_root(self):
        self.assertNotEqual(self.run_wrapper().returncode, 0)
        self.assertNotEqual(
            self.run_wrapper(str(self.root / "missing-program")).returncode, 0
        )
        env = dict(self.env, MARE_TMPDIR="relative-temp")
        self.assertNotEqual(
            self.run_wrapper(sys.executable, "-c", "pass", env=env).returncode, 0
        )
        env = dict(self.env, MARE_TMPDIR=str(self.root / "does-not-exist"))
        self.assertNotEqual(
            self.run_wrapper(sys.executable, "-c", "pass", env=env).returncode, 0
        )
        env = dict(self.env, MARE_TMPDIR="")
        self.assertNotEqual(
            self.run_wrapper(sys.executable, "-c", "pass", env=env).returncode, 0
        )
        sensitive = self.fake_home / ".local" / "state" / "mare"
        sensitive.mkdir(parents=True)
        env = dict(self.env, MARE_TMPDIR=str(sensitive))
        self.assertNotEqual(
            self.run_wrapper(sys.executable, "-c", "pass", env=env).returncode, 0
        )

    @unittest.skipIf(os.name == "nt", "POSIX system temporary-directory fallback")
    def test_unset_temp_variables_fall_back_to_system_temp(self):
        runtime_path = self.private_runtime()
        env = self.env.copy()
        for name in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP"):
            env.pop(name, None)
        env["XDG_RUNTIME_DIR"] = str(runtime_path)
        code = "import json,os; print(json.dumps([os.environ['HOME'], os.environ['TMPDIR']]))"
        result = self.run_wrapper(sys.executable, "-c", code, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        home, temp = map(Path, json.loads(result.stdout))
        root = home.parent
        self.assertEqual(root.parent, runtime_path)
        self.assertEqual(temp, root / "tmp")
        self.assertFalse(home.is_relative_to(Path("/tmp").resolve()))
        self.assertFalse(home.is_relative_to(temp))
        self.assertFalse(temp.is_relative_to(home))
        self.assertFalse(root.exists())

    @unittest.skipIf(os.name == "nt", "POSIX split temporary roots")
    def test_unsafe_temp_parents_use_a_separate_home_and_clean_both_roots(self):
        runtime_path = self.private_runtime()
        workspace = WRAPPER.resolve().parent.parent
        for base in (Path("/tmp"), self.fake_home, workspace):
            with (
                self.subTest(base=base),
                tempfile.TemporaryDirectory(dir=base) as parent,
            ):
                temp_parent = Path(parent).resolve()
                env = dict(
                    self.env,
                    MARE_TMPDIR=str(temp_parent),
                    TMPDIR=str(temp_parent),
                    XDG_RUNTIME_DIR=str(runtime_path),
                )
                code = "import json,os; print(json.dumps([os.environ['HOME'], os.environ['TMPDIR']]))"
                result = self.run_wrapper(sys.executable, "-c", code, env=env)
                self.assertEqual(result.returncode, 0, result.stderr)
                home, temp = map(Path, json.loads(result.stdout))
                self.assertEqual(home.parent.parent, runtime_path)
                self.assertEqual(temp.parent, temp_parent)
                self.assertFalse(home.is_relative_to(Path("/tmp").resolve()))
                self.assertFalse(home.is_relative_to(temp))
                self.assertFalse(temp.is_relative_to(home))
                self.assertFalse(home.parent.exists())
                self.assertFalse(temp.exists())

    @unittest.skipIf(os.name == "nt", "POSIX fallback validation")
    def test_invalid_runtime_falls_back_to_var_tmp(self):
        env = self.env.copy()
        for name in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP"):
            env.pop(name, None)
        for runtime in (
            self.fake_home,
            WRAPPER.resolve().parent.parent,
            Path("/tmp"),
            Path("relative"),
        ):
            with self.subTest(runtime=runtime):
                env["XDG_RUNTIME_DIR"] = str(runtime)
                result = self.run_wrapper(
                    sys.executable,
                    "-c",
                    "import os; print(os.environ['HOME'])",
                    env=env,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                home = Path(result.stdout.decode().strip())
                self.assertEqual(home.parent.parent, Path("/var/tmp").resolve())
                self.assertFalse(home.parent.exists())

    @unittest.skipIf(os.name == "nt", "POSIX fallback validation")
    def test_missing_safe_home_parent_fails_closed(self):
        with (
            mock.patch.dict(
                os.environ, {"XDG_RUNTIME_DIR": str(self.parent)}, clear=True
            ),
            mock.patch.object(nextest_test_home.os, "access", return_value=False),
            self.assertRaisesRegex(ValueError, "MARE_TMPDIR"),
        ):
            nextest_test_home._run([sys.executable, "-c", "pass"])
        env = self.env.copy()
        for name in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP", "XDG_RUNTIME_DIR"):
            env.pop(name, None)
        env["MARE_HOME"] = "/var/tmp"
        result = self.run_wrapper(sys.executable, "-c", "pass", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"MARE_TMPDIR", result.stderr)

    @unittest.skipIf(os.name == "nt", "POSIX fallback validation")
    def test_runtime_owner_and_darwin_temp_are_checked(self):
        runtime_path = self.private_runtime()
        env = dict(self.env, XDG_RUNTIME_DIR=str(runtime_path))
        exclusions = nextest_test_home._home_exclusions(env)
        with mock.patch.object(
            nextest_test_home.os, "getuid", return_value=os.getuid() + 1
        ):
            self.assertEqual(
                nextest_test_home._home_parent(env, exclusions),
                Path("/var/tmp").resolve(),
            )
        darwin_temp = self.parent / "darwin-user-temp"
        darwin_temp.mkdir()
        record = self.root / "darwin-paths"
        env.update(MARE_TMPDIR=str(darwin_temp), XDG_RUNTIME_DIR=str(runtime_path))
        code = "import json,os,sys; open(sys.argv[1],'w').write(json.dumps([os.environ['HOME'],os.environ['TMPDIR']]))"
        with (
            mock.patch.dict(os.environ, env, clear=True),
            mock.patch.object(
                nextest_test_home,
                "_darwin_user_temp",
                return_value=darwin_temp.resolve(),
            ),
        ):
            self.assertEqual(
                nextest_test_home._run([sys.executable, "-c", code, str(record)]), 0
            )
        home, temp = map(Path, json.loads(record.read_text()))
        self.assertEqual(home.parent.parent, runtime_path)
        self.assertEqual(temp.parent, darwin_temp.resolve())
        self.assertFalse(home.parent.exists())
        self.assertFalse(temp.exists())

    @unittest.skipIf(os.name == "nt", "POSIX Darwin directory validation")
    def test_darwin_confstr_path_and_failure(self):
        import ctypes

        value = os.fsencode(str(self.parent.resolve())) + b"\0"

        def confstr(key, buffer, size):
            self.assertEqual(key, 65537)
            if buffer is not None:
                self.assertEqual(size, len(value))
                ctypes.memmove(buffer, value, len(value))
            return len(value)

        library = mock.Mock(confstr=confstr)
        with (
            mock.patch.object(nextest_test_home.sys, "platform", "darwin"),
            mock.patch.object(ctypes, "CDLL", return_value=library),
        ):
            self.assertEqual(
                nextest_test_home._darwin_user_temp(), self.parent.resolve()
            )
            library.confstr = mock.Mock(return_value=0)
            with self.assertRaisesRegex(ValueError, "macOS user temporary"):
                nextest_test_home._darwin_user_temp()

    @unittest.skipIf(os.name == "nt", "POSIX split temporary roots")
    def test_replacing_either_split_root_still_cleans_the_other(self):
        runtime = self.private_runtime()
        with tempfile.TemporaryDirectory(dir="/tmp") as parent:
            env = dict(self.env, MARE_TMPDIR=parent, XDG_RUNTIME_DIR=str(runtime))
            victim = self.root / "victim-split"
            victim.mkdir()
            (victim / "sentinel").write_text("keep")
            code = """
import json,os,pathlib,sys
home = pathlib.Path(os.environ['HOME']).parent
tmp = pathlib.Path(os.environ['TMPDIR'])
target = home if sys.argv[1] == 'home' else tmp
target.rename(target.with_name(target.name + '-moved'))
target.symlink_to(sys.argv[2], target_is_directory=True)
print(json.dumps([str(home), str(tmp)]))
"""
            for replaced in ("home", "tmp"):
                with self.subTest(replaced=replaced):
                    result = self.run_wrapper(
                        sys.executable, "-c", code, replaced, str(victim), env=env
                    )
                    self.assertNotEqual(result.returncode, 0)
                    home, temp = map(Path, json.loads(result.stdout))
                    self.assertFalse((temp if replaced == "home" else home).exists())
                    self.assertEqual((victim / "sentinel").read_text(), "keep")

    @unittest.skipIf(os.name == "nt", "POSIX signal forwarding")
    def test_split_roots_are_cleaned_on_termination(self):
        runtime = self.private_runtime()
        with tempfile.TemporaryDirectory(dir="/tmp") as parent:
            env = dict(self.env, MARE_TMPDIR=parent, XDG_RUNTIME_DIR=str(runtime))
            code = """
import json,os,signal,sys
signal.signal(signal.SIGTERM, lambda sig, frame: sys.exit(128 + sig))
print(json.dumps([os.environ['HOME'],os.environ['TMPDIR']]), flush=True)
signal.pause()
"""
            p = subprocess.Popen(
                [sys.executable, str(WRAPPER), sys.executable, "-c", code],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                self.assertTrue(
                    select.select([p.stdout], [], [], 10)[0], "child did not start"
                )
                home, temp = map(Path, json.loads(p.stdout.readline()))
                p.send_signal(signal.SIGTERM)
                self.assertEqual(p.wait(timeout=10), -signal.SIGTERM)
                self.assertFalse(home.parent.exists())
                self.assertFalse(temp.exists())
            finally:
                if p.poll() is None:
                    p.kill()
                    p.wait()
                p.stdout.close()
                p.stderr.close()

    def test_appdata_directories_are_isolated(self):
        env = dict(
            self.env, APPDATA=str(self.fake_home), LOCALAPPDATA=str(self.fake_home)
        )
        code = """
import json, os
print(json.dumps([os.environ['MARE_HOME'], os.environ['APPDATA'], os.environ['LOCALAPPDATA']]))
"""
        result = self.run_wrapper(sys.executable, "-c", code, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        mare, appdata, local = map(Path, json.loads(result.stdout))
        self.assertEqual(appdata, mare.parent / "appdata")
        self.assertEqual(local, mare.parent / "local-appdata")

    def test_unset_toolchain_homes_keep_their_original_defaults(self):
        env = self.env.copy()
        env.pop("CARGO_HOME")
        env.pop("RUSTUP_HOME")
        code = "import json,os; print(json.dumps([os.environ['CARGO_HOME'],os.environ['RUSTUP_HOME']]))"
        result = self.run_wrapper(sys.executable, "-c", code, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            [str(self.fake_home / ".cargo"), str(self.fake_home / ".rustup")],
        )

    def test_temporary_parent_cannot_be_inside_system_home_state(self):
        real = self.root / "simulated-system-home"
        sensitive = real / ".local" / "state" / "mare"
        sensitive.mkdir(parents=True)
        env = dict(self.env, MARE_TMPDIR=str(sensitive))
        with (
            mock.patch.object(
                nextest_test_home, "_system_home", return_value=real, create=True
            ),
            self.assertRaises(ValueError),
        ):
            nextest_test_home._temp_parent(env)

    @unittest.skipIf(
        os.name == "nt", "symlink replacement fixture requires POSIX symlink creation"
    )
    def test_replaced_root_does_not_delete_an_external_directory(self):
        victim = self.root / "victim"
        victim.mkdir()
        sentinel = victim / "sentinel"
        sentinel.write_text("keep", encoding="utf-8")
        code = """
import os, pathlib, sys
root = pathlib.Path(os.environ['MARE_HOME']).parent
root.rename(root.with_name(root.name + '-moved'))
root.symlink_to(sys.argv[1], target_is_directory=True)
"""
        result = self.run_wrapper(sys.executable, "-c", code, str(victim))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_replaced_directory_with_new_inode_is_not_deleted(self):
        code = """
import os, pathlib
root = pathlib.Path(os.environ['MARE_HOME']).parent
root.rename(root.with_name(root.name + '-moved'))
root.mkdir()
(root / 'sentinel').write_text('keep')
print(root)
"""
        result = self.run_wrapper(sys.executable, "-c", code)
        self.assertNotEqual(result.returncode, 0)
        replacement = Path(result.stdout.decode().strip())
        self.assertEqual((replacement / "sentinel").read_text(encoding="utf-8"), "keep")

    def test_unverified_target_runner_fails_closed(self):
        marker = self.root / "runner-was-executed"
        env = dict(
            self.env, NEXTEST_VERSION="0.9.152", NEXTEST_BINARY_ID="fixture::target"
        )
        code = "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('ran')"
        result = self.run_wrapper(sys.executable, "-c", code, str(marker), env=env)
        self.assertNotEqual(result.returncode, 0, "an unverified runner must not start")
        self.assertFalse(marker.exists())

    def test_cross_platform_binary_fails_closed(self):
        binary = self.root / (
            "target-0123456789abcdef.exe"
            if os.name == "nt"
            else "target-0123456789abcdef"
        )
        binary.write_bytes(b"\x7fELF" if os.name == "nt" else b"MZ\x00\x00")
        binary.chmod(0o700)
        env = dict(
            self.env, NEXTEST_VERSION="0.9.152", NEXTEST_BINARY_ID="fixture::target"
        )
        result = self.run_wrapper(str(binary), env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"cross-platform test binary", result.stderr)

    def test_windows_acl_validation_rejects_unknown_or_broad_access(self):
        validate = nextest_test_home._validate_windows_dacl
        canonical = lambda sid: sid
        owner, user = "S-1-5-21-user", "S-1-5-21-user"
        validate(
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-21-user)"
            "(A;OICIIO;GA;;;CO)",
            user,
            owner,
            canonical,
        )
        for dacl in [
            "D:NO_ACCESS_CONTROL",
            "D:(A;OICI;FR;;;WD)",
            "D:(A;IO;FR;;;WD)",
            "D:(A;OICI;GA;;;CO)",
            "D:(A;CIOI;GA;;;CO)",
            "D:(OA;;FR;;;WD)",
            "D:(XA;;FR;;;WD;(TRUE))",
        ]:
            with self.subTest(dacl=dacl), self.assertRaises(ValueError):
                validate(dacl, user, owner, canonical)
        with self.assertRaises(ValueError):
            validate("D:(A;;FA;;;OW)", user, "S-1-5-21-other", canonical)

    def test_concurrent_and_subsequent_invocations_get_distinct_roots(self):
        code = "import os,sys; print(os.environ['MARE_HOME'], flush=True); sys.stdin.readline()"

        def start():
            return subprocess.Popen(
                [sys.executable, str(WRAPPER), sys.executable, "-c", code],
                env=self.env,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

        one, two = start(), start()
        try:
            roots = [
                Path(p.stdout.readline().decode().strip()).parent for p in (one, two)
            ]
            self.assertNotEqual(roots[0], roots[1])
            self.assertTrue(all(root.is_dir() for root in roots))
            for p in (one, two):
                p.stdin.write(b"\n")
                p.stdin.flush()
            for p in (one, two):
                self.assertEqual(p.wait(timeout=10), 0, p.stderr.read())
            self.assertTrue(all(not root.exists() for root in roots))
        finally:
            for p in (one, two):
                if p.poll() is None:
                    p.kill()
                    p.wait()
                for stream in (p.stdin, p.stdout, p.stderr):
                    stream.close()
        third = self.run_wrapper(
            sys.executable, "-c", "import os; print(os.environ['MARE_HOME'])"
        )
        self.assertEqual(third.returncode, 0, third.stderr)
        self.assertNotIn(Path(third.stdout.decode().strip()).parent, roots)

    def test_doctest_driver_isolates_merged_examples_and_preserves_failures(self):
        workspace = self.root / "doc-workspace"
        workspace.mkdir()
        manifest = workspace / "Cargo.toml"
        manifest.write_text(
            '[workspace]\nmembers = ["one", "two"]\nresolver = "2"\n', encoding="utf-8"
        )
        record = self.root / "doc-roots"
        snippet = (
            "//! use std::io::Write;\n"
            "//! writeln!(std::fs::OpenOptions::new().create(true).append(true).open("
            'std::env::var("DOC_PROBE_RECORD").unwrap()).unwrap(), "{}", '
            'std::env::var("MARE_HOME").unwrap()).unwrap();\n'
        )
        for member in ("one", "two"):
            crate = workspace / member
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                f'[package]\nname = "home-doc-{member}"\nversion = "0.1.0"\nedition = "2024"\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "//! ```\n"
                + snippet
                + "//! ```\n//! ```ignore\n//! panic!();\n//! ```\n"
                "//! ```\n" + snippet + "//! ```\n"
                '//! ```should_panic\n//! panic!("expected");\n//! ```\n'
                '//! ```compile_fail\n//! let _: u32 = "not a number";\n//! ```\n',
                encoding="utf-8",
            )
        env = dict(
            self.env,
            DOC_PROBE_RECORD=str(record),
            CARGO_TERM_COLOR="always",
        )
        for key, default in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")]:
            env[key] = os.environ.get(key) or str(
                Path(os.environ.get("HOME") or os.environ["USERPROFILE"]) / default
            )
        result = self.run_wrapper(
            "--doc",
            "--manifest-path",
            str(manifest),
            env=env,
            timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        roots = record.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(roots), 4, roots)
        self.assertEqual(len(set(roots)), 4, roots)
        self.assertTrue(all(not Path(root).exists() for root in roots))
        (workspace / "one" / "src" / "lib.rs").write_text(
            '//! ```\n//! panic!("doctest failure must propagate");\n//! ```\n',
            encoding="utf-8",
        )
        failed = self.run_wrapper(
            "--doc",
            "--manifest-path",
            str(manifest),
            env=env,
            timeout=120,
        )
        self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
        self.assertIn(b"doctest failure must propagate", failed.stdout + failed.stderr)

    def assert_termination_forwarded(self, command, env, marker, ready_stream):
        p = subprocess.Popen(
            command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        try:
            stream = getattr(p, ready_stream)
            self.assertTrue(
                select.select([stream], [], [], 10)[0], "child did not start"
            )
            root = Path(stream.readline().decode().strip()).parent
            p.send_signal(signal.SIGTERM)
            self.assertEqual(p.wait(timeout=10), -signal.SIGTERM)
            self.assertEqual(marker.read_text(), str(signal.SIGTERM))
            self.assertFalse(root.exists())
        finally:
            if p.poll() is None:
                p.kill()
                p.wait()
            p.stdout.close()
            p.stderr.close()

    @unittest.skipIf(os.name == "nt", "POSIX signal forwarding")
    def test_doctest_driver_forwards_termination_and_cleans_up(self):
        fake_cargo = self.root / "fake-cargo"
        marker = self.root / "doc-signal"
        fake_cargo.write_text(
            f"#!{sys.executable}\n"
            "import os,pathlib,signal,sys\n"
            "def stop(sig, frame):\n"
            "    pathlib.Path(os.environ['DOC_SIGNAL_MARKER']).write_text(str(sig))\n"
            "    sys.exit(128 + sig)\n"
            "signal.signal(signal.SIGTERM, stop)\n"
            "print(os.environ['MARE_HOME'], file=sys.stderr, flush=True)\n"
            "signal.pause()\n",
            encoding="utf-8",
        )
        fake_cargo.chmod(0o700)
        env = dict(
            self.env, MARE_DOC_CARGO=str(fake_cargo), DOC_SIGNAL_MARKER=str(marker)
        )
        self.assert_termination_forwarded(
            [sys.executable, str(WRAPPER), "--doc"],
            env,
            marker,
            "stderr",
        )

    @unittest.skipIf(os.name == "nt", "POSIX signal forwarding")
    def test_termination_reaches_child_and_cleans_up(self):
        marker = self.root / "child-received-signal"
        code = """
import os, pathlib, signal, sys
def stop(signum, frame):
    pathlib.Path(sys.argv[1]).write_text(str(signum))
    sys.exit(128 + signum)
signal.signal(signal.SIGTERM, stop)
print(os.environ['MARE_HOME'], flush=True)
signal.pause()
"""
        self.assert_termination_forwarded(
            [sys.executable, str(WRAPPER), sys.executable, "-c", code, str(marker)],
            self.env,
            marker,
            "stdout",
        )

    @unittest.skipIf(os.name == "nt", "POSIX signal forwarding")
    def test_repeated_termination_signals_reach_the_child(self):
        code = """
import os, signal, sys
count = 0
def stop(signum, frame):
    global count
    count += 1
    print('signal', count, flush=True)
    if count == 2: sys.exit(128 + signum)
signal.signal(signal.SIGTERM, stop)
print(os.getpid(), os.environ['MARE_HOME'], flush=True)
while True: signal.pause()
"""
        p = subprocess.Popen(
            [sys.executable, str(WRAPPER), sys.executable, "-c", code],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        child_pid = None
        try:
            self.assertTrue(
                select.select([p.stdout], [], [], 10)[0], "child did not start"
            )
            pid, home = p.stdout.readline().decode().split()
            child_pid = int(pid)
            p.send_signal(signal.SIGTERM)
            self.assertTrue(
                select.select([p.stdout], [], [], 5)[0],
                "first signal did not reach child",
            )
            self.assertEqual(p.stdout.readline().strip(), b"signal 1")
            p.send_signal(signal.SIGTERM)
            self.assertEqual(p.wait(timeout=5), -signal.SIGTERM)
            self.assertEqual(p.stdout.readline().strip(), b"signal 2")
            self.assertFalse(Path(home).parent.exists())
        finally:
            if p.poll() is None:
                if child_pid is not None:
                    try:
                        os.kill(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                p.kill()
                p.wait()
            p.stdout.close()
            p.stderr.close()


if __name__ == "__main__":
    unittest.main()
