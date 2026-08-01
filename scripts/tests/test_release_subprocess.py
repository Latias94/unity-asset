from __future__ import annotations

import os
import sys
import tempfile
import time
import unittest
from pathlib import Path


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))

from release_subprocess import (  # noqa: E402
    BoundedCommandTimeout,
    credential_free_environment,
    run_bounded_command,
)


class ReleaseSubprocessTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
