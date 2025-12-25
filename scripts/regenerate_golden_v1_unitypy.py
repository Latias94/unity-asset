#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from typing import Any, Dict, Optional, Tuple


@dataclass
class CaseKey:
    source: str
    asset_path: str
    path_id: int


def _repo_root() -> str:
    return os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))


def _load_unitypy(unitypy_repo: str):
    sys.path.insert(0, unitypy_repo)
    import UnityPy  # type: ignore

    return UnityPy


def _normalize_asset_path(p: str) -> str:
    return p.replace("\\", "/").lower()


def _find_container_pptr(env, asset_path: str, path_id: int):
    want_path = _normalize_asset_path(asset_path)
    for k, info in env.container.container:
        if _normalize_asset_path(k) != want_path:
            continue
        if int(info.asset.path_id) != int(path_id):
            continue
        return info.asset
    return None


def _suffix(path: str) -> str:
    path = path.replace("\\", "/")
    _, ext = os.path.splitext(path)
    return ext


def _as_int(v: Any) -> Optional[int]:
    if isinstance(v, bool):
        return None
    if isinstance(v, int):
        return v
    return None


def _as_float(v: Any) -> Optional[float]:
    if isinstance(v, (float, int)) and not isinstance(v, bool):
        return float(v)
    return None


def _collect_pptrs(obj: Any) -> Tuple[list[int], list[Tuple[int, int]]]:
    internal: set[int] = set()
    external: set[Tuple[int, int]] = set()

    def walk(v: Any) -> None:
        if isinstance(v, dict):
            fid = _as_int(v.get("m_FileID"))
            pid = _as_int(v.get("m_PathID"))
            if fid is not None and pid is not None and pid != 0:
                if fid == 0:
                    internal.add(pid)
                else:
                    external.add((fid, pid))
            for vv in v.values():
                walk(vv)
            return
        if isinstance(v, (list, tuple)):
            for vv in v:
                walk(vv)
            return

    walk(obj)
    return (sorted(internal), sorted(external))


def _load_json(path: str) -> Dict[str, Any]:
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def _dump_json(obj: Any) -> str:
    return json.dumps(obj, ensure_ascii=False, indent=2) + "\n"


