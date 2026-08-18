from __future__ import annotations

import hashlib
import importlib.util
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))
SCRIPT_PATH = SCRIPTS_ROOT / "assemble_release_assets.py"
SPEC = importlib.util.spec_from_file_location("assemble_release_assets", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
ASSEMBLER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ASSEMBLER
SPEC.loader.exec_module(ASSEMBLER)

from protocol_sdk_bundle import build_protocol_sdk_bundle  # noqa: E402
from release_evidence import canonical_json_bytes  # noqa: E402
from release_evidence_support import make_dist_plan, make_release_evidence  # noqa: E402


REPOSITORY_ROOT = SCRIPTS_ROOT.parent


def local_dist_plan() -> dict[str, object]:
    return make_dist_plan(tag="v0.4.0", version="0.4.0")


class ReleaseAssetAssemblyTests(unittest.TestCase):
    def make_fixture(self) -> tuple[Path, Path, str, Path, str, Path]:
        temporary = Path(tempfile.mkdtemp(prefix="unity-asset-release-assets-"))
        dist = temporary / "dist"
        dist.mkdir()
        plan = temporary / "release-dist-plan.json"
        plan.write_text(
            json.dumps(local_dist_plan(), sort_keys=True), encoding="utf-8", newline="\n"
        )
        plan_digest = hashlib.sha256(plan.read_bytes()).hexdigest()
        for index, name in enumerate(sorted(local_dist_plan()["artifacts"])):
            if name.endswith(".sha256"):
                continue
            directory = dist / ("windows" if name.endswith(".zip") else "unix")
            directory.mkdir(exist_ok=True)
            (directory / name).write_bytes(f"artifact-{index}".encode("ascii"))
        for name in sorted(local_dist_plan()["artifacts"]):
            if not name.endswith(".sha256"):
                continue
            archive_name = name.removesuffix(".sha256")
            archive = next(
                (
                    dist / directory / archive_name
                    for directory in ("windows", "unix")
                    if (dist / directory / archive_name).exists()
                ),
                None,
            )
            assert archive is not None
            directory = archive.parent
            (directory / name).write_text(
                f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive_name}\n",
                encoding="ascii",
                newline="\n",
            )
        protocol_directory = temporary / "protocol-sdk"
        protocol_metadata = build_protocol_sdk_bundle(
            REPOSITORY_ROOT,
            protocol_directory,
            "v0.4.0",
        )
        protocol_bundle = protocol_directory / protocol_metadata.artifact_name
        evidence = temporary / "release-evidence.json"
        evidence.write_bytes(
            canonical_json_bytes(
                make_release_evidence(
                    tag="v0.4.0",
                    version="0.4.0",
                    dist_plan_sha256=plan_digest,
                    dist_artifacts=sorted(local_dist_plan()["artifacts"]),
                    protocol_sdk=protocol_metadata.as_dict(),
                )
            )
        )
        digest = hashlib.sha256(evidence.read_bytes()).hexdigest()
        return temporary, evidence, digest, plan, plan_digest, protocol_bundle

    def assemble_fixture(self, output: Path) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        ASSEMBLER.assemble(
            temporary / "dist",
            evidence,
            digest,
            plan,
            plan_digest,
            protocol_bundle,
            output,
        )

    def test_flattens_exact_planned_artifacts_and_writes_complete_checksums(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        output = temporary / "assembled"
        ASSEMBLER.assemble(
            temporary / "dist",
            evidence,
            digest,
            plan,
            plan_digest,
            protocol_bundle,
            output,
        )

        expected_names = sorted(local_dist_plan()["artifacts"])
        self.assertEqual(
            sorted(path.name for path in output.iterdir()),
            sorted(
                [
                *expected_names,
                "SHA256SUMS",
                "release-dist-plan.json",
                "release-evidence.json",
                protocol_bundle.name,
                ]
            ),
        )
        checksummed_names = sorted(
            [
                *expected_names,
                "release-dist-plan.json",
                "release-evidence.json",
                protocol_bundle.name,
            ]
        )
        output_digests = {
            name: hashlib.sha256((output / name).read_bytes()).hexdigest()
            for name in checksummed_names
        }
        expected_checksums = "".join(
            f"{output_digests[name]}  {name}\n" for name in checksummed_names
        )
        self.assertEqual(
            (output / "SHA256SUMS").read_text(encoding="utf-8"), expected_checksums
        )
        self.assertEqual(
            (output / "release-evidence.json").read_bytes(), evidence.read_bytes()
        )
        self.assertEqual(
            (output / "release-dist-plan.json").read_bytes(), plan.read_bytes()
        )
        self.assertEqual(
            (output / protocol_bundle.name).read_bytes(), protocol_bundle.read_bytes()
        )
        for checksum_name in (
            name for name in expected_names if name.endswith(".sha256")
        ):
            archive_name = checksum_name.removesuffix(".sha256")
            self.assertEqual(
                (output / checksum_name).read_bytes(),
                f"{output_digests[archive_name]}  {archive_name}\n".encode("ascii"),
            )

    def test_accepts_and_canonicalizes_cargo_dist_sidecars(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        sidecars = sorted((temporary / "dist").rglob("*.sha256"))
        for sidecar in sidecars:
            archive_name = sidecar.name.removesuffix(".sha256")
            archive_digest = hashlib.sha256(
                sidecar.with_name(archive_name).read_bytes()
            ).hexdigest()
            sidecar.write_text(
                f"{archive_digest} *{archive_name}\n\n",
                encoding="ascii",
                newline="\n",
            )

        output = temporary / "assembled"
        ASSEMBLER.assemble(
            temporary / "dist",
            evidence,
            digest,
            plan,
            plan_digest,
            protocol_bundle,
            output,
        )

        for sidecar in sidecars:
            archive_name = sidecar.name.removesuffix(".sha256")
            archive_digest = hashlib.sha256((output / archive_name).read_bytes()).hexdigest()
            canonical_sidecar = f"{archive_digest}  {archive_name}\n".encode("ascii")
            self.assertEqual(
                (output / sidecar.name).read_bytes(),
                canonical_sidecar,
            )
            canonical_digest = hashlib.sha256(canonical_sidecar).hexdigest()
            self.assertIn(
                f"{canonical_digest}  {sidecar.name}\n",
                (output / "SHA256SUMS").read_text(encoding="ascii"),
            )

    def test_rejects_duplicate_flattened_names(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        name = next(iter(local_dist_plan()["artifacts"]))
        (temporary / "dist" / "collision").mkdir()
        (temporary / "dist" / "collision" / name).write_bytes(b"collision")
        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "collide"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                digest,
                plan,
                plan_digest,
                protocol_bundle,
                temporary / "out",
            )

    def test_rejects_an_omitted_planned_artifact(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        missing = next((temporary / "dist").rglob("*.zip"))
        missing.unlink()
        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "inventory differs"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                digest,
                plan,
                plan_digest,
                protocol_bundle,
                temporary / "out",
            )

    def test_rejects_evidence_or_plan_digest_mismatch(self) -> None:
        temporary, evidence, _, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "evidence SHA-256 does not match"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                "0" * 64,
                plan,
                plan_digest,
                protocol_bundle,
                temporary / "out",
            )
        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "dist plan SHA-256 mismatch"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                hashlib.sha256(evidence.read_bytes()).hexdigest(),
                plan,
                "0" * 64,
                protocol_bundle,
                temporary / "out",
            )

    def test_rejects_a_symlink_before_resolving_the_input(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        linked_dist = temporary / "linked-dist"
        try:
            linked_dist.symlink_to(temporary / "dist", target_is_directory=True)
        except OSError as error:
            self.skipTest(f"symlinks are unavailable: {error}")
        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "symlink or junction"):
            ASSEMBLER.assemble(
                linked_dist,
                evidence,
                digest,
                plan,
                plan_digest,
                protocol_bundle,
                temporary / "out",
            )

    def test_rejects_checksum_sidecar_with_a_wrong_digest(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        _, artifact_matrix = ASSEMBLER.load_dist_plan(
            plan,
            plan_digest,
            tag="v0.4.0",
            version="0.4.0",
        )
        last_checksum_name = artifact_matrix[-1].checksum_name
        sidecar = next(
            path
            for path in (temporary / "dist").rglob("*.sha256")
            if path.name == last_checksum_name
        )
        sidecar.write_text(
            f"{'0' * 64}  {sidecar.name.removesuffix('.sha256')}\n",
            encoding="ascii",
            newline="\n",
        )
        output = temporary / "out"

        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "checksum digest mismatch"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                digest,
                plan,
                plan_digest,
                protocol_bundle,
                output,
            )
        self.assertFalse(output.exists())

    def test_rejects_checksum_sidecar_with_a_noncanonical_filename(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        sidecar = next((temporary / "dist").rglob("*.sha256"))
        sidecar.write_text(
            f"{'0' * 64}  other-archive.tar.xz\n",
            encoding="ascii",
            newline="\n",
        )

        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "must contain one SHA-256 digest"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                digest,
                plan,
                plan_digest,
                protocol_bundle,
                temporary / "out",
            )

    def test_rejects_checksum_sidecar_with_multiple_lines(self) -> None:
        temporary, evidence, digest, plan, plan_digest, protocol_bundle = self.make_fixture()
        self.addCleanup(shutil.rmtree, temporary, ignore_errors=True)
        sidecar = next((temporary / "dist").rglob("*.sha256"))
        archive_name = sidecar.name.removesuffix(".sha256")
        archive = sidecar.with_name(archive_name)
        sidecar.write_text(
            f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive_name}\n"
            f"{'0' * 64}  other-archive.tar.xz\n",
            encoding="ascii",
            newline="\n",
        )

        with self.assertRaisesRegex(ASSEMBLER.AssemblyError, "must contain one SHA-256 digest"):
            ASSEMBLER.assemble(
                temporary / "dist",
                evidence,
                digest,
                plan,
                plan_digest,
                protocol_bundle,
                temporary / "out",
            )


if __name__ == "__main__":
    unittest.main()
