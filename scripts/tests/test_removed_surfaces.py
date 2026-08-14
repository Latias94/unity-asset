from __future__ import annotations

import re
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class RemovedSurfaceTests(unittest.TestCase):
    def test_search_runtime_stays_local_only(self) -> None:
        removed_identifiers = (
            "VerifiedLocalStreamV1",
            "TokenStore",
            "DaemonToken",
            "verify_bearer_token",
            "HEALTH_ENDPOINT",
            "SEARCH_ENDPOINT",
            "SUGGEST_ENDPOINT",
            "REFERENCES_ENDPOINT",
            "REINDEX_ENDPOINT",
            "STATUS_ENDPOINT",
            "TOKEN_ROTATE_ENDPOINT",
            "TcpListener",
            "TcpStream",
        )
        source_roots = (
            "apps/unity-asset-search-cli/src",
            "apps/unity-asset-search-daemon/src",
            "crates/unity-asset-search-core/src",
            "crates/unity-asset-search-index/src",
            "crates/unity-asset-search-local/src",
            "crates/unity-asset-search-protocol/src",
        )
        for relative_root in source_roots:
            for path in (REPOSITORY_ROOT / relative_root).rglob("*.rs"):
                source = path.read_text(encoding="utf-8")
                for identifier in removed_identifiers:
                    with self.subTest(path=path, identifier=identifier):
                        self.assertIsNone(
                            re.search(rf"\b{re.escape(identifier)}\b", source)
                        )

    def test_removed_modules_stay_absent(self) -> None:
        removed_paths = (
            "vendor/globset",
            "crates/unity-asset/src/workspace/adapter/yaml.rs",
            "crates/unity-asset-binary/src/unity_objects.rs",
        )
        for relative_path in removed_paths:
            with self.subTest(path=relative_path):
                self.assertFalse((REPOSITORY_ROOT / relative_path).exists())

    def test_removed_public_forwarding_stays_absent(self) -> None:
        schema_root = REPOSITORY_ROOT / "crates/unity-asset/src/schema"
        for path in schema_root.rglob("*.rs"):
            source = path.read_text(encoding="utf-8")
            for name in ("HierarchyNode", "HierarchyState", "ChildPlacement"):
                patterns = (
                    rf"\bpub\s+(?:struct|enum|type)\s+{re.escape(name)}\b",
                    rf"\bpub\s+use\b[^;]*\b{re.escape(name)}\b",
                )
                for pattern in patterns:
                    with self.subTest(path=path, name=name, pattern=pattern):
                        self.assertIsNone(re.search(pattern, source))

        facade_contracts = {
            "crates/unity-asset/src/lib.rs": r"\bpub\s+use\s+unity_asset_yaml\b",
            "crates/unity-asset-yaml/src/lib.rs": r"\bpub\s+use\s+unity_asset_core\b",
            "crates/unity-asset-binary/src/lib.rs": (
                r"\bpub\s+(?:mod|use)\s+unity_objects\b"
            ),
        }
        for relative_path, removed_export_pattern in facade_contracts.items():
            source = (REPOSITORY_ROOT / relative_path).read_text(encoding="utf-8")
            with self.subTest(path=relative_path, export=removed_export_pattern):
                self.assertIsNone(re.search(removed_export_pattern, source))


if __name__ == "__main__":
    unittest.main()
