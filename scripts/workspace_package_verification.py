"""Verify normalized package archives in isolated Cargo workspaces.

This module owns Cargo process isolation, portable archive extraction, resolved
source validation, and independent external-consumer compilation.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from typing import Any, Iterator, Mapping, Sequence
from urllib.parse import urlsplit

from release_path_safety import (
    ReleasePathSafetyError,
    portable_path_alias_key,
    portable_path_component_key,
)
from workspace_package_contract import (
    CRATES_IO_SOURCE,
    DocumentedFeatureProfile,
    VerificationError,
    WorkspacePackage,
    dependency_tables,
    load_toml,
    parse_cargo_metadata,
    production_closure,
    validate_documented_feature_profiles,
)


CONSUMER_PACKAGE_PREFIX = "unity-asset-package-consumer"
CARGO_CONFIG_NAMES = ("config.toml", "config")
CARGO_COMMAND_TIMEOUT_SECONDS = 1_200


def command_text(command: Sequence[str]) -> str:
    if os.name == "nt":
        return subprocess.list2cmdline(list(command))
    return shlex.join(command)


def run_visible(
    command: Sequence[str], *, cwd: Path, env: Mapping[str, str]
) -> None:
    print(f"$ {command_text(command)}", flush=True)
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            check=False,
            timeout=CARGO_COMMAND_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise VerificationError(
            f"command timed out after {CARGO_COMMAND_TIMEOUT_SECONDS}s: "
            f"{command_text(command)}"
        ) from error
    if result.returncode != 0:
        raise VerificationError(
            f"command failed with exit code {result.returncode}: "
            f"{command_text(command)}"
        )


def run_captured(
    command: Sequence[str], *, cwd: Path, env: Mapping[str, str]
) -> str:
    print(f"$ {command_text(command)}", flush=True)
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            check=False,
            text=True,
            encoding="utf-8",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=CARGO_COMMAND_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise VerificationError(
            f"command timed out after {CARGO_COMMAND_TIMEOUT_SECONDS}s: "
            f"{command_text(command)}"
        ) from error
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
        try:
            portable_path_component_key(component, "archive member")
        except ReleasePathSafetyError as error:
            raise VerificationError(
                f"{archive_path}: archive member is not portable: {member.name}"
            ) from error
    if not (member.isdir() or member.isfile()):
        raise VerificationError(
            f"{archive_path}: links and special files are forbidden: {member.name}"
        )
    return PurePosixPath(*parts)

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
                try:
                    portable_key = portable_path_alias_key(
                        relative.parts, "archive member"
                    )
                except ReleasePathSafetyError as error:
                    raise VerificationError(
                        f"{archive_path}: archive member is not portable: {member.name}"
                    ) from error
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


def relative_toml_path(path: Path, root: Path) -> str:
    return Path(os.path.relpath(path, root)).as_posix()


def write_workspace_manifest(
    workspace_root: Path,
    member_roots: Sequence[Path],
    unpacked_packages: Mapping[str, Path],
) -> Path:
    workspace_root.mkdir(parents=True, exist_ok=True)
    members = [toml_string(relative_toml_path(member, workspace_root)) for member in member_roots]
    patch_lines = [
        f"{name} = {{ path = {toml_string(relative_toml_path(path, workspace_root))} }}"
        for name, path in sorted(unpacked_packages.items())
    ]
    manifest = "\n".join(
        [
            "[workspace]",
            "resolver = \"3\"",
            "members = [",
            *(f"    {member}," for member in members),
            "]",
            "",
            "[patch.crates-io]",
            *patch_lines,
            "",
        ]
    )
    manifest_path = workspace_root / "Cargo.toml"
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    return manifest_path


def consumer_package_name(target: WorkspacePackage, profile: str) -> str:
    return f"{CONSUMER_PACKAGE_PREFIX}-{profile}-{target.name}"


def consumer_source(target: WorkspacePackage) -> str:
    crate_name = target.library_target_name
    if crate_name is None:
        raise VerificationError(f"{target.name} has no library target for a consumer")
    return f"//! External package-consumer compilation probe.\n\nuse {crate_name} as _;\n"


def create_consumer(
    consumer_root: Path,
    target: WorkspacePackage,
    profile: str,
    feature_names: Sequence[str],
    *,
    default_features: bool = True,
) -> tuple[str, Path]:
    source_root = consumer_root / "src"
    source_root.mkdir(parents=True)
    name = consumer_package_name(target, profile)
    dependency = toml_string("=" + target.version)
    if feature_names or not default_features:
        rendered_features = ", ".join(toml_string(feature) for feature in feature_names)
        dependency = (
            "{ "
            f"version = {toml_string('=' + target.version)}, "
            f"default-features = {'true' if default_features else 'false'}, "
            f"features = [{rendered_features}] "
            "}"
        )
    manifest = "\n".join(
        [
            "[package]",
            f"name = {toml_string(name)}",
            'version = "0.0.0"',
            'edition = "2024"',
            "publish = false",
            "",
            "[dependencies]",
            f"{target.name} = {dependency}",
            "",
        ]
    )
    manifest_path = consumer_root / "Cargo.toml"
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    (source_root / "lib.rs").write_text(
        consumer_source(target),
        encoding="utf-8",
        newline="\n",
    )
    return name, manifest_path


def create_consumer_workspace(
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
    unpacked_packages: Mapping[str, Path],
    profile: str,
) -> tuple[Path, dict[str, Path], set[str]]:
    consumers: dict[str, Path] = {}
    required_internal: set[str] = set()
    for target in closure:
        if not target.is_library:
            continue
        feature_names = target.feature_names if profile == "all-features" else ()
        if profile == "all-features" and not feature_names:
            continue
        consumer_name = consumer_package_name(target, profile)
        name, manifest_path = create_consumer(
            workspace_root / consumer_name,
            target,
            profile,
            feature_names,
            default_features=profile != "all-features",
        )
        consumers[name] = manifest_path
        required_internal.add(target.name)
    if not consumers:
        raise VerificationError(f"consumer profile {profile} has no library targets")
    manifest_path = write_workspace_manifest(
        workspace_root,
        [path.parent for path in consumers.values()],
        unpacked_packages,
    )
    return manifest_path, consumers, required_internal


def create_documented_feature_consumer_workspace(
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
    unpacked_packages: Mapping[str, Path],
    profile: DocumentedFeatureProfile,
) -> tuple[Path, dict[str, Path], set[str]]:
    """Create one isolated consumer for an exact documented feature profile."""

    if profile.target_kind != "dependency" or profile.target_name is not None:
        raise VerificationError(
            f"documented feature profile {profile.name} is not a dependency profile"
        )
    packages = {package.name: package for package in closure}
    target = packages.get(profile.package)
    if target is None:
        raise VerificationError(
            f"documented feature profile {profile.name} package is outside the release closure"
        )
    consumer_name = consumer_package_name(target, profile.name)
    name, manifest_path = create_consumer(
        workspace_root / consumer_name,
        target,
        profile.name,
        profile.features,
        default_features=profile.default_features,
    )
    consumers = {name: manifest_path}
    workspace_manifest = write_workspace_manifest(
        workspace_root,
        [manifest_path.parent],
        unpacked_packages,
    )
    return workspace_manifest, consumers, {target.name}


def create_documented_example_workspace(
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
    unpacked_packages: Mapping[str, Path],
    profile: DocumentedFeatureProfile,
) -> tuple[Path, WorkspacePackage, set[str]]:
    """Create a single-member workspace for one packaged documented example."""

    if profile.target_kind != "example" or profile.target_name is None:
        raise VerificationError(
            f"documented feature profile {profile.name} is not an example profile"
        )
    packages = {package.name: package for package in closure}
    target = packages.get(profile.package)
    if target is None:
        raise VerificationError(
            f"documented feature profile {profile.name} package is outside the release closure"
        )
    if profile.target_name not in target.example_target_names:
        raise VerificationError(
            f"documented feature profile {profile.name} targets a missing example: "
            f"{profile.target_name}"
        )
    target_root = unpacked_packages.get(target.name)
    if target_root is None:
        raise VerificationError(
            f"documented feature profile {profile.name} has no unpacked package"
        )
    dependency_closure = production_closure([target.name], packages)
    required_internal = {package.name for package in dependency_closure}
    manifest = write_workspace_manifest(
        workspace_root,
        [target_root],
        unpacked_packages,
    )
    return manifest, target, required_internal


def is_sha256_checksum(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def validate_resolved_workspace(
    metadata_text: str,
    repository_root: Path,
    local_manifests: Mapping[str, Path],
    unpacked_packages: Mapping[str, Path],
    expected_versions: Mapping[str, str],
    locked_registry_packages: set[tuple[str, str, str]],
    registry_source_root: Path,
    required_internal: set[str],
) -> None:
    metadata = parse_cargo_metadata(metadata_text)
    raw_packages = metadata.get("packages")
    resolve = metadata.get("resolve")
    if not isinstance(raw_packages, list) or not isinstance(resolve, dict):
        raise VerificationError("cargo metadata did not return packages")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list):
        raise VerificationError("cargo metadata resolve graph is incomplete")
    by_id = {
        package.get("id"): package
        for package in raw_packages
        if isinstance(package, dict) and isinstance(package.get("id"), str)
    }
    package_ids: set[str] = set()
    for node in nodes:
        if not isinstance(node, dict) or not isinstance(node.get("id"), str):
            raise VerificationError("cargo metadata contains an invalid resolve node")
        package_ids.add(node["id"])

    seen_internal: set[str] = set()
    seen_local: set[str] = set()
    for package_id in package_ids:
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

        local_manifest = local_manifests.get(name)
        if local_manifest is not None:
            if manifest_path != local_manifest.resolve() or source is not None:
                raise VerificationError(
                    f"local verification package {name} did not resolve from its temp root"
                )
            seen_local.add(name)
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
            seen_internal.add(name)
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

    missing_local = sorted(set(local_manifests) - seen_local)
    if missing_local:
        raise VerificationError(
            f"resolved graph does not contain local verification packages: {missing_local}"
        )
    missing_internal = sorted(required_internal - seen_internal)
    if missing_internal:
        raise VerificationError(
            f"resolved graph does not contain required unpacked packages: {missing_internal}"
        )


def validate_consumer_lock(
    lock_path: Path,
    internal_versions: Mapping[str, str],
    local_package_names: set[str] | None = None,
) -> set[tuple[str, str, str]]:
    local_package_names = local_package_names or set()
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

        if name in local_package_names:
            if source is not None:
                raise VerificationError(
                    f"{lock_path}: local verification package {name} unexpectedly has a source"
                )
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


def verify_temporary_workspace(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    repository_root: Path,
    workspace_manifest: Path,
    local_manifests: Mapping[str, Path],
    unpacked_packages: Mapping[str, Path],
    expected_versions: Mapping[str, str],
    required_internal: set[str],
    registry_source_root: Path,
    all_features: bool = False,
) -> None:
    run_visible(
        [cargo, "generate-lockfile", "--manifest-path", str(workspace_manifest)],
        cwd=cargo_cwd,
        env=environment,
    )
    locked_registry_packages = validate_consumer_lock(
        workspace_manifest.parent / "Cargo.lock",
        expected_versions,
        set(local_manifests),
    )
    metadata_command = [
        cargo,
        "metadata",
        "--manifest-path",
        str(workspace_manifest),
        "--format-version",
        "1",
        "--locked",
    ]
    if all_features:
        metadata_command.append("--all-features")
    metadata_text = run_captured(
        metadata_command,
        cwd=cargo_cwd,
        env=environment,
    )
    validate_resolved_workspace(
        metadata_text,
        repository_root,
        local_manifests,
        unpacked_packages,
        expected_versions,
        locked_registry_packages,
        registry_source_root,
        required_internal,
    )


def check_temporary_workspace(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    workspace_manifest: Path,
    all_features: bool,
    target_arguments: Sequence[str] = ("--lib", "--bins", "--examples"),
) -> None:
    command = [
        cargo,
        "check",
        "--manifest-path",
        str(workspace_manifest),
        "--workspace",
    ]
    command.extend(target_arguments)
    command.append("--locked")
    if all_features:
        command.append("--all-features")
    run_visible(command, cwd=cargo_cwd, env=environment)


def check_consumer_packages(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    workspace_manifest: Path,
    consumer_names: Sequence[str],
) -> None:
    for consumer_name in sorted(consumer_names):
        run_visible(
            [
                cargo,
                "check",
                "--manifest-path",
                str(workspace_manifest),
                "--package",
                consumer_name,
                "--lib",
                "--locked",
            ],
            cwd=cargo_cwd,
            env=environment,
        )


def check_documented_example(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    workspace_manifest: Path,
    profile: DocumentedFeatureProfile,
) -> None:
    """Compile exactly one documented packaged example without feature unification."""

    if profile.target_kind != "example" or profile.target_name is None:
        raise VerificationError(
            f"documented feature profile {profile.name} is not an example profile"
        )
    command = [
        cargo,
        "check",
        "--manifest-path",
        str(workspace_manifest),
        "--package",
        profile.package,
        "--example",
        profile.target_name,
        "--locked",
    ]
    if not profile.default_features:
        command.append("--no-default-features")
    if profile.features:
        command.extend(["--features", ",".join(profile.features)])
    run_visible(command, cwd=cargo_cwd, env=environment)


def verify_binary_package_standalone(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    repository_root: Path,
    workspace_root: Path,
    package: WorkspacePackage,
    packages: Mapping[str, WorkspacePackage],
    archive_paths: Mapping[str, Path],
    expected_versions: Mapping[str, str],
    registry_source_root: Path,
) -> None:
    closure = production_closure([package.name], packages)
    package_root = workspace_root / "packages"
    package_root.mkdir(parents=True)
    unpacked_packages = {
        dependency.name: unpack_archive(
            archive_paths[dependency.name], package_root, dependency
        )
        for dependency in closure
    }
    manifest = write_workspace_manifest(
        workspace_root,
        [unpacked_packages[package.name]],
        unpacked_packages,
    )
    required_internal = {package.name}
    verify_temporary_workspace(
        cargo=cargo,
        cargo_cwd=cargo_cwd,
        environment=environment,
        repository_root=repository_root,
        workspace_manifest=manifest,
        local_manifests={},
        unpacked_packages=unpacked_packages,
        expected_versions=expected_versions,
        required_internal=required_internal,
        registry_source_root=registry_source_root,
        all_features=False,
    )
    verify_temporary_workspace(
        cargo=cargo,
        cargo_cwd=cargo_cwd,
        environment=environment,
        repository_root=repository_root,
        workspace_manifest=manifest,
        local_manifests={},
        unpacked_packages=unpacked_packages,
        expected_versions=expected_versions,
        required_internal=required_internal,
        registry_source_root=registry_source_root,
        all_features=True,
    )
    check_temporary_workspace(
        cargo=cargo,
        cargo_cwd=cargo_cwd,
        environment=environment,
        workspace_manifest=manifest,
        all_features=False,
        target_arguments=("--bins",),
    )
    check_temporary_workspace(
        cargo=cargo,
        cargo_cwd=cargo_cwd,
        environment=environment,
        workspace_manifest=manifest,
        all_features=True,
        target_arguments=("--bins",),
    )
    run_visible(
        [
            cargo,
            "install",
            "--path",
            str(unpacked_packages[package.name]),
            "--root",
            str(workspace_root / "install-root"),
            "--locked",
            "--force",
        ],
        cwd=cargo_cwd,
        env=environment,
    )


def run_full_verification(
    *,
    cargo: str,
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
) -> None:
    cargo_cwd = configuration_clean_cargo_cwd(workspace_root)
    with tempfile.TemporaryDirectory(
        prefix="unity-asset-workspace-package-", ignore_cleanup_errors=True
    ) as temporary, tempfile.TemporaryDirectory(
        prefix="unity-asset-binary-package-", ignore_cleanup_errors=True
    ) as binary_temporary:
        temporary_root = Path(temporary).resolve()
        binary_temporary_root = Path(binary_temporary).resolve()
        cargo_home = temporary_root / "cargo-home"
        package_target = temporary_root / "package-target"
        archive_workspace_root = temporary_root / "archives"
        unpack_root = archive_workspace_root / "packages"
        consumer_workspace_root = temporary_root / "consumers"
        # `cargo install --path` discovers configuration from the package path,
        # so binary probes need a root outside the deliberately poisoned tree.
        standalone_workspace_root = binary_temporary_root / "standalone"
        verification_target = temporary_root / "verification-target"
        cargo_home.mkdir()
        unpack_root.mkdir(parents=True)
        # Fail closed if a future change runs Cargo below the poisoned ancestor.
        ancestor_config = temporary_root / ".cargo"
        ancestor_config.mkdir()
        (ancestor_config / "config.toml").write_text(
            "invalid Cargo config: this file must never be loaded\n",
            encoding="utf-8",
            newline="\n",
        )

        package_environment = isolated_cargo_environment(cargo_home, package_target)
        archive_paths: dict[str, Path] = {}
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
            archive_paths[package.name] = archive_path
            unpacked[package.name] = package_root

        versions = {package.name: package.version for package in closure}
        verification_environment = isolated_cargo_environment(
            cargo_home, verification_target
        )
        archive_manifest = write_workspace_manifest(
            archive_workspace_root,
            list(unpacked.values()),
            unpacked,
        )
        verify_temporary_workspace(
            cargo=cargo,
            cargo_cwd=cargo_cwd,
            environment=verification_environment,
            repository_root=workspace_root,
            workspace_manifest=archive_manifest,
            local_manifests={},
            unpacked_packages=unpacked,
            expected_versions=versions,
            required_internal=set(unpacked),
            registry_source_root=cargo_home / "registry" / "src",
        )
        check_temporary_workspace(
            cargo=cargo,
            cargo_cwd=cargo_cwd,
            environment=verification_environment,
            workspace_manifest=archive_manifest,
            all_features=False,
        )
        check_temporary_workspace(
            cargo=cargo,
            cargo_cwd=cargo_cwd,
            environment=verification_environment,
            workspace_manifest=archive_manifest,
            all_features=True,
        )

        for package in closure:
            if not package.binary_target_names:
                continue
            verify_binary_package_standalone(
                cargo=cargo,
                cargo_cwd=cargo_cwd,
                environment=verification_environment,
                repository_root=workspace_root,
                workspace_root=standalone_workspace_root / package.name,
                package=package,
                packages=source_packages,
                archive_paths=archive_paths,
                expected_versions=versions,
                registry_source_root=cargo_home / "registry" / "src",
            )

        for profile in ("default", "all-features"):
            consumer_manifest, consumers, required_internal = create_consumer_workspace(
                consumer_workspace_root / profile,
                closure,
                unpacked,
                profile,
            )
            verify_temporary_workspace(
                cargo=cargo,
                cargo_cwd=cargo_cwd,
                environment=verification_environment,
                repository_root=workspace_root,
                workspace_manifest=consumer_manifest,
                local_manifests=consumers,
                unpacked_packages=unpacked,
                expected_versions=versions,
                required_internal=required_internal,
                registry_source_root=cargo_home / "registry" / "src",
            )
            check_consumer_packages(
                cargo=cargo,
                cargo_cwd=cargo_cwd,
                environment=verification_environment,
                workspace_manifest=consumer_manifest,
                consumer_names=tuple(consumers),
            )

        for profile in validate_documented_feature_profiles(source_packages):
            profile_root = consumer_workspace_root / profile.name
            if profile.target_kind == "dependency":
                consumer_manifest, consumers, required_internal = (
                    create_documented_feature_consumer_workspace(
                        profile_root,
                        closure,
                        unpacked,
                        profile,
                    )
                )
                verify_temporary_workspace(
                    cargo=cargo,
                    cargo_cwd=cargo_cwd,
                    environment=verification_environment,
                    repository_root=workspace_root,
                    workspace_manifest=consumer_manifest,
                    local_manifests=consumers,
                    unpacked_packages=unpacked,
                    expected_versions=versions,
                    required_internal=required_internal,
                    registry_source_root=cargo_home / "registry" / "src",
                )
                check_consumer_packages(
                    cargo=cargo,
                    cargo_cwd=cargo_cwd,
                    environment=verification_environment,
                    workspace_manifest=consumer_manifest,
                    consumer_names=tuple(consumers),
                )
                continue

            example_manifest, _, required_internal = (
                create_documented_example_workspace(
                    profile_root,
                    closure,
                    unpacked,
                    profile,
                )
            )
            verify_temporary_workspace(
                cargo=cargo,
                cargo_cwd=cargo_cwd,
                environment=verification_environment,
                repository_root=workspace_root,
                workspace_manifest=example_manifest,
                local_manifests={},
                unpacked_packages=unpacked,
                expected_versions=versions,
                required_internal=required_internal,
                registry_source_root=cargo_home / "registry" / "src",
            )
            check_documented_example(
                cargo=cargo,
                cargo_cwd=cargo_cwd,
                environment=verification_environment,
                workspace_manifest=example_manifest,
                profile=profile,
            )
