from __future__ import annotations

import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class RemovedSurfaceTests(unittest.TestCase):
    def test_removed_modules_stay_absent(self) -> None:
        removed_paths = (
            ".github/workflows/upload-dist-assets.yml",
            "vendor/globset",
            "apps/unity-asset-search-daemon/src/ipc/mod.rs",
            "crates/unity-asset-search-local/src/pipe_rendezvous.rs",
            "crates/unity-asset-search-local/src/transport.rs",
            "crates/unity-asset-search-local/src/transport_unix.rs",
            "crates/unity-asset-search-local/src/transport_windows.rs",
            "crates/unity-asset-search-protocol/src/bootstrap.rs",
            "crates/unity-asset-search-protocol/src/framing.rs",
            "crates/unity-asset/src/workspace/adapter/yaml.rs",
            "crates/unity-asset-binary/src/unity_objects.rs",
            "integration/search-protocol/csharp/UnityAsset.SearchProtocol.Reference/Framing.cs",
        )
        for relative_path in removed_paths:
            with self.subTest(path=relative_path):
                self.assertFalse((REPOSITORY_ROOT / relative_path).exists())


if __name__ == "__main__":
    unittest.main()
