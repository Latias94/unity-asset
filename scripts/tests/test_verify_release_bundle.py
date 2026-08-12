from __future__ import annotations

import hashlib
import io
import json
import shutil
import stat
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_ROOT = REPOSITORY_ROOT / "scripts"
sys.path.insert(0, str(SCRIPTS_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from protocol_sdk_bundle import build_protocol_sdk_bundle  # noqa: E402
from release_evidence import canonical_json_bytes  # noqa: E402
from release_evidence_support import make_dist_plan, make_release_evidence  # noqa: E402
from release_binary_identity import (  # noqa: E402
    ReleaseBinaryIdentityError,
    verify_release_binary_identity,
    version_report,
)
from release_contract import (  # noqa: E402
    DISTRIBUTED_APPLICATION_NAMES,
    DISTRIBUTION_TARGET_TRIPLES,
    distribution_archive_name,
    distribution_executable_name,
)
from verify_release_bundle import ReleaseBundleError, verify_release_bundle  # noqa: E402


TARGETS = DISTRIBUTION_TARGET_TRIPLES
APPLICATIONS = DISTRIBUTED_APPLICATION_NAMES
SOURCE_COMMIT = "2" * 40
VERSION = "0.4.0"


def write_executable_archive(
    path: Path,
    application: str,
    target: str,
    source_commit: str = SOURCE_COMMIT,
    *,
    mode: int = 0o755,
    payload: bytes | None = None,
) -> None:
    executable = distribution_executable_name(application, target)
    contents = payload
    if contents is None:
        contents = (
            b"binary-prefix\0"
            + version_report(application, VERSION, source_commit, target).encode(
                "ascii"
            )
            + b"\0binary-suffix"
        )
    if path.suffix == ".zip":
        with zipfile.ZipFile(path, mode="w", compression=zipfile.ZIP_DEFLATED) as bundle:
            info = zipfile.ZipInfo(executable)
            info.compress_type = zipfile.ZIP_DEFLATED
            if not target.endswith("windows-msvc"):
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | mode) << 16
            bundle.writestr(info, contents)
        return
    member = tarfile.TarInfo(executable)
    member.size = len(contents)
    member.mode = mode
    with tarfile.open(path, mode="w:xz") as bundle:
        bundle.addfile(member, io.BytesIO(contents))

class ReleaseBundleTests(unittest.TestCase):
    def make_bundle(self) -> tuple[Path, str, Path, Path]:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-release-bundle-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        proof = Path(tempfile.mkdtemp(prefix="unity-asset-release-proof-"))
        self.addCleanup(shutil.rmtree, proof, ignore_errors=True)
        plan = make_dist_plan(tag="v0.4.0", version="0.4.0")
        plan_path = root / "release-dist-plan.json"
        plan_path.write_text(json.dumps(plan, sort_keys=True), encoding="utf-8")
        for application in APPLICATIONS:
            for target in TARGETS:
                write_executable_archive(
                    root / distribution_archive_name(application, target),
                    application,
                    target,
                )
        for name in sorted(plan["artifacts"]):
            if not name.endswith(".sha256"):
                continue
            archive_name = name.removesuffix(".sha256")
            (root / name).write_text(
                f"{hashlib.sha256((root / archive_name).read_bytes()).hexdigest()}  {archive_name}\n",
                encoding="ascii",
            )
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

    def test_rejects_an_archive_built_from_another_source_commit(self) -> None:
        root, evidence_digest, title, body = self.make_bundle()
        archive = root / "unity-asset-search-cli-x86_64-pc-windows-msvc.zip"
        write_executable_archive(
            archive,
            "unity-asset-search-cli",
            "x86_64-pc-windows-msvc",
            "3" * 40,
        )
        sidecar = root / f"{archive.name}.sha256"
        sidecar.write_text(
            f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n",
            encoding="ascii",
        )
        checksummed = sorted(
            path.name for path in root.iterdir() if path.name != "SHA256SUMS"
        )
        (root / "SHA256SUMS").write_text(
            "".join(
                f"{hashlib.sha256((root / name).read_bytes()).hexdigest()}  {name}\n"
                for name in checksummed
            ),
            encoding="ascii",
        )

        with self.assertRaisesRegex(ReleaseBundleError, "build identity"):
            verify_release_bundle(
                root,
                "v0.4.0",
                evidence_digest,
                release_title=title,
                release_body=body,
            )

    def test_build_identity_uses_closed_field_boundaries(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-binary-boundary-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        application = "unity-asset-search-cli"
        target = "x86_64-pc-windows-msvc"
        archive = root / f"{application}-{target}.zip"
        cases = (
            ("version-prefix", application, "10.4.0", SOURCE_COMMIT, target),
            (
                "package-suffix",
                f"{application}-helper",
                VERSION,
                SOURCE_COMMIT,
                target,
            ),
            (
                "target-abi",
                application,
                VERSION,
                SOURCE_COMMIT,
                "x86_64-unknown-linux-gnu",
            ),
        )
        for label, actual_package, actual_version, actual_commit, actual_target in cases:
            with self.subTest(label=label):
                payload = (
                    b"binary-prefix\0"
                    + version_report(
                        actual_package,
                        actual_version,
                        actual_commit,
                        actual_target,
                    ).encode("ascii")
                )
                write_executable_archive(
                    archive,
                    application,
                    target,
                    payload=payload,
                )
                with self.assertRaisesRegex(
                    ReleaseBinaryIdentityError, "build identity"
                ):
                    verify_release_binary_identity(
                        archive,
                        application=application,
                        target=target,
                        version=VERSION,
                        source_commit=SOURCE_COMMIT,
                    )

    def test_rejects_portable_member_aliases_in_zip_and_tar(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-binary-alias-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        application = "unity-asset-search-daemon"

        windows_target = "x86_64-pc-windows-msvc"
        zip_archive = root / f"{application}-{windows_target}.zip"
        write_executable_archive(zip_archive, application, windows_target)
        with zipfile.ZipFile(
            zip_archive, mode="a", compression=zipfile.ZIP_DEFLATED
        ) as bundle:
            bundle.writestr(f"{application.upper()}.EXE", b"replacement")
        with self.assertRaisesRegex(ReleaseBinaryIdentityError, "portable path alias"):
            verify_release_binary_identity(
                zip_archive,
                application=application,
                target=windows_target,
                version=VERSION,
                source_commit=SOURCE_COMMIT,
            )

        mac_target = "aarch64-apple-darwin"
        tar_archive = root / f"{application}-{mac_target}.tar.xz"
        payload = (
            b"binary-prefix\0"
            + version_report(
                application, VERSION, SOURCE_COMMIT, mac_target
            ).encode("ascii")
        )
        with tarfile.open(tar_archive, mode="w:xz") as bundle:
            for name, contents in (
                (application, payload),
                (application.upper(), b"replacement"),
            ):
                member = tarfile.TarInfo(name)
                member.size = len(contents)
                member.mode = 0o755
                bundle.addfile(member, io.BytesIO(contents))
        with self.assertRaisesRegex(ReleaseBinaryIdentityError, "portable path alias"):
            verify_release_binary_identity(
                tar_archive,
                application=application,
                target=mac_target,
                version=VERSION,
                source_commit=SOURCE_COMMIT,
            )

    def test_rejects_non_portable_extra_archive_members(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-binary-portability-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        application = "unity-asset-search-cli"
        target = "x86_64-pc-windows-msvc"
        archive = root / f"{application}-{target}.zip"
        write_executable_archive(archive, application, target)
        with zipfile.ZipFile(
            archive, mode="a", compression=zipfile.ZIP_DEFLATED
        ) as bundle:
            bundle.writestr("AUX.txt", b"not portable")
        with self.assertRaisesRegex(ReleaseBinaryIdentityError, "non-portable"):
            verify_release_binary_identity(
                archive,
                application=application,
                target=target,
                version=VERSION,
                source_commit=SOURCE_COMMIT,
            )

    def test_rejects_unix_archive_members_without_an_executable_mode(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-binary-mode-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        application = "unity-asset-search-daemon"
        for target in TARGETS:
            if target.endswith("windows-msvc"):
                continue
            with self.subTest(target=target):
                archive = root / f"{application}-{target}.tar.xz"
                write_executable_archive(
                    archive, application, target, mode=0o644
                )
                with self.assertRaisesRegex(
                    ReleaseBinaryIdentityError, "not marked executable"
                ):
                    verify_release_binary_identity(
                        archive,
                        application=application,
                        target=target,
                        version=VERSION,
                        source_commit=SOURCE_COMMIT,
                    )

    def test_rejects_corruption_after_an_early_build_identity_match(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-binary-crc-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        application = "unity-asset-search-cli"
        target = "x86_64-pc-windows-msvc"
        archive = root / f"{application}-{target}.zip"
        marker = b"corrupt-this-trailing-byte"
        payload = (
            b"binary-prefix\0"
            + version_report(application, VERSION, SOURCE_COMMIT, target).encode(
                "ascii"
            )
            + b"\0"
            + bytes(70 * 1024)
            + marker
        )
        with zipfile.ZipFile(archive, mode="w", compression=zipfile.ZIP_STORED) as bundle:
            bundle.writestr(f"{application}.exe", payload)
        encoded = bytearray(archive.read_bytes())
        marker_offset = encoded.index(marker)
        encoded[marker_offset] ^= 0x01
        archive.write_bytes(encoded)

        with self.assertRaisesRegex(ReleaseBinaryIdentityError, "cannot inspect"):
            verify_release_binary_identity(
                archive,
                application=application,
                target=target,
                version=VERSION,
                source_commit=SOURCE_COMMIT,
            )

    def test_rejects_an_unsupported_target_before_inspecting_its_bytes(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="unity-asset-binary-target-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        archive = root / "unity-asset-search-cli-i686-unknown-linux-gnu.tar.xz"
        archive.write_bytes(b"not an archive")
        with self.assertRaisesRegex(ReleaseBinaryIdentityError, "unsupported.*target"):
            verify_release_binary_identity(
                archive,
                application="unity-asset-search-cli",
                target="i686-unknown-linux-gnu",
                version=VERSION,
                source_commit=SOURCE_COMMIT,
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
