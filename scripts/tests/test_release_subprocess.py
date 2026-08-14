from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))

import release_subprocess as release_subprocess_module  # noqa: E402
from release_subprocess import (  # noqa: E402
    BoundedCommandCleanupError,
    BoundedCommandTimeout,
    credential_free_environment,
    run_bounded_command,
)


class ReleaseSubprocessTests(unittest.TestCase):
    @staticmethod
    def _fake_process() -> mock.Mock:
        process = mock.Mock()
        process.pid = 1234
        process.returncode = -9
        process.poll.return_value = None
        process.kill.return_value = None
        return process

    def _run_fake_process(
        self,
        process: mock.Mock,
        *,
        terminate: object | None = None,
        cleanup_timeout_seconds: float = 0.03,
    ) -> None:
        terminate_patch = mock.patch.object(
            release_subprocess_module,
            "_terminate_process_tree",
            side_effect=terminate,
        )
        with (
            mock.patch.object(
                release_subprocess_module.subprocess,
                "Popen",
                return_value=process,
            ),
            mock.patch.object(
                release_subprocess_module,
                "_WindowsJob",
                return_value=mock.Mock(),
            ),
            mock.patch.object(
                release_subprocess_module,
                "_CLEANUP_TIMEOUT_SECONDS",
                cleanup_timeout_seconds,
            ),
            terminate_patch,
        ):
            run_bounded_command(["worker"], timeout_seconds=0.01)

    def test_credential_free_environment_removes_release_and_cloud_secrets(self) -> None:
        environment = credential_free_environment(
            {
                "PATH": "kept",
                "CARGO_REGISTRY_TOKEN": "secret",
                "CARGO_REGISTRIES_CRATES_IO_TOKEN": "secret",
                "GH_TOKEN": "secret",
                "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "secret",
                "AWS_SESSION_TOKEN": "secret",
            }
        )
        self.assertEqual(environment, {"PATH": "kept"})

    def test_timeout_terminates_a_real_grandchild_process(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ready = root / "ready"
            escaped = root / "escaped"
            grandchild = (
                "import pathlib,time;"
                "time.sleep(3);"
                f"pathlib.Path({str(escaped)!r}).write_text('escaped', encoding='utf-8')"
            )
            parent = (
                "import pathlib,subprocess,sys,time;"
                f"subprocess.Popen([sys.executable,'-c',{grandchild!r}]);"
                f"pathlib.Path({str(ready)!r}).write_text('ready', encoding='utf-8');"
                "time.sleep(30)"
            )

            with self.assertRaises(BoundedCommandTimeout):
                run_bounded_command(
                    [sys.executable, "-c", parent],
                    timeout_seconds=2,
                    env=os.environ,
                )

            self.assertTrue(ready.is_file(), "parent did not start its grandchild")
            time.sleep(3.5)
            self.assertFalse(escaped.exists())

    def test_cleanup_failure_is_not_misreported_as_a_command_timeout(self) -> None:
        process = self._fake_process()
        process.communicate.side_effect = subprocess.TimeoutExpired(["worker"], 0.01)

        with self.assertRaises(BoundedCommandCleanupError) as raised:
            self._run_fake_process(process, terminate=OSError("kill denied"))

        self.assertNotIsInstance(raised.exception, BoundedCommandTimeout)
        self.assertEqual(raised.exception.operation, "terminating process tree")
        self.assertIsInstance(raised.exception.__cause__, OSError)
        self.assertIn("kill denied", str(raised.exception))

    def test_cleanup_deadline_exhaustion_still_closes_windows_job(self) -> None:
        process = self._fake_process()
        process.communicate.side_effect = subprocess.TimeoutExpired(["worker"], 0.01)
        windows_job = mock.Mock()

        with (
            mock.patch.object(release_subprocess_module.os, "name", "nt"),
            mock.patch.object(
                release_subprocess_module.subprocess,
                "Popen",
                return_value=process,
            ),
            mock.patch.object(
                release_subprocess_module,
                "_WindowsJob",
                return_value=windows_job,
            ),
            mock.patch.object(
                release_subprocess_module,
                "_CLEANUP_TIMEOUT_SECONDS",
                0.0,
            ),
        ):
            with self.assertRaises(BoundedCommandCleanupError) as raised:
                run_bounded_command(["worker"], timeout_seconds=0.01)

        self.assertEqual(raised.exception.operation, "terminating process tree")
        windows_job.close.assert_called_once_with()

    def test_slow_output_cleanup_cannot_overrun_cleanup_deadline(self) -> None:
        process = self._fake_process()
        communicate_calls = 0

        def communicate(*, timeout: float) -> tuple[str, None]:
            nonlocal communicate_calls
            communicate_calls += 1
            if communicate_calls == 1:
                raise subprocess.TimeoutExpired(["worker"], timeout)
            time.sleep(timeout)
            raise subprocess.TimeoutExpired(["worker"], timeout)

        process.communicate.side_effect = communicate

        started_at = time.monotonic()
        with self.assertRaises(BoundedCommandCleanupError) as raised:
            self._run_fake_process(process)
        elapsed = time.monotonic() - started_at

        self.assertEqual(raised.exception.operation, "collecting process output")
        self.assertLess(elapsed, 0.25)

    def test_windows_job_close_failure_has_its_own_operation(self) -> None:
        process = self._fake_process()
        process.communicate.side_effect = subprocess.TimeoutExpired(["worker"], 0.01)
        windows_job = mock.Mock()
        windows_job.close.side_effect = OSError("close denied")

        with (
            mock.patch.object(release_subprocess_module.os, "name", "nt"),
            mock.patch.object(
                release_subprocess_module.subprocess,
                "Popen",
                return_value=process,
            ),
            mock.patch.object(
                release_subprocess_module,
                "_WindowsJob",
                return_value=windows_job,
            ),
        ):
            with self.assertRaises(BoundedCommandCleanupError) as raised:
                run_bounded_command(["worker"], timeout_seconds=0.01)

        self.assertEqual(raised.exception.operation, "closing Windows job")
        self.assertIn("close denied", str(raised.exception))

    def test_keyboard_interrupt_is_not_reclassified_as_cleanup_failure(self) -> None:
        process = self._fake_process()
        process.communicate.side_effect = subprocess.TimeoutExpired(["worker"], 0.01)

        with self.assertRaises(KeyboardInterrupt):
            self._run_fake_process(process, terminate=KeyboardInterrupt())


if __name__ == "__main__":
    unittest.main()
