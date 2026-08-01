#!/usr/bin/env python3
"""Publish the reviewed crate set with bounded, byte-identical recovery."""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Callable, Protocol, Sequence
from urllib.parse import quote

from release_contract import PUBLISHABLE_PACKAGE_NAMES
from release_http import ReleaseHttpError, ReleaseHttpNotFound, download_with_deadline
from release_subprocess import (
    BoundedCommandTimeout,
    credential_free_environment,
    run_bounded_command,
)


VERSION_PATTERN = re.compile(r"\d+\.\d+\.\d+")
COMMAND_TIMEOUT_SECONDS = 900
DOWNLOAD_CONNECT_TIMEOUT_SECONDS = 30
DOWNLOAD_TOTAL_TIMEOUT_SECONDS = 120
MAX_REMOTE_ARCHIVE_BYTES = 128 * 1024 * 1024
MAX_REMOTE_METADATA_BYTES = 4 * 1024 * 1024
RELEASE_TOKEN_ENVIRONMENT_VARIABLE = "UNITY_ASSET_RELEASE_CARGO_TOKEN"


class PublishError(RuntimeError):
    """An actionable crates.io publication failure."""


class RetryablePublishError(PublishError):
    """A publication operation can be retried after bounded backoff."""


class RemoteBytesMismatch(PublishError):
    """A published archive is not the archive prepared from the tagged source."""


class RemotePackageState(Enum):
    """Remote observations used by the two-phase publication state machine."""

    MISSING = "missing"
    EXISTS_UNVERIFIED = "exists-unverified"
    VERIFIED = "verified"


@dataclass(frozen=True)
class PackageRemoteStatus:
    package: str
    state: RemotePackageState


@dataclass(frozen=True)
class PreparedPublication:
    """A fully packaged and remotely preflighted publication batch."""

    version: str
    packages: tuple[PackageRemoteStatus, ...]

    def __post_init__(self) -> None:
        names = tuple(status.package for status in self.packages)
        if len(names) != len(set(names)):
            raise PublishError("prepared publication contains duplicate packages")
        unresolved = [
            status.package
            for status in self.packages
            if status.state is RemotePackageState.EXISTS_UNVERIFIED
        ]
        if unresolved:
            raise PublishError(
                "prepared publication contains unverified existing packages: "
                + ", ".join(unresolved)
            )


class PublishBackend(Protocol):
    def package(self, package: str, version: str) -> None: ...

    def release_exists(self, package: str, version: str) -> bool: ...

    def verify_existing(self, package: str, version: str) -> None: ...

    def publish(self, package: str) -> None: ...


