#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
from dataclasses import dataclass
from typing import Iterable, List, Optional, Sequence, Set, Tuple


@dataclass(frozen=True)
class BundleDiff:
    bundle: str
    unitypy_total: int
    unity_asset_total: int
    missing_in_unity_asset: List[str]
    extra_in_unity_asset: List[str]


def _repo_root() -> str:
    return os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))


def _normalize_asset_path(p: str) -> str:
    return p.replace("\\", "/").strip().lower()


def _load_unitypy(unitypy_repo: str):
    sys.path.insert(0, unitypy_repo)
    import UnityPy  # type: ignore

    return UnityPy


def _iter_bundle_paths(paths: Sequence[str]) -> Iterable[str]:
    for p in paths:
        p = os.path.abspath(p)
        if os.path.isdir(p):
            for root, _, files in os.walk(p):
                for name in files:
                    if name.lower().endswith(".ab"):
                        yield os.path.join(root, name)
        else:
            yield p


def _sample(paths: List[str], sample: int, seed: int) -> List[str]:
    if sample <= 0 or sample >= len(paths):
        return paths
    rng = random.Random(seed)
    rng.shuffle(paths)
    return paths[:sample]


def _unitypy_container_keys(UnityPy, bundle_path: str) -> Set[str]:
    env = UnityPy.load(bundle_path)
    keys = [_normalize_asset_path(k) for k, _info in env.container.container]
    return set(keys)


def _run_unity_asset_find_object_container(unity_asset_exe: str, bundle_path: str) -> Set[str]:
    cmd = [
        unity_asset_exe,
        "find-object",
        "--input",
        bundle_path,
        "--include-unresolved",
    ]
    proc = subprocess.run(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            "unity-asset find-object failed\n"
            + f"bundle={bundle_path}\n"
            + f"cmd={' '.join(cmd)}\n"
            + proc.stdout
        )

    keys: Set[str] = set()
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line or line.startswith("?") or line.startswith("warning:"):
            continue
        if " -> " not in line:
            continue
        asset_path = line.split(" -> ", 1)[0].strip()
        if asset_path:
            keys.add(_normalize_asset_path(asset_path))
    return keys


def _detect_unity_asset_exe(repo_root: str, explicit: Optional[str]) -> str:
    if explicit:
        return os.path.abspath(explicit)

    candidates = [
        os.path.join(repo_root, "target", "debug", "unity-asset.exe"),
        os.path.join(repo_root, "target", "release", "unity-asset.exe"),
    ]
    for c in candidates:
        if os.path.isfile(c):
            return c

    raise FileNotFoundError(
        "unity-asset executable not found. Build it first:\n"
        "  cargo build -p unity-asset-cli --bin unity-asset\n"
        "or pass --unity-asset-exe <path-to-unity-asset.exe>"
    )


def _diff_sets(a: Set[str], b: Set[str]) -> Tuple[List[str], List[str]]:
    missing = sorted(a - b)
    extra = sorted(b - a)
    return (missing, extra)


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="Compare UnityPy container keys with unity-asset find-object output (sampled)."
    )
    parser.add_argument(
        "--unitypy-repo",
        default=os.path.join(_repo_root(), "repo-ref", "UnityPy"),
        help="Path to the UnityPy repository root (default: repo-ref/UnityPy).",
    )
    parser.add_argument(
        "--unity-asset-exe",
        default=None,
        help="Path to unity-asset.exe (default: target/debug/unity-asset.exe).",
    )
    parser.add_argument(
        "--bundles",
        nargs="+",
        required=True,
        help="One or more .ab files or directories to scan recursively for .ab files.",
    )
    parser.add_argument("--sample", type=int, default=20, help="Number of bundles to sample.")
    parser.add_argument("--seed", type=int, default=0, help="Random seed for sampling.")
    parser.add_argument(
        "--max-diff",
        type=int,
        default=50,
        help="Limit printed missing/extra lists per bundle.",
    )
    parser.add_argument(
        "--only-mismatches",
        action="store_true",
        help="Only print bundles that mismatch or error.",
    )
    parser.add_argument(
        "--json",
        default=None,
        help="Write full report JSON to this path.",
    )

    args = parser.parse_args(argv)

    repo_root = _repo_root()
    unity_asset_exe = _detect_unity_asset_exe(repo_root, args.unity_asset_exe)
    UnityPy = _load_unitypy(os.path.abspath(args.unitypy_repo))

    all_paths = sorted(set(_iter_bundle_paths(args.bundles)))
    if not all_paths:
        raise SystemExit("No bundle files found.")

    sampled = _sample(all_paths, args.sample, args.seed)

    diffs: List[BundleDiff] = []
    ok = 0
    failed = 0

    for path in sampled:
        try:
            unitypy = _unitypy_container_keys(UnityPy, path)
            unity_asset = _run_unity_asset_find_object_container(unity_asset_exe, path)
            missing, extra = _diff_sets(unitypy, unity_asset)
            diffs.append(
                BundleDiff(
                    bundle=os.path.abspath(path),
                    unitypy_total=len(unitypy),
                    unity_asset_total=len(unity_asset),
                    missing_in_unity_asset=missing,
                    extra_in_unity_asset=extra,
                )
            )
            if not missing and not extra:
                ok += 1
            else:
                failed += 1
        except Exception as e:
            failed += 1
            diffs.append(
                BundleDiff(
                    bundle=os.path.abspath(path),
                    unitypy_total=-1,
                    unity_asset_total=-1,
                    missing_in_unity_asset=[f"ERROR: {type(e).__name__}: {e}"],
                    extra_in_unity_asset=[],
                )
            )

    summary = {
        "sampled": len(sampled),
        "ok": ok,
        "mismatch_or_error": failed,
        "diffs": [
            {
                "bundle": d.bundle,
                "unitypy_total": d.unitypy_total,
                "unity_asset_total": d.unity_asset_total,
                "missing_in_unity_asset": d.missing_in_unity_asset,
                "extra_in_unity_asset": d.extra_in_unity_asset,
            }
            for d in diffs
        ],
    }

    for d in diffs:
        is_ok = not d.missing_in_unity_asset and not d.extra_in_unity_asset and d.unitypy_total >= 0
        if args.only_mismatches and is_ok:
            continue

        print(
            f"{d.bundle}\n"
            f"  unitypy_total={d.unitypy_total} unity_asset_total={d.unity_asset_total}\n"
            f"  missing={len(d.missing_in_unity_asset)} extra={len(d.extra_in_unity_asset)}"
        )
        if d.missing_in_unity_asset and args.max_diff > 0:
            head = d.missing_in_unity_asset[: args.max_diff]
            for k in head:
                print(f"    - missing: {k}")
            if len(d.missing_in_unity_asset) > len(head):
                print(f"    ... ({len(d.missing_in_unity_asset) - len(head)} more)")
        if d.extra_in_unity_asset and args.max_diff > 0:
            head = d.extra_in_unity_asset[: args.max_diff]
            for k in head:
                print(f"    + extra:   {k}")
            if len(d.extra_in_unity_asset) > len(head):
                print(f"    ... ({len(d.extra_in_unity_asset) - len(head)} more)")

    print(f"\nSUMMARY: sampled={len(sampled)} ok={ok} mismatch_or_error={failed}")

    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(summary, f, ensure_ascii=False, indent=2)

    return 0 if failed == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
