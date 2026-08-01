from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))

import release_http  # noqa: E402


class ReleaseHttpTests(unittest.TestCase):
    def test_worker_hashes_exact_bytes_and_enforces_size(self) -> None:
        payload = b"bounded release bytes"
        response = mock.MagicMock()
        response.__enter__.return_value.read.side_effect = [payload, b""]
        response.__exit__.return_value = False
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "download"
            with mock.patch.object(
                release_http.urllib.request,
                "urlopen",
                return_value=response,
            ):
                metadata = release_http._download_once(
                    "https://example.invalid/archive",
                    destination,
                    user_agent="test/1",
                    max_bytes=len(payload),
                    connect_timeout_seconds=1,
                )
            self.assertEqual(destination.read_bytes(), payload)
            self.assertEqual(metadata.encoded_bytes, len(payload))
            self.assertEqual(metadata.sha256, hashlib.sha256(payload).hexdigest())

        oversized = mock.MagicMock()
        oversized.__enter__.return_value.read.side_effect = [payload, b""]
        oversized.__exit__.return_value = False
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "download"
            with (
                mock.patch.object(
                    release_http.urllib.request,
                    "urlopen",
                    return_value=oversized,
                ),
                self.assertRaisesRegex(release_http.ReleaseHttpError, "maximum size"),
            ):
                release_http._download_once(
                    "https://example.invalid/archive",
                    destination,
                    user_agent="test/1",
                    max_bytes=4,
                    connect_timeout_seconds=1,
                )

    def test_http_404_is_a_typed_absence_not_a_generic_transport_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "download"
            not_found = urllib.error.HTTPError(
                "https://example.invalid/missing",
                404,
                "Not Found",
                {},
                None,
            )
            with (
                mock.patch.object(
                    release_http.urllib.request,
                    "urlopen",
                    side_effect=not_found,
                ),
                self.assertRaises(release_http.ReleaseHttpNotFound),
            ):
                release_http._download_once(
                    "https://example.invalid/missing",
                    destination,
                    user_agent="test/1",
                    max_bytes=1024,
                    connect_timeout_seconds=1,
                )

    def test_parent_preserves_the_worker_not_found_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "download"
            completed = subprocess.CompletedProcess(
                ["worker"],
                release_http.WORKER_NOT_FOUND_EXIT_CODE,
                stdout="",
                stderr="error: download returned HTTP 404",
            )
            with (
                mock.patch.object(
                    release_http.subprocess,
                    "run",
                    return_value=completed,
                ),
                self.assertRaises(release_http.ReleaseHttpNotFound),
            ):
                release_http.download_with_deadline(
                    "https://example.invalid/missing",
                    destination,
                    user_agent="test/1",
                    max_bytes=1024,
                    connect_timeout_seconds=1,
                    total_timeout_seconds=12,
                )

    def test_parent_enforces_a_hard_total_timeout_and_removes_partial_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "download"
            partial = destination.with_name(destination.name + ".partial")
            destination.write_bytes(b"partial")
            partial.write_bytes(b"partial")
            with mock.patch.object(
                release_http.subprocess,
                "run",
                side_effect=subprocess.TimeoutExpired(["worker"], 12),
            ):
                with self.assertRaisesRegex(release_http.ReleaseHttpError, "hard total timeout"):
                    release_http.download_with_deadline(
                        "https://example.invalid/archive",
                        destination,
                        user_agent="test/1",
                        max_bytes=1024,
                        connect_timeout_seconds=1,
                        total_timeout_seconds=12,
                    )
            self.assertFalse(destination.exists())
            self.assertFalse(partial.exists())

    def test_parent_rechecks_worker_metadata_and_strips_credentials(self) -> None:
        payload = b"verified"
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "download"

            def run(command, **kwargs):
                destination.write_bytes(payload)
                self.assertNotIn("CARGO_REGISTRY_TOKEN", kwargs["env"])
                self.assertNotIn("CARGO_REGISTRIES_CRATES_IO_TOKEN", kwargs["env"])
                self.assertNotIn("GH_TOKEN", kwargs["env"])
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=json.dumps(
                        {
                            "encoded_bytes": len(payload),
                            "sha256": hashlib.sha256(payload).hexdigest(),
                        }
                    ),
                    stderr="",
                )

            with (
                mock.patch.dict(
                    release_http.os.environ,
                    {
                        "CARGO_REGISTRY_TOKEN": "secret",
                        "CARGO_REGISTRIES_CRATES_IO_TOKEN": "secret",
                        "GH_TOKEN": "secret",
                    },
                ),
                mock.patch.object(release_http.subprocess, "run", side_effect=run),
            ):
                metadata = release_http.download_with_deadline(
                    "https://example.invalid/archive",
                    destination,
                    user_agent="test/1",
                    max_bytes=1024,
                    connect_timeout_seconds=1,
                    total_timeout_seconds=12,
                )
            self.assertEqual(metadata.sha256, hashlib.sha256(payload).hexdigest())


if __name__ == "__main__":
    unittest.main()
