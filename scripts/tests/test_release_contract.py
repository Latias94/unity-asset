from __future__ import annotations

import sys
import tomllib
import unittest
from copy import deepcopy
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_ROOT = REPOSITORY_ROOT / "scripts"
sys.path.insert(0, str(SCRIPTS_ROOT))

from release_contract import (  # noqa: E402
    CARGO_DIST_VERSION,
    DISTRIBUTED_APPLICATION_NAMES,
    DISTRIBUTION_TARGET_TRIPLES,
    PUBLISHABLE_PACKAGE_NAMES,
    ReleaseContractError,
    github_distribution_matrix,
    validate_local_dist_plan,
)


def artifact_name(application: str, target: str) -> str:
    extension = ".zip" if target.endswith("windows-msvc") else ".tar.xz"
    return f"{application}-{target}{extension}"


def valid_dist_plan() -> dict[str, object]:
    artifacts: dict[str, object] = {}
    releases: list[dict[str, object]] = []
    for application in DISTRIBUTED_APPLICATION_NAMES:
        release_artifacts: list[str] = []
        for target in DISTRIBUTION_TARGET_TRIPLES:
            name = artifact_name(application, target)
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
                "app_version": "0.4.0",
                "artifacts": release_artifacts,
            }
        )
    return {
        "dist_version": CARGO_DIST_VERSION,
        "announcement_tag": "v0.4.0",
        "announcement_tag_is_implicit": False,
        "announcement_is_prerelease": False,
        "artifacts": artifacts,
        "releases": releases,
    }


class ReleaseContractTests(unittest.TestCase):
    def validate(self, plan: dict[str, object]) -> tuple[str, ...]:
        return validate_local_dist_plan(plan, tag="v0.4.0", version="0.4.0")

    def test_product_release_policy_matches_workspace_configuration(self) -> None:
        root_manifest = tomllib.loads(
            (REPOSITORY_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        )
        workspace = root_manifest["workspace"]
        actual_packages: set[str] = set()
        for member in workspace["members"]:
            manifest = tomllib.loads(
                (REPOSITORY_ROOT / member / "Cargo.toml").read_text(encoding="utf-8")
            )
            package = manifest["package"]
            if package.get("publish") != []:
                actual_packages.add(package["name"])
        self.assertEqual(actual_packages, set(PUBLISHABLE_PACKAGE_NAMES))

        dist_workspace = tomllib.loads(
            (REPOSITORY_ROOT / "dist-workspace.toml").read_text(encoding="utf-8")
        )
        dist = dist_workspace["dist"]
        self.assertEqual(dist["cargo-dist-version"], CARGO_DIST_VERSION)
        self.assertEqual(set(dist["targets"]), set(DISTRIBUTION_TARGET_TRIPLES))

        actual_dist_apps: set[str] = set()
        for member in workspace["members"]:
            manifest = tomllib.loads(
                (REPOSITORY_ROOT / member / "Cargo.toml").read_text(encoding="utf-8")
            )
            if manifest.get("package", {}).get("metadata", {}).get("dist", {}).get("dist"):
                actual_dist_apps.add(manifest["package"]["name"])
        self.assertEqual(actual_dist_apps, set(DISTRIBUTED_APPLICATION_NAMES))

    def test_github_runner_matrix_exactly_covers_the_target_inventory(self) -> None:
        matrix = github_distribution_matrix()
        targets = [
            target
            for entry in matrix["include"]
            for target in entry["targets"].split(",")
        ]
        self.assertEqual(set(targets), set(DISTRIBUTION_TARGET_TRIPLES))
        self.assertEqual(len(targets), len(DISTRIBUTION_TARGET_TRIPLES))

    def test_rejects_implicit_release_tags(self) -> None:
        plan = valid_dist_plan()
        plan["announcement_tag_is_implicit"] = True

        with self.assertRaisesRegex(ReleaseContractError, "explicit release tag"):
            self.validate(plan)

    def test_rejects_prerelease_plans_for_stable_tags(self) -> None:
        plan = valid_dist_plan()
        plan["announcement_is_prerelease"] = True

        with self.assertRaisesRegex(ReleaseContractError, "prerelease dist plan"):
            self.validate(plan)

    def test_rejects_unreferenced_artifacts(self) -> None:
        plan = deepcopy(valid_dist_plan())
        artifacts = plan["artifacts"]
        assert isinstance(artifacts, dict)
        artifacts["orphan.tar.xz"] = {
            "name": "orphan.tar.xz",
            "kind": "executable-zip",
            "target_triples": [DISTRIBUTION_TARGET_TRIPLES[0]],
            "checksum": "orphan.tar.xz.sha256",
        }

        with self.assertRaisesRegex(ReleaseContractError, "unreferenced or missing"):
            self.validate(plan)

    def test_rejects_cross_product_archive_aliases(self) -> None:
        plan = deepcopy(valid_dist_plan())
        releases = plan["releases"]
        assert isinstance(releases, list)
        target = DISTRIBUTION_TARGET_TRIPLES[0]
        cli_archive = artifact_name("unity-asset-search-cli", target)
        daemon_archive = artifact_name("unity-asset-search-daemon", target)
        aliases = {
            cli_archive: daemon_archive,
            f"{cli_archive}.sha256": f"{daemon_archive}.sha256",
            daemon_archive: cli_archive,
            f"{daemon_archive}.sha256": f"{cli_archive}.sha256",
        }
        for release in releases:
            assert isinstance(release, dict)
            names = release["artifacts"]
            assert isinstance(names, list)
            names[:] = [aliases.get(name, name) for name in names]

        with self.assertRaisesRegex(ReleaseContractError, "must use its canonical archive"):
            self.validate(plan)

    def test_rejects_duplicate_archive_reference_within_a_release(self) -> None:
        plan = deepcopy(valid_dist_plan())
        releases = plan["releases"]
        assert isinstance(releases, list)
        artifacts = releases[0]["artifacts"]
        assert isinstance(artifacts, list)
        artifacts.append(artifacts[0])

        with self.assertRaisesRegex(ReleaseContractError, "duplicate artifact"):
            self.validate(plan)


if __name__ == "__main__":
    unittest.main()
