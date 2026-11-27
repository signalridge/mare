"""LuaLS / ast-grep subprocess runner, process cleanup, and temp workspace helpers."""

from __future__ import annotations

import concurrent.futures
import contextlib
import json
import os
import shutil
import signal
import subprocess
import tempfile
from collections.abc import Iterator
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


PRESET = ROOT / "crates/mare-plugin/preset"


BOUNDARY_DIAGNOSTICS = {
    "undefined-field",
    "undefined-doc-name",
    "param-type-mismatch",
    "assign-type-mismatch",
    "missing-parameter",
    "redundant-parameter",
    "luadoc-miss-symbol",
}


def diagnose(workspace: Path) -> dict:
    config = {
        "runtime.version": "Lua 5.5",
        "runtime.path": ["lua/?.lua", "lua/?/init.lua"],
        "workspace.library": ["types"],
        "workspace.checkThirdParty": False,
        "diagnostics.globals": ["mare", "ui"],
    }
    (workspace / ".luarc.json").write_text(json.dumps(config))
    report = workspace / "diagnostics.json"
    parent_tmp = str(workspace.parent)
    child_env = dict(os.environ, MARE_TMPDIR=parent_tmp, TMPDIR=parent_tmp, TEMP=parent_tmp, TMP=parent_tmp)
    result = subprocess.run(
        [
            "lua-language-server",
            f"--check={workspace}",
            "--check_format=json",
            f"--check_out_path={report}",
            f"--logpath={workspace.parent / 'logs'}",
            f"--metapath={workspace.parent / 'meta'}",
        ],
        env=child_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=300,
        check=False,
    )
    if result.returncode not in (0, 1) or not report.is_file():
        raise RuntimeError(f"LuaLS failed ({result.returncode}): {result.stdout}")
    report_text = report.read_text().strip()
    if not report_text:
        raise RuntimeError(
            f"LuaLS wrote empty report ({result.returncode}): {result.stdout}"
        )
    try:
        return json.loads(report_text)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"LuaLS wrote invalid JSON report ({result.returncode}): {error}"
        ) from error


def workspace_at(path: Path) -> None:
    path.mkdir(parents=True)
    shutil.copytree(PRESET / "types", path / "types")


def ast_grep_matches(returncode: int, stdout: str, stderr: str, what: str) -> list[dict]:
    """ast-grep's --json=compact result, or an error: a scanner that failed must never read as
    zero violations. No match is `[]` with exit status 1; anything else is a failure: another
    status, an `ERROR:` line on stderr, empty output, or JSON that isn't an array of matches."""
    if returncode not in (0, 1):
        raise RuntimeError(f"{what} failed (exit {returncode}): {stderr.strip()}")
    if "ERROR:" in stderr:
        raise RuntimeError(f"{what} reported an error: {stderr.strip()}")
    if not stdout.strip():
        raise RuntimeError(f"{what} produced no output (expected a JSON array)")
    try:
        matches = json.loads(stdout)
    except json.JSONDecodeError as err:
        raise RuntimeError(f"{what} produced invalid JSON: {err}") from err
    if not isinstance(matches, list):
        raise RuntimeError(f"{what} produced {type(matches).__name__}, not an array of matches")
    for match in matches:
        # what the callers read: the file and the byte range of the match
        offset = match.get("range", {}).get("byteOffset") if isinstance(match, dict) else None
        if not (
            isinstance(match.get("file") if isinstance(match, dict) else None, str)
            and isinstance(offset, dict)
            and isinstance(offset.get("start"), int)
            and isinstance(offset.get("end"), int)
        ):
            raise RuntimeError(f"{what} produced an entry that is not a match: {match!r:.200}")
    return matches


def default_temp_root(base_dir: Path | None = None) -> Path:
    if base_dir is not None:
        return base_dir
    for key in ("MARE_TMPDIR", "TMPDIR", "TEMP", "TMP"):
        value = os.environ.get(key)
        if value:
            return Path(value)
    return Path(tempfile.gettempdir())


def install_signal_cleanup() -> None:
    if hasattr(signal, "SIGTERM"):
        signal.signal(signal.SIGTERM, lambda _signum, _frame: (_ for _ in ()).throw(SystemExit(143)))


@contextlib.contextmanager
def isolated_temp_dir(prefix: str, base_dir: Path | None = None) -> Iterator[Path]:
    parent = default_temp_root(base_dir)
    parent.mkdir(parents=True, exist_ok=True)
    path = Path(tempfile.mkdtemp(prefix=prefix, dir=str(parent)))
    try:
        yield path
    finally:
        if path.exists():
            shutil.rmtree(path)


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


def _assign_win_job(proc: subprocess.Popen[str]) -> int | None:
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

    kernel32 = _win_kernel32()
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


def _kill_proc_tree(
    proc: subprocess.Popen[str], job_handle: int | None = None
) -> None:
    if os.name != "nt":
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        return

    terminated = False
    if job_handle is not None:
        terminated = bool(_win_kernel32().TerminateJobObject(job_handle, 1))
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


def _is_pid_alive(pid: int) -> bool:
    if os.name != "nt":
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True

    import ctypes

    kernel32 = _win_kernel32()
    handle = kernel32.OpenProcess(0x00101000, False, pid)
    if not handle:
        return ctypes.get_last_error() == 5
    try:
        return kernel32.WaitForSingleObject(handle, 0) == 0x00000102
    finally:
        kernel32.CloseHandle(handle)


def run_concurrent_workers(
    cmds: list[list[str]],
    cwd: Path,
    envs: list[dict[str, str]],
    timeout: float = 60.0,
) -> list[tuple[int, str, str]]:
    """Run worker subprocesses concurrently in isolated process groups with bounded cleanup."""
    procs: list[subprocess.Popen[str]] = []
    jobs: list[int | None] = []
    had_error = True
    popen_kwargs: dict[str, object] = {}
    if os.name == "nt":
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
                text=True,
                **popen_kwargs,
            )
            procs.append(proc)
            jobs.append(_assign_win_job(proc) if os.name == "nt" else None)
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(procs)) as pool:
            futures = [pool.submit(proc.communicate, timeout=timeout) for proc in procs]
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
                    _kill_proc_tree(proc, job_handle)
                    try:
                        proc.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        _kill_proc_tree(proc, job_handle)
                        for stream in (proc.stdout, proc.stderr):
                            if stream is not None:
                                stream.close()
                        try:
                            proc.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            pass
            finally:
                if job_handle is not None:
                    _win_kernel32().CloseHandle(job_handle)
