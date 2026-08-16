"""Define the reviewed workspace package publication contract.

This module owns package discovery, dependency-source policy, and the exact
publishable production closure. It performs no packaging or network work.
"""

from __future__ import annotations

import json
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator, Mapping, Sequence

from release_contract import PUBLISHABLE_PACKAGE_NAMES


DEPENDENCY_TABLES = ("dependencies", "build-dependencies")
ALL_DEPENDENCY_TABLES = (*DEPENDENCY_TABLES, "dev-dependencies")
CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"


class VerificationError(RuntimeError):
    """An actionable package verification failure."""


@dataclass(frozen=True)
class DocumentedFeatureProfile:
    """One public feature combination promised by repository documentation."""

    name: str
    package: str
    features: tuple[str, ...]
    default_features: bool


DOCUMENTED_FEATURE_PROFILES = (
    DocumentedFeatureProfile(
        name="readme-decode-media",
        package="unity-asset-decode",
        features=("audio", "texture-advanced"),
        default_features=True,
    ),
    DocumentedFeatureProfile(
        name="workspace-decode",
        package="unity-asset",
        features=("decode",),
        default_features=True,
    ),
)


@dataclass(frozen=True)
class WorkspacePackage:
    name: str
    version: str
    manifest_path: Path
    dependencies: tuple[Mapping[str, Any], ...]
    publish: object
    is_library: bool
    feature_names: tuple[str, ...]
    library_target_name: str | None = None
    binary_target_names: tuple[str, ...] = ()

    @property
    def directory(self) -> Path:
        return self.manifest_path.parent


def validate_documented_feature_profiles(
    packages: Mapping[str, WorkspacePackage],
) -> tuple[DocumentedFeatureProfile, ...]:
    """Validate documentation profiles against the discovered package graph."""

    names: set[str] = set()
    for profile in DOCUMENTED_FEATURE_PROFILES:
        if profile.name in names:
            raise VerificationError(
                f"duplicate documented feature profile: {profile.name}"
            )
        names.add(profile.name)
        package = packages.get(profile.package)
        if package is None:
            raise VerificationError(
                f"documented feature profile {profile.name} targets a missing package: "
                f"{profile.package}"
            )
        missing = sorted(set(profile.features) - set(package.feature_names))
        if missing:
            raise VerificationError(
                f"documented feature profile {profile.name} names unknown features: "
                + ", ".join(missing)
            )
        if not profile.features:
            raise VerificationError(
                f"documented feature profile {profile.name} must name features"
            )
        if not package.is_library:
            raise VerificationError(
                f"documented feature profile {profile.name} must target a library package"
            )
    return DOCUMENTED_FEATURE_PROFILES


def load_toml(path: Path) -> Mapping[str, Any]:
    try:
        with path.open("rb") as stream:
            document = tomllib.load(stream)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise VerificationError(f"cannot read TOML {path}: {error}") from error
    if not isinstance(document, dict):
        raise VerificationError(f"expected a TOML table at {path}")
    return document


def parse_cargo_metadata(metadata_text: str) -> Mapping[str, Any]:
    try:
        metadata = json.loads(metadata_text)
    except json.JSONDecodeError as error:
        raise VerificationError(f"cargo metadata returned invalid JSON: {error}") from error
    if not isinstance(metadata, dict):
        raise VerificationError("cargo metadata root must be an object")
    return metadata


