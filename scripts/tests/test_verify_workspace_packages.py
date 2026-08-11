"""Regression tests for the isolated workspace package verifier."""

from __future__ import annotations

import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_ROOT = REPOSITORY_ROOT / "scripts"
sys.path.insert(0, str(SCRIPTS_ROOT))

import workspace_package_contract as contract
import workspace_package_verification as verifier
import verify_workspace_packages as entrypoint


class PackageVerifierRejectionTests(unittest.TestCase):
    def expected_package(self, root: Path) -> object:
        return contract.WorkspacePackage(
            name="example-package",
            version="1.2.3",
            manifest_path=root / "source" / "Cargo.toml",
            dependencies=(),
            publish=None,
            is_library=False,
            feature_names=(),
        )

    def test_derives_a_dependency_first_closure_for_every_publishable_package(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            core_directory = root / "core"
            app_directory = root / "app"
            support_directory = root / "support"
            for directory in (core_directory, app_directory, support_directory):
                directory.mkdir()

            core = contract.WorkspacePackage(
                name="core",
                version="1.0.0",
                manifest_path=core_directory / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=(),
            )
            app = contract.WorkspacePackage(
                name="app",
                version="1.0.0",
                manifest_path=app_directory / "Cargo.toml",
                dependencies=(
                    {
                        "name": "core",
                        "source": None,
                        "path": str(core_directory),
                    },
                ),
                publish=None,
                is_library=False,
                feature_names=(),
            )
            support = contract.WorkspacePackage(
                name="support",
                version="1.0.0",
                manifest_path=support_directory / "Cargo.toml",
                dependencies=(),
                publish=[],
                is_library=False,
                feature_names=(),
            )

            closure = contract.production_closure(
                ["app"],
                {package.name: package for package in (app, core, support)}
            )

            self.assertEqual([package.name for package in closure], ["core", "app"])

    def test_rejects_a_release_package_removed_with_publish_false(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            packages = {
                name: contract.WorkspacePackage(
                    name=name,
                    version="1.0.0",
                    manifest_path=root / name / "Cargo.toml",
                    dependencies=(),
                    publish=[] if name == "app" else None,
                    is_library=False,
                    feature_names=(),
                )
                for name in ("core", "app")
            }
            with mock.patch.object(
                contract, "PUBLISHABLE_PACKAGE_NAMES", ("core", "app")
            ):
                with self.assertRaisesRegex(
                    contract.VerificationError, "missing expected packages: app"
                ):
                    contract.published_production_closure(packages)

    def test_consumer_loads_the_exact_reviewed_public_api_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            fixture_root = Path(temporary)
            fixture = fixture_root / "renamed-library-package" / "default.rs"
            fixture.parent.mkdir()
            fixture.write_text(
                "pub use custom_public_api::PromisedType;\n", encoding="utf-8"
            )
            package = contract.WorkspacePackage(
                name="renamed-library-package",
                version="1.0.0",
                manifest_path=Path("Cargo.toml"),
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=(),
                library_target_name="custom_public_api",
            )

            source = verifier.consumer_source(
                package,
                "default",
                fixture_root=fixture_root,
            )

            self.assertEqual(source, "pub use custom_public_api::PromisedType;\n")

    def test_consumer_rejects_a_missing_public_api_fixture(self) -> None:
        package = contract.WorkspacePackage(
            name="renamed-library-package",
            version="1.0.0",
            manifest_path=Path("Cargo.toml"),
            dependencies=(),
            publish=None,
            is_library=True,
            feature_names=(),
            library_target_name="custom_public_api",
        )
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                contract.VerificationError, "missing public API consumer fixture"
            ):
                verifier.consumer_source(
                    package,
                    "default",
                    fixture_root=Path(temporary),
                )

    def test_documented_readme_feature_profile_is_explicit_and_validated(self) -> None:
        decode = contract.WorkspacePackage(
            name="unity-asset-decode",
            version="0.4.0",
            manifest_path=Path("decode") / "Cargo.toml",
            dependencies=(),
            publish=None,
            is_library=True,
            feature_names=("audio", "texture", "texture-advanced"),
            library_target_name="unity_asset_decode",
        )
        workspace = contract.WorkspacePackage(
            name="unity-asset",
            version="0.4.0",
            manifest_path=Path("workspace") / "Cargo.toml",
            dependencies=(),
            publish=None,
            is_library=True,
            feature_names=("async", "decode", "mmap"),
            library_target_name="unity_asset",
        )

        profiles = contract.validate_documented_feature_profiles(
            {package.name: package for package in (decode, workspace)}
        )

        self.assertEqual(
            [
                (
                    profile.name,
                    profile.package,
                    profile.features,
                    profile.default_features,
                    profile.target_kind,
                    profile.target_name,
                )
                for profile in profiles
            ],
            [
                (
                    "readme-decode-media",
                    "unity-asset-decode",
                    ("audio", "texture-advanced"),
                    True,
                    "dependency",
                    None,
                ),
                (
                    "workspace-decode",
                    "unity-asset",
                    ("decode",),
                    True,
                    "dependency",
                    None,
                ),
            ],
        )

    def test_documented_feature_consumer_uses_only_the_promised_features(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = contract.WorkspacePackage(
                name="unity-asset-decode",
                version="0.4.0",
                manifest_path=root / "source" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=("audio", "texture", "texture-advanced"),
                library_target_name="unity_asset_decode",
            )
            profile = next(
                profile
                for profile in contract.DOCUMENTED_FEATURE_PROFILES
                if profile.name == "readme-decode-media"
            )

            _, manifest_path = verifier.create_consumer(
                root / "consumer",
                package,
                profile.name,
                profile.features,
                default_features=profile.default_features,
            )

            manifest = manifest_path.read_text(encoding="utf-8")
            self.assertIn('default-features = true', manifest)
            self.assertIn('features = ["audio", "texture-advanced"]', manifest)
            self.assertNotIn('"texture"', manifest)

    def test_workspace_decode_consumer_enables_only_the_documented_feature(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = contract.WorkspacePackage(
                name="unity-asset",
                version="0.4.0",
                manifest_path=root / "source" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=("async", "decode", "mmap"),
                library_target_name="unity_asset",
            )
            profile = next(
                profile
                for profile in contract.DOCUMENTED_FEATURE_PROFILES
                if profile.name == "workspace-decode"
            )
            _, manifest_path = verifier.create_consumer(
                root / "consumer",
                package,
                profile.name,
                profile.features,
                default_features=profile.default_features,
            )

            manifest = manifest_path.read_text(encoding="utf-8")
            self.assertIn('features = ["decode"]', manifest)
            self.assertNotIn('"async"', manifest)
            self.assertNotIn('"mmap"', manifest)

    def test_consumer_suite_batches_positive_and_removed_api_probes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            decode = contract.WorkspacePackage(
                name="unity-asset-decode",
                version="0.4.0",
                manifest_path=root / "source" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=("audio", "texture", "texture-advanced", "full"),
                library_target_name="unity_asset_decode",
            )
            workspace_package = contract.WorkspacePackage(
                name="unity-asset",
                version="0.4.0",
                manifest_path=root / "workspace" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=("async", "decode", "mmap"),
                library_target_name="unity_asset",
            )
            packages = (decode, workspace_package)
            unpacked = {
                package.name: root / "unpacked" / package.name
                for package in packages
            }

            workspace, consumers, positive, removed, required = (
                verifier.create_consumer_suite(
                    root / "consumers", packages, unpacked
                )
            )

            self.assertTrue(workspace.is_file())
            self.assertEqual(len(positive), 4)
            self.assertEqual(
                len(consumers), 4 + len(verifier.REMOVED_DECODE_API_PATHS)
            )
            self.assertTrue(removed.isdisjoint(positive))
            self.assertTrue(removed.issubset(consumers))
            self.assertEqual(len(removed), len(verifier.REMOVED_DECODE_API_PATHS))
            self.assertEqual(required, {"unity-asset-decode", "unity-asset"})
            self.assertEqual(
                {
                    (
                        consumers[name].parent / "src" / "lib.rs"
                    ).read_text(encoding="utf-8")
                    for name in removed
                },
                {
                    f"use unity_asset_decode::{path};\n"
                    for path in verifier.REMOVED_DECODE_API_PATHS
                },
            )
            for name in removed:
                removed_manifest = consumers[name].read_text(encoding="utf-8")
                self.assertIn('default-features = false', removed_manifest)
                self.assertIn('features = ["full"]', removed_manifest)

    def test_removed_api_probe_accepts_any_compile_failure(self) -> None:
        result = mock.Mock(
            returncode=101,
            stdout="",
            stderr="compile failed",
        )

        with mock.patch.object(verifier.subprocess, "run", return_value=result) as run:
            verifier.check_removed_api_consumers(
                cargo="cargo",
                cargo_cwd=Path("clean-cwd"),
                environment={"CARGO_HOME": "isolated"},
                workspace_manifest=Path("consumer-workspace") / "Cargo.toml",
                consumer_names=("removed-decode",),
            )

        self.assertEqual(
            run.call_args.args[0],
            [
                "cargo",
                "check",
                "--manifest-path",
                str(Path("consumer-workspace") / "Cargo.toml"),
                "--package",
                "removed-decode",
                "--lib",
                "--locked",
            ],
        )
        self.assertEqual(
            run.call_args.kwargs,
            {
                "cwd": Path("clean-cwd"),
                "env": {"CARGO_HOME": "isolated"},
                "check": False,
                "capture_output": True,
                "text": True,
                "timeout": verifier.CARGO_COMMAND_TIMEOUT_SECONDS,
            },
        )

    def test_removed_api_probe_rejects_a_reintroduced_symbol(self) -> None:
        result = mock.Mock(returncode=0, stdout="", stderr="")
        with (
            mock.patch.object(verifier.subprocess, "run", return_value=result),
            self.assertRaisesRegex(
                contract.VerificationError,
                "unexpectedly compiled.*removed-decode",
            ),
        ):
            verifier.check_removed_api_consumers(
                cargo="cargo",
                cargo_cwd=Path("clean-cwd"),
                environment={},
                workspace_manifest=Path("consumer-workspace") / "Cargo.toml",
                consumer_names=("removed-decode",),
            )

    def test_package_mode_is_the_safe_local_default(self) -> None:
        with mock.patch.object(sys, "argv", ["verify_workspace_packages.py"]):
            args = entrypoint.parse_args()

        self.assertEqual(args.mode, "packages")
        self.assertFalse(hasattr(args, "workspace_root"))

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
                with self.assertRaisesRegex(contract.VerificationError, message):
                    contract.reject_root_source_overrides(document, Path("Cargo.toml"))

    def test_rejects_unsafe_archive_members(self) -> None:
        expected_root = "example-package-1.2.3"
        cases = {
            "backslash": (f"{expected_root}\\escape", tarfile.REGTYPE),
            "parent": (f"{expected_root}/../escape", tarfile.REGTYPE),
            "link": (f"{expected_root}/escape", tarfile.SYMTYPE),
            "reserved": (f"{expected_root}/CON.txt", tarfile.REGTYPE),
            "colon": (f"{expected_root}/bad:name", tarfile.REGTYPE),
            "trailing-dot": (f"{expected_root}/bad.", tarfile.REGTYPE),
        }

        for name, (member_name, member_type) in cases.items():
            with self.subTest(name=name):
                member = tarfile.TarInfo(member_name)
                member.type = member_type
                with self.assertRaises(contract.VerificationError):
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
                    with self.assertRaisesRegex(contract.VerificationError, message):
                        verifier.validate_packaged_manifest(root, expected)

    def test_package_vcs_info_binds_archive_to_source_commit(self) -> None:
        source_commit = "a" * 40
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            vcs_info = root / ".cargo_vcs_info.json"
            vcs_info.write_text(
                json.dumps({"git": {"sha1": source_commit}}),
                encoding="utf-8",
            )

            verifier.validate_package_vcs_info(
                root,
                expected_source_commit=source_commit,
            )

            cases = {
                "mismatch": (
                    {"git": {"sha1": "b" * 40}},
                    "does not match source commit",
                ),
                "dirty": (
                    {"git": {"sha1": source_commit, "dirty": True}},
                    "marks the package dirty",
                ),
                "invalid_dirty": (
                    {"git": {"sha1": source_commit, "dirty": "true"}},
                    "invalid git.dirty",
                ),
            }
            for name, (document, message) in cases.items():
                with self.subTest(name=name):
                    vcs_info.write_text(json.dumps(document), encoding="utf-8")
                    with self.assertRaisesRegex(contract.VerificationError, message):
                        verifier.validate_package_vcs_info(
                            root,
                            expected_source_commit=source_commit,
                        )

    def test_package_vcs_info_is_required(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                contract.VerificationError, "missing package VCS identity"
            ):
                verifier.validate_package_vcs_info(
                    Path(temporary),
                    expected_source_commit="a" * 40,
                )

    def test_rejects_resolved_graph_that_leaks_the_repository_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace_root = root / "repository"
            consumer_manifest = root / "consumer" / "Cargo.toml"
            unpacked_target = root / "unpacked" / "unity-asset-search-index-1.2.3"
            registry_source_root = root / "cargo-home" / "registry" / "src"
            target_name = "unity-asset-search-index"
            target_id = f"{target_name} 1.2.3 (path+temporary)"
            consumer_name = "test-package-consumer"
            consumer_id = f"{consumer_name} 0.0.0 (path+temporary)"
            metadata = {
                "packages": [
                    {
                        "id": consumer_id,
                        "name": consumer_name,
                        "version": "0.0.0",
                        "source": None,
                        "manifest_path": str(consumer_manifest),
                    },
                    {
                        "id": target_id,
                        "name": target_name,
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
                contract.VerificationError, "did not resolve from its unpacked archive"
            ):
                verifier.validate_resolved_workspace(
                    json.dumps(metadata),
                    {consumer_name: consumer_manifest},
                    {target_name: unpacked_target},
                    {target_name: "1.2.3"},
                    registry_source_root,
                    {target_name},
                )

    def test_rejects_workspace_graph_missing_a_required_unpacked_package(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            consumer_name = "test-package-consumer"
            consumer_manifest = root / "consumer" / "Cargo.toml"
            target_name = "example-package"
            consumer_id = f"{consumer_name} 0.0.0 (path+temporary)"
            metadata = {
                "packages": [
                    {
                        "id": consumer_id,
                        "name": consumer_name,
                        "version": "0.0.0",
                        "source": None,
                        "manifest_path": str(consumer_manifest),
                    },
                ],
                "resolve": {"root": None, "nodes": [{"id": consumer_id, "deps": []}]},
            }

            with self.assertRaisesRegex(
                contract.VerificationError, "required unpacked packages"
            ):
                verifier.validate_resolved_workspace(
                    json.dumps(metadata),
                    {consumer_name: consumer_manifest},
                    {target_name: root / "unpacked" / target_name},
                    {target_name: "1.0.0"},
                    root / "cargo-home" / "registry" / "src",
                    {target_name},
                )

    def test_checks_each_consumer_package_independently(self) -> None:
        cargo_cwd = Path("clean-cargo-cwd")
        manifest = Path("consumer-workspace") / "Cargo.toml"
        environment = {"CARGO_HOME": "isolated"}

        with mock.patch.object(verifier, "run_visible") as run_visible:
            verifier.check_consumer_packages(
                cargo="cargo",
                cargo_cwd=cargo_cwd,
                environment=environment,
                workspace_manifest=manifest,
                consumer_names=("consumer-b", "consumer-a"),
            )

        self.assertEqual(run_visible.call_count, 2)
        commands = [call.args[0] for call in run_visible.call_args_list]
        self.assertEqual(
            commands,
            [
                [
                    "cargo",
                    "check",
                    "--manifest-path",
                    str(manifest),
                    "--package",
                    "consumer-a",
                    "--lib",
                    "--locked",
                ],
                [
                    "cargo",
                    "check",
                    "--manifest-path",
                    str(manifest),
                    "--package",
                    "consumer-b",
                    "--lib",
                    "--locked",
                ],
            ],
        )
        for call in run_visible.call_args_list:
            self.assertEqual(call.kwargs, {"cwd": cargo_cwd, "env": environment})

    def test_metadata_proves_the_isolated_resolve_graph(self) -> None:
        cargo_cwd = Path("clean-cargo-cwd")
        manifest = Path("consumer-workspace") / "Cargo.toml"
        environment = {"CARGO_HOME": "isolated"}
        metadata = '{"packages": [], "resolve": {"nodes": []}}'

        with (
            mock.patch.object(verifier, "run_captured", return_value=metadata) as run,
            mock.patch.object(
                verifier, "validate_resolved_workspace"
            ) as validate_resolved,
            mock.patch.object(verifier, "run_visible") as run_visible,
        ):
            verifier.verify_temporary_workspace(
                cargo="cargo",
                cargo_cwd=cargo_cwd,
                environment=environment,
                workspace_manifest=manifest,
                local_manifests={},
                unpacked_packages={},
                expected_versions={},
                required_internal=set(),
                registry_source_root=Path("registry"),
                all_features=True,
            )

        run_visible.assert_not_called()
        run.assert_called_once_with(
            [
                "cargo",
                "metadata",
                "--manifest-path",
                str(manifest),
                "--format-version",
                "1",
                "--all-features",
            ],
            cwd=cargo_cwd,
            env=environment,
        )
        validate_resolved.assert_called_once()

    def test_binary_verification_uses_archives_and_rejects_wrong_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            standalone_root = root / "standalone"
            dependency = contract.WorkspacePackage(
                name="example-core",
                version="1.2.3",
                manifest_path=root / "source-core" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=(),
            )
            package = contract.WorkspacePackage(
                name="example-package",
                version="1.2.3",
                manifest_path=root / "source" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=False,
                feature_names=(),
                binary_target_names=("example-tool",),
            )
            packages = {item.name: item for item in (dependency, package)}
            archives = {
                name: root / "archives" / f"{name}-1.2.3.crate" for name in packages
            }
            unpacked = {
                name: standalone_root / "packages" / f"{name}-1.2.3"
                for name in packages
            }
            executable = standalone_root / "install-root" / "bin" / (
                "example-tool.exe" if sys.platform == "win32" else "example-tool"
            )
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"test binary")

            def unpack(
                _archive: Path,
                _root: Path,
                item: contract.WorkspacePackage,
            ) -> Path:
                return unpacked[item.name]

            with (
                mock.patch.object(
                    verifier, "unpack_archive", side_effect=unpack
                ) as unpack_archive,
                mock.patch.object(
                    verifier,
                    "write_workspace_manifest",
                    return_value=standalone_root / "Cargo.toml",
                ),
                mock.patch.object(
                    verifier,
                    "production_closure",
                    return_value=(dependency, package),
                ),
                mock.patch.object(verifier, "verify_temporary_workspace") as verify,
                mock.patch.object(verifier, "run_visible") as install,
                mock.patch.object(
                    verifier, "run_captured", return_value="wrong identity\n"
                ),
            ):
                with self.assertRaisesRegex(
                    contract.VerificationError, "unexpected build identity"
                ):
                    verifier.verify_binary_packages_standalone(
                        cargo="cargo",
                        cargo_cwd=root,
                        environment={"CARGO_HOME": "isolated"},
                        workspace_root=standalone_root,
                        packages=packages,
                        archive_paths=archives,
                        expected_versions={name: "1.2.3" for name in packages},
                        expected_source_commit="a" * 40,
                        expected_build_target="x86_64-pc-windows-msvc",
                        registry_source_root=root / "cargo-home" / "registry" / "src",
                    )

            self.assertEqual(
                {call.args[0] for call in unpack_archive.call_args_list},
                set(archives.values()),
            )
            self.assertEqual(
                set(verify.call_args.kwargs["unpacked_packages"].values()),
                set(unpacked.values()),
            )
            install_path = install.call_args.args[0]
            self.assertEqual(
                install_path[install_path.index("--path") + 1],
                str(unpacked[package.name]),
            )


if __name__ == "__main__":
    unittest.main()
