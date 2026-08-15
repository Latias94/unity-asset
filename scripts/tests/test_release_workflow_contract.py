from __future__ import annotations

import re
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "release.yml"
CI_WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
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


def step_containing(job: str, marker: str) -> str:
    steps: list[str] = []
    current: list[str] = []
    for line in job.splitlines(keepends=True):
        if line.startswith("      - "):
            if current:
                steps.append("".join(current))
            current = [line]
        elif current:
            current.append(line)
    if current:
        steps.append("".join(current))
    matches = [step for step in steps if marker in step]
    if len(matches) != 1:
        raise AssertionError(f"expected one workflow step containing {marker!r}")
    return matches[0]


class ReleaseWorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        cls.ci = CI_WORKFLOW.read_text(encoding="utf-8")
        cls.jobs = {
            name: job_block(cls.workflow, name)
            for name in (
                "validate",
                "protocol-sdk",
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

    def test_workflow_syntax_and_action_supply_chain_are_guarded(self) -> None:
        uses_lines = [
            line for line in self.workflow.splitlines() if line.lstrip().startswith("uses:")
        ]
        self.assertTrue(uses_lines)
        for line in uses_lines:
            self.assertRegex(line, ACTION_PIN)
        self.assertIn(
            "go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.7",
            self.ci,
        )

    def test_ci_keeps_cross_platform_package_and_real_daemon_contracts(self) -> None:
        package_job = job_block(self.ci, "workspace-package")
        matrix = mapping_block(package_job, "matrix", 6)
        self.assertIn("- os: ubuntu-latest\n            mode: full", matrix)
        self.assertIn("- os: macos-latest\n            mode: packages", matrix)
        self.assertIn("- os: windows-latest\n            mode: packages", matrix)
        self.assertEqual(matrix.count("- os:"), 3)
        self.assertIn(
            "python scripts/verify_workspace_packages.py --mode ${{ matrix.mode }}",
            package_job,
        )
        self.assertIn("UNITY_ASSET_SOURCE_COMMIT: ${{ github.sha }}", self.ci)

        command = "python scripts/run_real_daemon_agent.py"
        self.assertIn(command, job_block(self.ci, "workspace-publication-platforms"))
        self.assertIn(command, self.jobs["platform-contracts"])

    def test_release_jobs_are_finite_and_only_explicit_dispatch_can_publish(self) -> None:
        for job_name, job in self.jobs.items():
            with self.subTest(job=job_name):
                self.assertRegex(job, r"(?m)^    timeout-minutes: [1-9][0-9]*$")

        trigger = mapping_block(self.workflow, "on", 0)
        self.assertIn("workflow_dispatch:", trigger)
        self.assertNotIn("push:", trigger)
        self.assertIn("- dry-run", trigger)
        self.assertIn("- publish", trigger)
        self.assertIn("if: inputs.mode == 'dry-run'", self.jobs["dry-run"])
        for job_name in ("attest", "github-draft", "publish", "github-release"):
            self.assertIn("if: inputs.mode == 'publish'", self.jobs[job_name])
        self.assertIn("${{ inputs.mode }}-${{ inputs.tag }}", self.workflow)
        self.assertIn("cancel-in-progress: false", self.workflow)

    def test_default_branch_controller_verifies_the_isolated_tag_candidate(self) -> None:
        validate = self.jobs["validate"]
        gate = step_containing(validate, 'expected_ref="refs/heads/$DEFAULT_BRANCH"')
        self.assertIn("EVENT_REF: ${{ github.ref }}", gate)
        self.assertIn(
            "DEFAULT_BRANCH: ${{ github.event.repository.default_branch }}",
            gate,
        )

        controller = step_containing(validate, "ref: ${{ github.sha }}")
        candidate = step_containing(validate, "ref: refs/tags/${{ env.RELEASE_TAG }}")
        self.assertNotIn("path: candidate", controller)
        self.assertIn("path: candidate", candidate)

        tag_verification = step_containing(validate, "python scripts/verify_release_tag.py")
        candidate_execution = step_containing(validate, "working-directory: candidate")
        self.assertIn("--repository-root candidate", tag_verification)
        self.assertLess(validate.index(tag_verification), validate.index(candidate_execution))
        self.assertNotIn("python candidate/scripts/", self.workflow)

        for job_name in ("msrv", "test", "package", "platform-contracts", "dist"):
            self.assertIn(
                "ref: ${{ needs.validate.outputs.commit }}", self.jobs[job_name]
            )
        for job_name in ("validate", "github-draft", "publish", "github-release"):
            verification = step_containing(
                self.jobs[job_name], "python scripts/verify_release_tag.py"
            )
            self.assertIn("--repository-root candidate", verification)

    def test_msrv_and_release_toolchain_come_from_the_candidate(self) -> None:
        validate = self.jobs["validate"]
        msrv = self.jobs["msrv"]
        self.assertIn("msrv: ${{ steps.source.outputs.msrv }}", validate)
        self.assertIn(
            "release_toolchain: ${{ steps.source.outputs.release_toolchain }}", validate
        )
        toolchain = step_containing(validate, 'pathlib.Path("candidate/rust-toolchain.toml")')
        self.assertIn(
            "toolchain: ${{ steps.toolchain.outputs.release_toolchain }}", validate
        )
        self.assertLess(validate.index(toolchain), validate.index("Install release Rust toolchain"))
        self.assertIn("toolchain: ${{ needs.validate.outputs.msrv }}", msrv)
        self.assertIn(
            "cargo +${{ needs.validate.outputs.msrv }} check --workspace --lib --all-features --locked",
            msrv,
        )

    def test_unprivileged_package_verification_precedes_one_credentialed_write(self) -> None:
        package = self.jobs["package"]
        self.assertIn("runs-on: ubuntu-latest", package)
        self.assertIn("needs: [validate, protocol-sdk]", package)
        self.assertIn("python scripts/verify_workspace_packages.py --mode full", package)

        publish = self.jobs["publish"]
        self.assertIn("needs: [validate, package, release-assets, github-draft]", publish)
        self.assertIn("name: crates-io-production", publish)
        reverify = step_containing(
            publish, "python scripts/verify_github_release_assets.py"
        )
        write = step_containing(publish, "python scripts/publish_workspace_packages.py")
        self.assertIn("--phase staged", reverify)
        self.assertIn("EXPECTED_RELEASE_ID: ${{ needs.github-draft.outputs.release_id }}", reverify)
        self.assertIn('--expected-release-id "$EXPECTED_RELEASE_ID"', reverify)
        self.assertIn("--repository-root candidate", write)
        self.assertIn("PUBLISH_CRATES: ${{ needs.validate.outputs.publish_crates }}", write)
        self.assertIn('read -r -a publish_crates <<< "$PUBLISH_CRATES"', write)
        self.assertIn('--packages "${publish_crates[@]}"', write)
        self.assertIn(
            "RUSTUP_TOOLCHAIN: ${{ needs.validate.outputs.release_toolchain }}", write
        )
        self.assertIn(
            "UNITY_ASSET_RELEASE_CARGO_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN_PRODUCTION }}",
            write,
        )
        self.assertEqual(
            self.workflow.count("secrets.CARGO_REGISTRY_TOKEN_PRODUCTION"), 1
        )
        self.assertNotIn("CARGO_REGISTRY_TOKEN:", self.workflow)
        self.assertLess(publish.index(reverify), publish.index(write))

    def test_github_assets_are_staged_read_back_and_only_then_published(self) -> None:
        draft = self.jobs["github-draft"]
        self.assertIn("needs: [validate, release-assets, attest]", draft)
        self.assertIn("contents: write", draft)
        preflight = step_containing(draft, "--phase preflight")
        upload = step_containing(draft, "needs_upload == 'true'")
        readback = step_containing(draft, "--phase staged")
        self.assertIn("--expected-evidence-sha256", preflight)
        self.assertIn("draft: true", upload)
        self.assertIn("overwrite_files: true", upload)
        self.assertIn('github-output "$GITHUB_OUTPUT"', readback)

        final = self.jobs["github-release"]
        self.assertIn("needs: [validate, publish, github-draft, release-assets]", final)
        self.assertIn("--phase publish", final)
        self.assertIn(
            'EXPECTED_RELEASE_ID: ${{ needs.github-draft.outputs.release_id }}', final
        )
        self.assertIn('--expected-release-id "$EXPECTED_RELEASE_ID"', final)

    def test_source_evidence_binds_distribution_sdk_and_native_artifacts(self) -> None:
        validate = self.jobs["validate"]
        release_assets = self.jobs["release-assets"]
        self.assertIn(
            "dist manifest --artifacts=local --output-format=json --no-local-paths",
            validate,
        )
        self.assertIn("python scripts/build_protocol_sdk_bundle.py", validate)
        self.assertIn("--repository-root candidate", validate)
        self.assertNotIn("dotnet build", validate)
        self.assertIn("dist_plan_sha256: ${{ steps.source.outputs.dist_plan_sha256 }}", validate)

        protocol_sdk = self.jobs["protocol-sdk"]
        self.assertIn("ref: ${{ github.sha }}", protocol_sdk)
        self.assertNotIn("path: candidate", protocol_sdk)
        self.assertNotIn("environment:", protocol_sdk)
        self.assertNotIn("outputs:", protocol_sdk)
        self.assertNotIn("secrets.", protocol_sdk)
        self.assertIn("name: release-evidence-${{ github.run_id }}", protocol_sdk)
        self.assertIn(
            "PROTOCOL_SDK_ARTIFACT: ${{ needs.validate.outputs.protocol_sdk_artifact }}",
            protocol_sdk,
        )
        self.assertIn("--bundle \"release-proof/$PROTOCOL_SDK_ARTIFACT\"", protocol_sdk)
        self.assertIn("dotnet build", protocol_sdk)
        self.assertLess(
            protocol_sdk.index("Download verified release evidence"),
            protocol_sdk.index("dotnet build"),
        )

        assembler = step_containing(release_assets, "python scripts/assemble_release_assets.py")
        self.assertIn("--expected-dist-plan-sha256", assembler)
        self.assertIn("--protocol-sdk-bundle", assembler)
        self.assertNotIn("attestations: write", release_assets)
        self.assertNotIn("id-token: write", release_assets)

        attest = self.jobs["attest"]
        self.assertIn("needs: [validate, release-assets]", attest)
        self.assertIn("attestations: write", attest)
        self.assertIn("id-token: write", attest)
        self.assertIn("actions/attest-build-provenance@", attest)

        dist = self.jobs["dist"]
        self.assertIn("dist build --artifacts=local", dist)
        native_probe = step_containing(dist, "python scripts/release_binary_identity.py")
        self.assertIn('--archive "$archive"', native_probe)
        self.assertIn('--output-directory "$extract_directory"', native_probe)
        self.assertIn('actual="$("$executable" --version', native_probe)
        self.assertLess(dist.index(native_probe), dist.index("Upload dist artifacts"))
        self.assertIn(
            "matrix: ${{ fromJSON(needs.validate.outputs.dist_matrix) }}", dist
        )

    def test_privileged_shell_steps_treat_validated_outputs_as_data(self) -> None:
        for job_name in ("github-draft", "publish", "github-release"):
            for step in self.jobs[job_name].split("      - name:")[1:]:
                if "\n        run:" not in step:
                    continue
                with self.subTest(job=job_name, step=step.splitlines()[0].strip()):
                    run = step.split("        run:", 1)[1]
                    self.assertNotIn("${{ needs.validate.outputs.", run)
                    self.assertNotIn("${{ needs.github-draft.outputs.", run)

    def test_release_metadata_and_dry_run_use_the_verified_proof(self) -> None:
        validate = self.jobs["validate"]
        self.assertIn(
            'python -m unittest discover -s scripts/tests -p "test_*.py"', validate
        )
        self.assertIn("github.com/rhysd/actionlint/cmd/actionlint@v1.7.7", validate)
        self.assertIn("--release-title-output", validate)
        self.assertIn("--release-body-output", validate)

        draft = self.jobs["github-draft"]
        self.assertIn("body_path: release-proof/release-notes.md", draft)
        dry_run = self.jobs["dry-run"]
        self.assertIn("python scripts/verify_release_bundle.py", dry_run)
        self.assertIn("name: release-evidence-${{ github.run_id }}", dry_run)
        self.assertIn("--release-title release-proof/release-title.txt", dry_run)
        self.assertIn("--release-body release-proof/release-notes.md", dry_run)


if __name__ == "__main__":
    unittest.main()
