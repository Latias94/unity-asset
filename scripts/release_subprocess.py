"""Bound release commands to one owned process tree and a credential-free environment."""

from __future__ import annotations

import ctypes
import os
import signal
import subprocess
import time
from pathlib import Path
from typing import Mapping, Sequence


SENSITIVE_ENVIRONMENT_VARIABLES = frozenset(
    {
        "ACTIONS_CACHE_URL",
        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
        "ACTIONS_ID_TOKEN_REQUEST_URL",
        "ACTIONS_RESULTS_URL",
        "ACTIONS_RUNTIME_TOKEN",
        "ACTIONS_RUNTIME_URL",
        "ARM_CLIENT_SECRET",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AZURE_CLIENT_SECRET",
        "CARGO_REGISTRIES_CRATES_IO_CREDENTIAL_PROVIDER",
        "CARGO_REGISTRIES_CRATES_IO_TOKEN",
        "CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS",
        "CARGO_REGISTRY_TOKEN",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "UNITY_ASSET_RELEASE_CARGO_TOKEN",
    }
)

_CLEANUP_TIMEOUT_SECONDS = 10.0


class BoundedCommandTimeout(TimeoutError):
    """A command exceeded its execution deadline."""


class BoundedCommandCleanupError(RuntimeError):
    """A started command could not be cleaned up within the cleanup deadline."""

    def __init__(self, message: str, *, operation: str) -> None:
        super().__init__(message)
        self.operation = operation


