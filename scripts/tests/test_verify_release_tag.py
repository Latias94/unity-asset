from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))
SCRIPT_PATH = SCRIPTS_ROOT / "verify_release_tag.py"
SPEC = importlib.util.spec_from_file_location("verify_release_tag", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
TAG_VERIFIER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = TAG_VERIFIER
SPEC.loader.exec_module(TAG_VERIFIER)


class ReleaseTagVerifierTests(unittest.TestCase):
    def identity(self) -> object:
        return TAG_VERIFIER.GitIdentity("v0.4.0", "b" * 40, "a" * 40)

    def test_accepts_a_remote_verified_annotated_tag_for_the_exact_commit(self) -> None:
        identity = self.identity()
        responses = [
            {"object": {"type": "tag", "sha": identity.tag_object}},
            {
                "tag": identity.tag,
                "object": {"type": "commit", "sha": identity.commit},
                "verification": {"verified": True},
            },
        ]
        with mock.patch.object(TAG_VERIFIER, "gh_json", side_effect=responses):
            TAG_VERIFIER.verify_remote_signed_tag(
                "example/unity-asset",
                identity,
                identity.tag_object,
                identity.tag_object,
            )

    def test_rejects_moved_or_unsigned_remote_tags(self) -> None:
        identity = self.identity()
        with mock.patch.object(
            TAG_VERIFIER,
            "gh_json",
            return_value={"object": {"type": "tag", "sha": "c" * 40}},
        ):
            with self.assertRaisesRegex(TAG_VERIFIER.VerificationError, "does not match"):
                TAG_VERIFIER.verify_remote_signed_tag(
                    "example/unity-asset", identity, identity.tag_object, None
                )

        with mock.patch.object(
            TAG_VERIFIER,
            "gh_json",
            side_effect=[
                {"object": {"type": "tag", "sha": identity.tag_object}},
                {
                    "tag": identity.tag,
                    "object": {"type": "commit", "sha": identity.commit},
                    "verification": {"verified": False},
                },
            ],
        ):
            with self.assertRaisesRegex(TAG_VERIFIER.VerificationError, "did not verify"):
                TAG_VERIFIER.verify_remote_signed_tag(
                    "example/unity-asset", identity, identity.tag_object, None
                )

    def test_refresh_uses_a_forced_tag_refspec_before_local_verification(self) -> None:
        arguments = mock.Mock(
            repository_root=Path("."),
            tag="v0.4.0",
            expected_commit="a" * 40,
            expected_tag_object="b" * 40,
            github_repository="example/unity-asset",
            expected_event_sha=None,
            refresh_tag=True,
        )
        identity = self.identity()
        with (
            mock.patch.object(TAG_VERIFIER, "parse_args", return_value=arguments),
            mock.patch.object(TAG_VERIFIER, "run_text", return_value="") as run_text,
            mock.patch.object(
                TAG_VERIFIER, "verify_git_identity", return_value=identity
            ),
            mock.patch.object(TAG_VERIFIER, "verify_remote_signed_tag"),
        ):
            self.assertEqual(TAG_VERIFIER.main(), 0)
        self.assertIn("+refs/tags/v0.4.0:refs/tags/v0.4.0", run_text.call_args.args[0])


if __name__ == "__main__":
    unittest.main()