@dataclass
class CargoBackend:
    repository_root: Path
    cargo: str
    token: str

    def run(
        self, command: Sequence[str], *, credentialed: bool = False
    ) -> subprocess.CompletedProcess[str]:
        environment = credential_free_environment()
        if credentialed:
            environment["CARGO_REGISTRY_TOKEN"] = self.token
        try:
            return run_bounded_command(
                command,
                cwd=self.repository_root,
                env=environment,
                timeout_seconds=COMMAND_TIMEOUT_SECONDS,
            )
        except BoundedCommandTimeout as error:
            raise RetryablePublishError(
                f"command timed out after {COMMAND_TIMEOUT_SECONDS}s: {' '.join(command)}"
            ) from error
        except OSError as error:
            raise RetryablePublishError(
                f"cannot start command {' '.join(command)}: {error}"
            ) from error

    def package(self, package: str, version: str) -> None:
        archive = self.archive_path(package, version)
        try:
            archive.unlink(missing_ok=True)
        except OSError as error:
            raise PublishError(
                f"cannot remove stale package archive {archive}: {error}"
            ) from error
        result = self.run(
            [self.cargo, "package", "--locked", "--no-verify", "-p", package]
        )
        if result.returncode != 0:
            raise PublishError(
                f"cannot package {package}: {result.stdout.rstrip() or result.returncode}"
            )
        if archive.is_symlink() or not archive.is_file():
            raise PublishError(f"cargo package did not create a regular archive: {archive}")

    def archive_path(self, package: str, version: str) -> Path:
        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
        if not target_dir.is_absolute():
            target_dir = self.repository_root / target_dir
        return target_dir / "package" / f"{package}-{version}.crate"

    def release_exists(self, package: str, version: str) -> bool:
        package_segment = quote(package, safe="")
        version_segment = quote(version, safe="")
        url = f"https://crates.io/api/v1/crates/{package_segment}/{version_segment}"
        try:
            with tempfile.TemporaryDirectory(
                prefix="unity-asset-crates-io-observation-"
            ) as temporary:
                download_with_deadline(
                    url,
                    Path(temporary) / "release.json",
                    user_agent="unity-asset-release-verifier/1",
                    max_bytes=MAX_REMOTE_METADATA_BYTES,
                    connect_timeout_seconds=DOWNLOAD_CONNECT_TIMEOUT_SECONDS,
                    total_timeout_seconds=DOWNLOAD_TOTAL_TIMEOUT_SECONDS,
                )
        except ReleaseHttpNotFound:
            return False
        except (OSError, ReleaseHttpError) as error:
            raise RetryablePublishError(
                f"cannot determine whether {package} {version} exists on crates.io: {error}"
            ) from error
        return True

    def verify_existing(self, package: str, version: str) -> None:
        archive = self.archive_path(package, version)
        try:
            local_bytes = archive.read_bytes()
        except OSError as error:
            raise PublishError(f"cannot read locally packaged crate {archive}: {error}") from error
        remote_bytes = download_crate(package, version)
        if local_bytes != remote_bytes:
            raise RemoteBytesMismatch(
                f"{package} {version} already exists with different archive bytes"
            )

    def publish(self, package: str) -> None:
        result = self.run(
            [
                self.cargo,
                "publish",
                "--locked",
                "--no-verify",
                "--registry",
                "crates-io",
                "-p",
                package,
            ],
            credentialed=True,
        )
        if result.returncode != 0:
            raise RetryablePublishError(
                f"cargo publish failed for {package}: {result.stdout.rstrip() or result.returncode}"
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Publish the reviewed workspace crate set in dependency order."
    )
    parser.add_argument(
        "--repository-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
    )
    parser.add_argument("--cargo", default=os.environ.get("CARGO", "cargo"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--packages", nargs="+", required=True)
    parser.add_argument("--max-attempts", type=int, default=10)
    parser.add_argument("--retry-delay-seconds", type=float, default=30.0)
    return parser.parse_args()


def download_crate(package: str, version: str) -> bytes:
    url = f"https://crates.io/api/v1/crates/{package}/{version}/download"
    try:
        with tempfile.TemporaryDirectory(
            prefix="unity-asset-crates-io-download-"
        ) as temporary:
            destination = Path(temporary) / f"{package}-{version}.crate"
            download_with_deadline(
                url,
                destination,
                user_agent="unity-asset-release-verifier/1",
                max_bytes=MAX_REMOTE_ARCHIVE_BYTES,
                connect_timeout_seconds=DOWNLOAD_CONNECT_TIMEOUT_SECONDS,
                total_timeout_seconds=DOWNLOAD_TOTAL_TIMEOUT_SECONDS,
            )
            return destination.read_bytes()
    except (OSError, ReleaseHttpError) as error:
        raise RetryablePublishError(
            f"cannot download {package} {version} from crates.io: {error}"
        ) from error


def validate_publication_request(packages: Sequence[str], version: str) -> None:
    if VERSION_PATTERN.fullmatch(version) is None:
        raise PublishError(f"release version must be MAJOR.MINOR.PATCH, got {version!r}")
    if len(packages) != len(set(packages)):
        raise PublishError("release package order contains duplicates")
    if tuple(packages) != PUBLISHABLE_PACKAGE_NAMES:
        missing = sorted(set(PUBLISHABLE_PACKAGE_NAMES) - set(packages))
        unexpected = sorted(set(packages) - set(PUBLISHABLE_PACKAGE_NAMES))
        details: list[str] = []
        if missing:
            details.append(f"missing: {', '.join(missing)}")
        if unexpected:
            details.append(f"unexpected: {', '.join(unexpected)}")
        if not details:
            details.append("package order differs from the reviewed dependency order")
        raise PublishError(
            "release package sequence differs from the reviewed contract ("
            + "; ".join(details)
            + ")"
        )


def inspect_remote_packages(
    backend: PublishBackend,
    packages: Sequence[str],
    version: str,
    *,
    max_attempts: int = 1,
    retry_delay_seconds: float = 0,
    sleep: Callable[[float], None] = time.sleep,
) -> tuple[PackageRemoteStatus, ...]:
    return tuple(
        PackageRemoteStatus(
            package=package,
            state=(
                RemotePackageState.EXISTS_UNVERIFIED
                if observe_release_with_retry(
                    backend,
                    package,
                    version,
                    max_attempts=max_attempts,
                    retry_delay_seconds=retry_delay_seconds,
                    sleep=sleep,
                )
                else RemotePackageState.MISSING
            ),
        )
        for package in packages
    )


def observe_release_with_retry(
    backend: PublishBackend,
    package: str,
    version: str,
    *,
    max_attempts: int,
    retry_delay_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> bool:
    if max_attempts < 1:
        raise PublishError("max attempts must be at least one")
    last_error: RetryablePublishError | None = None
    for attempt in range(1, max_attempts + 1):
        try:
            return backend.release_exists(package, version)
        except RetryablePublishError as error:
            last_error = error
        if attempt < max_attempts:
            sleep(retry_delay_seconds)
    raise PublishError(
        f"cannot observe {package} {version} on crates.io after "
        f"{max_attempts} attempts: {last_error}"
    )


def verify_known_existing(
    backend: PublishBackend,
    package: str,
    version: str,
    *,
    max_attempts: int,
    retry_delay_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> PackageRemoteStatus:
    if max_attempts < 1:
        raise PublishError("max attempts must be at least one")

    last_error: RetryablePublishError | None = None
    for attempt in range(1, max_attempts + 1):
        try:
            backend.verify_existing(package, version)
            print(f"{package} {version} is published with byte-identical contents")
            return PackageRemoteStatus(package, RemotePackageState.VERIFIED)
        except RemoteBytesMismatch:
            raise
        except RetryablePublishError as error:
            last_error = error

        if attempt < max_attempts:
            print(
                f"{package} exists but is not yet byte-verifiable "
                f"(attempt {attempt}/{max_attempts}); "
                f"waiting {retry_delay_seconds:g}s",
                flush=True,
            )
            sleep(retry_delay_seconds)

    suffix = f": {last_error}" if last_error is not None else ""
    raise PublishError(
        f"{package} {version} exists on crates.io but could not be byte-verified "
        f"after {max_attempts} attempts{suffix}"
    )


def publish_missing_package(
    backend: PublishBackend,
    package: str,
    version: str,
    *,
    max_attempts: int,
    retry_delay_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> PackageRemoteStatus:
    if max_attempts < 1:
        raise PublishError("max attempts must be at least one")

    accepted = False
    last_error: PublishError | None = None
    for attempt in range(1, max_attempts + 1):
        try:
            exists_before_publish = backend.release_exists(package, version)
        except RetryablePublishError as error:
            last_error = error
            exists_before_publish = None
        if exists_before_publish is True:
            verified = verify_known_existing(
                backend,
                package,
                version,
                max_attempts=max_attempts,
                retry_delay_seconds=retry_delay_seconds,
                sleep=sleep,
            )
            return verified

        if exists_before_publish is None:
            if attempt < max_attempts:
                sleep(retry_delay_seconds)
            continue

        if not accepted:
            try:
                backend.publish(package)
                accepted = True
            except RetryablePublishError as error:
                last_error = error

        try:
            exists_after_publish = backend.release_exists(package, version)
        except RetryablePublishError as error:
            last_error = error
            exists_after_publish = None
        if exists_after_publish is True:
            verified = verify_known_existing(
                backend,
                package,
                version,
                max_attempts=max_attempts,
                retry_delay_seconds=retry_delay_seconds,
                sleep=sleep,
            )
            return verified

        if attempt < max_attempts:
            print(
                f"{package} publication is not yet observable "
                f"(attempt {attempt}/{max_attempts}); "
                f"waiting {retry_delay_seconds:g}s",
                flush=True,
            )
            sleep(retry_delay_seconds)

    suffix = f": {last_error}" if last_error is not None else ""
    raise PublishError(
        f"{package} {version} was not observable as a byte-identical crates.io release "
        f"after {max_attempts} attempts{suffix}"
    )


def prepare_publication(
    backend: PublishBackend,
    packages: Sequence[str],
    version: str,
    *,
    max_attempts: int,
    retry_delay_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> PreparedPublication:
    if max_attempts < 1:
        raise PublishError("max attempts must be at least one")
    if retry_delay_seconds < 0:
        raise PublishError("retry delay must be non-negative")

    for package in packages:
        backend.package(package, version)

    observations = inspect_remote_packages(
        backend,
        packages,
        version,
        max_attempts=max_attempts,
        retry_delay_seconds=retry_delay_seconds,
        sleep=sleep,
    )
    preflight: list[PackageRemoteStatus] = []
    for observation in observations:
        if observation.state is RemotePackageState.MISSING:
            preflight.append(observation)
            continue
        preflight.append(
            verify_known_existing(
                backend,
                observation.package,
                version,
                max_attempts=max_attempts,
                retry_delay_seconds=retry_delay_seconds,
                sleep=sleep,
            )
        )

    return PreparedPublication(version=version, packages=tuple(preflight))


def commit_publication(
    backend: PublishBackend,
    publication: PreparedPublication,
    *,
    max_attempts: int,
    retry_delay_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    if max_attempts < 1:
        raise PublishError("max attempts must be at least one")
    if retry_delay_seconds < 0:
        raise PublishError("retry delay must be non-negative")

    for observation in publication.packages:
        if observation.state is not RemotePackageState.MISSING:
            continue
        publish_missing_package(
            backend,
            observation.package,
            publication.version,
            max_attempts=max_attempts,
            retry_delay_seconds=retry_delay_seconds,
            sleep=sleep,
        )


def publish_packages(
    backend: PublishBackend,
    packages: Sequence[str],
    version: str,
    *,
    max_attempts: int,
    retry_delay_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    publication = prepare_publication(
        backend,
        packages,
        version,
        max_attempts=max_attempts,
        retry_delay_seconds=retry_delay_seconds,
        sleep=sleep,
    )
    commit_publication(
        backend,
        publication,
        max_attempts=max_attempts,
        retry_delay_seconds=retry_delay_seconds,
        sleep=sleep,
    )


def main() -> int:
    args = parse_args()
    packages = tuple(args.packages)
    validate_publication_request(packages, args.version)
    if args.max_attempts < 1 or args.retry_delay_seconds < 0:
        raise PublishError("retry limits must be non-negative and attempts must be positive")
    token = os.environ.get(RELEASE_TOKEN_ENVIRONMENT_VARIABLE)
    if not token:
        raise PublishError(
            f"{RELEASE_TOKEN_ENVIRONMENT_VARIABLE} is required for publication"
        )
    if os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise PublishError("CARGO_REGISTRY_TOKEN must not be exported to the publication step")
    backend = CargoBackend(args.repository_root.resolve(), args.cargo, token)
    publish_packages(
        backend,
        packages,
        args.version,
        max_attempts=args.max_attempts,
        retry_delay_seconds=args.retry_delay_seconds,
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except PublishError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
