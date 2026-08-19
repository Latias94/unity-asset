from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))
from verify_release_bundle import (  # noqa: E402
    VerifiedReleaseAsset,
    VerifiedReleaseBundle,
)

SCRIPT_PATH = SCRIPTS_ROOT / "verify_github_release_assets.py"
SPEC = importlib.util.spec_from_file_location("github_release_assets", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
VERIFIER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = VERIFIER
SPEC.loader.exec_module(VERIFIER)


class GitHubReleaseAssetVerifierTests(unittest.TestCase):
    def metadata(self):
        return VERIFIER.ReleaseMetadata(title="v0.4.0", body="Release notes\n")

    def expected_assets(self) -> tuple[dict[str, object], dict[str, bytes]]:
        payloads = {
            "daemon.tar.xz": b"daemon",
            "SHA256SUMS": b"checksums\n",
            "release-evidence.json": b'{"commit":"abc"}\n',
            "release-dist-plan.json": b'{"tag":"v0.4.0"}\n',
        }
        expected = {
            name: VerifiedReleaseAsset(
                size=len(contents),
                sha256=hashlib.sha256(contents).hexdigest(),
            )
            for name, contents in payloads.items()
        }
        return expected, payloads

    def verified_bundle(self, expected: dict[str, object]) -> VerifiedReleaseBundle:
        return VerifiedReleaseBundle(
            evidence=SimpleNamespace(github_release=self.metadata().evidence()),
            assets=expected,
        )

    @staticmethod
    def payload_digest(payloads: dict[str, bytes], asset: object) -> str:
        return hashlib.sha256(payloads[asset.name]).hexdigest()

    def release(
        self,
        *,
        release_id: int = 42,
        draft: bool = True,
        title: str = "v0.4.0",
        body: str = "Release notes\n",
    ) -> dict[str, object]:
        return {
            "id": release_id,
            "tag_name": "v0.4.0",
            "target_commitish": "a" * 40,
            "draft": draft,
            "prerelease": False,
            "name": title,
            "body": body,
        }

    def cli_args(
        self,
        *,
        phase: str,
        expected_release_id: int | None,
        github_output: Path | None = None,
    ) -> SimpleNamespace:
        return SimpleNamespace(
            github_repository="owner/repository",
            tag="v0.4.0",
            commit="a" * 40,
            assets=Path("release-assets"),
            phase=phase,
            expected_release_id=expected_release_id,
            github_output=github_output,
            expected_title="v0.4.0",
            expected_body_file=Path("release-notes.md"),
            expected_evidence_sha256=hashlib.sha256(b"evidence").hexdigest(),
        )

    def main_patches(
        self,
        *,
        args: SimpleNamespace,
        releases: list[dict[str, object]],
    ):
        expected, payloads = self.expected_assets()
        remote = [
            VERIFIER.RemoteAsset(index, name, len(contents))
            for index, (name, contents) in enumerate(payloads.items(), start=1)
        ]
        bundle = self.verified_bundle(expected)
        return {
            "args": mock.patch.object(VERIFIER, "parse_args", return_value=args),
            "bundle": mock.patch.object(
                VERIFIER, "verify_release_bundle", return_value=bundle
            ),
            "metadata": mock.patch.object(
                VERIFIER,
                "read_expected_release_metadata",
                return_value=self.metadata(),
            ),
            "fetch": mock.patch.object(
                VERIFIER, "fetch_release", side_effect=releases
            ),
            "list_assets": mock.patch.object(
                VERIFIER, "list_remote_assets", return_value=remote
            ),
            "download": mock.patch.object(
                VERIFIER,
                "download_remote_asset",
                side_effect=lambda _repository, asset, _expected: self.payload_digest(
                    payloads, asset
                ),
            ),
            "publish": mock.patch.object(VERIFIER, "publish_draft"),
            "delete": mock.patch.object(VERIFIER, "delete_remote_asset"),
        }

    def test_preflight_allows_absent_or_matching_draft_but_not_published_partial(self) -> None:
        expected, payloads = self.expected_assets()
        absent = VERIFIER.examine_release(
            expected,
            None,
            tag="v0.4.0",
            commit="a" * 40,
            phase="preflight",
            expected_metadata=self.metadata(),
            assets_for_release=lambda _: [],
            download=lambda _asset, _expected: "",
        )
        self.assertTrue(absent.needs_upload)

        matching = VERIFIER.examine_release(
            expected,
            self.release(draft=True),
            tag="v0.4.0",
            commit="a" * 40,
            phase="preflight",
            expected_metadata=self.metadata(),
            assets_for_release=lambda _: [
                VERIFIER.RemoteAsset(1, "daemon.tar.xz", len(payloads["daemon.tar.xz"]))
            ],
            download=lambda asset, _expected: self.payload_digest(payloads, asset),
        )
        self.assertTrue(matching.needs_upload)

        complete = VERIFIER.examine_release(
            expected,
            self.release(draft=True),
            tag="v0.4.0",
            commit="a" * 40,
            phase="preflight",
            expected_metadata=self.metadata(),
            assets_for_release=lambda _: [
                VERIFIER.RemoteAsset(index, name, len(contents))
                for index, (name, contents) in enumerate(payloads.items(), start=1)
            ],
            download=lambda asset, _expected: self.payload_digest(payloads, asset),
        )
        self.assertFalse(complete.needs_upload)

        with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "incomplete"):
            VERIFIER.examine_release(
                expected,
                self.release(draft=False),
                tag="v0.4.0",
                commit="a" * 40,
                phase="preflight",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: [
                    VERIFIER.RemoteAsset(
                        1, "daemon.tar.xz", len(payloads["daemon.tar.xz"])
                    )
                ],
                download=lambda asset, _expected: self.payload_digest(payloads, asset),
            )

    def test_staged_and_published_phases_require_exact_byte_identical_inventory(self) -> None:
        expected, payloads = self.expected_assets()
        remote = [
            VERIFIER.RemoteAsset(index, name, len(contents))
            for index, (name, contents) in enumerate(payloads.items(), start=1)
        ]
        state = VERIFIER.examine_release(
            expected,
            self.release(draft=True),
            tag="v0.4.0",
            commit="a" * 40,
            phase="staged",
            expected_metadata=self.metadata(),
            assets_for_release=lambda _: remote,
            download=lambda asset, _expected: self.payload_digest(payloads, asset),
        )
        self.assertTrue(state.draft)

        with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "remains a draft"):
            VERIFIER.examine_release(
                expected,
                self.release(draft=True),
                tag="v0.4.0",
                commit="a" * 40,
                phase="published",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: remote,
                download=lambda asset, _expected: self.payload_digest(payloads, asset),
            )

        recovered = VERIFIER.examine_release(
            expected,
            self.release(draft=False),
            tag="v0.4.0",
            commit="a" * 40,
            phase="staged",
            expected_metadata=self.metadata(),
            assets_for_release=lambda _: remote,
            download=lambda asset, _expected: self.payload_digest(payloads, asset),
        )
        self.assertFalse(recovered.draft)

    def test_main_publishes_bound_draft_and_revalidates_same_release(self) -> None:
        args = self.cli_args(phase="publish", expected_release_id=42)
        patches = self.main_patches(
            args=args,
            releases=[self.release(draft=True), self.release(draft=False)],
        )
        with ExitStack() as stack:
            entered = {
                name: stack.enter_context(patch) for name, patch in patches.items()
            }
            self.assertEqual(VERIFIER.main(), 0)

        fetch = entered["fetch"]
        publish = entered["publish"]
        publish.assert_called_once_with(
            "owner/repository",
            42,
            tag="v0.4.0",
            commit="a" * 40,
            metadata=self.metadata(),
        )
        fetch.assert_has_calls(
            [
                mock.call("owner/repository", "v0.4.0", 42),
                mock.call("owner/repository", "v0.4.0", 42),
            ]
        )
        entered["bundle"].assert_called_once_with(
            Path("release-assets"),
            "v0.4.0",
            args.expected_evidence_sha256,
            expected_commit="a" * 40,
        )
        entered["metadata"].assert_called_once_with(
            "v0.4.0",
            Path("release-notes.md"),
            self.metadata().evidence(),
        )
        self.assertEqual(fetch.call_count, 2)

    def test_main_recovers_when_bound_release_was_already_published(self) -> None:
        args = self.cli_args(phase="publish", expected_release_id=42)
        patches = self.main_patches(args=args, releases=[self.release(draft=False)])
        with ExitStack() as stack:
            entered = {
                name: stack.enter_context(patch) for name, patch in patches.items()
            }
            self.assertEqual(VERIFIER.main(), 0)

        fetch = entered["fetch"]
        publish = entered["publish"]
        publish.assert_not_called()
        fetch.assert_called_once_with("owner/repository", "v0.4.0", 42)

    def test_full_workflow_rerun_accepts_exact_published_terminal_state(self) -> None:
        for phase in ("preflight", "staged"):
            with self.subTest(phase=phase):
                args = self.cli_args(phase=phase, expected_release_id=None)
                patches = self.main_patches(
                    args=args,
                    releases=[self.release(draft=False)],
                )
                with ExitStack() as stack:
                    entered = {
                        name: stack.enter_context(patch)
                        for name, patch in patches.items()
                    }
                    self.assertEqual(VERIFIER.main(), 0)

                entered["publish"].assert_not_called()
                entered["fetch"].assert_called_once()

    def test_main_reads_back_terminal_state_after_publish_request_error(self) -> None:
        args = self.cli_args(phase="publish", expected_release_id=42)
        patches = self.main_patches(
            args=args,
            releases=[self.release(draft=True), self.release(draft=False)],
        )
        with ExitStack() as stack:
            entered = {
                name: stack.enter_context(patch) for name, patch in patches.items()
            }
            entered["publish"].side_effect = VERIFIER.ReleaseAssetError(
                "client timed out"
            )
            self.assertEqual(VERIFIER.main(), 0)

        self.assertEqual(entered["fetch"].call_count, 2)

    def test_main_preserves_publish_error_when_readback_is_not_terminal(self) -> None:
        args = self.cli_args(phase="publish", expected_release_id=42)
        patches = self.main_patches(
            args=args,
            releases=[self.release(draft=True), self.release(draft=True)],
        )
        with ExitStack() as stack:
            entered = {
                name: stack.enter_context(patch) for name, patch in patches.items()
            }
            entered["publish"].side_effect = VERIFIER.ReleaseAssetError(
                "client timed out"
            )
            with self.assertRaisesRegex(
                VERIFIER.ReleaseAssetError,
                "publish request failed and immediate readback",
            ):
                VERIFIER.main()

    def test_main_rejects_release_id_change_after_patch(self) -> None:
        args = self.cli_args(phase="publish", expected_release_id=42)
        patches = self.main_patches(
            args=args,
            releases=[
                self.release(release_id=42, draft=True),
                self.release(release_id=43, draft=False),
            ],
        )
        with ExitStack() as stack:
            for patch in patches.values():
                stack.enter_context(patch)
            with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "release ID"):
                VERIFIER.main()

    def test_rejects_extra_or_mismatched_remote_assets(self) -> None:
        expected, payloads = self.expected_assets()
        with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "unexpected assets"):
            VERIFIER.examine_release(
                expected,
                self.release(),
                tag="v0.4.0",
                commit="a" * 40,
                phase="staged",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: [
                    VERIFIER.RemoteAsset(1, "unexpected.zip", 3)
                ],
                download=lambda _asset, _expected: hashlib.sha256(b"bad").hexdigest(),
            )
        with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "SHA-256 mismatch"):
            VERIFIER.examine_release(
                expected,
                self.release(),
                tag="v0.4.0",
                commit="a" * 40,
                phase="staged",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: [
                    VERIFIER.RemoteAsset(
                        1, "daemon.tar.xz", len(payloads["daemon.tar.xz"])
                    )
                ],
                download=lambda _asset, _expected: hashlib.sha256(
                    b"tampered"
                ).hexdigest(),
            )

    def test_release_metadata_is_verified_at_every_existing_release_boundary(self) -> None:
        expected, payloads = self.expected_assets()
        remote = [
            VERIFIER.RemoteAsset(index, name, len(contents))
            for index, (name, contents) in enumerate(payloads.items(), start=1)
        ]
        for phase, draft in (("preflight", True), ("staged", True), ("publish", False)):
            with self.subTest(phase=phase), self.assertRaisesRegex(
                VERIFIER.ReleaseAssetError, "title or body"
            ):
                VERIFIER.examine_release(
                    expected,
                    self.release(draft=draft, body="tampered\n"),
                    tag="v0.4.0",
                    commit="a" * 40,
                    phase=phase,
                    expected_metadata=self.metadata(),
                    assets_for_release=lambda _: remote,
                    download=lambda asset, _expected: self.payload_digest(
                        payloads, asset
                    ),
                    expected_release_id=42 if phase == "publish" else None,
                )

    def test_invalid_remote_metadata_is_mapped_to_release_asset_error(self) -> None:
        expected, _ = self.expected_assets()
        with self.assertRaisesRegex(
            VERIFIER.ReleaseAssetError, "invalid title or body"
        ):
            VERIFIER.examine_release(
                expected,
                self.release(body="invalid\0body"),
                tag="v0.4.0",
                commit="a" * 40,
                phase="staged",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: self.fail(
                    "invalid metadata must fail before listing assets"
                ),
                download=lambda _asset, _expected: self.fail(
                    "invalid metadata must fail before downloads"
                ),
            )

    def test_publish_rewrites_verified_metadata_with_state_transition(self) -> None:
        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value={"id": 42},
        ) as gh_json:
            VERIFIER.publish_draft(
                "owner/repository",
                42,
                tag="v0.4.0",
                commit="a" * 40,
                metadata=self.metadata(),
            )

        gh_json.assert_called_once_with(
            "PATCH",
            "repos/owner/repository/releases/42",
            json_body={
                "tag_name": "v0.4.0",
                "target_commitish": "a" * 40,
                "name": "v0.4.0",
                "body": "Release notes\n",
                "draft": False,
                "prerelease": False,
            },
        )

    def test_gh_json_sends_large_request_body_via_stdin(self) -> None:
        body = "release notes\n" * 20_000
        completed = subprocess.CompletedProcess(
            args=["gh"],
            returncode=0,
            stdout='{"id":42}',
            stderr="",
        )
        with mock.patch.object(VERIFIER, "run_gh", return_value=completed) as run_gh:
            payload = VERIFIER.gh_json(
                "PATCH",
                "repos/owner/repository/releases/42",
                json_body={"body": body, "draft": False},
            )

        self.assertEqual(payload, {"id": 42})
        arguments = run_gh.call_args.args[0]
        self.assertEqual(arguments[-3:], ["--input", "-", "repos/owner/repository/releases/42"])
        self.assertNotIn(body, arguments)
        request = json.loads(run_gh.call_args.kwargs["input_text"])
        self.assertEqual(request, {"body": body, "draft": False})

    def test_fetch_release_lists_drafts_and_rejects_duplicate_tags(self) -> None:
        release = self.release(draft=True)
        other_release = self.release(release_id=41)
        other_release["tag_name"] = "v0.3.0"
        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value=[[other_release], [release]],
        ) as gh_json:
            self.assertEqual(
                VERIFIER.fetch_release("owner/repository", "v0.4.0"),
                release,
            )

        gh_json.assert_called_once_with(
            "GET",
            "repos/owner/repository/releases?per_page=100",
            paginate=True,
        )

        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value=[[other_release]],
        ):
            self.assertIsNone(
                VERIFIER.fetch_release("owner/repository", "v0.4.0")
            )

        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value=[[release], [self.release(release_id=43)]],
        ):
            with self.assertRaisesRegex(
                VERIFIER.ReleaseAssetError,
                "multiple releases",
            ):
                VERIFIER.fetch_release("owner/repository", "v0.4.0")

    def test_fetch_release_uses_bound_release_id_for_drafts(self) -> None:
        release = self.release(draft=True)
        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value=release,
        ) as gh_json:
            self.assertEqual(
                VERIFIER.fetch_release("owner/repository", "v0.4.0", 42),
                release,
            )

        gh_json.assert_called_once_with(
            "GET",
            "repos/owner/repository/releases/42",
            allow_not_found=True,
        )

    def test_run_gh_maps_operating_system_errors(self) -> None:
        with mock.patch.object(
            VERIFIER.subprocess,
            "run",
            side_effect=OSError("gh is unavailable"),
        ):
            with self.assertRaisesRegex(
                VERIFIER.ReleaseAssetError, "cannot execute GitHub CLI"
            ):
                VERIFIER.run_gh(["api", "repos/owner/repository"])

    def test_remote_asset_download_streams_to_a_bounded_digest(self) -> None:
        contents = b"daemon"
        asset = VERIFIER.RemoteAsset(1, "daemon.tar.xz", len(contents))
        expected = VerifiedReleaseAsset(
            size=len(contents),
            sha256=hashlib.sha256(contents).hexdigest(),
        )

        class FakeProcess:
            def __init__(self, payload: bytes):
                self.stdout = io.BytesIO(payload)
                self.killed = False

            def wait(self, timeout=None):
                return 0

            def kill(self):
                self.killed = True

        process = FakeProcess(contents)
        with mock.patch.object(
            VERIFIER.subprocess, "Popen", return_value=process
        ):
            digest = VERIFIER.download_remote_asset(
                "owner/repository", asset, expected
            )

        self.assertEqual(digest, expected.sha256)

        oversized = FakeProcess(contents + b"!")
        with mock.patch.object(
            VERIFIER.subprocess, "Popen", return_value=oversized
        ):
            with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "byte limit"):
                VERIFIER.download_remote_asset(
                    "owner/repository", asset, expected
                )

        self.assertTrue(oversized.killed)

    def test_preflight_deletes_only_expected_starter_assets(self) -> None:
        args = self.cli_args(phase="preflight", expected_release_id=None)
        patches = self.main_patches(args=args, releases=[self.release(draft=True)])
        expected, payloads = self.expected_assets()
        remote = [
            VERIFIER.RemoteAsset(1, "daemon.tar.xz", 0, "starter"),
            *[
                VERIFIER.RemoteAsset(index, name, len(contents), "uploaded")
                for index, (name, contents) in enumerate(
                    (
                        (name, contents)
                        for name, contents in payloads.items()
                        if name != "daemon.tar.xz"
                    ),
                    start=2,
                )
            ],
        ]
        with ExitStack() as stack:
            entered = {
                name: stack.enter_context(patch) for name, patch in patches.items()
            }
            stack.enter_context(
                mock.patch.object(VERIFIER, "list_remote_assets", return_value=remote)
            )
            self.assertEqual(VERIFIER.main(), 0)

        entered["delete"].assert_called_once_with("owner/repository", 1)

    def test_starter_assets_are_rejected_outside_bound_draft_preflight(self) -> None:
        expected, _ = self.expected_assets()
        with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "starter"):
            VERIFIER.examine_release(
                expected,
                self.release(draft=True),
                tag="v0.4.0",
                commit="a" * 40,
                phase="staged",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: [
                    VERIFIER.RemoteAsset(1, "daemon.tar.xz", 0, "starter")
                ],
                download=lambda _asset, _expected: self.fail(
                    "starter assets must not be downloaded"
                ),
            )

    def test_unexpected_starter_assets_remain_fail_closed(self) -> None:
        expected, _ = self.expected_assets()
        with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "unexpected assets"):
            VERIFIER.examine_release(
                expected,
                self.release(draft=True),
                tag="v0.4.0",
                commit="a" * 40,
                phase="preflight",
                expected_metadata=self.metadata(),
                assets_for_release=lambda _: [
                    VERIFIER.RemoteAsset(1, "unknown.zip", 0, "starter")
                ],
                download=lambda _asset, _expected: self.fail(
                    "unexpected assets must not be downloaded"
                ),
            )

    def test_list_remote_assets_requires_a_known_state(self) -> None:
        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value=[[{"id": 1, "name": "daemon.tar.xz", "size": 0}]],
        ):
            with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "state"):
                VERIFIER.list_remote_assets("owner/repository", 42)

        with mock.patch.object(
            VERIFIER,
            "gh_json",
            return_value=[[
                {
                    "id": 1,
                    "name": "daemon.tar.xz",
                    "size": 0,
                    "state": "unknown",
                }
            ]],
        ):
            with self.assertRaisesRegex(VERIFIER.ReleaseAssetError, "unsupported state"):
                VERIFIER.list_remote_assets("owner/repository", 42)

    def test_expected_release_metadata_normalizes_only_line_endings(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            body = root / "release-notes.md"
            body.write_bytes(b"First line\r\nSecond line\r\n")
            expected = VERIFIER.ReleaseMetadata(
                title="v0.4.0",
                body="First line\nSecond line\n",
            )
            metadata = VERIFIER.read_expected_release_metadata(
                "v0.4.0",
                body,
                expected.evidence(),
            )

        self.assertEqual(metadata, expected)

if __name__ == "__main__":
    unittest.main()
