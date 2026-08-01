#!/usr/bin/env python3
"""Install the release-pinned cargo-dist after verifying its installer bytes."""

from __future__ import annotations

import argparse
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Mapping

from release_contract import CARGO_DIST_VERSION
from release_http import DownloadMetadata, ReleaseHttpError, download_with_deadline
from release_subprocess import (
    BoundedCommandTimeout,
    credential_free_environment,
    run_bounded_command,
)


INSTALLER_SHA256 = "611710171d9c963884ea53a45d63dc7e8a22ee9c86f20f3767818fadc076d04e"
INSTALLER_URL = (
    "https://github.com/axodotdev/cargo-dist/releases/download/"
    f"v{CARGO_DIST_VERSION}/cargo-dist-installer.sh"
)
VERSION_PATTERN = re.compile(r"^cargo-dist (\d+\.\d+\.\d+)(?: \([^)]+\))?$")
COMMAND_TIMEOUT_SECONDS = 120
DOWNLOAD_TOTAL_TIMEOUT_SECONDS = 120
DOWNLOAD_CONNECT_TIMEOUT_SECONDS = 30
MAX_INSTALLER_BYTES = 4 * 1024 * 1024


class InstallError(RuntimeError):
    """An actionable cargo-dist installation failure."""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Install the repository-pinned cargo-dist release safely."
    )
    parser.add_argument("--shell", help="POSIX shell used to execute the installer")
    parser.add_argument(
        "--dist",
        default=os.environ.get("DIST", "dist"),
        help="cargo-dist executable to verify (default: DIST or dist)",
    )
    return parser.parse_args()


def download_installer(destination: Path) -> str:
    try:
        metadata = download_with_deadline(
            INSTALLER_URL,
            destination,
            user_agent="unity-asset-release-installer/1",
            max_bytes=MAX_INSTALLER_BYTES,
            connect_timeout_seconds=DOWNLOAD_CONNECT_TIMEOUT_SECONDS,
            total_timeout_seconds=DOWNLOAD_TOTAL_TIMEOUT_SECONDS,
        )
    except ReleaseHttpError as error:
        raise InstallError(f"cannot download cargo-dist installer: {error}") from error
    return metadata.sha256


def select_shell(explicit: str | None) -> str:
    if explicit is not None:
        candidate = shutil.which(explicit)
        if candidate is None:
            raise InstallError(f"requested shell is not available: {explicit}")
        return candidate
    for name in ("sh", "bash"):
        candidate = shutil.which(name)
        if candidate is not None:
            return candidate
    raise InstallError("cargo-dist installer requires sh or bash")


def installer_environment(directory: Path) -> dict[str, str]:
    environment = credential_free_environment()
    path = environment.get("PATH", "")
    if shutil.which("sha256sum", path=path) is not None:
        return environment
    if shutil.which("shasum", path=path) is None:
        raise InstallError("cargo-dist installer requires sha256sum or shasum")
    shim = directory / "sha256sum"
    shim.write_text("#!/bin/sh\nexec shasum -a 256 \"$@\"\n", encoding="utf-8")
    shim.chmod(0o700)
    environment["PATH"] = f"{directory}{os.pathsep}{path}"
    return environment


def run_checked(
    command: list[str], *, cwd: Path | None = None, env: Mapping[str, str] | None = None
) -> str:
    try:
        result = run_bounded_command(
            command,
            cwd=cwd,
            env=env,
            timeout_seconds=COMMAND_TIMEOUT_SECONDS,
        )
    except BoundedCommandTimeout as error:
        raise InstallError(
            f"command timed out after {COMMAND_TIMEOUT_SECONDS}s: {' '.join(command)}"
        ) from error
    except OSError as error:
        raise InstallError(f"cannot start command {' '.join(command)}: {error}") from error
    if result.returncode != 0:
        output = result.stdout.rstrip()
        suffix = f"\n{output}" if output else ""
        raise InstallError(
            f"command failed with exit code {result.returncode}: "
            f"{' '.join(command)}{suffix}"
        )
    return result.stdout.strip()


def verify_installed_version(dist: str) -> None:
    output = run_checked([dist, "--version"])
    lines = [line.strip() for line in output.splitlines() if line.strip()]
    match = VERSION_PATTERN.fullmatch(lines[0]) if len(lines) == 1 else None
    if match is None or match.group(1) != CARGO_DIST_VERSION:
        raise InstallError(
            f"installed cargo-dist version mismatch: expected {CARGO_DIST_VERSION}, "
            f"got {output!r}"
        )


def main() -> int:
    args = parse_args()
    shell = select_shell(args.shell)
    with tempfile.TemporaryDirectory(prefix="unity-asset-cargo-dist-") as temporary:
        installer = Path(temporary) / "cargo-dist-installer.sh"
        actual_sha256 = download_installer(installer)
        if actual_sha256 != INSTALLER_SHA256:
            raise InstallError(
                "cargo-dist installer SHA-256 mismatch: "
                f"expected {INSTALLER_SHA256}, got {actual_sha256}"
            )
        run_checked(
            [shell, installer.name],
            cwd=installer.parent,
            env=installer_environment(installer.parent),
        )
    verify_installed_version(args.dist)
    print(
        f"installed cargo-dist {CARGO_DIST_VERSION} from verified installer "
        f"sha256:{INSTALLER_SHA256}"
    )
    return 0



if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except InstallError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
