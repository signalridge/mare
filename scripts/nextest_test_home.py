#!/usr/bin/env python3
"""Give each nextest list/run process a private, disposable set of home paths."""

import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

_HANDLED_SIGNALS = (
    signal.SIGINT,
    signal.SIGTERM,
    signal.SIGBREAK if os.name == "nt" else signal.SIGHUP,
)


def _install_signal_handlers(handler):
    return {sig: signal.signal(sig, handler) for sig in _HANDLED_SIGNALS}


def _restore_signal_handlers(handlers):
    for sig, handler in handlers.items():
        signal.signal(sig, handler)


def _within(path, ancestor):
    try:
        path.relative_to(ancestor)
        return True
    except ValueError:
        return False


def _system_home():
    if os.name != "nt":
        import pwd

        return Path(pwd.getpwuid(os.getuid()).pw_dir)
    import ctypes
    import uuid
    from ctypes import wintypes

    ole = ctypes.WinDLL("ole32", use_last_error=True)
    shell = ctypes.WinDLL("shell32", use_last_error=True)
    ole.CoInitializeEx.argtypes = [ctypes.c_void_p, wintypes.DWORD]
    ole.CoInitializeEx.restype = ctypes.c_long
    ole.CoTaskMemFree.argtypes = [ctypes.c_void_p]
    ole.CoUninitialize.argtypes = []
    shell.SHGetKnownFolderPath.argtypes = [
        ctypes.c_void_p,
        wintypes.DWORD,
        wintypes.HANDLE,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    shell.SHGetKnownFolderPath.restype = ctypes.c_long
    initialized = ole.CoInitializeEx(None, 2)
    if (
        initialized < 0 and initialized != -2147417850
    ):  # RPC_E_CHANGED_MODE means COM is already initialized.
        raise OSError(
            f"cannot initialize COM to inspect the user profile: {initialized:#x}"
        )
    value = ctypes.c_void_p()
    try:
        folder = (ctypes.c_ubyte * 16).from_buffer_copy(
            uuid.UUID("5e6c858f-0e22-4760-9afe-ea3317b67173").bytes_le
        )
        status = shell.SHGetKnownFolderPath(
            ctypes.byref(folder), 0, None, ctypes.byref(value)
        )
        if status < 0 or not value.value:
            raise OSError(f"cannot determine the Windows user profile: {status:#x}")
        return Path(ctypes.wstring_at(value))
    finally:
        if value.value:
            ole.CoTaskMemFree(value)
        if initialized >= 0:
            ole.CoUninitialize()


def _configured_mare_dirs(env):
    if env.get("MARE_HOME"):
        yield Path(env["MARE_HOME"])
    for key in ("XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
        if env.get(key):
            yield Path(env[key]) / "mare"


def _temp_parent(env):
    raw = next(
        (env[key] for key in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP") if key in env),
        None,
    )
    if raw is None and os.name != "nt":
        raw = tempfile.gettempdir()
    if not raw or not os.path.isabs(raw):
        raise ValueError(
            "MARE_TMPDIR, TMPDIR, TEMP or TMP must name an absolute temporary directory"
        )
    parent = Path(raw).resolve(strict=True)
    if not parent.is_dir():
        raise ValueError("temporary root is not a directory")
    homes = _user_homes(env)
    protected = list(_configured_mare_dirs(env))
    for home in homes:
        protected += [
            home / ".config" / "mare",
            home / ".local" / "state" / "mare",
            home / ".local" / "share" / "mare",
            home / ".cache" / "mare",
        ]
    for item in protected:
        if _within(parent, item.resolve()):
            raise ValueError(
                f"temporary root is inside a mare user directory: {parent}"
            )
    if os.name == "nt":
        _check_windows_private(parent)
    return parent


def _darwin_user_temp():
    if sys.platform != "darwin":
        return None
    import ctypes

    confstr = ctypes.CDLL(None, use_errno=True).confstr
    confstr.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_size_t]
    confstr.restype = ctypes.c_size_t
    size = confstr(65537, None, 0)  # _CS_DARWIN_USER_TEMP_DIR
    if not size:
        raise ValueError("cannot determine the macOS user temporary directory")
    buffer = ctypes.create_string_buffer(size)
    actual = confstr(65537, buffer, size)
    if not actual or actual > size:
        raise ValueError("cannot determine the macOS user temporary directory")
    path = Path(os.fsdecode(buffer.value))
    if not path.is_absolute():
        raise ValueError("macOS user temporary directory is not absolute")
    return path.resolve()


def _user_homes(env):
    homes = [Path(env[key]) for key in ("HOME", "USERPROFILE") if env.get(key)]
    homes.append(_system_home())
    return homes


def _home_exclusions(env):
    roots = [
        Path("/tmp").resolve(),
        Path(__file__).resolve().parent.parent,
        *(home.resolve() for home in _user_homes(env)),
    ]
    roots.extend(path.resolve() for path in _configured_mare_dirs(env))
    darwin = _darwin_user_temp()
    if darwin is not None:
        roots.append(darwin)
    return roots


def _home_parent(env, exclusions):
    for raw, require_owner in ((env.get("XDG_RUNTIME_DIR"), True), ("/var/tmp", False)):
        if not raw or not os.path.isabs(raw):
            continue
        try:
            parent = Path(raw).resolve(strict=True)
            if (
                parent.is_dir()
                and os.access(parent, os.W_OK | os.X_OK)
                and (not require_owner or parent.stat().st_uid == os.getuid())
                and not any(_within(parent, root) for root in exclusions)
            ):
                return parent
        except OSError:
            continue
    raise ValueError(
        "no safe home parent; set MARE_TMPDIR outside /tmp, the user home, and the workspace"
    )


def _validate_windows_dacl(sddl, user, owner, canonical):
    trusted = {user, "S-1-5-18", "S-1-5-32-544"}
    if owner not in trusted or not sddl.startswith("D:") or "NO_ACCESS_CONTROL" in sddl:
        raise ValueError("temporary directory has an untrusted owner or null DACL")
    entries = sddl[sddl.find("(") :] if "(" in sddl else ""
    while entries:
        if not entries.startswith("(") or ")" not in entries:
            raise ValueError("unreadable temporary-directory DACL")
        entry, entries = entries[1:].split(")", 1)
        fields = entry.split(";")
        if len(fields) != 6 or "(" in entry:
            raise ValueError("unsupported temporary-directory ACE")
        kind, sid = fields[0], fields[5]
        if kind == "D":
            continue
        if kind != "A":
            raise ValueError("unsupported temporary-directory ACE")
        if (
            sid in ("SY", "BA")
            or (sid == "OW" and owner in trusted)
            or (sid == "CO" and "IO" in {fields[1][i : i + 2] for i in range(0, len(fields[1]), 2)})
        ):
            continue
        if canonical(sid) not in trusted:
            raise ValueError("temporary directory grants another principal access")


def _check_windows_private(path):
    """Reject a DACL granting a directory to anyone other than its user/SYSTEM/admins."""
    import ctypes
    from ctypes import wintypes

    advapi = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    ptr = ctypes.c_void_p
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.LocalFree.argtypes = [ptr]
    kernel.LocalFree.restype = ptr
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    advapi.OpenProcessToken.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.HANDLE),
    ]
    advapi.GetTokenInformation.argtypes = [
        wintypes.HANDLE,
        ctypes.c_int,
        ptr,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
    ]
    advapi.ConvertSidToStringSidW.argtypes = [ptr, ctypes.POINTER(ptr)]
    advapi.ConvertStringSidToSidW.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(ptr)]
    advapi.GetNamedSecurityInfoW.argtypes = [
        wintypes.LPWSTR,
        ctypes.c_int,
        wintypes.DWORD,
        ptr,
        ptr,
        ptr,
        ptr,
        ctypes.POINTER(ptr),
    ]
    advapi.ConvertSecurityDescriptorToStringSecurityDescriptorW.argtypes = [
        ptr,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(ptr),
        ptr,
    ]

    def sid_string(sid):
        value = ptr()
        if not advapi.ConvertSidToStringSidW(sid, ctypes.byref(value)):
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            return ctypes.wstring_at(value)
        finally:
            kernel.LocalFree(value)

    token = wintypes.HANDLE()
    if not advapi.OpenProcessToken(kernel.GetCurrentProcess(), 8, ctypes.byref(token)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        size = wintypes.DWORD()
        advapi.GetTokenInformation(token, 1, None, 0, ctypes.byref(size))
        buffer = ctypes.create_string_buffer(size.value)
        if not advapi.GetTokenInformation(token, 1, buffer, size, ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        user = sid_string(ctypes.cast(buffer, ctypes.POINTER(ptr)).contents)
    finally:
        kernel.CloseHandle(token)

    descriptor = ptr()
    owner_sid = ptr()
    error = advapi.GetNamedSecurityInfoW(
        str(path),
        1,
        5,
        ctypes.byref(owner_sid),
        None,
        None,
        None,
        ctypes.byref(descriptor),
    )
    if error:
        raise ctypes.WinError(error)
    try:
        owner = sid_string(owner_sid)
        text = ptr()
        if not advapi.ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor, 1, 4, ctypes.byref(text), None
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            sddl = ctypes.wstring_at(text)
        finally:
            kernel.LocalFree(text)
    finally:
        kernel.LocalFree(descriptor)

    def canonical(sid):
        parsed = ptr()
        if not advapi.ConvertStringSidToSidW(sid, ctypes.byref(parsed)):
            raise ValueError(f"unreadable temporary-directory DACL SID: {path}")
        try:
            return sid_string(parsed)
        finally:
            kernel.LocalFree(parsed)

    _validate_windows_dacl(sddl, user, owner, canonical)


def _is_reparse_or_symlink(info):
    return stat.S_ISLNK(info.st_mode) or bool(
        getattr(info, "st_file_attributes", 0) & 0x400
    )


def _verify_nextest_command(command):
    if not os.environ.get("NEXTEST_VERSION"):
        return
    binary_id = (
        os.environ.get("NEXTEST_BINARY_ID", "")
        .rsplit("::", 1)[-1]
        .rsplit("/", 1)[-1]
        .replace("-", "_")
    )
    name = Path(command[0]).name
    if not binary_id or not re.fullmatch(
        re.escape(binary_id) + r"-[0-9a-f]{16}(?:\.exe)?", name, re.IGNORECASE
    ):
        raise ValueError(
            "unverified nextest target runner: run cross-target tests natively"
        )
    with open(command[0], "rb") as binary:
        magic = binary.read(4)
    native = (
        magic.startswith(b"MZ")
        if os.name == "nt"
        else magic == b"\x7fELF"
        if sys.platform.startswith("linux")
        else magic
        in (
            b"\xfe\xed\xfa\xce",
            b"\xce\xfa\xed\xfe",
            b"\xfe\xed\xfa\xcf",
            b"\xcf\xfa\xed\xfe",
            b"\xca\xfe\xba\xbe",
            b"\xbe\xba\xfe\xca",
            b"\xca\xfe\xba\xbf",
            b"\xbf\xba\xfe\xca",
        )
    )
    if not native:
        raise ValueError(
            "cross-platform test binary cannot be verified against this private home"
        )


def _rmtree_readonly(func, path, exc):
    if not isinstance(exc, PermissionError):
        raise exc
    try:
        info = os.lstat(path)
    except OSError:
        raise exc
    if _is_reparse_or_symlink(info):
        raise exc
    mode = stat.S_IWRITE | stat.S_IREAD
    if stat.S_ISDIR(info.st_mode):
        mode |= stat.S_IEXEC
    os.chmod(path, mode)
    func(path)


def _cleanup(parent, root, identity):
    try:
        info = root.lstat()
    except FileNotFoundError:
        return
    if (
        root.parent != parent
        or _is_reparse_or_symlink(info)
        or (info.st_dev, info.st_ino) != identity
    ):
        raise RuntimeError(f"refusing to remove a replaced temporary root: {root}")
    if sys.version_info >= (3, 12):
        shutil.rmtree(root, onexc=_rmtree_readonly)
    else:
        shutil.rmtree(root, onerror=lambda f, p, ei: _rmtree_readonly(f, p, ei[1]))


class _SignalRelay:
    def __init__(self):
        self.received = []
        self.child = None
        self.old = {}

    def install(self):
        self.old = _install_signal_handlers(self.forward)

    def send(self, signum):
        if self.child is not None:
            try:
                self.child.send_signal(
                    signal.CTRL_BREAK_EVENT
                    if os.name == "nt" and signum == signal.SIGBREAK
                    else signum
                )
            except ProcessLookupError:
                pass

    def forward(self, signum, _frame):
        if not self.received:
            self.received.append(signum)
        self.send(signum)

    def spawn(self, command, **options):
        if self.received:
            return None
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        self.child = subprocess.Popen(command, creationflags=flags, **options)
        if self.received:
            self.send(self.received[0])
        return self.child

    def status(self, code):
        if self.received:
            return -self.received[0] if os.name != "nt" else 128 + self.received[0]
        return code

    def restore(self):
        _restore_signal_handlers(self.old)


def _run(command, *, doctest_harness=False, doctest_child=False):
    if not command:
        raise ValueError("missing test command")
    if not (doctest_harness or doctest_child):
        _verify_nextest_command(command)
    explicit_temp = any(
        key in os.environ for key in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP")
    )
    temp_parent = _temp_parent(os.environ) if explicit_temp or os.name == "nt" else None
    exclusions = _home_exclusions(os.environ) if os.name != "nt" else []
    if temp_parent is not None and not any(
        _within(temp_parent, root) for root in exclusions
    ):
        parent = temp_parent
    else:
        parent = _home_parent(os.environ, exclusions)
    relay = _SignalRelay()
    roots = []
    code = 0

    try:
        relay.install()
        root = Path(tempfile.mkdtemp(prefix="mn-", dir=parent))
        info = root.lstat()
        roots.append((parent, root, (info.st_dev, info.st_ino)))
        if os.name == "nt":
            _check_windows_private(root)
        if temp_parent is not None and temp_parent != parent:
            temp = Path(tempfile.mkdtemp(prefix="mt-", dir=temp_parent))
            info = temp.lstat()
            roots.append((temp_parent, temp, (info.st_dev, info.st_ino)))
        else:
            temp = root / "tmp"
            temp.mkdir(mode=0o700)
        env = os.environ.copy()
        original_home = Path(
            env.get("HOME") or env.get("USERPROFILE") or str(Path.home())
        )
        env.setdefault("CARGO_HOME", str(original_home / ".cargo"))
        env.setdefault("RUSTUP_HOME", str(original_home / ".rustup"))
        for variable, name in [
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("MARE_HOME", "mare"),
            ("XDG_STATE_HOME", "xdg-state"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
            ("APPDATA", "appdata"),
            ("LOCALAPPDATA", "local-appdata"),
        ]:
            directory = root / name
            directory.mkdir(exist_ok=True, mode=0o700)
            env[variable] = str(directory)
        for variable in ("TMPDIR", "TEMP", "TMP"):
            env[variable] = str(temp)
        env["MARE_NEXTEST_TEST_HOME"] = "1"
        env["MARE_NEXTEST_HOME_DEPTH"] = str(
            int(env.get("MARE_NEXTEST_HOME_DEPTH", "0")) + 1
        )
        if doctest_harness:
            # The 2024 merged harness spawns this path for each example, setting
            # RUSTDOC_DOCTEST_RUN_NB_TEST. The per-run probe checks this rustdoc contract.
            env["MARE_DOCTEST_BINARY"] = os.path.abspath(command[0])
            env["MARE_DOCTEST_PYTHON"] = sys.executable
            launcher = (
                Path(__file__).with_suffix(".cmd")
                if os.name == "nt"
                else Path(__file__)
            )
            env["RUSTDOC_DOCTEST_BIN_PATH"] = str(launcher.resolve())
        elif doctest_child:
            env.pop("RUSTDOC_DOCTEST_BIN_PATH", None)
            env.pop("MARE_DOCTEST_BINARY", None)
            env.pop("MARE_DOCTEST_PYTHON", None)
        child = relay.spawn(command, env=env)
        if child is not None:
            code = child.wait()
    finally:
        try:
            if relay.child is not None and relay.child.poll() is None:
                relay.child.terminate()
                relay.child.wait()
        finally:
            try:
                error = None
                for parent, owned, identity in reversed(roots):
                    try:
                        _cleanup(parent, owned, identity)
                    except (OSError, RuntimeError) as exc:
                        if error is None:
                            error = exc
                if error is not None:
                    raise error
            finally:
                relay.restore()
    return relay.status(code)


def _doc_capture(cargo, arguments, env):
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--doc-command",
        cargo,
        *arguments,
    ]
    relay = _SignalRelay()
    try:
        relay.install()
        child = relay.spawn(command, env=env, stdout=subprocess.PIPE)
        out, _ = child.communicate() if child is not None else (b"", b"")
        code = child.returncode if child is not None else 0
    finally:
        relay.restore()
    return subprocess.CompletedProcess(command, relay.status(code), out, b"")


def _doc_names(output):
    text = output.decode("utf-8")
    names = [line[:-6] for line in text.splitlines() if line.endswith(": test")]
    counts = re.findall(r"(?m)^(\d+) tests?, (\d+) benchmarks?\s*$", text)
    if (
        not counts
        or any(int(benches) for _, benches in counts)
        or sum(int(count) for count, _ in counts) != len(names)
    ):
        raise ValueError("could not enumerate every doctest unambiguously")
    return Counter(names)


def _verify_doctest_runtool(cargo, env):
    # Rustdoc can merge 2024-edition examples into one executable. Verify that this
    # toolchain invokes our child launcher once per example before trusting its output.
    with tempfile.TemporaryDirectory(
        prefix="mare-doctest-probe-", dir=_temp_parent(env)
    ) as directory:
        fixture = Path(directory)
        (fixture / "src").mkdir()
        (fixture / "Cargo.toml").write_text(
            '[package]\nname="mare-doctest-probe"\nversion="0.1.0"\nedition="2024"\n',
            encoding="utf-8",
        )
        snippet = (
            "//! ```\n//! use std::io::Write;\n"
            "//! writeln!(std::fs::OpenOptions::new().create(true).append(true).open("
            'std::env::var("MARE_DOCTEST_PROBE_PATH").unwrap()).unwrap(), "{}", '
            'std::env::var("MARE_HOME").unwrap()).unwrap();\n//! ```\n'
        )
        (fixture / "src" / "lib.rs").write_text(snippet + snippet, encoding="utf-8")
        record = fixture / "roots"
        probe_env = dict(env, MARE_DOCTEST_PROBE_PATH=str(record))
        result = _doc_capture(
            cargo,
            [
                "test",
                "--manifest-path",
                str(fixture / "Cargo.toml"),
                "--doc",
                "--",
                "--test-threads=1",
            ],
            probe_env,
        )
        if result.returncode:
            sys.stdout.buffer.write(result.stdout)
            print("nextest test home: rustdoc isolation probe failed", file=sys.stderr)
            return result.returncode
        roots = (
            record.read_text(encoding="utf-8").splitlines() if record.exists() else []
        )
        if (
            len(roots) != 2
            or len(set(roots)) != 2
            or any(Path(root).exists() for root in roots)
        ):
            raise ValueError("rustdoc did not isolate and clean each merged doctest")
    return 0


def _run_doctests(args):
    cargo = os.environ.get("MARE_DOC_CARGO", "cargo")
    manifest = []
    if args:
        if len(args) != 2 or args[0] != "--manifest-path" or not os.path.isabs(args[1]):
            raise ValueError("--doc accepts only --manifest-path with an absolute path")
        manifest = args
    env = os.environ.copy()
    # Sub-commands run with ephemeral XDG_CACHE_HOME dirs; disable the mbx cargo shim so it does
    # not point target/ at a deleted cache dir or leave background GC files behind during cleanup.
    env["MBX_DISABLE"] = "1"
    existing = env.get("CARGO_ENCODED_RUSTDOCFLAGS")
    flags = existing.split("\x1f") if existing else env.get("RUSTDOCFLAGS", "").split()
    if any(flag.startswith("--test-runtool") for flag in flags):
        raise ValueError("existing rustdoc test runtool cannot be combined safely")
    flags += [
        "--test-runtool",
        sys.executable,
        "--test-runtool-arg",
        str(Path(__file__).resolve()),
        "--test-runtool-arg=--doc-runtool",
    ]
    env["CARGO_ENCODED_RUSTDOCFLAGS"] = "\x1f".join(flags)
    if any(key.startswith("CARGO_TARGET_") and key.endswith("_RUNNER") for key in env):
        raise ValueError(
            "doctest target runners cannot be combined with the isolation runtool"
        )
    probe = _verify_doctest_runtool(cargo, env)
    if probe:
        return probe

    base = ["test", "--workspace", "--doc", *manifest, "--", "--color=never"]

    def run(extra):
        result = _doc_capture(cargo, [*base, *extra], env)
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        return result

    result = run(["--list"])
    if result.returncode:
        return result.returncode
    all_tests = _doc_names(result.stdout)
    ignored = Counter()
    if all_tests:
        result = run(["--list", "--ignored"])
        if result.returncode:
            return result.returncode
        ignored = _doc_names(result.stdout)
        if ignored - all_tests:
            raise ValueError("ignored doctests not present in full workspace list")
    result = run(["--test-threads=1"])
    if result.returncode:
        return result.returncode
    summaries = re.findall(
        rb"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        result.stdout,
    )
    expected = (sum((all_tests - ignored).values()), 0, sum(ignored.values()))
    if (
        not summaries
        or tuple(sum(int(row[i]) for row in summaries) for i in range(3)) != expected
    ):
        raise ValueError("doctest result count did not match workspace enumeration")
    return 0


class _DocInterrupted(Exception):
    def __init__(self, signum):
        self.signum = signum


def _doc_driver(args):
    old = {}

    def interrupted(signum, _frame):
        raise _DocInterrupted(signum)

    try:
        old = _install_signal_handlers(interrupted)
        return _run_doctests(args)
    except _DocInterrupted as exc:
        return -exc.signum if os.name != "nt" else 128 + exc.signum
    finally:
        _restore_signal_handlers(old)


def main():
    try:
        args = sys.argv[1:]
        if args[:1] == ["--doc"]:
            result = _doc_driver(args[1:])
        elif args[:1] == ["--doc-command"]:
            result = _run(args[1:])
        elif args[:1] == ["--doc-runtool"]:
            result = _run(args[1:], doctest_harness=True)
        elif os.environ.get("RUSTDOC_DOCTEST_RUN_NB_TEST") and os.environ.get(
            "MARE_DOCTEST_BINARY"
        ):
            result = _run(
                [os.environ["MARE_DOCTEST_BINARY"], *args], doctest_child=True
            )
        else:
            result = _run(args)
    except (OSError, ValueError, RuntimeError) as exc:
        print(f"nextest test home: {exc}", file=sys.stderr)
        return 1
    if result < 0 and os.name != "nt":
        sig = -result
        signal.signal(sig, signal.SIG_DFL)
        os.kill(os.getpid(), sig)
    return result


if __name__ == "__main__":
    sys.exit(main())
