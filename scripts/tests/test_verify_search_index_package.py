"""Regression tests for the isolated search-index package verifier."""

from __future__ import annotations

import importlib.util
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
VERIFIER_PATH = REPOSITORY_ROOT / "scripts" / "verify_search_index_package.py"
SPEC = importlib.util.spec_from_file_location("search_index_package_verifier", VERIFIER_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load package verifier from {VERIFIER_PATH}")
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)


class PackageVerifierRejectionTests(unittest.TestCase):
    def expected_package(self, root: Path) -> object:
        return verifier.WorkspacePackage(
            name="example-package",
            version="1.2.3",
            manifest_path=root / "source" / "Cargo.toml",
            dependencies=(),
            publish=None,
        )

    def test_rejects_root_source_overrides(self) -> None:
        cases = {
            "patch": (
                {"patch": {"crates-io": {"globset": {"path": "vendor/globset"}}}},
                "forbidden",
            ),
            "replace": (
                {"replace": {"globset:0.4.0": {"path": "vendor/globset"}}},
                "override",
            ),
        }

        for name, (document, message) in cases.items():
            with self.subTest(name=name):
                with self.assertRaisesRegex(verifier.VerificationError, message):
                    verifier.reject_root_source_overrides(document, Path("Cargo.toml"))

    def test_rejects_unsafe_archive_members(self) -> None:
        expected_root = "example-package-1.2.3"
        cases = {
            "backslash": (f"{expected_root}\\escape", tarfile.REGTYPE),
            "parent": (f"{expected_root}/../escape", tarfile.REGTYPE),
            "link": (f"{expected_root}/escape", tarfile.SYMTYPE),
        }

        for name, (member_name, member_type) in cases.items():
            with self.subTest(name=name):
                member = tarfile.TarInfo(member_name)
                member.type = member_type
                with self.assertRaises(verifier.VerificationError):
                    verifier.validate_archive_member(
                        Path("example-package-1.2.3.crate"), member, expected_root
                    )

    def test_rejects_non_normalized_manifest_dependencies(self) -> None:
        cases = {
            "workspace": (
                '[dependencies]\nexample = { workspace = true }\n',
                "still inherits from the workspace",
            ),
            "path": (
                '[dependencies]\nexample = { version = "1", path = "../example" }\n',
                "retains a repository path dependency",
            ),
            "git": (
                '[dependencies]\nexample = { version = "1", git = "https://example.invalid/repo" }\n',
                "retains a Git dependency",
            ),
            "ignore": ('[dependencies]\nignore = "0.4"\n', "forbidden package 'ignore'"),
        }

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest_path = root / "Cargo.toml"
            expected = self.expected_package(root)
            for name, (dependency, message) in cases.items():
                with self.subTest(name=name):
                    manifest_path.write_text(
                        "[package]\n"
                        'name = "example-package"\n'
                        'version = "1.2.3"\n\n'
                        f"{dependency}",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(verifier.VerificationError, message):
                        verifier.validate_packaged_manifest(root, expected)

    def test_rejects_untrusted_consumer_lock_entries(self) -> None:
        valid_checksum = "a" * 64
        cases = {
            "git_source": (
                'source = "git+https://example.invalid/repository"\n'
                f'checksum = "{valid_checksum}"\n',
                "non-crates.io source",
            ),
            "bad_checksum": (
                f'source = "{verifier.CRATES_IO_SOURCE}"\n'
                'checksum = "not-a-checksum"\n',
                "invalid checksum",
            ),
            "missing_checksum": (
                f'source = "{verifier.CRATES_IO_SOURCE}"\n',
                "invalid checksum",
            ),
        }

        with tempfile.TemporaryDirectory() as temporary:
            lock_path = Path(temporary) / "Cargo.lock"
            for name, (source_lines, message) in cases.items():
                with self.subTest(name=name):
                    lock_path.write_text(
                        "version = 4\n\n"
                        "[[package]]\n"
                        'name = "untrusted-package"\n'
                        'version = "1.0.0"\n'
                        f"{source_lines}",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(verifier.VerificationError, message):
                        verifier.validate_consumer_lock(lock_path, {})

    def test_rejects_resolved_graph_that_leaks_the_repository_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace_root = root / "repository"
            consumer_manifest = root / "consumer" / "Cargo.toml"
            unpacked_target = root / "unpacked" / "unity-asset-search-index-1.2.3"
            registry_source_root = root / "cargo-home" / "registry" / "src"
            target_id = "unity-asset-search-index 1.2.3 (path+temporary)"
            consumer_id = "unity-asset-search-index-package-consumer 0.0.0 (path+temporary)"
            metadata = {
                "packages": [
                    {
                        "id": consumer_id,
                        "name": verifier.CONSUMER_PACKAGE,
                        "version": "0.0.0",
                        "source": None,
                        "manifest_path": str(consumer_manifest),
                    },
                    {
                        "id": target_id,
                        "name": verifier.TARGET_PACKAGE,
                        "version": "1.2.3",
                        "source": None,
                        "manifest_path": str(workspace_root / "crates" / "index" / "Cargo.toml"),
                    },
                ],
                "resolve": {
                    "root": consumer_id,
                    "nodes": [
                        {"id": consumer_id, "deps": [{"pkg": target_id}]},
                        {"id": target_id, "deps": []},
                    ],
                },
            }

            with self.assertRaisesRegex(
                verifier.VerificationError, "leaked a repository checkout path"
            ):
                verifier.validate_resolved_graph(
                    json.dumps(metadata),
                    workspace_root,
                    consumer_manifest,
                    {verifier.TARGET_PACKAGE: unpacked_target},
                    {verifier.TARGET_PACKAGE: "1.2.3"},
                    set(),
                    registry_source_root,
                )


if __name__ == "__main__":
    unittest.main()
