from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DAEMON_BINARY = "unity-asset-search-daemon"
DAEMON_BUILD_IDENTITY_ENV = "UNITY_ASSET_SEARCH_DAEMON_BUILD_IDENTITY"
BUILD_IDENTITY_PATTERN = re.compile(
    rf"unity-asset\.build-identity\.v1\{{version=[^;{{}}]+;"
    rf"source-commit=(?:[0-9a-f]{{40}}|unknown);package={DAEMON_BINARY};"
    r"target=[A-Za-z0-9_.-]+\}"
)


def run(command: list[str], *, environment: dict[str, str] | None = None) -> None:
    print("$ " + " ".join(command), flush=True)
    subprocess.run(
        command,
        cwd=REPOSITORY_ROOT,
        env=environment,
        check=True,
    )


def build_binaries(cargo: str) -> Path:
    command = [
        cargo,
        "build",
        "--locked",
        "-p",
        "unity-asset-search-daemon",
        "--bin",
        DAEMON_BINARY,
        "--message-format=json",
    ]
    print("$ " + " ".join(command), flush=True)
    result = subprocess.run(
        command,
        cwd=REPOSITORY_ROOT,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    executables = {
        Path(message["executable"]).resolve()
        for line in result.stdout.splitlines()
        if (message := json.loads(line)).get("reason") == "compiler-artifact"
        and message.get("target", {}).get("name") == DAEMON_BINARY
        and "bin" in message.get("target", {}).get("kind", ())
        and message.get("executable")
    }
    if len(executables) != 1:
        raise RuntimeError(
            "Cargo did not report exactly one built daemon executable: "
            f"{sorted(str(path) for path in executables)}"
        )
    return executables.pop()


def daemon_build_identity(daemon: Path) -> str:
    command = [str(daemon), "--version"]
    print("$ " + " ".join(command), flush=True)
    try:
        result = subprocess.run(
            command,
            cwd=REPOSITORY_ROOT,
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )
    except FileNotFoundError as error:
        raise FileNotFoundError(f"built daemon binary is missing: {daemon}") from error
    if result.returncode != 0:
        raise RuntimeError(
            f"built daemon --version failed with exit code {result.returncode}: "
            f"{result.stderr.strip()}"
        )
    if result.stderr:
        raise RuntimeError(
            f"built daemon --version wrote stderr: {result.stderr.strip()}"
        )
    lines = result.stdout.splitlines()
    prefix = f"{DAEMON_BINARY} "
    if len(lines) != 1 or not lines[0].startswith(prefix):
        raise RuntimeError(
            f"built daemon reported an unexpected --version output: {result.stdout!r}"
        )
    report = lines[0][len(prefix) :]
    if BUILD_IDENTITY_PATTERN.fullmatch(report) is None:
        raise RuntimeError(
            f"built daemon reported an invalid build identity: {report!r}"
        )
    return report


def main() -> int:
    cargo = os.environ.get("CARGO", "cargo")
    daemon = build_binaries(cargo)
    build_identity = daemon_build_identity(daemon)

    environment = os.environ.copy()
    environment["UNITY_ASSET_SEARCH_DAEMON"] = str(daemon)
    environment[DAEMON_BUILD_IDENTITY_ENV] = build_identity
    run(
        [
            cargo,
            "nextest",
            "run",
            "--locked",
            "--test-threads",
            "1",
            "--run-ignored",
            "ignored-only",
            "-p",
            "unity-asset-search-cli",
            "--test",
            "real_daemon_agent",
        ],
        environment=environment,
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode) from error
    except (FileNotFoundError, RuntimeError) as error:
        print(f"real daemon agent harness failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