def discover_workspace_packages(metadata_text: str) -> dict[str, WorkspacePackage]:
    metadata = parse_cargo_metadata(metadata_text)

    raw_members = metadata.get("workspace_members")
    raw_packages = metadata.get("packages")
    if not isinstance(raw_members, list) or not isinstance(raw_packages, list):
        raise VerificationError("cargo metadata omitted workspace packages")
    workspace_members = {member for member in raw_members if isinstance(member, str)}

    packages: dict[str, WorkspacePackage] = {}
    for raw in raw_packages:
        if not isinstance(raw, dict) or raw.get("id") not in workspace_members:
            continue
        name = raw.get("name")
        version = raw.get("version")
        manifest_path = raw.get("manifest_path")
        dependencies = raw.get("dependencies")
        targets = raw.get("targets")
        features = raw.get("features")
        if (
            not isinstance(name, str)
            or not isinstance(version, str)
            or not isinstance(manifest_path, str)
            or not isinstance(dependencies, list)
            or any(not isinstance(dependency, dict) for dependency in dependencies)
            or not isinstance(targets, list)
            or any(not isinstance(target, dict) for target in targets)
            or not isinstance(features, dict)
            or any(not isinstance(feature, str) for feature in features)
        ):
            raise VerificationError("cargo metadata contains an invalid workspace package")
        if name in packages:
            raise VerificationError(f"duplicate workspace package name: {name}")
        packages[name] = WorkspacePackage(
            name=name,
            version=version,
            manifest_path=Path(manifest_path).resolve(),
            dependencies=tuple(dependencies),
            publish=raw.get("publish"),
            is_library=any(
                isinstance(target.get("kind"), list) and "lib" in target["kind"]
                for target in targets
            ),
            feature_names=tuple(sorted(feature for feature in features if feature != "default")),
            library_target_name=next(
                (
                    target.get("name")
                    for target in targets
                    if isinstance(target.get("kind"), list)
                    and "lib" in target["kind"]
                    and isinstance(target.get("name"), str)
                ),
                None,
            ),
            binary_target_names=tuple(
                sorted(
                    target["name"]
                    for target in targets
                    if isinstance(target.get("kind"), list)
                    and "bin" in target["kind"]
                    and isinstance(target.get("name"), str)
                )
            ),
        )

    if not packages:
        raise VerificationError("cargo metadata returned an empty workspace")
    return packages


def dependency_tables(
    document: Mapping[str, Any], *, include_dev: bool
) -> Iterator[tuple[str, Mapping[str, Any]]]:
    names = ALL_DEPENDENCY_TABLES if include_dev else DEPENDENCY_TABLES
    for name in names:
        table = document.get(name)
        if table is not None:
            if not isinstance(table, dict):
                raise VerificationError(f"{name} must be a TOML table")
            yield name, table

    targets = document.get("target", {})
    if targets is None:
        return
    if not isinstance(targets, dict):
        raise VerificationError("target must be a TOML table")
    for target_name, target in targets.items():
        if not isinstance(target, dict):
            raise VerificationError(f"target.{target_name} must be a TOML table")
        for name in names:
            table = target.get(name)
            if table is not None:
                if not isinstance(table, dict):
                    raise VerificationError(
                        f"target.{target_name}.{name} must be a TOML table"
                    )
                yield f"target.{target_name}.{name}", table


def dependency_location(
    package: WorkspacePackage, dependency: Mapping[str, Any]
) -> str:
    name = dependency.get("name")
    rename = dependency.get("rename")
    kind = dependency.get("kind") or "normal"
    target = dependency.get("target") or "all-targets"
    alias = rename if isinstance(rename, str) and rename else name
    return f"{package.manifest_path}:{kind}:{target}:{alias}"


def internal_dependency_package(
    package: WorkspacePackage,
    dependency: Mapping[str, Any],
    packages: Mapping[str, WorkspacePackage],
) -> WorkspacePackage | None:
    location = dependency_location(package, dependency)
    name = dependency.get("name")
    source = dependency.get("source")
    raw_path = dependency.get("path")
    if not isinstance(name, str) or not name:
        raise VerificationError(f"{location}: cargo metadata dependency name is invalid")
    internal = packages.get(name)
    if internal is not None:
        if source is not None:
            raise VerificationError(
                f"{location}: dependency named like workspace package {name!r} "
                f"must be a workspace path dependency, not {source}"
            )
        if not isinstance(raw_path, str) or not raw_path:
            raise VerificationError(
                f"{location}: dependency on workspace package {name!r} must "
                "retain its exact workspace path"
            )
        resolved_path = Path(raw_path).resolve()
        if resolved_path != internal.directory.resolve():
            raise VerificationError(
                f"{location}: workspace package {name!r} resolves to the wrong "
                f"path ({resolved_path})"
            )
        return internal

    if raw_path is not None:
        raise VerificationError(
            f"{location}: repository/external path dependency is forbidden"
        )
    if source != CRATES_IO_SOURCE:
        raise VerificationError(
            f"{location}: third-party dependency {name!r} must come from "
            f"crates.io, not {source!r}"
        )
    return None


