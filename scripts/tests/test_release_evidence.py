from __future__ import annotations

import json
import hashlib
import os
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPTS_ROOT = Path(__file__).resolve().parents[1]
TESTS_ROOT = Path(__file__).resolve().parent
if str(SCRIPTS_ROOT) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_ROOT))
if str(TESTS_ROOT) not in sys.path:
    sys.path.insert(0, str(TESTS_ROOT))

from release_atomic import (  # noqa: E402
    ReleaseAtomicWriteError,
    atomic_write_bytes,
)
from release_evidence import (  # noqa: E402
    ReleaseEvidenceError,
    canonical_json_bytes,
    load_release_evidence,
    parse_release_evidence,
)
from release_evidence_support import make_release_evidence  # noqa: E402


class ReleaseEvidenceTests(unittest.TestCase):
    def test_complete_evidence_round_trips_canonically(self) -> None:
        payload = make_release_evidence(dist_artifacts=("a.tar.xz", "a.tar.xz.sha256"))
        evidence = parse_release_evidence(
            payload,
            expected_tag="v1.2.3",
            expected_version="1.2.3",
            expected_commit="2" * 40,
            expected_tag_object="1" * 40,
            expected_dist_plan_sha256="d" * 64,
            expected_dist_artifacts=("a.tar.xz", "a.tar.xz.sha256"),
            expected_protocol_sdk=payload["protocol_sdk"],
            expected_github_release=payload["github_release"],
        )
        self.assertEqual(evidence.as_dict(), payload)

    def test_rejects_schema_topology_and_profile_drift(self) -> None:
        for mutate, message in (
            (lambda payload: payload.__setitem__("extra", True), "invalid schema"),
            (
                lambda payload: payload["publish_order"].reverse(),
                "publish order does not match",
            ),
            (
                lambda payload: payload["packages"].reverse(),
                "package topology/order",
            ),
            (
                lambda payload: payload["documented_feature_profiles"].clear(),
                "feature profiles",
            ),
        ):
            with self.subTest(message=message):
                payload = make_release_evidence()
                mutate(payload)
                with self.assertRaisesRegex(ReleaseEvidenceError, message):
                    parse_release_evidence(payload)

    def test_rejects_cross_field_identity_and_digest_mismatches(self) -> None:
        cases = (
            ("version", "9.9.9", "version does not match"),
            ("dist_plan_sha256", "not-a-digest", "lowercase SHA-256"),
            ("release_toolchain", "1.70.0", "older than the MSRV"),
        )
        for key, value, message in cases:
            with self.subTest(key=key):
                payload = make_release_evidence()
                payload[key] = value
                with self.assertRaisesRegex(ReleaseEvidenceError, message):
                    parse_release_evidence(payload)

    def test_loader_rejects_noncanonical_and_duplicate_json(self) -> None:
        payload = make_release_evidence()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "release-evidence.json"
            path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
            with self.assertRaisesRegex(ReleaseEvidenceError, "not canonically encoded"):
                load_release_evidence(path)

            canonical = canonical_json_bytes(payload)
            duplicate = canonical.replace(
                b'{"cargo_lock_sha256":',
                b'{"cargo_lock_sha256":"' + b"0" * 64 + b'","cargo_lock_sha256":',
                1,
            )
            path.write_bytes(duplicate)
            with self.assertRaisesRegex(ReleaseEvidenceError, "duplicate key"):
                load_release_evidence(path)

    def test_loader_binds_exact_canonical_bytes_and_digest(self) -> None:
        payload = make_release_evidence()
        encoded = canonical_json_bytes(payload)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "release-evidence.json"
            path.write_bytes(encoded)
            evidence = load_release_evidence(
                path,
                expected_sha256=hashlib.sha256(encoded).hexdigest(),
                expected_tag="v1.2.3",
            )
            self.assertEqual(evidence.commit, "2" * 40)
            with self.assertRaisesRegex(ReleaseEvidenceError, "SHA-256 does not match"):
                load_release_evidence(path, expected_sha256="0" * 64)


class ReleaseAtomicWriteTests(unittest.TestCase):
    def test_uses_random_temporary_leaf_and_replaces_regular_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "proof.json"
            predictable = root / "proof.json.tmp"
            predictable.write_bytes(b"sentinel")
            atomic_write_bytes(output, b"first", "proof")
            atomic_write_bytes(output, b"second", "proof")
            self.assertEqual(output.read_bytes(), b"second")
            self.assertEqual(predictable.read_bytes(), b"sentinel")

    def test_rejects_final_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            target.write_bytes(b"safe")
            link = root / "proof.json"
            try:
                os.symlink(target, link)
            except (OSError, NotImplementedError):
                self.skipTest("symlink creation is unavailable")
            with self.assertRaisesRegex(ReleaseAtomicWriteError, "symlink|regular file"):
                atomic_write_bytes(link, b"unsafe", "proof")
            self.assertEqual(target.read_bytes(), b"safe")


if __name__ == "__main__":
    unittest.main()
