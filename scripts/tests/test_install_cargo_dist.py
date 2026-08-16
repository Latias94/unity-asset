from __future__ import annotations

import importlib.util
import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))
SCRIPT_PATH = SCRIPTS_ROOT / "install_cargo_dist.py"
SPEC = importlib.util.spec_from_file_location("install_cargo_dist", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
INSTALLER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = INSTALLER
SPEC.loader.exec_module(INSTALLER)


class CargoDistInstallerTests(unittest.TestCase):
    def test_rejects_an_unavailable_explicit_shell(self) -> None:
        with mock.patch.object(INSTALLER.shutil, "which", return_value=None):
            with self.assertRaisesRegex(INSTALLER.InstallError, "not available"):
                INSTALLER.select_shell("missing-shell")

    def test_accepts_only_the_pinned_installed_version(self) -> None:
        with mock.patch.object(
            INSTALLER,
            "run_checked",
            return_value=f"cargo-dist {INSTALLER.CARGO_DIST_VERSION}",
        ):
            INSTALLER.verify_installed_version("dist")

        with mock.patch.object(
            INSTALLER,
            "run_checked",
            return_value="cargo-dist 0.30.2",
        ):
            with self.assertRaisesRegex(INSTALLER.InstallError, "version mismatch"):
                INSTALLER.verify_installed_version("dist")

        for output in (
            f"cargo-dist {INSTALLER.CARGO_DIST_VERSION}-malicious",
            f"cargo-dist {INSTALLER.CARGO_DIST_VERSION}\nextra output",
        ):
            with self.subTest(output=output), mock.patch.object(
                INSTALLER, "run_checked", return_value=output
            ):
                with self.assertRaisesRegex(INSTALLER.InstallError, "version mismatch"):
                    INSTALLER.verify_installed_version("dist")

    def test_download_hashes_the_exact_installer_bytes(self) -> None:
        payload = b"#!/bin/sh\necho verified\n"
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "installer.sh"
            metadata = INSTALLER.DownloadMetadata(
                encoded_bytes=len(payload),
                sha256=hashlib.sha256(payload).hexdigest(),
            )

            def download(*_args, **_kwargs):
                destination.write_bytes(payload)
                return metadata

            with mock.patch.object(
                INSTALLER, "download_with_deadline", side_effect=download
            ):
                digest = INSTALLER.download_installer(destination)

            self.assertEqual(destination.read_bytes(), payload)
            self.assertEqual(
                digest,
                hashlib.sha256(payload).hexdigest(),
            )

    def test_download_enforces_a_total_deadline(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "installer.sh"
            with mock.patch.object(
                INSTALLER,
                "download_with_deadline",
                side_effect=INSTALLER.ReleaseHttpError(
                    "download exceeded its hard total timeout of 120s"
                ),
            ):
                with self.assertRaisesRegex(INSTALLER.InstallError, "hard total timeout"):
                    INSTALLER.download_installer(destination)

    def test_cli_entrypoint_reports_an_install_error(self) -> None:
        result = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--shell", "definitely-missing-shell"],
            check=False,
            text=True,
            encoding="utf-8",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("requested shell is not available", result.stderr)

    def test_creates_a_shasum_compatibility_shim_when_sha256sum_is_unavailable(self) -> None:
        def which(name: str, *, path: str | None = None) -> str | None:
            del path
            return "/usr/bin/shasum" if name == "shasum" else None

        with tempfile.TemporaryDirectory() as temporary, mock.patch.dict(
            os.environ,
            {
                "PATH": os.environ.get("PATH", ""),
                "GH_TOKEN": "secret",
                "CARGO_REGISTRIES_CRATES_IO_TOKEN": "secret",
            },
            clear=True,
        ):
            directory = Path(temporary)
            with mock.patch.object(INSTALLER.shutil, "which", side_effect=which):
                environment = INSTALLER.installer_environment(directory)

            shim = directory / "sha256sum"
            self.assertTrue(shim.is_file())
            self.assertEqual(
                shim.read_text(encoding="utf-8"),
                '#!/bin/sh\nexec shasum -a 256 "$@"\n',
            )
            self.assertTrue(
                environment["PATH"].startswith(f"{directory}{INSTALLER.os.pathsep}")
            )
            self.assertNotIn("GH_TOKEN", environment)
            self.assertNotIn("CARGO_REGISTRIES_CRATES_IO_TOKEN", environment)

    def test_rejects_an_unverified_installer_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, mock.patch.object(
            INSTALLER.shutil, "which", return_value=None
        ):
            with self.assertRaisesRegex(INSTALLER.InstallError, "sha256sum or shasum"):
                INSTALLER.installer_environment(Path(temporary))

    def test_main_rejects_a_digest_mismatch_before_running_the_installer(self) -> None:
        arguments = mock.Mock(shell="sh", dist="dist")
        with (
            mock.patch.object(INSTALLER, "parse_args", return_value=arguments),
            mock.patch.object(INSTALLER, "select_shell", return_value="/bin/sh"),
            mock.patch.object(INSTALLER, "download_installer", return_value="0" * 64),
            mock.patch.object(INSTALLER, "run_checked") as run_checked,
        ):
            with self.assertRaisesRegex(INSTALLER.InstallError, "SHA-256 mismatch"):
                INSTALLER.main()
        run_checked.assert_not_called()

    def test_main_runs_the_verified_installer_then_checks_dist(self) -> None:
        arguments = mock.Mock(shell="sh", dist="dist")
        with (
            mock.patch.object(INSTALLER, "parse_args", return_value=arguments),
            mock.patch.object(INSTALLER, "select_shell", return_value="/bin/sh"),
            mock.patch.object(
                INSTALLER,
                "download_installer",
                return_value=INSTALLER.INSTALLER_SHA256,
            ),
            mock.patch.object(INSTALLER, "installer_environment", return_value={}),
            mock.patch.object(
                INSTALLER,
                "run_checked",
                side_effect=["", f"cargo-dist {INSTALLER.CARGO_DIST_VERSION}"],
            ) as run_checked,
        ):
            self.assertEqual(INSTALLER.main(), 0)
        self.assertEqual(run_checked.call_args_list[0].args[0][0], "/bin/sh")
        self.assertEqual(run_checked.call_args_list[1].args[0], ["dist", "--version"])


if __name__ == "__main__":
    unittest.main()