def reject_root_source_overrides(
    root_document: Mapping[str, Any], root_manifest: Path
) -> None:
    patch = root_document.get("patch")
    if isinstance(patch, dict) and patch:
        entries = []
        for registry, overrides in patch.items():
            if isinstance(overrides, dict):
                entries.extend(f"{registry}:{name}" for name in overrides)
            else:
                entries.append(str(registry))
        rendered_entries = ", ".join(sorted(entries))
        raise VerificationError(
            f"{root_manifest}: root [patch] tables are forbidden for packaged "
            f"workspace package verification (found: {rendered_entries})"
        )
    if root_document.get("replace"):
        raise VerificationError(
            f"{root_manifest}: deprecated [replace] source overrides are forbidden"
        )


def production_closure(
    target_names: Sequence[str],
    packages: Mapping[str, WorkspacePackage],
) -> list[WorkspacePackage]:
    if not target_names:
        raise VerificationError("package verification target set is empty")
    missing = sorted(set(target_names) - packages.keys())
    if missing:
        raise VerificationError(f"workspace does not contain verification targets: {missing}")

    order: list[WorkspacePackage] = []
    states: dict[str, str] = {}
    stack: list[str] = []

    def visit(name: str) -> None:
        state = states.get(name)
        if state == "done":
            return
        if state == "visiting":
            cycle_start = stack.index(name)
            cycle = " -> ".join((*stack[cycle_start:], name))
            raise VerificationError(f"internal production dependency cycle: {cycle}")

        states[name] = "visiting"
        stack.append(name)
        package = packages[name]
        for dependency in package.dependencies:
            if dependency.get("kind") == "dev":
                continue
            internal = internal_dependency_package(package, dependency, packages)
            if internal is not None:
                visit(internal.name)
        stack.pop()
        states[name] = "done"
        order.append(package)

    for target_name in sorted(target_names):
        visit(target_name)
    return order


def published_production_closure(
    packages: Mapping[str, WorkspacePackage],
) -> list[WorkspacePackage]:
    expected = set(PUBLISHABLE_PACKAGE_NAMES)
    actual = {package.name for package in packages.values() if package.publish != []}
    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    if missing or unexpected:
        details: list[str] = []
        if missing:
            details.append(f"missing expected packages: {', '.join(missing)}")
        if unexpected:
            details.append(f"unexpected publishable packages: {', '.join(unexpected)}")
        raise VerificationError(
            "workspace publishable package set differs from the reviewed release "
            f"contract ({'; '.join(details)})"
        )
    closure = production_closure(PUBLISHABLE_PACKAGE_NAMES, packages)
    actual_order = tuple(package.name for package in closure)
    if actual_order != PUBLISHABLE_PACKAGE_NAMES:
        raise VerificationError(
            "workspace publishable dependency order differs from the reviewed "
            f"release contract: {actual_order!r}"
        )
    return closure


def validate_source_dependencies(
    closure: Sequence[WorkspacePackage],
    packages: Mapping[str, WorkspacePackage],
) -> None:
    for package in closure:
        publish = package.publish
        if publish == []:
            raise VerificationError(
                f"{package.manifest_path}: internal release dependency "
                f"{package.name} is marked publish = false"
            )
        if publish is not None:
            if not isinstance(publish, list) or any(
                not isinstance(registry, str) for registry in publish
            ):
                raise VerificationError(
                    f"{package.manifest_path}: cargo metadata publish policy is invalid"
                )
            if "crates-io" not in publish:
                raise VerificationError(
                    f"{package.manifest_path}: internal release dependency "
                    f"{package.name} cannot be published to crates.io"
                )

        for dependency in package.dependencies:
            internal_dependency_package(package, dependency, packages)
