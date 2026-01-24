#!/usr/bin/env python3
"""
Export script-specific MonoBehaviour TypeTrees from UnityPy into this repo's JSON registry format.

This is the recommended "external workflow" for closing the UnityPy parity gap around
MonoBehaviour TypeTree generation (UnityPy uses TypeTreeGeneratorAPI).

Output format:
- JSON registry schema 2, with `script_id` entries (32-hex Hash128)
- Each entry contains a `type_tree` that can be loaded via `Environment::set_type_tree_registry_from_paths`.

Requirements:
- Python environment that can import UnityPy dependencies
- TypeTreeGeneratorAPI installed (UnityPy's TypeTreeGenerator wrapper depends on it)

Example (managed build):
  python scripts/export_unitypy_script_typetrees.py ^
    --input "C:\\path\\to\\bundle.ab" ^
    --managed-dir "C:\\path\\to\\Game_Data\\Managed" ^
    --output "C:\\tmp\\script-typetrees.json"

Example (IL2CPP build):
  python scripts/export_unitypy_script_typetrees.py ^
    --input "C:\\path\\to\\bundle.ab" ^
    --game-root "C:\\path\\to\\Game" ^
    --output "C:\\tmp\\script-typetrees.json"
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, Optional, Set, Tuple


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def setup_unitypy_import() -> None:
    root = repo_root()
    unitypy_path = root / "repo-ref" / "UnityPy"
    sys.path.insert(0, str(unitypy_path))


def hex32(b: bytes) -> str:
    return b.hex()


def version_prefix(unity_version: str) -> Optional[str]:
    """
    Convert e.g. "2020.3.48f1" -> "2020.3.*" (prefix match in the registry).
    """
    parts = unity_version.split(".")
    if len(parts) < 2:
        return None
    major = parts[0]
    minor = parts[1]
    if not major.isdigit() or not minor.isdigit():
        return None
    return f"{major}.{minor}.*"


def to_rust_typetree_node(node: Any) -> Dict[str, Any]:
    children = getattr(node, "m_Children", None) or []
    return {
        "type_name": getattr(node, "m_Type", "") or "",
        "name": getattr(node, "m_Name", "") or "",
        "byte_size": int(getattr(node, "m_ByteSize", 0) or 0),
        "index": int(getattr(node, "m_Index", 0) or 0),
        "type_flags": int(getattr(node, "m_TypeFlags", 0) or 0),
        "version": int(getattr(node, "m_Version", 0) or 0),
        "meta_flags": int(getattr(node, "m_MetaFlag", 0) or 0),
        "level": int(getattr(node, "m_Level", 0) or 0),
        "type_str_offset": 0,
        "name_str_offset": 0,
        "ref_type_hash": 0,
        "children": [to_rust_typetree_node(c) for c in children],
    }


def to_rust_typetree(root_node: Any) -> Dict[str, Any]:
    return {
        "nodes": [to_rust_typetree_node(root_node)],
        "string_buffer": [],
        "version": 0,
        "platform": 0,
        "has_type_dependencies": False,
    }


@dataclass(frozen=True)
class ScriptKey:
    script_id_hex: str


@dataclass
class ScriptInfo:
    unity_version: str
    assembly: str
    fullname: str
    type_tree: Dict[str, Any]


def iter_monobehaviour_scripts(env: Any) -> Iterable[Tuple[ScriptKey, ScriptInfo]]:
    # UnityPy env.objects excludes dependencies by default.
    for obj in getattr(env, "objects", []):
        type_name = getattr(getattr(obj, "type", None), "name", None)
        if type_name != "MonoBehaviour":
            continue

        serialized_type = getattr(obj, "serialized_type", None)
        script_id = getattr(serialized_type, "script_id", None)
        if not isinstance(script_id, (bytes, bytearray)) or len(script_id) != 16:
            continue

        try:
            mb = obj.parse_monobehaviour_head()
        except Exception:
            continue

        try:
            script = mb.m_Script.deref_parse_as_object()
        except Exception:
            continue

        ns = getattr(script, "m_Namespace", "") or ""
        cls = getattr(script, "m_ClassName", "") or ""
        assembly = getattr(script, "m_AssemblyName", "") or ""
        if not cls or not assembly:
            continue

        if ns:
            fullname = f"{ns}.{cls}"
        else:
            fullname = cls

        unity_version = getattr(getattr(obj, "assets_file", None), "unity_version", None)
        if not isinstance(unity_version, str) or not unity_version:
            unity_version = getattr(getattr(env, "file", None), "unity_version", "") or ""

        yield (
            ScriptKey(script_id_hex=hex32(bytes(script_id))),
            ScriptInfo(
                unity_version=unity_version,
                assembly=assembly,
                fullname=fullname,
                type_tree={},
            ),
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--input",
        action="append",
        required=True,
        help="Input AssetBundle/.assets path (repeatable).",
    )
    parser.add_argument(
        "--output",
        required=True,
        help="Output JSON registry path (schema 2).",
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument(
        "--game-root",
        help="Game root (contains GameAssembly.dll and *_Data/...). Used for IL2CPP builds.",
    )
    group.add_argument(
        "--managed-dir",
        help="Managed folder containing .dll files (e.g. *_Data/Managed).",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Print progress to stderr.",
    )
    args = parser.parse_args()

    setup_unitypy_import()
    import UnityPy  # noqa: E402
    from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator  # noqa: E402

    generator: Any
    try:
        generator = TypeTreeGenerator("2020.3.0f1")
    except ImportError as e:
        sys.stderr.write(
            "TypeTreeGeneratorAPI is not installed (UnityPy TypeTreeGenerator unavailable).\n"
            "Install it in your python env, then retry.\n"
            f"Original error: {e}\n"
        )
        return 2

    if args.game_root:
        generator.load_local_game(args.game_root)
    else:
        generator.load_local_dll_folder(args.managed_dir)

    scripts: Dict[ScriptKey, ScriptInfo] = {}
    seen_inputs: Set[str] = set()

    for input_path in args.input:
        input_path = os.path.expanduser(input_path)
        if input_path in seen_inputs:
            continue
        seen_inputs.add(input_path)

        if args.verbose:
            sys.stderr.write(f"[unitypy] load: {input_path}\n")

        env = UnityPy.load(input_path)
        for key, info in iter_monobehaviour_scripts(env):
            if key in scripts:
                continue
            try:
                node = generator.get_nodes_up(info.assembly, info.fullname)
            except Exception:
                continue
            if not node:
                continue

            info.type_tree = to_rust_typetree(node)
            scripts[key] = info

    entries = []
    for key, info in sorted(scripts.items(), key=lambda kv: kv[0].script_id_hex):
        uv = version_prefix(info.unity_version)
        entry: Dict[str, Any] = {
            "unity_version": uv,
            "class_id": 114,
            "script_id": key.script_id_hex,
            "type_tree": info.type_tree,
            "assembly": info.assembly,
            "fullname": info.fullname,
        }
        entries.append(entry)

    out = {
        "schema": 2,
        "entries": entries,
    }

    out_path = Path(args.output)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(out, indent=2, sort_keys=True), encoding="utf-8")
    if args.verbose:
        sys.stderr.write(f"[unitypy] wrote {len(entries)} script typetrees: {out_path}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

