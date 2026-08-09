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

    def test_consumer_uses_the_metadata_library_target_name(self) -> None:
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
        self.assertIn("use custom_public_api as _;", verifier.consumer_source(package))

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
            example_target_names=("export_textures",),
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
                (
                    "export-textures-example",
                    "unity-asset-decode",
                    ("texture",),
                    True,
                    "example",
                    "export_textures",
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
            unpacked = {package.name: root / "unpacked" / package.name}
            profile = next(
                profile
                for profile in contract.DOCUMENTED_FEATURE_PROFILES
                if profile.name == "readme-decode-media"
            )

            workspace, consumers, required = (
                verifier.create_documented_feature_consumer_workspace(
                    root / "consumer-workspace",
                    (package,),
                    unpacked,
                    profile,
                )
            )

            self.assertTrue(workspace.is_file())
            self.assertEqual(required, {"unity-asset-decode"})
            manifest = next(iter(consumers.values())).read_text(encoding="utf-8")
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
            _, consumers, _ = verifier.create_documented_feature_consumer_workspace(
                root / "consumer-workspace",
                (package,),
                {package.name: root / "unpacked" / package.name},
                profile,
            )

            manifest = next(iter(consumers.values())).read_text(encoding="utf-8")
            self.assertIn('features = ["decode"]', manifest)
            self.assertNotIn('"async"', manifest)
            self.assertNotIn('"mmap"', manifest)

    def test_documented_example_uses_a_single_member_workspace_and_exact_flags(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dependency = contract.WorkspacePackage(
                name="unity-asset-binary",
                version="0.4.0",
                manifest_path=root / "binary" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=(),
                library_target_name="unity_asset_binary",
            )
            package = contract.WorkspacePackage(
                name="unity-asset-decode",
                version="0.4.0",
                manifest_path=root / "decode" / "Cargo.toml",
                dependencies=(
                    {
                        "name": dependency.name,
                        "source": None,
                        "path": str(dependency.directory),
                    },
                ),
                publish=None,
                is_library=True,
                feature_names=("audio", "texture", "texture-advanced"),
                library_target_name="unity_asset_decode",
                example_target_names=("export_textures",),
            )
            closure = (dependency, package)
            unpacked = {
                dependency.name: root / "unpacked" / dependency.name,
                package.name: root / "unpacked" / package.name,
            }
            profile = next(
                profile
                for profile in contract.DOCUMENTED_FEATURE_PROFILES
                if profile.name == "export-textures-example"
            )

            manifest, target, required = verifier.create_documented_example_workspace(
                root / "example-workspace",
                closure,
                unpacked,
                profile,
            )

            self.assertEqual(target, package)
            self.assertEqual(required, {dependency.name, package.name})
            workspace_text = manifest.read_text(encoding="utf-8")
            self.assertIn(
                verifier.toml_string(
                    verifier.relative_toml_path(unpacked[package.name], manifest.parent)
                ),
                workspace_text,
            )
            self.assertNotIn(
                verifier.toml_string(
                    verifier.relative_toml_path(
                        unpacked[dependency.name], manifest.parent
                    )
                ) + ",",
                workspace_text.split("[patch.crates-io]", 1)[0],
            )

            with mock.patch.object(verifier, "run_visible") as run_visible:
                verifier.check_documented_example(
                    cargo="cargo",
                    cargo_cwd=root,
                    environment={"CARGO_HOME": "isolated"},
                    workspace_manifest=manifest,
                    profile=profile,
                )

            self.assertEqual(
                run_visible.call_args.args[0],
                [
                    "cargo",
                    "check",
                    "--manifest-path",
                    str(manifest),
                    "--package",
                    "unity-asset-decode",
                    "--example",
                    "export_textures",
                    "--locked",
                    "--features",
                    "texture",
                ],
            )

    def test_documented_example_verification_only_reunpacks_the_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dependency = contract.WorkspacePackage(
                name="unity-asset-binary",
                version="0.4.0",
                manifest_path=root / "source" / "binary" / "Cargo.toml",
                dependencies=(),
                publish=None,
                is_library=True,
                feature_names=(),
                library_target_name="unity_asset_binary",
            )
            package = contract.WorkspacePackage(
                name="unity-asset-decode",
                version="0.4.0",
                manifest_path=root / "source" / "decode" / "Cargo.toml",
                dependencies=(
                    {
                        "name": dependency.name,
                        "source": None,
                        "path": str(dependency.directory),
                    },
                ),
                publish=None,
                is_library=True,
                feature_names=("texture",),
                library_target_name="unity_asset_decode",
                example_target_names=("export_textures",),
            )
            closure = (dependency, package)
            profile = next(
                profile
                for profile in contract.DOCUMENTED_FEATURE_PROFILES
                if profile.name == "export-textures-example"
            )
            workspace_root = root / "example-workspace"
            archive_paths = {
                dependency.name: root / "archives" / "unity-asset-binary-0.4.0.crate",
                package.name: root / "archives" / "unity-asset-decode-0.4.0.crate",
            }
            verified_paths = {
                dependency.name: root / "archive-workspace" / "unity-asset-binary-0.4.0",
                package.name: root / "archive-workspace" / "unity-asset-decode-0.4.0",
            }
            verification_arguments = {
                "cargo": "cargo",
                "cargo_cwd": root,
                "environment": {"CARGO_HOME": "isolated"},
                "repository_root": root / "repository",
                "workspace_root": workspace_root,
                "closure": closure,
                "profile": profile,
                "archive_paths": archive_paths,
                "expected_versions": {
                    dependency.name: dependency.version,
                    package.name: package.version,
                },
                "registry_source_root": root / "registry" / "src",
            }
            with self.assertRaisesRegex(
                verifier.VerificationError,
                f"no verified archive root for {dependency.name}",
            ):
                verifier.verify_documented_example_standalone(
                    **verification_arguments,
                    verified_unpacked_packages={
                        package.name: verified_paths[package.name]
                    },
                )

            dedicated_target = (
                workspace_root / "packages" / "unity-asset-decode-0.4.0"
            )
            unpacked_names: list[str] = []

            def unpack(
                archive_path: Path,
                unpack_root: Path,
                unpacked_package: contract.WorkspacePackage,
            ) -> Path:
                self.assertEqual(archive_path, archive_paths[unpacked_package.name])
                self.assertEqual(unpack_root, workspace_root / "packages")
                unpacked_names.append(unpacked_package.name)
                return dedicated_target

            with (
                mock.patch.object(verifier, "unpack_archive", side_effect=unpack),
                mock.patch.object(verifier, "verify_temporary_workspace") as verify,
                mock.patch.object(verifier, "check_documented_example") as check,
            ):
                verifier.verify_documented_example_standalone(
                    **verification_arguments,
                    verified_unpacked_packages=verified_paths,
                )

            manifest = workspace_root / "Cargo.toml"
            self.assertEqual(unpacked_names, [package.name])
            verify.assert_called_once()
            self.assertEqual(
                verify.call_args.kwargs["unpacked_packages"],
                {
                    dependency.name: verified_paths[dependency.name],
                    package.name: dedicated_target,
                },
            )
            self.assertEqual(
                verify.call_args.kwargs["required_internal"],
                {dependency.name, package.name},
            )
            check.assert_called_once_with(
                cargo="cargo",
                cargo_cwd=root,
                environment={"CARGO_HOME": "isolated"},
                workspace_manifest=manifest,
                profile=profile,
            )
            workspace_text = manifest.read_text(encoding="utf-8")
            self.assertIn(
                verifier.toml_string(
                    verifier.relative_toml_path(dedicated_target, manifest.parent)
                ),
                workspace_text,
            )
            self.assertNotIn(
                verifier.toml_string(
                    verifier.relative_toml_path(
                        verified_paths[package.name], manifest.parent
                    )
                ),
                workspace_text,
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

    def test_rejects_untrusted_consumer_lock_entries(self) -> None:
        valid_checksum = "a" * 64
        cases = {
            "git_source": (
                'source = "git+https://example.invalid/repository"\n'
                f'checksum = "{valid_checksum}"\n',
                "non-crates.io source",
            ),
            "bad_checksum": (
                        f'source = "{contract.CRATES_IO_SOURCE}"\n'
                'checksum = "not-a-checksum"\n',
                "invalid checksum",
            ),
            "missing_checksum": (
                        f'source = "{contract.CRATES_IO_SOURCE}"\n',
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
                    with self.assertRaisesRegex(contract.VerificationError, message):
                        verifier.validate_consumer_lock(lock_path, {})

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
                contract.VerificationError, "leaked a repository checkout path"
            ):
                verifier.validate_resolved_workspace(
                    json.dumps(metadata),
                    workspace_root,
                    {consumer_name: consumer_manifest},
                    {target_name: unpacked_target},
                    {target_name: "1.2.3"},
                    set(),
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
                    root / "repository",
                    {consumer_name: consumer_manifest},
                    {target_name: root / "unpacked" / target_name},
                    {target_name: "1.0.0"},
                    set(),
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

    def test_binary_verification_uses_a_dedicated_archive_closure(self) -> None:
        temporary = tempfile.TemporaryDirectory(
            prefix="unity-asset-package-verifier-test-"
        )
        self.addCleanup(temporary.cleanup)
        test_root = Path(temporary.name)
        clean_cargo_cwd = test_root / "clean-cargo-cwd"
        standalone_root = test_root / "poisoned-ancestor" / "standalone"
        dependency = contract.WorkspacePackage(
            name="example-core",
            version="1.2.3",
            manifest_path=Path("source-core") / "Cargo.toml",
            dependencies=(),
            publish=None,
            is_library=True,
            feature_names=(),
        )
        package = contract.WorkspacePackage(
            name="example-package",
            version="1.2.3",
            manifest_path=Path("source") / "Cargo.toml",
            dependencies=(),
            publish=None,
            is_library=False,
            feature_names=(),
            binary_target_names=("example-tool",),
        )
        archive_paths = {
            dependency.name: test_root / "archives" / "example-core-1.2.3.crate",
            package.name: test_root / "archives" / "example-package-1.2.3.crate",
        }
        dedicated_paths = {
            dependency.name: standalone_root / "packages" / "example-core-1.2.3",
            package.name: standalone_root / "packages" / "example-package-1.2.3",
        }

        def unpack(
            _archive_path: Path,
            _unpack_root: Path,
            unpacked_package: contract.WorkspacePackage,
        ) -> Path:
            return dedicated_paths[unpacked_package.name]

        with (
            mock.patch.object(
                verifier, "unpack_archive", side_effect=unpack
            ) as unpack_archive,
            mock.patch.object(
                verifier,
                "write_workspace_manifest",
                return_value=standalone_root / "Cargo.toml",
            ) as write_workspace_manifest,
            mock.patch.object(
                verifier,
                "production_closure",
                return_value=(dependency, package),
            ),
            mock.patch.object(
                verifier, "verify_temporary_workspace"
            ) as verify_temporary_workspace,
            mock.patch.object(
                verifier, "check_temporary_workspace"
            ) as check_temporary_workspace,
            mock.patch.object(verifier, "run_visible") as run_visible,
        ):
            verifier.verify_binary_package_standalone(
                cargo="cargo",
                cargo_cwd=clean_cargo_cwd,
                environment={"CARGO_HOME": "isolated"},
                repository_root=Path("repository"),
                workspace_root=standalone_root,
                package=package,
                packages={
                    dependency.name: dependency,
                    package.name: package,
                },
                archive_paths=archive_paths,
                expected_versions={
                    dependency.name: dependency.version,
                    package.name: package.version,
                },
                registry_source_root=Path("cargo-home") / "registry" / "src",
            )

        self.assertEqual(
            unpack_archive.call_args_list,
            [
                mock.call(
                    archive_paths[dependency.name],
                    standalone_root / "packages",
                    dependency,
                ),
                mock.call(
                    archive_paths[package.name],
                    standalone_root / "packages",
                    package,
                ),
            ],
        )
        write_workspace_manifest.assert_called_once_with(
            standalone_root,
            [dedicated_paths[package.name]],
            dedicated_paths,
        )
        self.assertEqual(verify_temporary_workspace.call_count, 2)
        self.assertFalse(
            verify_temporary_workspace.call_args_list[0].kwargs["all_features"]
        )
        self.assertTrue(
            verify_temporary_workspace.call_args_list[1].kwargs["all_features"]
        )
        for call in verify_temporary_workspace.call_args_list:
            self.assertEqual(call.kwargs["required_internal"], {package.name})
        self.assertEqual(check_temporary_workspace.call_count, 2)
        for call in check_temporary_workspace.call_args_list:
            self.assertEqual(call.kwargs["target_arguments"], ("--bins",))
        self.assertEqual(run_visible.call_count, 1)
        self.assertEqual(run_visible.call_args.kwargs["cwd"], clean_cargo_cwd)
        self.assertIn(
            str(dedicated_paths[package.name]), run_visible.call_args.args[0]
        )


if __name__ == "__main__":
    unittest.main()
