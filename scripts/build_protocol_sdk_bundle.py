#!/usr/bin/env python3
"""Generate the deterministic C# search-protocol SDK and fixture bundle."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Sequence

from protocol_sdk_bundle import (
    ProtocolSdkBundleError,
    build_protocol_sdk_bundle,
    extract_protocol_sdk_bundle,
    verify_protocol_sdk_bundle,
)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Build the deterministic, versioned C# reference codec, JSON Schema, and "
            "golden-fixture release bundle."
        )
    )
    parser.add_argument(
        "--repository-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
    )
    parser.add_argument(
        "--release-tag",
        required=True,
        help="Exact release tag in vMAJOR.MINOR.PATCH form.",
    )
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument(
        "--output-directory",
        type=Path,
        help="Directory that receives the versioned ZIP archive.",
    )
    source.add_argument(
        "--bundle",
        type=Path,
        help="Existing versioned ZIP archive to verify or extract.",
    )
    parser.add_argument(
        "--extract-directory",
        type=Path,
        help=(
            "After building or verifying, safely extract the exact archive below this "
            "directory for consumer compilation."
        ),
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if args.bundle is None:
        metadata = build_protocol_sdk_bundle(
            args.repository_root,
            args.output_directory,
            args.release_tag,
        )
        bundle_path = args.output_directory / metadata.artifact_name
        if args.extract_directory is not None:
            extracted = extract_protocol_sdk_bundle(
                bundle_path,
                args.extract_directory,
                args.release_tag,
            )
            if extracted != metadata:
                raise ProtocolSdkBundleError(
                    "extracted protocol SDK bundle does not match its verified evidence"
                )
    else:
        bundle_path = args.bundle
        if args.extract_directory is None:
            metadata = verify_protocol_sdk_bundle(bundle_path, args.release_tag)
        else:
            metadata = extract_protocol_sdk_bundle(
                bundle_path,
                args.extract_directory,
                args.release_tag,
            )
    print(metadata.canonical_json())
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ProtocolSdkBundleError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
