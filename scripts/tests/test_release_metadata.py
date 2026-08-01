from __future__ import annotations

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))

from release_metadata import (  # noqa: E402
    ReleaseMetadataError,
    metadata_from_changelog,
    verify_metadata_evidence,
    write_metadata_files,
)


class ReleaseMetadataTests(unittest.TestCase):
    def test_extracts_only_the_exact_version_section(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            changelog = root / "CHANGELOG.md"
            changelog.write_text(
                "# Changelog\n\n"
                "## [Unreleased]\n\nFuture work.\n\n"
                "## [1.2.3] - 2026-08-01\n\n### Added\n\n- Stable feature.\n\n"
                "## [1.2.2] - 2026-07-01\n\nOlder work.\n",
                encoding="utf-8",
            )

            metadata = metadata_from_changelog(changelog, "v1.2.3", "1.2.3")

            self.assertEqual(metadata.title, "v1.2.3")
            self.assertEqual(metadata.body, "### Added\n\n- Stable feature.\n")

    def test_rejects_missing_or_duplicate_version_sections(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            changelog = Path(temporary) / "CHANGELOG.md"
            changelog.write_text("# Changelog\n\n## [Unreleased]\n\nWork.\n", encoding="utf-8")
            with self.assertRaisesRegex(ReleaseMetadataError, "exactly one"):
                metadata_from_changelog(changelog, "v1.2.3", "1.2.3")

            changelog.write_text(
                "## [1.2.3]\n\nOne.\n\n## [1.2.3]\n\nTwo.\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ReleaseMetadataError, "exactly one"):
                metadata_from_changelog(changelog, "v1.2.3", "1.2.3")

    def test_requires_an_exact_heading_with_a_real_calendar_date(self) -> None:
        invalid_headings = (
            "## [1.2.3]",
            "## [1.2.3] - 2026-02-30",
            "## [1.2.3] - 2026-8-1",
            "## [1.2.3] - 2026-08-01 extra",
        )
        with tempfile.TemporaryDirectory() as temporary:
            changelog = Path(temporary) / "CHANGELOG.md"
            for heading in invalid_headings:
                with self.subTest(heading=heading):
                    changelog.write_text(
                        f"# Changelog\n\n{heading}\n\nBody.\n",
                        encoding="utf-8",
                    )
                    with self.assertRaises(ReleaseMetadataError):
                        metadata_from_changelog(changelog, "v1.2.3", "1.2.3")

    def test_written_files_and_evidence_bind_exact_normalized_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            changelog = root / "CHANGELOG.md"
            changelog.write_text(
                "## [1.2.3] - 2026-08-01\r\n\r\nBody.\r\n",
                encoding="utf-8",
            )
            metadata = metadata_from_changelog(changelog, "v1.2.3", "1.2.3")
            title = root / "title.txt"
            body = root / "body.md"
            write_metadata_files(metadata, title, body)
            evidence = root / "release-evidence.json"
            evidence.write_text(
                json.dumps({"github_release": metadata.evidence()}),
                encoding="utf-8",
            )

            verified = verify_metadata_evidence(
                evidence,
                title.read_text(encoding="utf-8").strip(),
                body.read_text(encoding="utf-8"),
            )

            self.assertEqual(verified, metadata)
            self.assertEqual(
                metadata.evidence()["body_sha256"],
                hashlib.sha256(b"Body.\n").hexdigest(),
            )

            body.write_text("Tampered.\n", encoding="utf-8")
            with self.assertRaisesRegex(ReleaseMetadataError, "does not match"):
                verify_metadata_evidence(
                    evidence,
                    title.read_text(encoding="utf-8").strip(),
                    body.read_text(encoding="utf-8"),
                )

            body.write_text(" Body.\n", encoding="utf-8")
            with self.assertRaisesRegex(ReleaseMetadataError, "does not match"):
                verify_metadata_evidence(
                    evidence,
                    title.read_text(encoding="utf-8").strip(),
                    body.read_text(encoding="utf-8"),
                )

    def test_metadata_outputs_do_not_follow_symbolic_links(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / "outside.txt"
            outside.write_text("preserve me\n", encoding="utf-8")
            linked_title = root / "title.txt"
            try:
                linked_title.symlink_to(outside)
            except OSError as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            with self.assertRaisesRegex(
                ReleaseMetadataError, "absent or a regular file"
            ):
                write_metadata_files(
                    metadata_from_changelog(
                        self._write_changelog(root), "v1.2.3", "1.2.3"
                    ),
                    linked_title,
                    root / "body.md",
                )

            self.assertEqual(outside.read_text(encoding="utf-8"), "preserve me\n")

    @staticmethod
    def _write_changelog(root: Path) -> Path:
        changelog = root / "CHANGELOG.md"
        changelog.write_text(
            "## [1.2.3] - 2026-08-01\n\nBody.\n",
            encoding="utf-8",
        )
        return changelog


if __name__ == "__main__":
    unittest.main()
