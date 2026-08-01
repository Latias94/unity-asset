from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from collections import deque
from pathlib import Path
from unittest import mock


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))
SCRIPT_PATH = SCRIPTS_ROOT / "publish_workspace_packages.py"
SPEC = importlib.util.spec_from_file_location("publish_workspace_packages", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
PUBLISHER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = PUBLISHER
SPEC.loader.exec_module(PUBLISHER)


class FakeBackend:
    def __init__(
        self,
        exists: list[bool],
        *,
        publish_errors: list[Exception | None] | None = None,
        verify_errors: list[Exception | None] | None = None,
    ) -> None:
        self.exists = deque(exists)
        self.publish_errors = deque(publish_errors or [])
        self.verify_errors = deque(verify_errors or [])
        self.packaged: list[tuple[str, str]] = []
        self.published: list[str] = []
        self.verified: list[tuple[str, str]] = []

    def package(self, package: str, version: str) -> None:
        self.packaged.append((package, version))

    def release_exists(self, package: str, version: str) -> bool:
        del package, version
        return self.exists.popleft() if self.exists else False

    def verify_existing(self, package: str, version: str) -> None:
        self.verified.append((package, version))
        if self.verify_errors:
            error = self.verify_errors.popleft()
            if error is not None:
                raise error

    def publish(self, package: str) -> None:
        self.published.append(package)
        if self.publish_errors:
            error = self.publish_errors.popleft()
            if error is not None:
                raise error


class RecordingBackend:
    def __init__(
        self,
        exists: dict[str, list[bool | Exception]],
        *,
        verify_errors: dict[str, list[Exception | None]] | None = None,
    ) -> None:
        self.exists = {
            package: deque(outcomes) for package, outcomes in exists.items()
        }
        self.verify_errors = {
            package: deque(errors)
            for package, errors in (verify_errors or {}).items()
        }
        self.events: list[tuple[str, str]] = []
        self.published: list[str] = []

    def package(self, package: str, version: str) -> None:
        del version
        self.events.append(("package", package))

    def release_exists(self, package: str, version: str) -> bool:
        del version
        self.events.append(("inspect", package))
        outcome = self.exists[package].popleft()
        if isinstance(outcome, Exception):
            raise outcome
        return outcome

    def verify_existing(self, package: str, version: str) -> None:
        del version
        self.events.append(("verify", package))
        errors = self.verify_errors.get(package)
        if errors:
            error = errors.popleft()
            if error is not None:
                raise error

    def publish(self, package: str) -> None:
        self.events.append(("publish", package))
        self.published.append(package)


class WorkspacePackagePublisherTests(unittest.TestCase):
    def test_crates_io_download_uses_the_hard_deadline_adapter(self) -> None:
        payload = b"crate bytes"

        def download(_url, destination, **kwargs):
            self.assertEqual(
                kwargs["total_timeout_seconds"],
                PUBLISHER.DOWNLOAD_TOTAL_TIMEOUT_SECONDS,
            )
            destination.write_bytes(payload)

        with mock.patch.object(
            PUBLISHER,
            "download_with_deadline",
            side_effect=download,
        ):
            self.assertEqual(PUBLISHER.download_crate("example", "1.2.3"), payload)

        with mock.patch.object(
            PUBLISHER,
            "download_with_deadline",
            side_effect=PUBLISHER.ReleaseHttpError("hard total timeout"),
        ):
            with self.assertRaisesRegex(
                PUBLISHER.RetryablePublishError,
                "hard total timeout",
            ):
                PUBLISHER.download_crate("example", "1.2.3")

    def test_later_preflight_mismatch_prevents_every_publish_write(self) -> None:
        backend = RecordingBackend(
            {"base": [False], "leaf": [True]},
            verify_errors={
                "leaf": [PUBLISHER.RemoteBytesMismatch("different bytes")]
            },
        )

        with self.assertRaisesRegex(PUBLISHER.RemoteBytesMismatch, "different bytes"):
            PUBLISHER.publish_packages(
                backend,
                ("base", "leaf"),
                "1.2.3",
                max_attempts=2,
                retry_delay_seconds=0,
                sleep=lambda _: None,
            )

        self.assertEqual(backend.published, [])
        self.assertEqual(
            backend.events,
            [
                ("package", "base"),
                ("package", "leaf"),
                ("inspect", "base"),
                ("inspect", "leaf"),
                ("verify", "leaf"),
            ],
        )

    def test_preflight_observation_failure_prevents_every_publish_write(self) -> None:
        backend = RecordingBackend(
            {"base": [False], "leaf": [PUBLISHER.RetryablePublishError("offline")]}
        )

        with self.assertRaisesRegex(PUBLISHER.PublishError, "offline"):
            PUBLISHER.publish_packages(
                backend,
                ("base", "leaf"),
                "1.2.3",
                max_attempts=1,
                retry_delay_seconds=0,
                sleep=lambda _: None,
            )

        self.assertEqual(backend.published, [])

    def test_commit_publishes_missing_packages_in_input_order(self) -> None:
        backend = RecordingBackend(
            {
                "base": [False, False, True],
                "leaf": [False, False, True],
            }
        )

        PUBLISHER.publish_packages(
            backend,
            ("base", "leaf"),
            "1.2.3",
            max_attempts=1,
            retry_delay_seconds=0,
            sleep=lambda _: None,
        )

        self.assertEqual(backend.published, ["base", "leaf"])
        self.assertEqual(
            backend.events,
            [
                ("package", "base"),
                ("package", "leaf"),
                ("inspect", "base"),
                ("inspect", "leaf"),
                ("inspect", "base"),
                ("publish", "base"),
                ("inspect", "base"),
                ("verify", "base"),
                ("inspect", "leaf"),
                ("publish", "leaf"),
                ("inspect", "leaf"),
                ("verify", "leaf"),
            ],
        )

    def test_uncredentialed_cargo_subprocess_removes_ambient_registry_token(self) -> None:
        backend = PUBLISHER.CargoBackend(Path("repository"), "cargo-custom", "passed-token")
        completed = subprocess.CompletedProcess([], 0, stdout="")
        with (
            mock.patch.dict(
                os.environ,
                {
                    "CARGO_REGISTRY_TOKEN": "ambient-token",
                    "UNITY_ASSET_RELEASE_CARGO_TOKEN": "carrier-token",
                    "UNCHANGED": "kept",
                },
                clear=True,
            ),
            mock.patch.object(
                PUBLISHER, "run_bounded_command", return_value=completed
            ) as run,
        ):
            backend.run(["cargo-custom", "metadata"])

        environment = run.call_args.kwargs["env"]
        self.assertEqual(environment, {"UNCHANGED": "kept"})

    def test_release_observation_only_treats_http_404_as_missing(self) -> None:
        backend = PUBLISHER.CargoBackend(Path("repository"), "cargo-custom", "token")

        with mock.patch.object(PUBLISHER, "download_with_deadline") as download:
            self.assertTrue(backend.release_exists("example", "1.2.3"))
        self.assertEqual(
            download.call_args.args[0],
            "https://crates.io/api/v1/crates/example/1.2.3",
        )

        with mock.patch.object(
            PUBLISHER,
            "download_with_deadline",
            side_effect=PUBLISHER.ReleaseHttpNotFound("missing"),
        ):
            self.assertFalse(backend.release_exists("example", "1.2.3"))

        with mock.patch.object(
            PUBLISHER,
            "download_with_deadline",
            side_effect=PUBLISHER.ReleaseHttpError("network unavailable"),
        ):
            with self.assertRaisesRegex(
                PUBLISHER.RetryablePublishError,
                "cannot determine whether example 1.2.3 exists",
            ):
                backend.release_exists("example", "1.2.3")

    def test_publish_subprocess_targets_crates_io_and_only_injects_passed_token(self) -> None:
        backend = PUBLISHER.CargoBackend(Path("repository"), "cargo-custom", "passed-token")
        completed = subprocess.CompletedProcess([], 0, stdout="")
        with (
            mock.patch.dict(
                os.environ,
                {
                    "CARGO_REGISTRY_TOKEN": "ambient-token",
                    "CARGO_REGISTRIES_CRATES_IO_TOKEN": "ambient-token",
                    "UNITY_ASSET_RELEASE_CARGO_TOKEN": "carrier-token",
                    "UNCHANGED": "kept",
                },
                clear=True,
            ),
            mock.patch.object(
                PUBLISHER, "run_bounded_command", return_value=completed
            ) as run,
        ):
            backend.publish("example")

        command = run.call_args.args[0]
        self.assertEqual(
            command,
            [
                "cargo-custom",
                "publish",
                "--locked",
                "--no-verify",
                "--registry",
                "crates-io",
                "-p",
                "example",
            ],
        )
        environment = run.call_args.kwargs["env"]
        self.assertEqual(
            environment,
            {"UNCHANGED": "kept", "CARGO_REGISTRY_TOKEN": "passed-token"},
        )

    def test_preflight_retries_remote_observation_without_publishing(self) -> None:
        backend = RecordingBackend(
            {
                "base": [PUBLISHER.RetryablePublishError("offline"), False],
                "leaf": [False],
            }
        )
        pauses: list[float] = []

        publication = PUBLISHER.prepare_publication(
            backend,
            ("base", "leaf"),
            "1.2.3",
            max_attempts=2,
            retry_delay_seconds=3,
            sleep=pauses.append,
        )

        self.assertEqual(
            [status.state for status in publication.packages],
            [PUBLISHER.RemotePackageState.MISSING, PUBLISHER.RemotePackageState.MISSING],
        )
        self.assertEqual(backend.published, [])
        self.assertEqual(pauses, [3])

    def test_package_removes_the_exact_stale_archive_before_running_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, mock.patch.dict(
            os.environ,
            {"CARGO_TARGET_DIR": temporary},
            clear=False,
        ):
            backend = PUBLISHER.CargoBackend(Path("repository"), "cargo", "token")
            archive = backend.archive_path("example", "1.2.3")
            archive.parent.mkdir(parents=True)
            archive.write_bytes(b"stale")

            def package(_command, **_kwargs):
                self.assertFalse(archive.exists())
                archive.write_bytes(b"fresh")
                return subprocess.CompletedProcess([], 0, stdout="")

            with mock.patch.object(
                PUBLISHER,
                "run_bounded_command",
                side_effect=package,
            ):
                backend.package("example", "1.2.3")

            self.assertEqual(archive.read_bytes(), b"fresh")

    def test_remote_observation_models_missing_and_exists_unverified(self) -> None:
        backend = FakeBackend([False, True])

        observations = PUBLISHER.inspect_remote_packages(
            backend,
            ("missing-package", "existing-package"),
            "1.2.3",
        )

        self.assertEqual(
            [observation.state for observation in observations],
            [
                PUBLISHER.RemotePackageState.MISSING,
                PUBLISHER.RemotePackageState.EXISTS_UNVERIFIED,
            ],
        )

    def test_first_publish_waits_for_and_verifies_remote_archive(self) -> None:
        backend = FakeBackend([False, False, True])
        PUBLISHER.publish_packages(
            backend,
            ("example",),
            "1.2.3",
            max_attempts=2,
            retry_delay_seconds=0,
            sleep=lambda _: None,
        )
        self.assertEqual(backend.packaged, [("example", "1.2.3")])
        self.assertEqual(backend.published, ["example"])
        self.assertEqual(backend.verified, [("example", "1.2.3")])

    def test_existing_matching_release_never_uses_publish_credentials(self) -> None:
        backend = FakeBackend([True])
        PUBLISHER.publish_packages(
            backend,
            ("example",),
            "1.2.3",
            max_attempts=1,
            retry_delay_seconds=0,
            sleep=lambda _: None,
        )
        self.assertEqual(backend.published, [])
        self.assertEqual(backend.verified, [("example", "1.2.3")])

    def test_mismatched_existing_archive_is_non_retryable(self) -> None:
        backend = FakeBackend(
            [True],
            verify_errors=[PUBLISHER.RemoteBytesMismatch("different bytes")],
        )
        with self.assertRaisesRegex(PUBLISHER.RemoteBytesMismatch, "different bytes"):
            PUBLISHER.publish_packages(
                backend,
                ("example",),
                "1.2.3",
                max_attempts=3,
                retry_delay_seconds=0,
                sleep=lambda _: None,
            )
        self.assertEqual(backend.published, [])

    def test_prepared_publication_contains_only_commit_safe_states(self) -> None:
        backend = FakeBackend([False, True])

        publication = PUBLISHER.prepare_publication(
            backend,
            ("missing", "existing"),
            "1.2.3",
            max_attempts=1,
            retry_delay_seconds=0,
            sleep=lambda _: None,
        )

        self.assertEqual(
            publication.packages,
            (
                PUBLISHER.PackageRemoteStatus(
                    "missing", PUBLISHER.RemotePackageState.MISSING
                ),
                PUBLISHER.PackageRemoteStatus(
                    "existing", PUBLISHER.RemotePackageState.VERIFIED
                ),
            ),
        )
        self.assertEqual(backend.published, [])

    def test_prepared_publication_rejects_unverified_existing_state(self) -> None:
        with self.assertRaisesRegex(
            PUBLISHER.PublishError, "unverified existing packages: example"
        ):
            PUBLISHER.PreparedPublication(
                "1.2.3",
                (
                    PUBLISHER.PackageRemoteStatus(
                        "example", PUBLISHER.RemotePackageState.EXISTS_UNVERIFIED
                    ),
                ),
            )

    def test_known_existing_unverified_retries_reads_without_publishing(self) -> None:
        backend = FakeBackend(
            [True],
            verify_errors=[
                PUBLISHER.RetryablePublishError("download unavailable"),
                PUBLISHER.RetryablePublishError("download unavailable"),
            ],
        )
        pauses: list[float] = []

        with self.assertRaisesRegex(
            PUBLISHER.PublishError,
            "exists on crates.io but could not be byte-verified after 2 attempts",
        ):
            PUBLISHER.publish_packages(
                backend,
                ("example",),
                "1.2.3",
                max_attempts=2,
                retry_delay_seconds=2,
                sleep=pauses.append,
            )

        self.assertEqual(backend.published, [])
        self.assertEqual(
            backend.verified,
            [("example", "1.2.3"), ("example", "1.2.3")],
        )
        self.assertEqual(pauses, [2])

    def test_preflight_missing_publish_race_verifies_the_winner(self) -> None:
        backend = FakeBackend(
            [False, False, False, True],
            publish_errors=[PUBLISHER.RetryablePublishError("already exists")],
        )
        pauses: list[float] = []
        PUBLISHER.publish_packages(
            backend,
            ("example",),
            "1.2.3",
            max_attempts=3,
            retry_delay_seconds=2,
            sleep=pauses.append,
        )
        self.assertEqual(backend.published, ["example"])
        self.assertEqual(pauses, [2])
        self.assertEqual(backend.verified, [("example", "1.2.3")])

    def test_retry_exhaustion_is_bounded_after_an_accepted_publish(self) -> None:
        backend = FakeBackend([False, False, False, False])
        with self.assertRaisesRegex(PUBLISHER.PublishError, "after 2 attempts"):
            PUBLISHER.publish_packages(
                backend,
                ("example",),
                "1.2.3",
                max_attempts=2,
                retry_delay_seconds=0,
                sleep=lambda _: None,
            )
        self.assertEqual(backend.published, ["example"])

    def test_release_package_set_is_exact(self) -> None:
        packages = list(PUBLISHER.PUBLISHABLE_PACKAGE_NAMES)
        PUBLISHER.validate_publication_request(packages, "1.2.3")
        with self.assertRaisesRegex(PUBLISHER.PublishError, "missing"):
            PUBLISHER.validate_publication_request(packages[:-1], "1.2.3")
        packages[0], packages[1] = packages[1], packages[0]
        with self.assertRaisesRegex(PUBLISHER.PublishError, "package order"):
            PUBLISHER.validate_publication_request(packages, "1.2.3")


if __name__ == "__main__":
    unittest.main()
