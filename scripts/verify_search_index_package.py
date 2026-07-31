#!/usr/bin/env python3
"""Verify that the packaged search index is independent of the repository.

The verifier deliberately uses only the Python standard library. It packages the
search-index crate and its internal production dependencies, validates every
normalized packaged manifest, and then checks a temporary consumer with an
isolated Cargo home. Only the unpacked internal archives may be path sources;
all third-party packages must resolve from a registry.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unicodedata
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterator, Mapping, Sequence
from urllib.parse import urlsplit


TARGET_PACKAGE = "unity-asset-search-index"
CONSUMER_PACKAGE = "unity-asset-search-index-package-consumer"
DEPENDENCY_TABLES = ("dependencies", "build-dependencies")
ALL_DEPENDENCY_TABLES = (*DEPENDENCY_TABLES, "dev-dependencies")
CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
CARGO_CONFIG_NAMES = ("config.toml", "config")
WINDOWS_RESERVED_COMPONENTS = {
    "AUX",
    "CLOCK$",
    "CON",
    "CONIN$",
    "CONOUT$",
    "NUL",
    "PRN",
}
WINDOWS_RESERVED_DEVICE_DIGITS = frozenset("123456789¹²³")


class VerificationError(RuntimeError):
    """An actionable package verification failure."""


@dataclass(frozen=True)
class WorkspacePackage:
    name: str
    version: str
    manifest_path: Path
    dependencies: tuple[Mapping[str, Any], ...]
    publish: object

    @property
    def directory(self) -> Path:
        return self.manifest_path.parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Package unity-asset-search-index and verify an isolated registry-backed "
            "consumer."
        )
    )
    parser.add_argument(
        "--workspace-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Workspace root containing Cargo.toml (default: repository root).",
    )
    parser.add_argument(
        "--cargo",
        default=os.environ.get("CARGO", "cargo"),
        help="Cargo executable to invoke (default: CARGO or cargo).",
    )
    parser.add_argument(
        "--preflight-only",
        action="store_true",
        help="Run Cargo metadata and dependency-policy checks without packaging.",
    )
    return parser.parse_args()


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
        if (
            not isinstance(name, str)
            or not isinstance(version, str)
            or not isinstance(manifest_path, str)
            or not isinstance(dependencies, list)
            or any(not isinstance(dependency, dict) for dependency in dependencies)
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
    if name == "ignore":
        raise VerificationError(f"{location}: dependency 'ignore' is forbidden")

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
            f"search-index verification (found: {rendered_entries})"
        )
    if root_document.get("replace"):
        raise VerificationError(
            f"{root_manifest}: deprecated [replace] source overrides are forbidden"
        )


def production_closure(
    target_name: str,
    packages: Mapping[str, WorkspacePackage],
) -> list[WorkspacePackage]:
    if target_name not in packages:
        raise VerificationError(f"workspace does not contain {target_name}")

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

    visit(target_name)
    return order


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


def command_text(command: Sequence[str]) -> str:
    if os.name == "nt":
        return subprocess.list2cmdline(list(command))
    return shlex.join(command)


def run_visible(
    command: Sequence[str], *, cwd: Path, env: Mapping[str, str]
) -> None:
    print(f"$ {command_text(command)}", flush=True)
    result = subprocess.run(command, cwd=cwd, env=env, check=False)
    if result.returncode != 0:
        raise VerificationError(
            f"command failed with exit code {result.returncode}: "
            f"{command_text(command)}"
        )


def run_captured(
    command: Sequence[str], *, cwd: Path, env: Mapping[str, str]
) -> str:
    print(f"$ {command_text(command)}", flush=True)
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=False,
        text=True,
        encoding="utf-8",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        details = "\n".join(
            part.rstrip() for part in (result.stdout, result.stderr) if part.strip()
        )
        suffix = f"\n{details}" if details else ""
        raise VerificationError(
            f"command failed with exit code {result.returncode}: "
            f"{command_text(command)}{suffix}"
        )
    if result.stderr.strip():
        print(result.stderr.rstrip(), file=sys.stderr)
    return result.stdout


def proxy_without_credentials(value: str) -> str | None:
    try:
        parsed = urlsplit(value)
    except ValueError:
        return None
    if (
        parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        return None
    return value


def isolated_cargo_environment(cargo_home: Path, target_dir: Path) -> dict[str, str]:
    passthrough = (
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "PROCESSOR_IDENTIFIER",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        "TEMP",
        "TMP",
        "TMPDIR",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NIX_SSL_CERT_FILE",
    )
    environment = {
        key: value
        for key in passthrough
        if (value := os.environ.get(key)) is not None
    }
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        value = os.environ.get(key) or os.environ.get(key.lower())
        if value is not None and (safe_proxy := proxy_without_credentials(value)) is not None:
            environment[key] = safe_proxy
    no_proxy = os.environ.get("NO_PROXY") or os.environ.get("no_proxy")
    if no_proxy is not None:
        environment["NO_PROXY"] = no_proxy

    sandbox_home = cargo_home.parent / "home"
    sandbox_home.mkdir(exist_ok=True)
    environment["HOME"] = str(sandbox_home)
    environment["USERPROFILE"] = str(sandbox_home)
    if os.name == "nt":
        home_drive, home_path = os.path.splitdrive(str(sandbox_home))
        environment["HOMEDRIVE"] = home_drive
        environment["HOMEPATH"] = home_path

    rustup_home = os.environ.get("RUSTUP_HOME")
    if rustup_home is None:
        host_home = os.environ.get("USERPROFILE" if os.name == "nt" else "HOME")
        if host_home is not None:
            candidate = Path(host_home) / ".rustup"
            if candidate.is_dir():
                rustup_home = str(candidate)
    if rustup_home is not None:
        environment["RUSTUP_HOME"] = rustup_home

    environment["CARGO_HOME"] = str(cargo_home)
    environment["CARGO_TARGET_DIR"] = str(target_dir)
    environment["CARGO_REGISTRIES_CRATES_IO_PROTOCOL"] = "sparse"
    environment["CARGO_TERM_COLOR"] = "never"
    environment["CARGO_BUILD_JOBS"] = "1"
    return environment


def configuration_clean_cargo_cwd(workspace_root: Path) -> Path:
    anchor = workspace_root.anchor
    if not anchor:
        raise VerificationError(f"workspace has no filesystem root: {workspace_root}")
    try:
        root = Path(anchor).resolve(strict=True)
    except OSError as error:
        raise VerificationError(
            f"cannot resolve Cargo configuration root {anchor}: {error}"
        ) from error
    if not root.is_dir() or root.parent != root:
        raise VerificationError(f"invalid Cargo configuration root: {root}")

    for name in CARGO_CONFIG_NAMES:
        candidate = root / ".cargo" / name
        if os.path.lexists(candidate):
            raise VerificationError(
                f"Cargo configuration at filesystem root is forbidden: {candidate}"
            )
    return root


def locate_archive(package_target: Path, package: WorkspacePackage) -> Path:
    expected_name = f"{package.name}-{package.version}.crate"
    archive = package_target / "package" / expected_name
    if not archive.is_file():
        raise VerificationError(f"missing packaged archive: {archive}")
    return archive


def validate_archive_member(
    archive_path: Path,
    member: tarfile.TarInfo,
    expected_root_name: str,
) -> PurePosixPath:
    name = member.name[:-1] if member.isdir() and member.name.endswith("/") else member.name
    if "\\" in name:
        raise VerificationError(
            f"{archive_path}: archive member uses a backslash: {member.name}"
        )
    parts = name.split("/")
    if not parts or any(part in {"", ".", ".."} for part in parts):
        raise VerificationError(
            f"{archive_path}: unsafe archive member path: {member.name}"
        )
    if parts[0] != expected_root_name:
        raise VerificationError(
            f"{archive_path}: archive member escapes expected root "
            f"{expected_root_name}: {member.name}"
        )
    for component in parts:
        if (
            ":" in component
            or component.endswith((".", " "))
            or any(ord(character) < 32 for character in component)
            or is_windows_reserved_component(component)
        ):
            raise VerificationError(
                f"{archive_path}: archive member is not portable: {member.name}"
            )
    if not (member.isdir() or member.isfile()):
        raise VerificationError(
            f"{archive_path}: links and special files are forbidden: {member.name}"
        )
    return PurePosixPath(*parts)


def is_windows_reserved_component(component: str) -> bool:
    stem = component.split(".", 1)[0].upper()
    return stem in WINDOWS_RESERVED_COMPONENTS or (
        len(stem) == 4
        and stem[:3] in {"COM", "LPT"}
        and stem[3] in WINDOWS_RESERVED_DEVICE_DIGITS
    )


def unpack_archive(archive_path: Path, unpack_root: Path, package: WorkspacePackage) -> Path:
    expected_root_name = f"{package.name}-{package.version}"
    package_root = unpack_root / expected_root_name
    try:
        with tarfile.open(archive_path, mode="r:gz") as archive:
            members = archive.getmembers()
            portable_paths: set[tuple[str, ...]] = set()
            for member in members:
                relative = validate_archive_member(
                    archive_path, member, expected_root_name
                )
                portable_key = tuple(
                    unicodedata.normalize("NFC", component).casefold()
                    for component in relative.parts
                )
                if portable_key in portable_paths:
                    raise VerificationError(
                        f"{archive_path}: archive contains a portable path alias: "
                        f"{member.name}"
                    )
                portable_paths.add(portable_key)
            archive.extractall(path=unpack_root, members=members, filter="data")
    except (OSError, tarfile.TarError) as error:
        raise VerificationError(f"cannot unpack {archive_path}: {error}") from error

    if not package_root.is_dir():
        raise VerificationError(f"{archive_path}: missing unpacked root {package_root}")
    return package_root


def raw_packaged_dependencies(
    document: Mapping[str, Any], manifest_path: Path
) -> Iterator[tuple[str, str, Mapping[str, Any]]]:
    for location, table in dependency_tables(document, include_dev=True):
        for alias, raw_value in table.items():
            if isinstance(raw_value, str):
                values: Mapping[str, Any] = {"version": raw_value}
            elif isinstance(raw_value, dict):
                values = raw_value
            else:
                raise VerificationError(
                    f"{manifest_path}: {location}.{alias} must be a string or table"
                )
            package_name = values.get("package", alias)
            if not isinstance(package_name, str) or not package_name:
                raise VerificationError(
                    f"{manifest_path}: {location}.{alias}.package is invalid"
                )
            yield f"{location}.{alias}", package_name, values


def validate_packaged_manifest(package_root: Path, expected: WorkspacePackage) -> None:
    manifest_path = package_root / "Cargo.toml"
    document = load_toml(manifest_path)
    package = document.get("package")
    if not isinstance(package, dict):
        raise VerificationError(f"{manifest_path}: normalized manifest has no [package]")
    if package.get("name") != expected.name or package.get("version") != expected.version:
        raise VerificationError(
            f"{manifest_path}: expected {expected.name} {expected.version}, got "
            f"{package.get('name')} {package.get('version')}"
        )
    if document.get("patch") or document.get("replace"):
        raise VerificationError(
            f"{manifest_path}: packaged manifests must not contain source overrides"
        )

    for location, package_name, values in raw_packaged_dependencies(
        document, manifest_path
    ):
        if values.get("workspace") is True:
            raise VerificationError(
                f"{manifest_path}: {location} still inherits from the workspace"
            )
        if "path" in values:
            raise VerificationError(
                f"{manifest_path}: {location} retains a repository path dependency"
            )
        if "git" in values:
            raise VerificationError(
                f"{manifest_path}: {location} retains a Git dependency"
            )
        if package_name == "ignore":
            raise VerificationError(
                f"{manifest_path}: {location} depends on forbidden package 'ignore'"
            )


def toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=True)


def create_consumer(
    consumer_root: Path,
    target: WorkspacePackage,
    unpacked_packages: Mapping[str, Path],
) -> Path:
    source_root = consumer_root / "src"
    source_root.mkdir(parents=True)
    patch_lines = [
        f"{name} = {{ path = {toml_string(path.as_posix())} }}"
        for name, path in sorted(unpacked_packages.items())
    ]
    manifest = "\n".join(
        [
            "[package]",
            f"name = {toml_string(CONSUMER_PACKAGE)}",
            'version = "0.0.0"',
            'edition = "2024"',
            "publish = false",
            "",
            "[dependencies]",
            f"{target.name} = {toml_string('=' + target.version)}",
            "",
            "[patch.crates-io]",
            *patch_lines,
            "",
        ]
    )
    manifest_path = consumer_root / "Cargo.toml"
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    (source_root / "lib.rs").write_text(
        """//! Isolated package-consumer compilation probe.

