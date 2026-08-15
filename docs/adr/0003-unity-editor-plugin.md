# ADR 0003: Unity Editor Plugin Integration And Repository Strategy

- Status: Accepted
- Date: 2026-07-30
- Superseded in part by: ADR 0005's HTTP client and transport decisions

## Context

`unity-asset-search-daemon` provides a project-bound local search experience. Unity users need an
Editor integration that can start the matching daemon, issue typed search and reference requests,
observe indexing state, and navigate returned Unity identities without blocking the Editor thread.

Unity package development has different repository, CI, and release constraints from the Rust
workspace: UPM requires `.meta` files, Editor-version qualification, and native binaries for every
supported platform.

## Decision

### Keep The Plugin In A Separate Repository

Maintain the Unity integration as a UPM package in a dedicated repository, versioned independently
but released against an explicit compatible daemon and business-protocol revision. This Rust
repository owns the language-neutral contract, canonical fixtures, and Unity-independent C#
reference codec; only the plugin repository claims concrete Unity Editor support.

### Consume The Same Local IPC Contract

The plugin uses the same frozen bootstrap, framed business protocol, typed operations, and response
validation as the Rust CLI. It does not use HTTP, bearer tokens, ports, URLs, or compatibility
fallbacks.

The in-repository `netstandard2.0` C# package owns framing, canonical DTO codecs, validation, and a
transport-neutral `Stream` adapter interface. The plugin owns source-level platform transport and
peer-verification adapters where its managed profile lacks the required APIs:

- Unix-domain socket connection plus endpoint and effective-UID verification on Linux/macOS.
- Named-pipe connection plus descriptor, server process, and `SecurityContextIdV1` verification on
  Windows.

Project ID, daemon instance ID, and negotiated business revision are bound before any operation is
sent. The plugin validates every response against its request rather than inferring behavior from
display text.

### Own Process Lifecycle In The Editor Adapter

The plugin derives the explicit Unity project root from the open project, resolves the deterministic
private endpoint namespace, and starts a bundled matching daemon when no valid instance is
available. It never probes parent directories and does not persist PID, port, or token files under
`Library/`.

The adapter handles startup races, attach, graceful instance-bound shutdown, crash detection, and
Editor-domain reload without taking ownership of the search index. Readiness and failure state come
from the protocol status model.

### Bundle Matching Native Binaries

Production UPM releases include the matching daemon binary per supported platform under a package
tool directory. Development may allow an explicitly configured local binary. Runtime downloading
is outside the initial contract because it adds network availability, signature verification, and
rollback requirements.

### Navigation Scope

Initial navigation opens or pings assets using returned project-relative locations. Object-level
navigation uses structured object addresses, file IDs, hierarchy paths, and script-symbol evidence
only when the indexed entity provides them; the plugin does not reconstruct identities from display
text.

## Consequences

Benefits:

- Rust and Unity repositories keep focused build systems and release lifecycles.
- CLI, Unity, and agent clients share one strict protocol and the same capability evidence.
- Matching binaries and fixtures make breaking changes fail during bootstrap rather than at an
  arbitrary operation.
- Unity's main thread remains free of indexing and binary parsing work.

Costs:

- Release automation must coordinate UPM packages, native binaries, protocol fixtures, and platform
  tests.
- The plugin requires platform-specific IPC and peer-verification code.
- Cross-repository compatibility and supported Editor versions need explicit release metadata.

## Alternatives Considered

1. Keep the plugin in this repository: rejected because Unity metadata and Editor CI would obscure
   the Rust engine's ownership and release graph.
2. Retain localhost HTTP for Unity only: rejected because it would preserve a second security and
   protocol surface after the Rust clients migrate.
3. Use pure text scanning in the plugin: rejected because it cannot provide immutable generation
   consistency, object-level reference identity, ranking, or bounded incremental indexing.
