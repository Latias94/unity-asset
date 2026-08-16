from __future__ import annotations

import importlib
import sys
import unittest
from pathlib import Path


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))


RELEASE_ENTRYPOINT_MODULES = (
    "assemble_release_assets",
    "build_protocol_sdk_bundle",
    "install_cargo_dist",
    "publish_workspace_packages",
    "verify_github_release_assets",
    "verify_release_bundle",
    "verify_release_source",
    "verify_release_tag",
    "verify_workspace_packages",
)


class ReleaseScriptImportTests(unittest.TestCase):
    def test_every_release_entrypoint_imports_from_a_clean_checkout(self) -> None:
        for module in RELEASE_ENTRYPOINT_MODULES:
            with self.subTest(module=module):
                imported = importlib.import_module(module)
                self.assertTrue(callable(getattr(imported, "main", None)))


if __name__ == "__main__":
    unittest.main()