use std::ffi::OsStr;

use unity_asset_search_index::{
    IndexPaths, ScanTraversalLimits, SearchIndexOptions, is_search_ignore_v1_file_name,
};

#[allow(dead_code)]
fn public_api_probe(
    paths: Option<IndexPaths>,
    options: SearchIndexOptions,
    limits: ScanTraversalLimits,
) -> bool {
    let _ = (paths, options, limits);
    is_search_ignore_v1_file_name(OsStr::new(".unity-asset-search-ignore"))
}
""",
        encoding="utf-8",
        newline="\n",
    )
    return manifest_path


def is_sha256_checksum(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def reachable_package_ids(metadata: Mapping[str, Any]) -> set[str]:
    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict):
        raise VerificationError("cargo metadata did not return a resolve graph")
    root = resolve.get("root")
    nodes = resolve.get("nodes")
    if not isinstance(root, str) or not isinstance(nodes, list):
        raise VerificationError("cargo metadata resolve graph is incomplete")

    edges: dict[str, list[str]] = {}
    for node in nodes:
        if not isinstance(node, dict) or not isinstance(node.get("id"), str):
            raise VerificationError("cargo metadata contains an invalid resolve node")
        dependencies: list[str] = []
        for dependency in node.get("deps", []):
            if not isinstance(dependency, dict) or not isinstance(
                dependency.get("pkg"), str
            ):
                raise VerificationError("cargo metadata contains an invalid dependency edge")
            dependencies.append(dependency["pkg"])
        edges[node["id"]] = dependencies

    reachable: set[str] = set()
    pending = [root]
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        reachable.add(package_id)
        pending.extend(edges.get(package_id, []))
    return reachable


def validate_resolved_graph(
    metadata_text: str,
    workspace_root: Path,
    consumer_manifest: Path,
    unpacked_packages: Mapping[str, Path],
    expected_versions: Mapping[str, str],
    locked_registry_packages: set[tuple[str, str, str]],
    registry_source_root: Path,
) -> None:
    metadata = parse_cargo_metadata(metadata_text)

    packages = metadata.get("packages")
    if not isinstance(packages, list):
        raise VerificationError("cargo metadata did not return packages")
    by_id = {
        package.get("id"): package
        for package in packages
        if isinstance(package, dict) and isinstance(package.get("id"), str)
    }

    reachable = reachable_package_ids(metadata)
    found_target = False
    repository_root = workspace_root.resolve()
    expected_consumer_manifest = consumer_manifest.resolve()

    for package_id in reachable:
        package = by_id.get(package_id)
        if package is None:
            raise VerificationError(f"resolve graph references missing package {package_id}")
        name = package.get("name")
        version = package.get("version")
        source = package.get("source")
        raw_manifest_path = package.get("manifest_path")
        if not isinstance(name, str) or not isinstance(version, str):
            raise VerificationError(f"invalid package identity in cargo metadata: {package_id}")
        if not isinstance(raw_manifest_path, str):
            raise VerificationError(f"missing manifest path for {package_id}")
        manifest_path = Path(raw_manifest_path).resolve()

        if manifest_path.is_relative_to(repository_root):
            raise VerificationError(
                f"resolved graph leaked a repository checkout path: {manifest_path}"
            )
        if name == "ignore":
            raise VerificationError("resolved graph contains forbidden package 'ignore'")

        if name == CONSUMER_PACKAGE:
            if manifest_path != expected_consumer_manifest or source is not None:
                raise VerificationError("consumer package did not resolve from its temp root")
            continue

        unpacked = unpacked_packages.get(name)
        if unpacked is not None:
            expected_version = expected_versions[name]
            if version != expected_version:
                raise VerificationError(
                    f"internal package {name} resolved as {version}, expected "
                    f"{expected_version}"
                )
            if source is not None or not manifest_path.is_relative_to(unpacked.resolve()):
                raise VerificationError(
                    f"internal package {name} did not resolve from its unpacked archive: "
                    f"{manifest_path} ({source})"
                )
            if name == TARGET_PACKAGE:
                found_target = True
            continue

        if name == "globset" and source != CRATES_IO_SOURCE:
            raise VerificationError(
                f"globset resolved from a patch or path source: {source or manifest_path}"
            )
        if source != CRATES_IO_SOURCE:
            raise VerificationError(
                f"third-party package {name} {version} did not resolve from crates.io: "
                f"{source or manifest_path}"
            )
        if (name, version, source) not in locked_registry_packages:
            raise VerificationError(
                f"cargo metadata contains an unlocked registry package: "
                f"{name} {version} ({source})"
            )
        if not manifest_path.is_relative_to(registry_source_root.resolve()):
            raise VerificationError(
                f"registry package {name} {version} resolved outside the isolated "
                f"Cargo home: {manifest_path}"
            )

    if not found_target:
        raise VerificationError(f"resolved graph does not contain {TARGET_PACKAGE}")


def validate_consumer_lock(
    lock_path: Path,
    internal_versions: Mapping[str, str],
) -> set[tuple[str, str, str]]:
    document = load_toml(lock_path)
    packages = document.get("package")
    if not isinstance(packages, list):
        raise VerificationError(f"{lock_path}: Cargo.lock contains no packages")

    registry_packages: set[tuple[str, str, str]] = set()
    for package in packages:
        if not isinstance(package, dict):
            raise VerificationError(f"{lock_path}: invalid package entry")
        name = package.get("name")
        version = package.get("version")
        source = package.get("source")
        checksum = package.get("checksum")
        if not isinstance(name, str) or not isinstance(version, str):
            raise VerificationError(f"{lock_path}: invalid package identity")
        if name == "ignore":
            raise VerificationError(f"{lock_path}: forbidden package 'ignore' was resolved")

        if name == CONSUMER_PACKAGE:
            if source is not None:
                raise VerificationError(f"{lock_path}: consumer unexpectedly has a source")
            continue

        if name in internal_versions:
            if version != internal_versions[name] or source is not None:
                raise VerificationError(
                    f"{lock_path}: internal package {name} must come from the unpacked "
                    "archive at the expected version"
                )
            continue

        if name == "globset" and source != CRATES_IO_SOURCE:
            raise VerificationError(f"{lock_path}: globset resolved from a patch")
        if source != CRATES_IO_SOURCE:
            raise VerificationError(
                f"{lock_path}: third-party package {name} {version} has non-crates.io "
                f"source {source}"
            )
        if not is_sha256_checksum(checksum):
            raise VerificationError(
                f"{lock_path}: registry package {name} {version} has an invalid checksum"
            )
        registry_packages.add((name, version, source))
    return registry_packages


def run_full_verification(
    *,
    cargo: str,
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
) -> None:
    cargo_cwd = configuration_clean_cargo_cwd(workspace_root)
    with tempfile.TemporaryDirectory(
        prefix="unity-asset-search-index-package-", ignore_cleanup_errors=True
    ) as temporary:
        temporary_root = Path(temporary).resolve()
        cargo_home = temporary_root / "cargo-home"
        package_target = temporary_root / "package-target"
        unpack_root = temporary_root / "packages"
        consumer_root = temporary_root / "consumer"
        consumer_target = temporary_root / "consumer-target"
        cargo_home.mkdir()
        unpack_root.mkdir()
        consumer_root.mkdir()
        # Fail closed if a future change runs Cargo below the poisoned ancestor.
        ancestor_config = temporary_root / ".cargo"
        ancestor_config.mkdir()
        (ancestor_config / "config.toml").write_text(
            "invalid Cargo config: this file must never be loaded\n",
            encoding="utf-8",
            newline="\n",
        )

        package_environment = isolated_cargo_environment(cargo_home, package_target)
        unpacked: dict[str, Path] = {}
        source_packages = {package.name: package for package in closure}
        for package in closure:
            command = [
                cargo,
                "package",
                "--manifest-path",
                str(workspace_root / "Cargo.toml"),
                "--package",
                package.name,
                "--locked",
                "--no-verify",
                "--allow-dirty",
            ]
            for name in sorted(unpacked):
                source_root = source_packages[name].directory.resolve()
                command.extend(
                    [
                        "--config",
                        f"patch.crates-io.{name}.path="
                        f"{toml_string(source_root.as_posix())}",
                    ]
                )
            run_visible(
                command,
                cwd=cargo_cwd,
                env=package_environment,
            )
            archive_path = locate_archive(package_target, package)
            package_root = unpack_archive(archive_path, unpack_root, package)
            validate_packaged_manifest(package_root, package)
            unpacked[package.name] = package_root

        target = next(package for package in closure if package.name == TARGET_PACKAGE)
        consumer_manifest = create_consumer(consumer_root, target, unpacked)
        consumer_environment = isolated_cargo_environment(cargo_home, consumer_target)
        run_visible(
            [cargo, "generate-lockfile", "--manifest-path", str(consumer_manifest)],
            cwd=cargo_cwd,
            env=consumer_environment,
        )

        versions = {package.name: package.version for package in closure}
        locked_registry_packages = validate_consumer_lock(
            consumer_root / "Cargo.lock", versions
        )
        metadata_text = run_captured(
            [
                cargo,
                "metadata",
                "--manifest-path",
                str(consumer_manifest),
                "--format-version",
                "1",
                "--locked",
            ],
            cwd=cargo_cwd,
            env=consumer_environment,
        )
        validate_resolved_graph(
            metadata_text,
            workspace_root,
            consumer_manifest,
            unpacked,
            versions,
            locked_registry_packages,
            cargo_home / "registry" / "src",
        )
        run_visible(
            [
                cargo,
                "check",
                "--manifest-path",
                str(consumer_manifest),
                "--locked",
                "--all-targets",
            ],
            cwd=cargo_cwd,
            env=consumer_environment,
        )


def main() -> int:
    args = parse_args()
    workspace_root = args.workspace_root.resolve()
    root_manifest = workspace_root / "Cargo.toml"
    root_document = load_toml(root_manifest)
    reject_root_source_overrides(root_document, root_manifest)

    cargo_cwd = configuration_clean_cargo_cwd(workspace_root)
    with tempfile.TemporaryDirectory(
        prefix="unity-asset-search-index-preflight-", ignore_cleanup_errors=True
    ) as temporary:
        temporary_root = Path(temporary).resolve()
        preflight_environment = isolated_cargo_environment(
            temporary_root / "cargo-home", temporary_root / "target"
        )
        metadata_text = run_captured(
            [
                args.cargo,
                "metadata",
                "--manifest-path",
                str(root_manifest),
                "--format-version",
                "1",
                "--no-deps",
                "--locked",
            ],
            cwd=cargo_cwd,
            env=preflight_environment,
        )
    packages = discover_workspace_packages(metadata_text)
    closure = production_closure(TARGET_PACKAGE, packages)
    validate_source_dependencies(closure, packages)

    names = ", ".join(package.name for package in closure)
    print(f"preflight passed; package order: {names}")
    if args.preflight_only:
        return 0

    run_full_verification(
        cargo=args.cargo,
        workspace_root=workspace_root,
        closure=closure,
    )
    print(
        "search-index package verification passed: no ignore dependency, no "
        "patched globset, no repository path dependency, and all third-party "
        "packages resolved from official crates.io"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
