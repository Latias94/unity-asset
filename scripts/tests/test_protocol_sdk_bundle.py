from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path, PurePosixPath
from unittest import mock


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_ROOT))

import build_protocol_sdk_bundle as BUNDLE_CLI  # noqa: E402
import protocol_sdk_bundle as BUNDLE  # noqa: E402


class ProtocolSdkBundleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name) / "repository"
        reference = (
            self.root
            / "integration/search-protocol/csharp/UnityAsset.SearchProtocol.Reference"
        )
        fixtures = self.root / "integration/search-protocol/fixtures"
        schemas = self.root / "integration/search-protocol/schema"
        self.write(
            reference / "UnityAsset.SearchProtocol.Reference.csproj",
            b"<Project Sdk=\"Microsoft.NET.Sdk\" />\n",
        )
        self.write(reference / "Codecs.cs", b"public static class Codecs {}\n")
        self.write(reference / "Nested" / "StrictJson.cs", b"internal class StrictJson {}\n")
        self.write(reference / "Generated.g.cs", b"generated\n")
        self.write(reference / "Generated.designer.cs", b"designer\n")
        self.write(reference / "bin" / "Debug" / "Reference.dll", b"binary\n")
        self.write(reference / "obj" / "project.assets.json", b"{}\n")
        self.write(fixtures / "manifest.json", b'{"fixture_format":2}\n')
        self.write(
            fixtures / "requests" / "search-v2.json",
            b'{"operation":"search"}\n',
        )
        self.write(
            schemas / "bootstrap-v2.schema.json",
            b'{"$id":"bootstrap","$schema":"https://json-schema.org/draft/2020-12/schema"}\n',
        )
        self.write(
            schemas / "business-v5.schema.json",
            b'{"$id":"business","$schema":"https://json-schema.org/draft/2020-12/schema"}\n',
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    @staticmethod
    def write(path: Path, contents: bytes) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)

    def build(self, output_name: str) -> tuple[Path, object]:
        output = Path(self.temporary.name) / output_name
        metadata = BUNDLE.build_protocol_sdk_bundle(self.root, output, "v0.4.0")
        return output / metadata.artifact_name, metadata

    def test_build_is_byte_deterministic_and_inventory_is_canonical(self) -> None:
        first_path, first_metadata = self.build("first")
        second_path, second_metadata = self.build("second")

        self.assertEqual(first_path.read_bytes(), second_path.read_bytes())
        self.assertEqual(first_metadata, second_metadata)
        self.assertEqual(
            first_metadata.artifact_name,
            "unity-asset-search-protocol-sdk-v0.4.0.zip",
        )
        self.assertEqual(
            BUNDLE.verify_protocol_sdk_bundle(first_path, "v0.4.0"),
            first_metadata,
        )

        with zipfile.ZipFile(first_path) as archive:
            infos = archive.infolist()
            names = [info.filename for info in infos]
            self.assertEqual(names, sorted(names))
            self.assertTrue(
                all(info.date_time == BUNDLE.FIXED_ZIP_TIMESTAMP for info in infos)
            )
            self.assertTrue(
                all(
                    ((info.external_attr >> 16) & 0xFFFF)
                    == BUNDLE.ARCHIVE_FILE_MODE
                    for info in infos
                )
            )
            self.assertFalse(any("/bin/" in name or "/obj/" in name for name in names))
            self.assertTrue(any(name.endswith("Generated.g.cs") for name in names))
            self.assertTrue(any(name.endswith("Generated.designer.cs") for name in names))

            root = BUNDLE.archive_root_for_tag("v0.4.0")
            manifest = json.loads(
                archive.read(f"{root}/{BUNDLE.MANIFEST_FILENAME}")
            )
            inventoried = [file["path"] for file in manifest["files"]]
            self.assertEqual(inventoried, sorted(inventoried))
            self.assertEqual(
                inventoried,
                [
                    "csharp/UnityAsset.SearchProtocol.Reference/Codecs.cs",
                    "csharp/UnityAsset.SearchProtocol.Reference/Generated.designer.cs",
                    "csharp/UnityAsset.SearchProtocol.Reference/Generated.g.cs",
                    "csharp/UnityAsset.SearchProtocol.Reference/Nested/StrictJson.cs",
                    "csharp/UnityAsset.SearchProtocol.Reference/"
                    "UnityAsset.SearchProtocol.Reference.csproj",
                    "fixtures/manifest.json",
                    "fixtures/requests/search-v2.json",
                    "schema/bootstrap-v2.schema.json",
                    "schema/business-v5.schema.json",
                ],
            )

    def test_cli_writes_archive_and_prints_canonical_release_evidence(self) -> None:
        output = Path(self.temporary.name) / "cli"
        extracted = Path(self.temporary.name) / "extracted"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            result = BUNDLE_CLI.main(
                [
                    "--repository-root",
                    str(self.root),
                    "--release-tag",
                    "v0.4.0",
                    "--output-directory",
                    str(output),
                    "--extract-directory",
                    str(extracted),
                ]
            )

        self.assertEqual(result, 0)
        evidence = json.loads(stdout.getvalue())
        self.assertEqual(evidence["schema"], BUNDLE.BUNDLE_METADATA_SCHEMA)
        self.assertEqual(evidence["release_tag"], "v0.4.0")
        artifact = output / evidence["artifact_name"]
        self.assertTrue(artifact.is_file())
        self.assertEqual(
            BUNDLE.verify_protocol_sdk_bundle(artifact, "v0.4.0").sha256,
            evidence["sha256"],
        )
        extracted_project = (
            extracted
            / BUNDLE.archive_root_for_tag("v0.4.0")
            / "csharp/UnityAsset.SearchProtocol.Reference/"
            "UnityAsset.SearchProtocol.Reference.csproj"
        )
        self.assertTrue(extracted_project.is_file())

    def test_existing_bundle_cli_verifies_and_extracts_the_exact_archive(self) -> None:
        bundle_path, expected = self.build("bundle-source")
        extracted = Path(self.temporary.name) / "verified-extracted"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            result = BUNDLE_CLI.main(
                [
                    "--release-tag",
                    "v0.4.0",
                    "--bundle",
                    str(bundle_path),
                    "--extract-directory",
                    str(extracted),
                ]
            )

        self.assertEqual(result, 0)
        self.assertEqual(json.loads(stdout.getvalue()), expected.as_dict())
        self.assertTrue(
            (
                extracted
                / BUNDLE.archive_root_for_tag("v0.4.0")
                / "fixtures/manifest.json"
            ).is_file()
        )

    def test_text_inputs_are_canonical_across_lf_and_crlf_checkouts(self) -> None:
        first_path, _ = self.build("lf")
        first_bytes = first_path.read_bytes()
        source = (
            self.root
            / "integration/search-protocol/csharp/"
            "UnityAsset.SearchProtocol.Reference/Codecs.cs"
        )
        source.write_bytes(b"\xef\xbb\xbfpublic static class Codecs {}\r\n")
        second_path, _ = self.build("crlf")

        self.assertEqual(first_bytes, second_path.read_bytes())

    def test_invalid_utf8_source_is_rejected(self) -> None:
        source = self.root / "integration/search-protocol/fixtures/invalid-utf8.json"
        source.write_bytes(b"\xff")

        with self.assertRaisesRegex(BUNDLE.ProtocolSdkBundleError, "valid UTF-8"):
            self.build("invalid-utf8")

    def test_inventory_budget_stops_before_reading_the_overflowing_file(self) -> None:
        source = Path(self.temporary.name) / "budget-source"
        self.write(source / "a.cs", b"aaaa")
        self.write(source / "b.cs", b"bbbb")
        original_read_bytes = Path.read_bytes
        reads: list[str] = []

        def tracked_read_bytes(path: Path) -> bytes:
            reads.append(path.name)
            return original_read_bytes(path)

        with (
            mock.patch.object(BUNDLE, "MAX_BUNDLE_BYTES", 7),
            mock.patch.object(Path, "read_bytes", tracked_read_bytes),
            self.assertRaisesRegex(BUNDLE.ProtocolSdkBundleError, "byte budget"),
        ):
            BUNDLE._collect_directory(
                source,
                PurePosixPath("source"),
                exclude_generated=False,
                inventory=BUNDLE._SourceInventory(),
            )

        self.assertEqual(reads, ["a.cs"])

    def test_portable_archive_policy_rejects_windows_aliases(self) -> None:
        for path in ("fixtures/CON.json", "fixtures/trailing. ", "fixtures/a:b.json"):
            with self.subTest(path=path), self.assertRaisesRegex(
                BUNDLE.ProtocolSdkBundleError, "unsafe archive path"
            ):
                BUNDLE._validated_archive_path(path)
        self.assertEqual(
            BUNDLE._portable_archive_key("fixtures/Café.json"),
            BUNDLE._portable_archive_key("fixtures/Cafe\u0301.json"),
        )

    def test_verifier_rejects_noncanonical_text_even_when_manifest_matches(self) -> None:
        sources = (
            BUNDLE._SourceFile(
                "csharp/UnityAsset.SearchProtocol.Reference/Codecs.cs",
                b"public class Codecs {}\r\n",
            ),
            BUNDLE._SourceFile(
                "csharp/UnityAsset.SearchProtocol.Reference/"
                "UnityAsset.SearchProtocol.Reference.csproj",
                b"<Project />\n",
            ),
            BUNDLE._SourceFile("fixtures/manifest.json", b"{}\n"),
            BUNDLE._SourceFile(
                "schema/bootstrap-v2.schema.json",
                b'{"$schema":"https://json-schema.org/draft/2020-12/schema"}\n',
            ),
            BUNDLE._SourceFile(
                "schema/business-v5.schema.json",
                b'{"$schema":"https://json-schema.org/draft/2020-12/schema"}\n',
            ),
        )
        bundle = BUNDLE._build_bundle_bytes("v0.4.0", sources)
        bundle_path = (
            Path(self.temporary.name) / BUNDLE.archive_name_for_tag("v0.4.0")
        )
        bundle_path.write_bytes(bundle)

        with self.assertRaisesRegex(BUNDLE.ProtocolSdkBundleError, "canonical UTF-8 LF"):
            BUNDLE.verify_protocol_sdk_bundle(bundle_path, "v0.4.0")

    def test_verifier_rejects_payload_tampering(self) -> None:
        bundle_path, _ = self.build("tamper")
        with zipfile.ZipFile(bundle_path, mode="r") as archive:
            entries = [
                (info, archive.read(info.filename)) for info in archive.infolist()
            ]
        target = next(
            index
            for index, (info, _) in enumerate(entries)
            if info.filename.endswith("/Codecs.cs")
        )
        info, original = entries[target]
        tampered = bytes([original[0] ^ 1]) + original[1:]
        entries[target] = (info, tampered)
        with zipfile.ZipFile(
            bundle_path,
            mode="w",
            compression=zipfile.ZIP_STORED,
            allowZip64=False,
        ) as archive:
            for entry_info, contents in entries:
                archive.writestr(entry_info, contents)

        with self.assertRaisesRegex(BUNDLE.ProtocolSdkBundleError, "digest mismatch"):
            BUNDLE.verify_protocol_sdk_bundle(bundle_path, "v0.4.0")

        extraction = Path(self.temporary.name) / "tampered-extraction"
        with self.assertRaisesRegex(BUNDLE.ProtocolSdkBundleError, "digest mismatch"):
            BUNDLE.extract_protocol_sdk_bundle(
                bundle_path,
                extraction,
                "v0.4.0",
            )
        self.assertFalse(
            (extraction / BUNDLE.archive_root_for_tag("v0.4.0")).exists()
        )

    def test_verifier_rejects_archive_path_traversal(self) -> None:
        bundle_path, _ = self.build("path-traversal")
        root = BUNDLE.archive_root_for_tag("v0.4.0")
        with zipfile.ZipFile(bundle_path, mode="r") as archive:
            entries = [
                (info, archive.read(info.filename)) for info in archive.infolist()
            ]
        entries.append(
            (BUNDLE._zip_info(f"{root}/fixtures/../escape.json"), b"{}\n")
        )
        entries.sort(key=lambda entry: entry[0].filename)
        with zipfile.ZipFile(
            bundle_path,
            mode="w",
            compression=zipfile.ZIP_STORED,
            allowZip64=False,
        ) as archive:
            for entry_info, contents in entries:
                archive.writestr(entry_info, contents)

        with self.assertRaisesRegex(BUNDLE.ProtocolSdkBundleError, "unsafe archive path"):
            BUNDLE.verify_protocol_sdk_bundle(bundle_path, "v0.4.0")

    def test_builder_rejects_symlinked_source_files(self) -> None:
        outside = Path(self.temporary.name) / "outside.json"
        outside.write_text("{}\n", encoding="utf-8")
        linked = (
            self.root / "integration/search-protocol/fixtures/linked-fixture.json"
        )
        try:
            os.symlink(outside, linked)
        except (NotImplementedError, OSError) as error:
            self.skipTest(f"symlink creation is unavailable: {error}")

        with self.assertRaisesRegex(
            BUNDLE.ProtocolSdkBundleError, "symlink or junction"
        ):
            BUNDLE.build_protocol_sdk_bundle(
                self.root,
                Path(self.temporary.name) / "symlink-output",
                "v0.4.0",
            )

    def test_builder_rejects_output_inside_an_input_tree(self) -> None:
        fixture_root = self.root / "integration/search-protocol/fixtures"
        with self.assertRaisesRegex(
            BUNDLE.ProtocolSdkBundleError, "outside the bundled source trees"
        ):
            BUNDLE.build_protocol_sdk_bundle(
                self.root,
                fixture_root / "release-output",
                "v0.4.0",
            )

    def test_builder_requires_both_public_schema_entrypoints(self) -> None:
        (self.root / "integration/search-protocol/schema/business-v5.schema.json").unlink()
        with self.assertRaisesRegex(
            BUNDLE.ProtocolSdkBundleError, "protocol SDK source inventory is incomplete"
        ):
            self.build("missing-schema")

    def test_release_tag_cannot_inject_an_archive_path(self) -> None:
        with self.assertRaisesRegex(
            BUNDLE.ProtocolSdkBundleError, "vMAJOR.MINOR.PATCH"
        ):
            BUNDLE.archive_name_for_tag("v0.4.0/../../escape")


if __name__ == "__main__":
    unittest.main()