class _WindowsJob:
    """Own a Windows process tree through a kill-on-close Job Object."""

    def __init__(self, process: subprocess.Popen[object]) -> None:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        ntdll = ctypes.WinDLL("ntdll", use_last_error=True)

        class IoCounters(ctypes.Structure):
            _fields_ = [
                ("ReadOperationCount", ctypes.c_ulonglong),
                ("WriteOperationCount", ctypes.c_ulonglong),
                ("OtherOperationCount", ctypes.c_ulonglong),
                ("ReadTransferCount", ctypes.c_ulonglong),
                ("WriteTransferCount", ctypes.c_ulonglong),
                ("OtherTransferCount", ctypes.c_ulonglong),
            ]

        class BasicLimitInformation(ctypes.Structure):
            _fields_ = [
                ("PerProcessUserTimeLimit", ctypes.c_longlong),
                ("PerJobUserTimeLimit", ctypes.c_longlong),
                ("LimitFlags", ctypes.c_uint32),
                ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t),
                ("ActiveProcessLimit", ctypes.c_uint32),
                ("Affinity", ctypes.c_size_t),
                ("PriorityClass", ctypes.c_uint32),
                ("SchedulingClass", ctypes.c_uint32),
            ]

        class ExtendedLimitInformation(ctypes.Structure):
            _fields_ = [
                ("BasicLimitInformation", BasicLimitInformation),
                ("IoInfo", IoCounters),
                ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t),
                ("PeakJobMemoryUsed", ctypes.c_size_t),
            ]

        kernel32.CreateJobObjectW.argtypes = (ctypes.c_void_p, ctypes.c_wchar_p)
        kernel32.CreateJobObjectW.restype = ctypes.c_void_p
        kernel32.SetInformationJobObject.argtypes = (
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint32,
        )
        kernel32.SetInformationJobObject.restype = ctypes.c_int
        kernel32.AssignProcessToJobObject.argtypes = (
            ctypes.c_void_p,
            ctypes.c_void_p,
        )
        kernel32.AssignProcessToJobObject.restype = ctypes.c_int
        kernel32.TerminateJobObject.argtypes = (ctypes.c_void_p, ctypes.c_uint32)
        kernel32.TerminateJobObject.restype = ctypes.c_int
        kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
        kernel32.CloseHandle.restype = ctypes.c_int
        ntdll.NtResumeProcess.argtypes = (ctypes.c_void_p,)
        ntdll.NtResumeProcess.restype = ctypes.c_long

        handle = kernel32.CreateJobObjectW(None, None)
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        self._kernel32 = kernel32
        self._handle = handle
        try:
            limits = ExtendedLimitInformation()
            limits.BasicLimitInformation.LimitFlags = 0x00002000
            if not kernel32.SetInformationJobObject(
                handle,
                9,
                ctypes.byref(limits),
                ctypes.sizeof(limits),
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            process_handle = ctypes.c_void_p(int(process._handle))
            if not kernel32.AssignProcessToJobObject(handle, process_handle):
                raise ctypes.WinError(ctypes.get_last_error())
            status = ntdll.NtResumeProcess(process_handle)
            if status < 0:
                raise OSError(f"NtResumeProcess failed with NTSTATUS 0x{status & 0xFFFFFFFF:08x}")
        except Exception:
            self.close()
            raise

    def terminate(self) -> None:
        if self._handle and not self._kernel32.TerminateJobObject(self._handle, 1):
            raise ctypes.WinError(ctypes.get_last_error())

    def close(self) -> None:
        if self._handle:
            if not self._kernel32.CloseHandle(self._handle):
                raise ctypes.WinError(ctypes.get_last_error())
            self._handle = None


def credential_free_environment(
    source: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Copy an environment while removing release and cloud credentials."""

    environment = dict(os.environ if source is None else source)
    for name in SENSITIVE_ENVIRONMENT_VARIABLES:
        environment.pop(name, None)
    return environment


def _terminate_process_tree(
    process: subprocess.Popen[object], windows_job: _WindowsJob | None
) -> None:
    if os.name == "nt":
        if windows_job is not None:
            windows_job.terminate()
        elif process.poll() is None:
            process.kill()
        return

    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError:
        if process.poll() is None:
            process.kill()


def _cleanup_started_process(
    process: subprocess.Popen[object],
    windows_job: _WindowsJob | None,
    *,
    command: Sequence[str],
    failure_context: str,
) -> None:
    deadline = time.monotonic() + _CLEANUP_TIMEOUT_SECONDS

    def remaining(operation: str) -> float:
        seconds = deadline - time.monotonic()
        if seconds <= 0:
            raise BoundedCommandCleanupError(
                f"{failure_context}; cleanup deadline expired while {operation}: "
                f"{' '.join(command)}",
                operation=operation,
            )
        return seconds

    def cleanup_error(
        operation: str, error: Exception
    ) -> BoundedCommandCleanupError:
        detail = str(error) or type(error).__name__
        return BoundedCommandCleanupError(
            f"{failure_context}; cleanup failed while {operation}: {detail}: "
            f"{' '.join(command)}",
            operation=operation,
        )

    termination_failure: BaseException | None = None
    try:
        remaining("terminating process tree")
        _terminate_process_tree(process, windows_job)
    except BaseException as error:
        termination_failure = error

    close_failure: Exception | None = None
    if windows_job is not None:
        try:
            windows_job.close()
        except Exception as error:
            close_failure = error

    if termination_failure is not None:
        if not isinstance(termination_failure, Exception):
            raise termination_failure
        if isinstance(termination_failure, BoundedCommandCleanupError):
            raise termination_failure
        raise cleanup_error(
            "terminating process tree", termination_failure
        ) from termination_failure
    if close_failure is not None:
        raise cleanup_error("closing Windows job", close_failure) from close_failure

    operation = "collecting process output"
    try:
        process.communicate(timeout=remaining(operation))
    except subprocess.TimeoutExpired as error:
        raise BoundedCommandCleanupError(
            f"{failure_context}; cleanup deadline expired while {operation}: "
            f"{' '.join(command)}",
            operation=operation,
        ) from error
    except Exception as error:
        raise cleanup_error(operation, error) from error


def run_bounded_command(
    command: Sequence[str],
    *,
    timeout_seconds: float,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a text command and own the complete process tree until completion."""

    if not command or timeout_seconds <= 0:
        raise ValueError("command and timeout must be non-empty and positive")
    popen_arguments: dict[str, object] = {}
    if os.name == "nt":
        popen_arguments["creationflags"] = (
            getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
            | getattr(subprocess, "CREATE_SUSPENDED", 0)
        )
    else:
        popen_arguments["start_new_session"] = True

    windows_job: _WindowsJob | None = None
    try:
        process = subprocess.Popen(
            list(command),
            cwd=cwd,
            env=None if env is None else dict(env),
            text=True,
            encoding="utf-8",
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            **popen_arguments,
        )
    except OSError:
        raise

    if os.name == "nt":
        try:
            windows_job = _WindowsJob(process)
        except Exception as setup_error:
            _cleanup_started_process(
                process,
                None,
                command=command,
                failure_context=f"Windows job setup failed: {setup_error}",
            )
            raise

    try:
        stdout, _ = process.communicate(timeout=timeout_seconds)
    except subprocess.TimeoutExpired as error:
        cleanup_job = windows_job
        windows_job = None
        _cleanup_started_process(
            process,
            cleanup_job,
            command=command,
            failure_context=f"command exceeded its {timeout_seconds:g}s deadline",
        )
        raise BoundedCommandTimeout(
            f"command exceeded its {timeout_seconds:g}s deadline: {' '.join(command)}"
        ) from error
    finally:
        if windows_job is not None:
            windows_job.close()

    return subprocess.CompletedProcess(
        args=list(command),
        returncode=process.returncode,
        stdout=stdout,
        stderr=None,
    )
