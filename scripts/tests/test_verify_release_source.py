"""Regression tests for release source identity verification."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_ROOT = REPOSITORY_ROOT / "scripts"
sys.path.insert(0, str(SCRIPTS_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))
VERIFIER_PATH = SCRIPTS_ROOT / "verify_release_source.py"
SPEC = importlib.util.spec_from_file_location("release_source_verifier", VERIFIER_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load release verifier from {VERIFIER_PATH}")
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)

from release_evidence_support import make_release_evidence  # noqa: E402


def run_git(repository: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=repository,
        check=True,
        text=True,
        encoding="utf-8",
        stdout=subprocess.PIPE,
    )
    return result.stdout.strip()


def valid_dist_plan() -> dict[str, object]:
    targets = (
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-musl",
    )
    applications = ("unity-asset-search-cli", "unity-asset-search-daemon")
    artifacts: dict[str, object] = {}
    releases: list[dict[str, object]] = []
    for application in applications:
        release_artifacts: list[str] = []
        for target in targets:
            extension = ".zip" if target.endswith("windows-msvc") else ".tar.xz"
            name = f"{application}-{target}{extension}"
            checksum = f"{name}.sha256"
            artifacts[name] = {
                "name": name,
                "kind": "executable-zip",
                "target_triples": [target],
                "checksum": checksum,
            }
            artifacts[checksum] = {
                "name": checksum,
                "kind": "checksum",
                "target_triples": [target],
            }
            release_artifacts.extend((name, checksum))
        releases.append(
            {
                "app_name": application,
                "app_version": "1.2.3",
                "artifacts": release_artifacts,
            }
        )
    return {
        "dist_version": "0.30.3",
        "announcement_tag": "v1.2.3",
        "announcement_tag_is_implicit": False,
        "announcement_is_prerelease": False,
        "artifacts": artifacts,
        "releases": releases,
    }


class ReleaseSourceVerifierTests(unittest.TestCase):
    def create_tagged_repository(self, root: Path) -> tuple[Path, str]:
        repository = root / "repository"
        repository.mkdir()
        run_git(repository, "init", "--quiet")
        run_git(repository, "config", "user.name", "Release Test")
        run_git(repository, "config", "user.email", "release-test@example.invalid")
        run_git(repository, "config", "core.autocrlf", "false")
        (repository / "tracked.txt").write_text("release source\n", encoding="utf-8")
        run_git(repository, "add", "tracked.txt")
        run_git(repository, "commit", "--quiet", "-m", "release source")
        run_git(repository, "tag", "-a", "-m", "v1.2.3", "v1.2.3")
        return repository, run_git(repository, "rev-parse", "HEAD")

    def test_accepts_a_clean_checkout_at_the_peeled_tag_commit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            repository, commit = self.create_tagged_repository(Path(temporary))

            identity = verifier.verify_git_identity(repository, "v1.2.3", commit)

            self.assertEqual(identity.commit, commit)
            self.assertNotEqual(identity.tag_object, commit)
            self.assertEqual(
                run_git(repository, "rev-parse", "refs/tags/v1.2.3"), identity.tag_object
            )

    def test_rejects_head_or_worktree_drift_from_the_tag(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            repository, commit = self.create_tagged_repository(Path(temporary))
            (repository / "tracked.txt").write_text("dirty source\n", encoding="utf-8")
            with self.assertRaisesRegex(verifier.VerificationError, "not clean"):
                verifier.verify_git_identity(repository, "v1.2.3", commit)

            run_git(repository, "add", "tracked.txt")
            run_git(repository, "commit", "--quiet", "-m", "drift")
            with self.assertRaisesRegex(verifier.VerificationError, "does not match"):
                verifier.verify_git_identity(repository, "v1.2.3", None)

    def test_rejects_non_release_tags_and_partial_event_commits(self) -> None:
        with self.assertRaisesRegex(verifier.VerificationError, "vMAJOR.MINOR.PATCH"):
            verifier.parse_release_tag("release-1.2.3")
        with self.assertRaisesRegex(verifier.VerificationError, "vMAJOR.MINOR.PATCH"):
            verifier.parse_release_tag("v1.2.3-rc.1")
        with self.assertRaisesRegex(verifier.VerificationError, "vMAJOR.MINOR.PATCH"):
            verifier.parse_release_tag("v01.2.3")

        with tempfile.TemporaryDirectory() as temporary:
            repository, commit = self.create_tagged_repository(Path(temporary))
            with self.assertRaisesRegex(verifier.VerificationError, "full lowercase"):
                verifier.verify_git_identity(repository, "v1.2.3", commit[:12])

            run_git(repository, "tag", "--delete", "v1.2.3")
            run_git(repository, "tag", "v1.2.3")
            with self.assertRaisesRegex(verifier.VerificationError, "annotated"):
                verifier.verify_git_identity(repository, "v1.2.3", commit)

    def test_canonical_evidence_is_stable_and_content_addressed(self) -> None:
        payload = {"schema": verifier.EVIDENCE_SCHEMA, "commit": "a" * 40}
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "release-evidence.json"
            first = verifier.write_canonical_json(output, payload)
            second = verifier.write_canonical_json(output, payload)

            self.assertEqual(first, second)
            self.assertEqual(first, hashlib.sha256(output.read_bytes()).hexdigest())
            self.assertEqual(
                output.read_text(encoding="utf-8"),
                '{"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",'
                '"schema":"unity-asset.release-evidence.v3"}\n',
            )

    def test_cargo_lock_digest_comes_from_the_committed_regular_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            repository, _ = self.create_tagged_repository(Path(temporary))
            lockfile = repository / "Cargo.lock"
            committed_bytes = b"version = 3\n"
            lockfile.write_bytes(committed_bytes)
            run_git(repository, "add", "Cargo.lock")
            run_git(repository, "commit", "--quiet", "-m", "add lockfile")

            lockfile.write_bytes(b"uncommitted drift\n")

            self.assertEqual(
                verifier.sha256_git_blob(repository, "Cargo.lock"),
                hashlib.sha256(committed_bytes).hexdigest(),
            )

    def test_release_toolchain_and_msrv_require_exact_patch_versions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text(
                '[workspace]\nresolver = "3"\n\n'
                '[workspace.package]\nrust-version = "1.88.0"\n',
                encoding="utf-8",
            )
            (root / "rust-toolchain.toml").write_text(
                '[toolchain]\nchannel = "1.97.1"\n', encoding="utf-8"
            )
            self.assertEqual(
                verifier.workspace_release_contract(root), ("1.88.0", "1.97.1")
            )

            (root / "Cargo.toml").write_text(
                '[workspace]\nresolver = "3"\n\n'
                '[workspace.package]\nrust-version = "1.88"\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(verifier.VerificationError, "exact patch"):
                verifier.workspace_release_contract(root)

    def test_dist_plan_is_bound_to_the_tag_version_and_complete_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            plan = Path(temporary) / "release-dist-plan.json"
            plan.write_text(json.dumps(valid_dist_plan()), encoding="utf-8")
            digest, artifacts = verifier.dist_plan_evidence(plan, "v1.2.3", "1.2.3")
            self.assertEqual(digest, hashlib.sha256(plan.read_bytes()).hexdigest())
            self.assertEqual(len(artifacts), 16)

            invalid = valid_dist_plan()
            del invalid["artifacts"]["unity-asset-search-cli-x86_64-apple-darwin.tar.xz"]
            plan.write_text(json.dumps(invalid), encoding="utf-8")
            with self.assertRaisesRegex(verifier.VerificationError, "invalid release dist plan"):
                verifier.dist_plan_evidence(plan, "v1.2.3", "1.2.3")

    def test_main_records_locked_metadata_toolchains_and_plan_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = root / "release-dist-plan.json"
            plan.write_text(json.dumps(valid_dist_plan()), encoding="utf-8")
            evidence = root / "release-evidence.json"
            github_output = root / "github-output"
            protocol_sdk_bundle = root / "protocol-sdk.zip"
            protocol_sdk_bundle.write_bytes(b"sdk")
            release_title = root / "release-title.txt"
            release_body = root / "release-notes.md"
            identity = verifier.GitIdentity("v1.2.3", "b" * 40, "a" * 40)
            fixture = make_release_evidence(
                dist_plan_sha256=hashlib.sha256(plan.read_bytes()).hexdigest(),
                dist_artifacts=sorted(valid_dist_plan()["artifacts"]),
            )
            protocol_sdk = mock.Mock(
                artifact_name="unity-asset-search-protocol-sdk-v1.2.3.zip",
                as_dict=mock.Mock(return_value=fixture["protocol_sdk"]),
            )
            github_release = mock.Mock(
                evidence=mock.Mock(return_value=fixture["github_release"])
            )
            arguments = mock.Mock(
                repository_root=root,
                tag="v1.2.3",
                expected_commit=None,
                dist_plan=plan,
                protocol_sdk_bundle=protocol_sdk_bundle,
                release_title_output=release_title,
                release_body_output=release_body,
                evidence=evidence,
                github_output=github_output,
                cargo="cargo",
            )
            with (
                mock.patch.object(verifier, "parse_args", return_value=arguments),
                mock.patch.object(verifier, "verify_git_identity", return_value=identity),
                mock.patch.object(
                    verifier,
                    "workspace_release_contract",
                    return_value=("1.88.0", "1.97.1"),
                ),
                mock.patch.object(
                    verifier,
                    "package_evidence",
                    return_value=fixture["packages"],
                ),
                mock.patch.object(
                    verifier,
                    "documented_feature_profile_evidence",
                    return_value=fixture["documented_feature_profiles"],
                ),
                mock.patch.object(
                    verifier,
                    "sha256_git_blob",
                    return_value="c" * 64,
                ),
                mock.patch.object(
                    verifier,
                    "protocol_sdk_evidence",
                    return_value=protocol_sdk,
                ),
                mock.patch.object(
                    verifier,
                    "release_metadata",
                    return_value=github_release,
                ),
                mock.patch.object(verifier, "run_text", return_value="{}") as run_text,
            ):
                self.assertEqual(verifier.main(), 0)

            metadata_command = run_text.call_args.args[0]
            self.assertIn("--locked", metadata_command)
            payload = json.loads(evidence.read_text(encoding="utf-8"))
            self.assertEqual(payload["msrv"], "1.88.0")
            self.assertEqual(payload["release_toolchain"], "1.97.1")
            self.assertEqual(payload["dist_artifacts"], sorted(valid_dist_plan()["artifacts"]))
            self.assertEqual(
                payload["protocol_sdk"]["artifact_name"],
                "unity-asset-search-protocol-sdk-v1.2.3.zip",
            )
            self.assertEqual(payload["github_release"]["title"], "v1.2.3")
            self.assertEqual(
                payload["documented_feature_profiles"][0]["features"],
                ["audio", "texture-advanced"],
            )
            outputs = github_output.read_text(encoding="utf-8")
            self.assertIn("msrv=1.88.0", outputs)
            self.assertIn("release_toolchain=1.97.1", outputs)
            self.assertIn("dist_plan_sha256=", outputs)
            self.assertIn(
                "protocol_sdk_artifact=unity-asset-search-protocol-sdk-v1.2.3.zip",
                outputs,
            )
            self.assertIn('dist_matrix={"include":[', outputs)


if __name__ == "__main__":
    unittest.main()
