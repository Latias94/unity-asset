from __future__ import annotations

import hashlib
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_ROOT = REPOSITORY_ROOT / "scripts"
sys.path.insert(0, str(SCRIPTS_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from protocol_sdk_bundle import build_protocol_sdk_bundle  # noqa: E402
from release_evidence import canonical_json_bytes  # noqa: E402
from release_evidence_support import make_release_evidence  # noqa: E402
from verify_release_bundle import ReleaseBundleError, verify_release_bundle  # noqa: E402


TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
)
APPLICATIONS = ("unity-asset-search-cli", "unity-asset-search-daemon")


def build_plan() -> dict[str, object]:
    artifacts: dict[str, object] = {}
    releases: list[dict[str, object]] = []
    for application in APPLICATIONS:
        names: list[str] = []
        for target in TARGETS:
            extension = ".zip" if target.endswith("windows-msvc") else ".tar.xz"
            archive = f"{application}-{target}{extension}"
            checksum = f"{archive}.sha256"
            artifacts[archive] = {
                "name": archive,
                "kind": "executable-zip",
                "target_triples": [target],
                "checksum": checksum,
            }
            artifacts[checksum] = {
                "name": checksum,
                "kind": "checksum",
                "target_triples": [target],
            }
            names.extend((archive, checksum))
        releases.append(
            {"app_name": application, "app_version": "0.4.0", "artifacts": names}
        )
    return {
        "dist_version": "0.30.3",
        "announcement_tag": "v0.4.0",
        "announcement_tag_is_implicit": False,
        "announcement_is_prerelease": False,
        "artifacts": artifacts,
        "releases": releases,
    }


class ReleaseBundleTests(unittest.TestCase):
    def make_bundle(self) -> tuple[Path, str, Path, Path]:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-release-bundle-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        proof = Path(tempfile.mkdtemp(prefix="unity-asset-release-proof-"))
        self.addCleanup(shutil.rmtree, proof, ignore_errors=True)
        plan = build_plan()
        plan_path = root / "release-dist-plan.json"
        plan_path.write_text(json.dumps(plan, sort_keys=True), encoding="utf-8")
        for name in sorted(plan["artifacts"]):
            path = root / name
            if name.endswith(".sha256"):
                archive_name = name.removesuffix(".sha256")
                path.write_text(
                    f"{hashlib.sha256((root / archive_name).read_bytes()).hexdigest()}  {archive_name}\n",
                    encoding="ascii",
                )
            else:
                path.write_bytes(name.encode("ascii"))
        protocol = build_protocol_sdk_bundle(REPOSITORY_ROOT, root / "protocol", "v0.4.0")
        shutil.move(root / "protocol" / protocol.artifact_name, root / protocol.artifact_name)
        (root / "protocol").rmdir()
        title = proof / "release-title.txt"
        body = proof / "release-notes.md"
        title.write_text("v0.4.0\n", encoding="utf-8", newline="\n")
        body.write_text("Release notes.\n", encoding="utf-8", newline="\n")
        evidence = make_release_evidence(
            tag="v0.4.0",
            version="0.4.0",
            dist_plan_sha256=hashlib.sha256(plan_path.read_bytes()).hexdigest(),
            dist_artifacts=sorted(plan["artifacts"]),
            protocol_sdk=protocol.as_dict(),
        )
        evidence_path = root / "release-evidence.json"
        evidence_path.write_bytes(canonical_json_bytes(evidence))
        checksummed = sorted(path.name for path in root.iterdir())
        (root / "SHA256SUMS").write_text(
            "".join(
                f"{hashlib.sha256((root / name).read_bytes()).hexdigest()}  {name}\n"
                for name in checksummed
            ),
            encoding="ascii",
        )
        return root, hashlib.sha256(evidence_path.read_bytes()).hexdigest(), title, body

    def test_accepts_a_complete_evidence_bound_bundle(self) -> None:
        root, evidence_digest, title, body = self.make_bundle()
        verify_release_bundle(
            root,
            "v0.4.0",
            evidence_digest,
            release_title=title,
            release_body=body,
        )

    def test_rejects_a_tampered_archive_even_if_the_global_manifest_is_unchanged(self) -> None:
        root, evidence_digest, title, body = self.make_bundle()
        archive = next(
            path
            for path in root.iterdir()
            if path.name.startswith("unity-asset-search-cli-")
            and not path.name.endswith(".sha256")
        )
        archive.write_bytes(b"tampered")
        with self.assertRaisesRegex(ReleaseBundleError, "sidecar|digest"):
            verify_release_bundle(
                root,
                "v0.4.0",
                evidence_digest,
                release_title=title,
                release_body=body,
            )

    def test_rejects_tampered_title_or_body_proof(self) -> None:
        root, evidence_digest, title, body = self.make_bundle()
        body.write_text("Different notes.\n", encoding="utf-8")
        with self.assertRaisesRegex(ReleaseBundleError, "does not match release evidence"):
            verify_release_bundle(
                root,
                "v0.4.0",
                evidence_digest,
                release_title=title,
                release_body=body,
            )


if __name__ == "__main__":
    unittest.main()
