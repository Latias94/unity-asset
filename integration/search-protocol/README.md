# Unity Asset Search Protocol Integration

This directory is the language-neutral integration surface for `unity-asset-search-protocol` revision 1. It contains canonical JSON fixtures, a Unity-independent C# reference codec, and an executable conformance runner.

The Rust crate remains the source of truth for the protocol. These artifacts make the wire contract reviewable and testable by clients that cannot link Rust directly.

## Layout

```text
fixtures/
  manifest.json              Fixture inventory and expected peer binding
  bootstrap/                 Hello, accepted, and rejected bootstrap messages
  requests/                  One canonical request for every v1 operation
  responses/                 One canonical response for every v1 operation and one API error
  invalid/                   Binding/revision fixtures that must be rejected
csharp/
  UnityAsset.SearchProtocol.Reference/    netstandard2.0 codec library
  UnityAsset.SearchProtocol.Conformance/  net8.0 fixture runner
```

## Wire Contract

Every message is UTF-8 JSON preceded by a four-byte unsigned big-endian payload length. The length excludes the header. Decoders reject truncated headers, length mismatches, trailing frame bytes, and payloads that exceed the operation-specific limit.

Bootstrap is revision-independent and uses `bootstrap_version: 1`. A client sends `BootstrapHelloV1` with a project ID, daemon instance ID, and a strictly increasing non-empty list of supported business revisions. The daemon returns either the highest common revision or one of these closed rejection codes:

- `project_mismatch`
- `instance_mismatch`
- `no_common_revision`

Business revision 1 contains these operations:

- `capabilities`
- `status`
- `search`
- `suggest`
- `references`
- `reindex_admit`
- `reindex_status`
- `reindex_wait`
- `reindex_cancel`
- `shutdown`

Request and response envelopes bind every exchange to a protocol revision, request ID, project ID, daemon instance ID, and query-policy ID. A response is valid only in the context of its originating request.

## Canonical JSON

Fixture payloads are compact JSON with Rust/Serde field order. A terminal CR/LF belongs to the repository text file and is not part of the framed payload. Canonical encoders:

- emit object fields in contract order;
- omit only absent optional fields; required collections and maps remain present even when empty;
- emit fixed IDs with their exact v1 prefix and lowercase hexadecimal payload;
- emit map entries in Unicode scalar-value ordinal key order;
- emit integers without alternate number spellings.

Decoders reject unknown, duplicate, and non-canonical-order properties. They also validate byte/count limits, portable paths, lifecycle state invariants, nested protocol revisions, query-policy bindings, reference request echoes, and reindex operation IDs.

Reference cursors bind the generation and query-policy ID and carry a SHA-256 binding over the direction plus canonical selector JSON. `coverage.complete` describes analysis/projection completeness, while `coverage.truncated` independently describes result pagination; complete coverage may therefore still return a `next_cursor`. A succeeded reindex operation must publish an applied or already-applied generation, report no active build, and identify the same generation in its completion receipt and status snapshot.

Aggregate limits are measured from canonical JSON, not from unescaped source strings. Search responses allow at most 10 MiB of hit JSON, 4 MiB of diagnostic JSON, and 15 MiB for the complete operation response; producers retain the largest ranked hit prefix that fits. Suggestion arrays and status path fields each have their own aggregate limits below the 256 KiB response frame. Reindex receipts retain the largest publish-warning prefix that fits their 64-entry, 4 KiB-per-entry, and 224 KiB aggregate limits, followed by an explicit omission warning when necessary. API errors and persisted generation-failure messages share a 16 KiB UTF-8 message limit.

## C# Reference Codec

`UnityAsset.SearchProtocol.Reference` targets `netstandard2.0`, uses no Unity API, and depends on `System.Text.Json` 8.0.5. It exposes:

- strict fixed-ID parsers;
- bootstrap encode, decode, and negotiation;
- business envelope encode, decode, construction, and request/response validation;
- bounded frame encode/decode helpers;
- `IProtocolTransportAdapter` as the platform connection boundary.

Domain operation payloads are retained as schema-validated JSON rather than duplicated as a second public object model. This keeps the reference implementation aligned with the Rust DTOs while still rejecting invalid nested data. Use `BusinessCodec.CreateRequest`, `CreateSuccessResponse`, or `CreateErrorResponse` to construct outbound envelopes from operation payload JSON.

The asynchronous framed stream is deliberately internal. Public callers operate at the request/response exchange boundary, so they cannot pipeline unrelated messages or reuse a connection after cancellation, truncated I/O, or an invalid frame. Any such failure poisons and closes the connection.

```csharp
var binding = new ProtocolBinding(
    ProtocolConstants.BusinessProtocolRevision,
    ProjectId.Parse(projectId),
    DaemonInstanceId.Parse(instanceId),
    QueryPolicyId.Parse(queryPolicyId));

RequestEnvelopeV1 request = BusinessCodec.CreateRequest(
    binding,
    RequestId.Parse(requestId),
    "search",
    Encoding.UTF8.GetBytes("{\"query\":\"player\",\"limit\":25}"));

byte[] frame = FrameCodec.EncodeRequest(request);
```

The external Unity plugin owns the platform transport implementation, endpoint discovery, process lifecycle, and editor-thread integration. Its adapter only needs to return a connected, readable, writable `Stream`:

```csharp
public sealed class EditorTransportAdapter : IProtocolTransportAdapter
{
    public Task<Stream> ConnectAsync(CancellationToken cancellationToken)
    {
        // Open the platform-specific local IPC endpoint here.
        throw new NotImplementedException();
    }
}
```

Do not treat a successfully parsed response as trusted until `ResponseEnvelopeV1.ValidateFor(request)` succeeds. The frame convenience methods perform that validation automatically.

## Conformance

Run the focused suite from the repository root:

```text
dotnet run --project integration/search-protocol/csharp/UnityAsset.SearchProtocol.Conformance/UnityAsset.SearchProtocol.Conformance.csproj -- integration/search-protocol/fixtures
```

The runner verifies that:

- every listed fixture is non-empty and every JSON fixture is listed;
- all ten request and response operations are covered;
- bootstrap hello/accepted/rejected and a structured error are covered;
- decode followed by canonical encode reproduces every valid fixture byte-for-byte;
- every fixture survives bounded frame round-trip;
- protocol revision, project, and daemon instance mismatches are rejected;
- malformed frame lengths and non-canonical fixed IDs are rejected.

See [COMPATIBILITY.md](COMPATIBILITY.md) for revision and runtime policy.
