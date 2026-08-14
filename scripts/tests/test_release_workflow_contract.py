from __future__ import annotations

import re
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "release.yml"
CI_WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
REMOVED_BACKFILL = (
    REPOSITORY_ROOT / ".github" / "workflows" / "upload-dist-assets.yml"
)
ACTION_PIN = re.compile(r"^\s*uses:\s*[^\s@]+@[0-9a-f]{40}(?:\s+#.*)?$")


def mapping_block(document: str, key: str, indent: int) -> str:
    lines = document.splitlines(keepends=True)
    prefix = " " * indent
    start = next(
        index for index, line in enumerate(lines) if line == f"{prefix}{key}:\n"
    )
    block = [lines[start]]
    for line in lines[start + 1 :]:
        if line.strip() and len(line) - len(line.lstrip(" ")) <= indent:
            break
        block.append(line)
    return "".join(block)


def job_block(document: str, job: str) -> str:
    return mapping_block(document, job, 2)


def step_block(job: str, name: str) -> str:
    lines = job.splitlines(keepends=True)
    marker = f"      - name: {name}\n"
    start = lines.index(marker)
    block = [lines[start]]
    for line in lines[start + 1 :]:
        if line.startswith("      - "):
            break
        block.append(line)
    return "".join(block)


class ReleaseWorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        cls.ci = CI_WORKFLOW.read_text(encoding="utf-8")
        cls.jobs = {
            name: job_block(cls.workflow, name)
            for name in (
                "validate",
                "msrv",
                "test",
                "package",
                "platform-contracts",
                "github-draft",
                "publish",
                "dist",
                "release-assets",
                "attest",
                "dry-run",
                "github-release",
            )
        }

    def test_release_actions_are_pinned_to_full_object_ids(self) -> None:
        uses_lines = [
            line for line in self.workflow.splitlines() if line.lstrip().startswith("uses:")
        ]
        self.assertTrue(uses_lines)
        for line in uses_lines:
            self.assertRegex(line, ACTION_PIN)

    def test_pull_request_ci_runs_actionlint(self) -> None:
        self.assertIn(
            "go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.7",
            self.ci,
        )

    def test_ci_runs_package_consumers_on_every_os_and_binary_identity_once(self) -> None:
        package_job = job_block(self.ci, "workspace-package")
        matrix = mapping_block(package_job, "matrix", 6)
        self.assertIn("- os: ubuntu-latest\n            mode: full", matrix)
        self.assertIn("- os: macos-latest\n            mode: packages", matrix)
        self.assertIn("- os: windows-latest\n            mode: packages", matrix)
        self.assertEqual(matrix.count("- os:"), 3)
        self.assertEqual(matrix.count("mode: packages"), 2)
        self.assertEqual(matrix.count("mode: full"), 1)
        self.assertNotIn("mode: binaries", matrix)
        self.assertIn(
            "python scripts/verify_workspace_packages.py --mode ${{ matrix.mode }}",
            package_job,
        )
        self.assertIn("UNITY_ASSET_SOURCE_COMMIT: ${{ github.sha }}", self.ci)

    def test_platform_jobs_run_the_explicit_real_daemon_agent_harness(self) -> None:
        ci_platform = job_block(self.ci, "workspace-publication-platforms")
        release_platform = job_block(self.workflow, "platform-contracts")
        command = "python scripts/run_real_daemon_agent.py"
        self.assertIn(command, ci_platform)
        self.assertIn(command, release_platform)

    def test_every_release_job_has_a_finite_execution_bound(self) -> None:
        for job_name, job in self.jobs.items():
            with self.subTest(job=job_name):
                self.assertRegex(job, r"(?m)^    timeout-minutes: [1-9][0-9]*$")

    def test_only_explicit_mode_separated_dispatch_can_publish(self) -> None:
        self.assertFalse(REMOVED_BACKFILL.exists())
        trigger = mapping_block(self.workflow, "on", 0)
        self.assertIn("workflow_dispatch:", trigger)
        self.assertNotIn("push:", trigger)
        self.assertIn("Existing annotated vMAJOR.MINOR.PATCH tag", trigger)
        self.assertIn("- dry-run", trigger)
        self.assertIn("- publish", trigger)
        self.assertNotIn("cargo dist ", self.workflow)
        validate = self.jobs["validate"]
        dispatch_gate = step_block(
            validate, "Require triggering ref to match selected tag"
        )
        self.assertIn("EVENT_REF: ${{ github.ref }}", dispatch_gate)
        self.assertIn('expected_ref="refs/tags/$RELEASE_TAG"', dispatch_gate)
        self.assertIn('[[ "$EVENT_REF" != "$expected_ref" ]]', dispatch_gate)
        self.assertLess(
            validate.index("Require triggering ref to match selected tag"),
            validate.index("Checkout selected tag"),
        )
        self.assertIn("ref: refs/tags/${{ env.RELEASE_TAG }}", validate)
        self.assertIn("if: inputs.mode == 'dry-run'", self.jobs["dry-run"])
        self.assertIn("python scripts/verify_release_bundle.py", self.jobs["dry-run"])
        for job_name in ("attest", "github-draft", "publish", "github-release"):
            self.assertIn("if: inputs.mode == 'publish'", self.jobs[job_name])
        for job_name in ("msrv", "test", "package", "platform-contracts", "dist"):
            self.assertIn(
                "ref: ${{ needs.validate.outputs.commit }}", self.jobs[job_name]
            )
        self.assertIn("${{ inputs.mode }}-${{ inputs.tag }}", self.workflow)
        self.assertIn("cancel-in-progress: false", self.workflow)

    def test_real_msrv_is_selected_despite_the_repository_override(self) -> None:
        validate = self.jobs["validate"]
        msrv = self.jobs["msrv"]
        self.assertIn("msrv: ${{ steps.source.outputs.msrv }}", validate)
        self.assertIn("release_toolchain: ${{ steps.source.outputs.release_toolchain }}", validate)
        self.assertIn("toolchain: ${{ needs.validate.outputs.msrv }}", msrv)
        toolchain = step_block(validate, "Read tracked release Rust toolchain")
        setup = step_block(validate, "Install release Rust toolchain")
        self.assertIn('pathlib.Path("rust-toolchain.toml")', toolchain)
        self.assertIn(
            "toolchain: ${{ steps.toolchain.outputs.release_toolchain }}", setup
        )
        self.assertNotRegex(validate, r"(?m)^\s+toolchain: 1\.[0-9]+\.[0-9]+$")
        self.assertLess(
            validate.index("Read tracked release Rust toolchain"),
            validate.index("Install release Rust toolchain"),
        )
        gate = step_block(msrv, "Check every library against the locked graph")
        self.assertIn("rustc +${{ needs.validate.outputs.msrv }} --version", gate)
        self.assertIn(
            "cargo +${{ needs.validate.outputs.msrv }} check --workspace --lib --all-features --locked",
            gate,
        )

    def test_publish_credentials_are_scoped_to_the_protected_publish_step(self) -> None:
        publish = self.jobs["publish"]
        self.assertIn(
            "needs: [validate, release-assets, github-draft]", publish
        )
        self.assertIn("timeout-minutes: 90", publish)
        self.assertIn("name: crates-io-production", publish)
        publish_step = step_block(
            publish, "Publish verified workspace packages in dependency order"
        )
        self.assertIn(
            "UNITY_ASSET_RELEASE_CARGO_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN_PRODUCTION }}",
            publish_step,
        )
        self.assertIn("python scripts/publish_workspace_packages.py", publish_step)
        self.assertNotIn("CARGO_REGISTRY_TOKEN:", self.workflow)
        self.assertEqual(
            self.workflow.count("secrets.CARGO_REGISTRY_TOKEN_PRODUCTION"), 1
        )

    def test_release_keeps_the_full_package_and_binary_gate_on_ubuntu(self) -> None:
        package = self.jobs["package"]
        self.assertIn("runs-on: ubuntu-latest", package)
        self.assertNotIn("strategy:", package)
        self.assertNotIn("UNITY_ASSET_SOURCE_COMMIT", package)
        self.assertRegex(
            package,
            r"(?m)^\s+run: python scripts/verify_workspace_packages\.py --mode full$",
        )
        self.assertIn("timeout-minutes: 45", package)

    def test_asset_publication_is_verified_before_crates_io_and_after_upload(self) -> None:
        draft = self.jobs["github-draft"]
        self.assertIn("needs: [validate, release-assets, attest]", draft)
        self.assertIn("contents: write", draft)
        self.assertIn(
            "release_id: ${{ steps.staged.outputs.release_id }}", draft
        )
        preflight = step_block(draft, "Reject a pre-existing non-identical release")
        upload = step_block(draft, "Create or update the verified draft release")
        readback = step_block(draft, "Read back the complete staged asset set")
        self.assertIn("--phase preflight", preflight)
        for argument in (
            "--expected-title",
            "--expected-body-file",
            "--expected-evidence-sha256",
        ):
            self.assertIn(argument, preflight)
        self.assertIn("if: steps.preflight.outputs.needs_upload == 'true'", upload)
        self.assertIn("draft: true", upload)
        self.assertIn("overwrite_files: true", upload)
        self.assertIn("id: staged", readback)
        self.assertIn("--phase staged", readback)
        self.assertIn('--github-output "$GITHUB_OUTPUT"', readback)

        final = self.jobs["github-release"]
        self.assertIn("needs: [validate, publish, github-draft, release-assets]", final)
        self.assertIn("--phase publish", final)
        self.assertIn(
            '--expected-release-id "${{ needs.github-draft.outputs.release_id }}"',
            final,
        )
        self.assertNotIn("softprops/action-gh-release", final)

    def test_production_approval_rechecks_the_bound_draft_before_crates_io(self) -> None:
        publish = self.jobs["publish"]
        download_name = "Download attested release assets after production approval"
        verify_name = "Reverify staged GitHub Release after production approval"
        crates_name = "Publish verified workspace packages in dependency order"
        download = step_block(publish, download_name)
        verification = step_block(publish, verify_name)

        self.assertIn("name: release-assets-${{ github.run_id }}", download)
        self.assertIn("path: release-assets", download)
        self.assertIn("python scripts/verify_github_release_assets.py", verification)
        self.assertIn('--tag "$RELEASE_TAG"', verification)
        self.assertIn(
            '--commit "${{ needs.validate.outputs.commit }}"', verification
        )
        self.assertIn("--assets release-assets", verification)
        self.assertIn("--phase staged", verification)
        self.assertIn("--expected-body-file", verification)
        self.assertIn("--expected-evidence-sha256", verification)
        self.assertIn(
            '--expected-release-id "${{ needs.github-draft.outputs.release_id }}"',
            verification,
        )
        self.assertLess(publish.index(download_name), publish.index(verify_name))
        self.assertLess(publish.index(verify_name), publish.index(crates_name))

    def test_source_evidence_binds_the_exact_dist_plan_and_artifact_inventory(self) -> None:
        validate = self.jobs["validate"]
        release_assets = self.jobs["release-assets"]
        self.assertIn("dist manifest --artifacts=local --output-format=json --no-local-paths", validate)
        self.assertIn("--dist-plan", validate)
        self.assertIn("dist_plan_sha256: ${{ steps.source.outputs.dist_plan_sha256 }}", validate)
        self.assertIn("python scripts/build_protocol_sdk_bundle.py", validate)
        self.assertIn("--extract-directory", validate)
        self.assertIn("Compile the exact generated search protocol SDK", validate)
        self.assertIn("UnityAsset.SearchProtocol.Reference.csproj", validate)
        self.assertIn("protocol_sdk_artifact: ${{ steps.source.outputs.protocol_sdk_artifact }}", validate)
        self.assertIn("dist_matrix: ${{ steps.source.outputs.dist_matrix }}", validate)
        assembler = step_block(release_assets, "Assemble collision-free release assets")
        self.assertIn("--expected-dist-plan-sha256", assembler)
        self.assertIn("--protocol-sdk-bundle", assembler)
        self.assertNotIn("attestations: write", release_assets)
        self.assertNotIn("id-token: write", release_assets)
        self.assertNotIn("actions/attest-build-provenance@", release_assets)
        attest = self.jobs["attest"]
        self.assertIn("needs: [validate, release-assets]", attest)
        self.assertIn("attestations: write", attest)
        self.assertIn("id-token: write", attest)
        self.assertIn("actions/attest-build-provenance@", attest)
        for job in (validate, self.jobs["dist"], release_assets):
            self.assertIn("overwrite: true", job)
        self.assertIn("dist build --artifacts=local", self.jobs["dist"])
        native_probe = step_block(
            self.jobs["dist"],
            "Execute native release binaries and verify build identities",
        )
        self.assertIn(
            'archive="target/distrib/$application-$target$extension"', native_probe
        )
        self.assertIn("shutil.unpack_archive", native_probe)
        self.assertIn('test "$executable_count" -eq 1', native_probe)
        self.assertIn('actual="$("$executable" --version', native_probe)
        self.assertIn("unity-asset.build-identity.v1{", native_probe)
        self.assertIn("test ! -s \"$stderr_file\"", native_probe)
        self.assertNotIn('target/$target/release/$application', native_probe)
        self.assertLess(
            self.jobs["dist"].index("Execute native release binaries"),
            self.jobs["dist"].index("Upload dist artifacts"),
        )
        self.assertIn(
            "matrix: ${{ fromJSON(needs.validate.outputs.dist_matrix) }}",
            self.jobs["dist"],
        )

    def test_release_metadata_and_contract_tests_are_release_inputs(self) -> None:
        validate = self.jobs["validate"]
        self.assertIn(
            'python -m unittest discover -s scripts/tests -p "test_*.py"',
            validate,
        )
        self.assertIn("github.com/rhysd/actionlint/cmd/actionlint@v1.7.7", validate)
        self.assertIn("--release-title-output", validate)
        self.assertIn("--release-body-output", validate)
        self.assertNotIn("extract-release-notes", self.workflow)
        draft = self.jobs["github-draft"]
        self.assertIn("body_path: release-proof/release-notes.md", draft)
        self.assertIn("name: ${{ env.RELEASE_TAG }}", draft)
        dry_run = self.jobs["dry-run"]
        self.assertIn("name: release-evidence-${{ github.run_id }}", dry_run)
        self.assertIn("--release-title release-proof/release-title.txt", dry_run)
        self.assertIn("--release-body release-proof/release-notes.md", dry_run)

    def test_tag_validation_is_a_single_scripted_contract_at_each_boundary(self) -> None:
        for job_name in ("validate", "github-draft", "publish", "github-release"):
            self.assertIn("python scripts/verify_release_tag.py", self.jobs[job_name])
        initial_validation = step_block(
            self.jobs["validate"], "Require GitHub-verified annotated tag"
        )
        self.assertIn('--expected-event-sha "${{ github.sha }}"', initial_validation)
        for job_name in ("github-draft", "publish", "github-release"):
            self.assertNotIn("--expected-event-sha", self.jobs[job_name])
        self.assertNotIn("source-recheck:", self.workflow)
        self.assertIn("cancel-in-progress: false", self.workflow)


if __name__ == "__main__":
    unittest.main()
