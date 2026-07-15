# Unity Asset Workspace

This context describes how loaded Unity assets are identified, inspected, changed, exported, and projected into derived indexes.

## Language

**Asset Source**:
A physical Unity asset input or a nested member that contributes bytes to an Asset Workspace.
_Avoid_: File, input, binary source

**Physical Origin**:
The runtime filesystem location currently bound to a root Asset Source. It is never part of persisted object identity.
_Avoid_: Object address, source alias

**Source Alias**:
A portable, workspace-relative name that keeps a root Asset Source stable when the workspace moves.
_Avoid_: Absolute path, canonical path

**Source Locator**:
A serializable Source Alias followed by exact archive, WebFile, or bundle containment steps. A streamed resource uses its actual container-member edge rather than a synthetic resource edge.
_Avoid_: Asset index tuple, physical path

**Source Ownership**:
The nesting relationship that connects an outer artifact, its container members, SerializedFiles, and streamed-resource sidecars.
_Avoid_: Asset index tuple

**Object Identity**:
The opaque identity of one YAML or binary Unity object within a specific Workspace Revision.
_Avoid_: Global key, path ID alone

**Revisioned Object Handle**:
An Object Identity bound to an Asset Workspace namespace and Workspace Revision for safe use across module interfaces.
_Avoid_: Bare object ID

**Object Address**:
A validated, serializable Source Locator plus object-local key that an Asset Workspace resolves to an Object Identity.
_Avoid_: Display string, raw key fields

**Asset Workspace**:
A coherent, revisioned set of Asset Sources and the views derived from them.
_Avoid_: Mutable environment, edit session

**Workspace Snapshot**:
An immutable view of an Asset Workspace at one Workspace Revision.
_Avoid_: Cache

**Workspace Revision**:
The content identity of the sources and format contracts represented by a Workspace Snapshot.
_Avoid_: Timestamp, changed flag

**DigestV1**:
The versioned BLAKE3-256 byte-identity contract used by source fingerprints, plans, prepared artifacts, journals, extraction manifests, and search generations.
_Avoid_: Size-and-mtime fingerprint, ad hoc hash

**Asset Load Budget**:
A shared limit ledger consumed before untrusted parsing, allocation, recursion, member expansion, or decompression.
_Avoid_: Parser-local limits, post-allocation checks

**Mutation Plan**:
A deterministic, serializable sequence of guarded changes against one Workspace Revision.
_Avoid_: Callback edit, setter batch

**Prepared Change**:
A fully resolved and validated Mutation Plan with a read-your-writes view and a complete artifact manifest, ready for commit.
_Avoid_: Pending writes

**Publication Target**:
The validated destination and deterministic recovery root for publishing a Prepared Change.
_Avoid_: Output path string

**Prepared Artifact**:
A budgeted, seekable byte image that has been independently reparsed and can be streamed to its final destination without re-encoding.
_Avoid_: Output buffer, encoder closure

**Change Set**:
The structured difference between two Workspace Revisions, including changed sources, objects, references, and identity remaps.
_Avoid_: Changed flag

**Commit Report**:
The canonical, transaction-keyed result of publication, including the achieved atomicity, Change Set, identity remap, and recovery locator; recovery can redeliver it idempotently.
_Avoid_: Save success boolean

**Artifact Set**:
The complete, collision-checked set of output artifacts produced by a Prepared Change or an Extraction Plan.
_Avoid_: Output files

**Mutation Recipe**:
A semantic Unity edit that lowers to generic field, reference, schema, or streamed-resource mutations.
_Avoid_: Typed setter

**Reference Graph**:
The revision-bound index of normalized object references, their field paths, raw targets, and resolution states.
_Avoid_: Dependency graph, object graph

**Extraction Plan**:
A deterministic selection and representation plan that maps resolved objects to an Artifact Set.
_Avoid_: Export job list

**Search Generation**:
A committed Search Everything read model whose search documents, reverse references, and state all represent the same Workspace Revision.
_Avoid_: Index state file