def _update_case_from_unitypy(case: Dict[str, Any], obj: Dict[str, Any]) -> None:
    case["name"] = obj.get("m_Name") or case.get("name", "")

    expect = case.get("expect") or {}
    kind = expect.get("kind")

    if kind == "audioclip_streamed":
        res = obj.get("m_Resource") or {}
        source = res.get("m_Source")
        if isinstance(source, str) and source:
            expect["stream_path_suffix"] = _suffix(source)
        off = _as_int(res.get("m_Offset"))
        size = _as_int(res.get("m_Size"))
        if off is not None:
            expect["stream_offset"] = off
        if size is not None:
            expect["stream_size"] = size
        cf = _as_int(obj.get("m_CompressionFormat"))
        if cf is not None:
            expect["compression_format"] = cf

    elif kind == "texture2d":
        sd = obj.get("m_StreamData") or {}
        path = sd.get("path")
        if isinstance(path, str) and path:
            expect["stream_path_suffix"] = _suffix(path)
        off = _as_int(sd.get("offset"))
        size = _as_int(sd.get("size"))
        if off is not None:
            expect["stream_offset"] = off
        if size is not None:
            expect["stream_size"] = size
        cis = _as_int(obj.get("m_CompleteImageSize"))
        if cis is not None:
            expect["complete_image_size"] = cis

        w = _as_int(obj.get("m_Width"))
        h = _as_int(obj.get("m_Height"))
        tf = _as_int(obj.get("m_TextureFormat"))
        if w is not None:
            expect["width"] = w
        if h is not None:
            expect["height"] = h
        if tf is not None:
            expect["texture_format"] = tf

    elif kind == "sprite":
        rect = obj.get("m_Rect") or {}
        w = _as_float(rect.get("width"))
        h = _as_float(rect.get("height"))
        if w is not None:
            expect["rect_width"] = w
        if h is not None:
            expect["rect_height"] = h

        # Cross-object reference sanity: Sprite render data points at a Texture2D via PPtr.
        rd = obj.get("m_RD") or {}
        tex = rd.get("texture") or {}
        fid = _as_int(tex.get("m_FileID"))
        pid = _as_int(tex.get("m_PathID"))
        if fid is not None:
            expect["texture_file_id"] = fid
        if pid is not None:
            expect["texture_path_id"] = pid

        # Render data buffers are a good cross-engine regression signal (alignment + byte arrays).
        idx = rd.get("m_IndexBuffer")
        if isinstance(idx, (bytes, bytearray)):
            idx = bytes(idx)
            expect["index_buffer_len"] = len(idx)
            expect["index_buffer_prefix"] = list(idx[:8])
        elif isinstance(idx, list) and idx and all(isinstance(x, int) for x in idx):
            expect["index_buffer_len"] = len(idx)
            expect["index_buffer_prefix"] = idx[:8]

        vd = rd.get("m_VertexData") or {}
        vertex_bytes = vd.get("m_DataSize")
        if isinstance(vertex_bytes, (bytes, bytearray)):
            vertex_bytes = bytes(vertex_bytes)
            expect["vertex_data_len"] = len(vertex_bytes)
            expect["vertex_data_prefix"] = list(vertex_bytes[:8])
        elif isinstance(vertex_bytes, list) and vertex_bytes and all(
            isinstance(x, int) for x in vertex_bytes
        ):
            expect["vertex_data_len"] = len(vertex_bytes)
            expect["vertex_data_prefix"] = vertex_bytes[:8]

        pptr_internal, pptr_external = _collect_pptrs(obj)
        expect["pptr_internal"] = pptr_internal
        expect["pptr_external"] = [[fid, pid] for (fid, pid) in pptr_external]

    elif kind == "mesh":
        vd = obj.get("m_VertexData") or {}
        vertex_bytes = vd.get("m_DataSize")
        if isinstance(vertex_bytes, (bytes, bytearray)):
            vertex_bytes = bytes(vertex_bytes)
            expect["vertex_data_len"] = len(vertex_bytes)
            expect["vertex_data_prefix"] = list(vertex_bytes[:8])
        elif isinstance(vertex_bytes, list) and vertex_bytes and all(
            isinstance(x, int) for x in vertex_bytes
        ):
            expect["vertex_data_len"] = len(vertex_bytes)
            expect["vertex_data_prefix"] = vertex_bytes[:8]

        idx = obj.get("m_IndexBuffer")
        if isinstance(idx, (bytes, bytearray)):
            idx = bytes(idx)
            expect["index_buffer_len"] = len(idx)
            expect["index_buffer_prefix"] = list(idx[:8])
        elif isinstance(idx, list) and idx and all(isinstance(x, int) for x in idx):
            expect["index_buffer_len"] = len(idx)
            expect["index_buffer_prefix"] = idx[:8]

        pptr_internal, pptr_external = _collect_pptrs(obj)
        expect["pptr_internal"] = pptr_internal
        expect["pptr_external"] = [[fid, pid] for (fid, pid) in pptr_external]

    elif kind == "peek_only":
        pptr_internal, pptr_external = _collect_pptrs(obj)
        expect["pptr_internal"] = pptr_internal
        expect["pptr_external"] = [[fid, pid] for (fid, pid) in pptr_external]

    case["expect"] = expect


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Regenerate tests/golden/golden_v1.json fields using local repo-ref/UnityPy."
    )
    ap.add_argument(
        "--golden",
        default=os.path.join("tests", "golden", "golden_v1.json"),
        help="Input golden JSON (default: tests/golden/golden_v1.json)",
    )
    ap.add_argument(
        "--unitypy-repo",
        default=os.path.join("repo-ref", "UnityPy"),
        help="Path to UnityPy repo (default: repo-ref/UnityPy)",
    )
    ap.add_argument(
        "--write",
        action="store_true",
        help="Overwrite the input file in place (otherwise prints to stdout)",
    )
    args = ap.parse_args()

    root = _repo_root()
    golden_path = os.path.join(root, args.golden)
    unitypy_repo = os.path.join(root, args.unitypy_repo)

    try:
        UnityPy = _load_unitypy(unitypy_repo)
    except Exception as e:
        print(
            "Failed to import UnityPy.\n"
            "Expected a local checkout at repo-ref/UnityPy and Python deps installed.\n"
            "Minimal deps for these samples: fsspec attrs lz4 brotli Pillow\n"
            f"Error: {e}",
            file=sys.stderr,
        )
        return 2

    golden = _load_json(golden_path)
    if golden.get("schema") != 1:
        print(f"Unexpected golden schema: {golden.get('schema')}", file=sys.stderr)
        return 2

    env_cache: Dict[str, Any] = {}

    for case in golden.get("cases", []):
        source_rel = case["source"]
        source_abs = os.path.join(root, source_rel)
        key = CaseKey(
            source=source_abs,
            asset_path=case["asset_path"],
            path_id=int(case["path_id"]),
        )

        env = env_cache.get(source_abs)
        if env is None:
            env = UnityPy.load(source_abs)
            env_cache[source_abs] = env

        pptr = _find_container_pptr(env, key.asset_path, key.path_id)
        if pptr is None:
            print(
                f"Case not found in UnityPy container: source={source_rel} asset_path={key.asset_path} path_id={key.path_id}",
                file=sys.stderr,
            )
            continue

        obj_dict = pptr.deref_parse_as_dict()
        _update_case_from_unitypy(case, obj_dict)

    out_text = _dump_json(golden)
    if args.write:
        with open(golden_path, "w", encoding="utf-8") as f:
            f.write(out_text)
    else:
        sys.stdout.write(out_text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
