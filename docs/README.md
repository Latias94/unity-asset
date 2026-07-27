# Documentation

## Start Here

- [Domain vocabulary](../CONTEXT.md): identity, revisions, plans, prepared authority, recovery,
  references, extraction, and search handoff.
- [Examples](EXAMPLES.md): runnable library examples and the typed workspace, reference,
  extraction, and search CLI workflows.
- [Migrating to AssetWorkspace](MIGRATING_TO_ASSET_WORKSPACE.md): breaking migration guide from
  the superseded aggregate API.
- [Workspace transaction ADR](adr/0004-asset-workspace-transactions.md): prepare, publication,
  recovery, and consistency decisions.

## Compatibility and Operations

- [UnityPy compatibility](UNITYPY_PARITY.md): format support, architectural differences, evidence,
  and remaining gaps.
- [Script TypeTrees](SCRIPT_TYPETREES.md): external MonoBehaviour schema generation and immutable
  `WorkspaceOptions` registry loading.
- [UnityCN/Tuanjie notes](UNITYCN_NOTES.md): version, bundle-flag, and signed path-ID observations.
- [Releasing](RELEASING.md): maintainer release process.

## Architecture

- [Architecture Decision Records](adr/README.md)
- [Roadmap](ROADMAP.md)
- [Engineering knowledge index](knowledge/engineering/index.md)

## Performance Contracts

- [TypeTree traversal performance baseline](TYPE_TREE_PERFORMANCE_BASELINE.md)
- [Prepared artifact performance baseline](PREPARED_ARTIFACT_PERFORMANCE_BASELINE.md)

Runtime measurements are machine-specific diagnostics. The deterministic fixture counters,
budgets, digests, and conformance tests described by each baseline are the portable contract.
