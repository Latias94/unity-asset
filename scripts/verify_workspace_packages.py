#!/usr/bin/env python3
"""Verify that every published workspace package is repository-independent."""

from __future__ import annotations

import argparse
import os
import sys
import tempfile
from pathlib import Path

from workspace_package_contract import (
    VerificationError,
    discover_workspace_packages,
    load_toml,
    published_production_closure,
    reject_root_source_overrides,
    validate_source_dependencies,
)
from workspace_package_verification import (
    configuration_clean_cargo_cwd,
    isolated_cargo_environment,
    run_captured,
    run_verification,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Package all publishable workspace crates and verify isolated "
            "registry-backed archive and consumer workspaces."
        )
    )
    parser.add_argument(
        "--cargo",
        default=os.environ.get("CARGO", "cargo"),
        help="Cargo executable to invoke (default: CARGO or cargo).",
    )
    parser.add_argument(
        "--mode",
        choices=("preflight", "packages", "full"),
        default="packages",
        help=(
            "Verification depth: preflight checks policy only; packages mode also "
            "proves unpacked archives and external consumers; full additionally "
            "installs and probes published binaries (default: packages)."
        ),
    )
    parser.add_argument(
        "--archive-output",
        type=Path,
        help="Export the verified .crate files for a later trusted publication job.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.archive_output is not None and args.mode != "full":
        raise VerificationError("--archive-output requires --mode full")
    workspace_root = Path(__file__).resolve().parent.parent
    root_manifest = workspace_root / "Cargo.toml"
    root_document = load_toml(root_manifest)
    reject_root_source_overrides(root_document, root_manifest)

    cargo_cwd = configuration_clean_cargo_cwd(workspace_root)
    with tempfile.TemporaryDirectory(
        prefix="unity-asset-workspace-package-preflight-", ignore_cleanup_errors=True
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
    closure = published_production_closure(packages)
    validate_source_dependencies(closure, packages)

    names = ", ".join(package.name for package in closure)
    print(f"preflight passed; package order: {names}")
    if args.mode == "preflight":
        return 0

    run_verification(
        cargo=args.cargo,
        workspace_root=workspace_root,
        closure=closure,
        verify_binaries=args.mode == "full",
        archive_output=(
            args.archive_output.resolve() if args.archive_output is not None else None
        ),
    )
    print(
        f"workspace package verification ({args.mode}) passed: every publishable "
        "archive and consumer resolved only from unpacked internal archives and "
        "crates.io"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
