"""Verify normalized package archives in isolated Cargo workspaces.

This module owns Cargo process isolation, portable archive extraction, resolved
source validation, and independent external-consumer compilation.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path
from typing import Any, Iterator, Mapping, Sequence
from urllib.parse import urlsplit

from release_path_safety import (
    ReleasePathSafetyError,
    portable_path_alias_key,
)
from release_binary_identity import version_report
from release_contract import GIT_OBJECT_PATTERN
from release_subprocess import (
    BoundedCommandCleanupError,
    BoundedCommandTimeout,
    run_bounded_command_captured,
    run_bounded_command_visible,
)
from workspace_package_contract import (
    CRATES_IO_SOURCE,
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
BUILD_TARGET_PATTERN = re.compile(r"[A-Za-z0-9_.-]+")
PACKAGE_CONSUMER_FIXTURE_ROOT = (
    Path(__file__).resolve().parent.parent / "integration" / "package-consumers"
)
PUBLIC_API_FIXTURE_ROOT = PACKAGE_CONSUMER_FIXTURE_ROOT / "public-api"
def command_text(command: Sequence[str]) -> str:
    if os.name == "nt":
        return subprocess.list2cmdline(list(command))
    return shlex.join(command)


def run_visible(
    command: Sequence[str], *, cwd: Path, env: Mapping[str, str]
) -> None:
    print(f"$ {command_text(command)}", flush=True)
    try:
        returncode = run_bounded_command_visible(
            command,
            cwd=cwd,
            env=env,
            timeout_seconds=CARGO_COMMAND_TIMEOUT_SECONDS,
        )
    except BoundedCommandTimeout as error:
        raise VerificationError(
            f"command timed out after {CARGO_COMMAND_TIMEOUT_SECONDS}s: "
            f"{command_text(command)}"
        ) from error
    except BoundedCommandCleanupError as error:
        raise VerificationError(
            f"command cleanup failed: {command_text(command)}: {error}"
        ) from error
    if returncode != 0:
        raise VerificationError(
            f"command failed with exit code {returncode}: "
            f"{command_text(command)}"
        )


def run_captured(
    command: Sequence[str], *, cwd: Path, env: Mapping[str, str]
) -> str:
    print(f"$ {command_text(command)}", flush=True)
    try:
        result = run_bounded_command_captured(
            command,
            cwd=cwd,
            env=env,
            timeout_seconds=CARGO_COMMAND_TIMEOUT_SECONDS,
        )
    except BoundedCommandTimeout as error:
        raise VerificationError(
            f"command timed out after {CARGO_COMMAND_TIMEOUT_SECONDS}s: "
            f"{command_text(command)}"
        ) from error
    except BoundedCommandCleanupError as error:
        raise VerificationError(
            f"command cleanup failed: {command_text(command)}: {error}"
        ) from error
    stdout = result.stdout or ""
    stderr = result.stderr or ""
    if result.returncode != 0:
        details = "\n".join(
            part.rstrip() for part in (stdout, stderr) if part.strip()
        )
        suffix = f"\n{details}" if details else ""
        raise VerificationError(
            f"command failed with exit code {result.returncode}: "
            f"{command_text(command)}{suffix}"
        )
    if stderr.strip():
        print(stderr.rstrip(), file=sys.stderr)
    return stdout


def repository_source_commit(
    repository_root: Path, *, cwd: Path, environment: Mapping[str, str]
) -> str:
    """Return the exact source commit embedded into verified binary packages."""

    commit = run_captured(
        ["git", "-C", str(repository_root), "rev-parse", "--verify", "HEAD"],
        cwd=cwd,
        env=environment,
    ).strip()
    if GIT_OBJECT_PATTERN.fullmatch(commit) is None:
        raise VerificationError(
            f"repository HEAD is not a full lowercase Git commit ID: {commit!r}"
        )
    return commit


def cargo_host_target(
    cargo: str, *, cwd: Path, environment: Mapping[str, str]
) -> str:
    """Return the host target used by configuration-isolated binary installs."""

    version = run_captured([cargo, "-vV"], cwd=cwd, env=environment)
    hosts = [line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: ")]
    if len(hosts) != 1 or BUILD_TARGET_PATTERN.fullmatch(hosts[0]) is None:
        raise VerificationError(f"cargo -vV returned an invalid host target: {version!r}")
    return hosts[0]


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
        "RUSTUP_TOOLCHAIN",
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
) -> tuple[str, ...]:
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
    if not (member.isdir() or member.isfile()):
        raise VerificationError(
            f"{archive_path}: links and special files are forbidden: {member.name}"
        )
    try:
        return portable_path_alias_key(parts, "archive member")
    except ReleasePathSafetyError as error:
        raise VerificationError(
            f"{archive_path}: archive member is not portable: {member.name}"
        ) from error

def unpack_archive(archive_path: Path, unpack_root: Path, package: WorkspacePackage) -> Path:
    expected_root_name = f"{package.name}-{package.version}"
    package_root = unpack_root / expected_root_name
    try:
        with tarfile.open(archive_path, mode="r:gz") as archive:
            members = archive.getmembers()
            portable_paths: set[tuple[str, ...]] = set()
            for member in members:
                portable_key = validate_archive_member(
                    archive_path, member, expected_root_name
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
) -> Iterator[tuple[str, Mapping[str, Any]]]:
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
            yield f"{location}.{alias}", values


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

    for location, values in raw_packaged_dependencies(document, manifest_path):
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

def validate_package_vcs_info(
    package_root: Path,
    *,
    expected_source_commit: str,
) -> None:
    """Bind an unpacked Cargo archive to the repository commit that produced it."""

    vcs_path = package_root / ".cargo_vcs_info.json"
    try:
        document = json.loads(vcs_path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise VerificationError(
            f"{vcs_path}: missing package VCS identity"
        ) from error
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"{vcs_path}: invalid package VCS identity: {error}") from error

    git = document.get("git") if isinstance(document, dict) else None
    if not isinstance(git, dict):
        raise VerificationError(f"{vcs_path}: package VCS identity has no git object")
    source_commit = git.get("sha1")
    if not isinstance(source_commit, str) or GIT_OBJECT_PATTERN.fullmatch(source_commit) is None:
        raise VerificationError(f"{vcs_path}: invalid git.sha1 in package VCS identity")
    if source_commit != expected_source_commit:
        raise VerificationError(
            f"{vcs_path}: package commit {source_commit} does not match source commit "
            f"{expected_source_commit}"
        )
    dirty = git.get("dirty", False)
    if not isinstance(dirty, bool):
        raise VerificationError(f"{vcs_path}: invalid git.dirty in package VCS identity")
    if dirty:
        raise VerificationError(f"{vcs_path}: package VCS identity marks the package dirty")


def toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=True)


def render_consumer_dependency(
    version: str,
    feature_names: Sequence[str],
    *,
    default_features: bool = True,
) -> str:
    if not feature_names and default_features:
        return toml_string("=" + version)
    rendered_features = ", ".join(toml_string(feature) for feature in feature_names)
    return (
        "{ "
        f"version = {toml_string('=' + version)}, "
        f"default-features = {'true' if default_features else 'false'}, "
        f"features = [{rendered_features}] "
        "}"
    )


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


def consumer_source(
    target: WorkspacePackage,
    profile: str,
    *,
    fixture_root: Path = PUBLIC_API_FIXTURE_ROOT,
) -> str:
    if target.library_target_name is None:
        raise VerificationError(f"{target.name} has no library target for a consumer")
    fixture_path = fixture_root / target.name / f"{profile}.rs"
    try:
        source = fixture_path.read_text(encoding="utf-8")
    except OSError as error:
        raise VerificationError(
            f"missing public API consumer fixture for {target.name}/{profile}: "
            f"{fixture_path}"
        ) from error
    if not source.strip():
        raise VerificationError(
            f"public API consumer fixture is empty for {target.name}/{profile}: "
            f"{fixture_path}"
        )
    return source


def write_consumer_package(
    consumer_root: Path,
    *,
    name: str,
    dependency_name: str,
    dependency: str,
    source: str,
) -> Path:
    source_root = consumer_root / "src"
    source_root.mkdir(parents=True)
    manifest = "\n".join(
        [
            "[package]",
            f"name = {toml_string(name)}",
            'version = "0.0.0"',
            'edition = "2024"',
            "publish = false",
            "",
            "[dependencies]",
            f"{dependency_name} = {dependency}",
            "",
        ]
    )
    manifest_path = consumer_root / "Cargo.toml"
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    (source_root / "lib.rs").write_text(
        source,
        encoding="utf-8",
        newline="\n",
    )
    return manifest_path


def create_consumer(
    consumer_root: Path,
    target: WorkspacePackage,
    profile: str,
    feature_names: Sequence[str],
    *,
    default_features: bool = True,
    fixture_root: Path = PUBLIC_API_FIXTURE_ROOT,
) -> tuple[str, Path]:
    name = consumer_package_name(target, profile)
    manifest_path = write_consumer_package(
        consumer_root,
        name=name,
        dependency_name=target.name,
        dependency=render_consumer_dependency(
            target.version,
            feature_names,
            default_features=default_features,
        ),
        source=consumer_source(target, profile, fixture_root=fixture_root),
    )
    return name, manifest_path


def create_consumer_suite(
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
    unpacked_packages: Mapping[str, Path],
) -> tuple[
    Path,
    dict[str, Path],
    set[str],
]:
    """Create one resolver graph while preserving per-consumer feature checks."""

    packages = {package.name: package for package in closure}
    consumer_manifests: dict[str, Path] = {}
    required_internal: set[str] = set()
    for target in closure:
        if not target.is_library:
            continue
        consumer_name = consumer_package_name(target, "default")
        name, manifest_path = create_consumer(
            workspace_root / consumer_name,
            target,
            "default",
            (),
        )
        consumer_manifests[name] = manifest_path
        required_internal.add(target.name)

    if not consumer_manifests:
        raise VerificationError("default consumer suite has no library targets")

    for profile in validate_documented_feature_profiles(packages):
        target = packages[profile.package]
        consumer_name = consumer_package_name(target, profile.name)
        name, manifest_path = create_consumer(
            workspace_root / consumer_name,
            target,
            profile.name,
            profile.features,
            default_features=profile.default_features,
        )
        consumer_manifests[name] = manifest_path
        required_internal.add(target.name)

    required_packages = {
        package.name: unpacked_packages[package.name]
        for package in production_closure(sorted(required_internal), packages)
    }

    workspace_manifest = write_workspace_manifest(
        workspace_root,
        [path.parent for path in consumer_manifests.values()],
        required_packages,
    )
    return (
        workspace_manifest,
        consumer_manifests,
        required_internal,
    )


def validate_resolved_workspace(
    metadata_text: str,
    local_manifests: Mapping[str, Path],
    unpacked_packages: Mapping[str, Path],
    expected_versions: Mapping[str, str],
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

    resolved_local_manifests = {
        name: path.resolve() for name, path in local_manifests.items()
    }
    resolved_unpacked_packages = {
        name: path.resolve() for name, path in unpacked_packages.items()
    }
    resolved_registry_source_root = registry_source_root.resolve()
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

        local_manifest = resolved_local_manifests.get(name)
        if local_manifest is not None:
            if manifest_path != local_manifest or source is not None:
                raise VerificationError(
                    f"local verification package {name} did not resolve from its temp root"
                )
            seen_local.add(name)
            continue

        unpacked = resolved_unpacked_packages.get(name)
        if unpacked is not None:
            expected_version = expected_versions[name]
            if version != expected_version:
                raise VerificationError(
                    f"internal package {name} resolved as {version}, expected "
                    f"{expected_version}"
                )
            if source is not None or not manifest_path.is_relative_to(unpacked):
                raise VerificationError(
                    f"internal package {name} did not resolve from its unpacked archive: "
                    f"{manifest_path} ({source})"
                )
            seen_internal.add(name)
            continue

        if source != CRATES_IO_SOURCE:
            raise VerificationError(
                f"third-party package {name} {version} did not resolve from crates.io: "
                f"{source or manifest_path}"
            )
        if not manifest_path.is_relative_to(resolved_registry_source_root):
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


def verify_temporary_workspace(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    workspace_manifest: Path,
    local_manifests: Mapping[str, Path],
    unpacked_packages: Mapping[str, Path],
    expected_versions: Mapping[str, str],
    required_internal: set[str],
    registry_source_root: Path,
    all_features: bool = False,
) -> None:
    metadata_command = [
        cargo,
        "metadata",
        "--manifest-path",
        str(workspace_manifest),
        "--format-version",
        "1",
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
        local_manifests,
        unpacked_packages,
        expected_versions,
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


def verify_binary_packages_standalone(
    *,
    cargo: str,
    cargo_cwd: Path,
    environment: Mapping[str, str],
    workspace_root: Path,
    packages: Mapping[str, WorkspacePackage],
    archive_paths: Mapping[str, Path],
    expected_versions: Mapping[str, str],
    expected_source_commit: str,
    expected_build_target: str,
    registry_source_root: Path,
) -> None:
    binary_packages = tuple(
        package for package in packages.values() if package.binary_target_names
    )
    if not binary_packages:
        raise VerificationError("release closure has no binary packages")
    closure = production_closure(
        [package.name for package in binary_packages], packages
    )
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
        [unpacked_packages[package.name] for package in binary_packages],
        unpacked_packages,
    )
    verify_temporary_workspace(
        cargo=cargo,
        cargo_cwd=cargo_cwd,
        environment=environment,
        workspace_manifest=manifest,
        local_manifests={},
        unpacked_packages=unpacked_packages,
        expected_versions=expected_versions,
        required_internal=set(unpacked_packages),
        registry_source_root=registry_source_root,
        all_features=False,
    )
    install_root = workspace_root / "install-root"
    executable_suffix = ".exe" if os.name == "nt" else ""
    for package in binary_packages:
        run_visible(
            [
                cargo,
                "install",
                "--path",
                str(unpacked_packages[package.name]),
                "--root",
                str(install_root),
                "--locked",
            ],
            cwd=cargo_cwd,
            env=environment,
        )
        for target_name in package.binary_target_names:
            executable = install_root / "bin" / f"{target_name}{executable_suffix}"
            if executable.is_symlink() or not executable.is_file():
                raise VerificationError(
                    "installed binary target is missing or not a regular file: "
                    f"{executable}"
                )
            actual = run_captured(
                [str(executable), "--version"], cwd=cargo_cwd, env=environment
            )
            expected = (
                f"{target_name} "
                f"{version_report(package.name, package.version, expected_source_commit, expected_build_target)}\n"
            )
            if actual != expected:
                raise VerificationError(
                    f"installed binary {target_name} reported unexpected build identity: "
                    f"expected {expected.strip()!r}, got {actual.strip()!r}"
                )


def run_verification(
    *,
    cargo: str,
    workspace_root: Path,
    closure: Sequence[WorkspacePackage],
    verify_binaries: bool,
) -> None:
    cargo_cwd = configuration_clean_cargo_cwd(workspace_root)
    with tempfile.TemporaryDirectory(
        prefix="unity-asset-workspace-package-", ignore_cleanup_errors=True
    ) as temporary:
        temporary_root = Path(temporary).resolve()
        cargo_home = temporary_root / "cargo-home"
        package_target = temporary_root / "package-target"
        archive_workspace_root = temporary_root / "archives"
        unpack_root = archive_workspace_root / "packages"
        consumer_workspace_root = temporary_root / "consumers"
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
        source_commit = (
            repository_source_commit(
                workspace_root, cwd=cargo_cwd, environment=package_environment
            )
            if verify_binaries
            else None
        )
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
            ]
            if not verify_binaries:
                command.append("--allow-dirty")
            package_dependencies = production_closure(
                [package.name], source_packages
            )
            for dependency in package_dependencies:
                if dependency.name == package.name:
                    continue
                source_root = dependency.directory.resolve()
                command.extend(
                    [
                        "--config",
                        f"patch.crates-io.{dependency.name}.path="
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
            if source_commit is not None:
                validate_package_vcs_info(
                    package_root,
                    expected_source_commit=source_commit,
                )
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
            workspace_manifest=archive_manifest,
            local_manifests={},
            unpacked_packages=unpacked,
            expected_versions=versions,
            required_internal=set(unpacked),
            registry_source_root=cargo_home / "registry" / "src",
            all_features=True,
        )
        check_temporary_workspace(
            cargo=cargo,
            cargo_cwd=cargo_cwd,
            environment=verification_environment,
            workspace_manifest=archive_manifest,
            all_features=True,
        )

        if source_commit is not None:
            # `cargo install --path` discovers configuration from the package path,
            # so binary probes need a root outside the deliberately poisoned tree.
            with tempfile.TemporaryDirectory(
                prefix="unity-asset-binary-package-", ignore_cleanup_errors=True
            ) as binary_temporary:
                verify_binary_packages_standalone(
                    cargo=cargo,
                    cargo_cwd=cargo_cwd,
                    environment=verification_environment,
                    workspace_root=Path(binary_temporary).resolve() / "standalone",
                    packages=source_packages,
                    archive_paths=archive_paths,
                    expected_versions=versions,
                    expected_source_commit=source_commit,
                    expected_build_target=cargo_host_target(
                        cargo,
                        cwd=cargo_cwd,
                        environment=verification_environment,
                    ),
                    registry_source_root=cargo_home / "registry" / "src",
                )

        (
            consumer_manifest,
            consumer_manifests,
            required_internal,
        ) = create_consumer_suite(
            consumer_workspace_root,
            closure,
            unpacked,
        )
        verify_temporary_workspace(
            cargo=cargo,
            cargo_cwd=cargo_cwd,
            environment=verification_environment,
            workspace_manifest=consumer_manifest,
            local_manifests=consumer_manifests,
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
            consumer_names=tuple(consumer_manifests),
        )
