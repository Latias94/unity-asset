using System;
using System.Collections.Generic;
using System.Linq;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace UnityAsset.SearchProtocol.Reference
{
    internal static class ContractValidator
    {
        private const int MaxErrorMessageBytes = 16 * 1024;
        private const int MaxApiErrorJsonBytes = 224 * 1024;
        private const int MaxReindexPublishWarningBytes = 4 * 1024;
        private const int MaxReindexPublishWarnings = 64;
        private const int MaxReindexPublishWarningsJsonBytes = 224 * 1024;
        private const int MaxSearchDiagnosticsJsonBytes = 4 * 1024 * 1024;
        private const int MaxSearchHitsJsonBytes = 10 * 1024 * 1024;
        private const int MaxSearchResponseJsonBytes = 15 * 1024 * 1024;
        private const int MaxStatusPathsJsonBytes = 224 * 1024;
        private static readonly string[] BackgroundReindexOperationProperties =
        {
            "origin",
            "operation_id",
            "state",
        };

        private static readonly string[] Operations =
        {
            "capabilities",
            "status",
            "search",
            "suggest",
            "references",
            "reindex_admit",
            "reindex_status",
            "reindex_wait",
            "reindex_cancel",
            "shutdown",
        };

        internal static string ValidateRequest(JsonElement root)
        {
            StrictJson.Properties(
                root,
                "request envelope",
                "protocol_revision",
                "request_id",
                "project_id",
                "daemon_instance_id",
                "query_policy_id",
                "operation");
            StrictJson.RequireRevision(StrictJson.Required(root, "protocol_revision", "request envelope"), "request envelope.protocol_revision");
            RequestId.Parse(StrictJson.String(StrictJson.Required(root, "request_id", "request envelope"), "request envelope.request_id"));
            ProjectId.Parse(StrictJson.String(StrictJson.Required(root, "project_id", "request envelope"), "request envelope.project_id"));
            DaemonInstanceId.Parse(StrictJson.String(StrictJson.Required(root, "daemon_instance_id", "request envelope"), "request envelope.daemon_instance_id"));
            QueryPolicyId envelopePolicy = QueryPolicyId.Parse(
                StrictJson.String(
                    StrictJson.Required(root, "query_policy_id", "request envelope"),
                    "request envelope.query_policy_id"));

            JsonElement operation = StrictJson.Required(root, "operation", "request envelope");
            StrictJson.Properties(operation, "request envelope.operation", "kind", "request");
            string kind = StrictJson.Enum(
                StrictJson.Required(operation, "kind", "request envelope.operation"),
                "request envelope.operation.kind",
                Operations);
            JsonElement request = StrictJson.Required(operation, "request", "request envelope.operation");
            ValidateRequestPayload(kind, request, "request envelope.operation.request", envelopePolicy);
            return kind;
        }

        internal static string? ValidateResponse(JsonElement root, out bool isError)
        {
            StrictJson.Properties(
                root,
                "response envelope",
                "protocol_revision",
                "request_id",
                "project_id",
                "daemon_instance_id",
                "query_policy_id",
                "outcome",
                "value");
            StrictJson.RequireRevision(StrictJson.Required(root, "protocol_revision", "response envelope"), "response envelope.protocol_revision");
            RequestId.Parse(StrictJson.String(StrictJson.Required(root, "request_id", "response envelope"), "response envelope.request_id"));
            ProjectId.Parse(StrictJson.String(StrictJson.Required(root, "project_id", "response envelope"), "response envelope.project_id"));
            DaemonInstanceId.Parse(StrictJson.String(StrictJson.Required(root, "daemon_instance_id", "response envelope"), "response envelope.daemon_instance_id"));
            QueryPolicyId envelopePolicy = QueryPolicyId.Parse(
                StrictJson.String(StrictJson.Required(root, "query_policy_id", "response envelope"), "response envelope.query_policy_id"));

            string outcome = StrictJson.Enum(
                StrictJson.Required(root, "outcome", "response envelope"),
                "response envelope.outcome",
                "success",
                "error");
            JsonElement value = StrictJson.Required(root, "value", "response envelope");
            isError = outcome == "error";
            if (isError)
            {
                ValidateApiError(value, "response envelope.value", envelopePolicy);
                return null;
            }

            StrictJson.Properties(value, "response envelope.value", "kind", "response");
            string kind = StrictJson.Enum(
                StrictJson.Required(value, "kind", "response envelope.value"),
                "response envelope.value.kind",
                Operations);
            JsonElement response = StrictJson.Required(value, "response", "response envelope.value");
            ValidateResponsePayload(kind, response, "response envelope.value.response", envelopePolicy);
            return kind;
        }

        internal static IReadOnlyList<BackgroundReindexOperation> MaterializeBackgroundReindexOperations(
            JsonElement operations)
        {
            int count = operations.GetArrayLength();
            if (count == 0)
            {
                return Array.Empty<BackgroundReindexOperation>();
            }

            var result = new BackgroundReindexOperation[count];
            int index = 0;
            foreach (JsonElement operation in operations.EnumerateArray())
            {
                result[index] = new BackgroundReindexOperation(
                    ParseBackgroundReindexOrigin(operation.GetProperty("origin").GetString()!),
                    OperationId.Parse(operation.GetProperty("operation_id").GetString()!),
                    ParseReindexOperationState(operation.GetProperty("state").GetString()!));
                index++;
            }
            return result;
        }

        internal static SearchCapabilities MaterializeSearchCapabilities(JsonElement capabilities)
        {
            return new SearchCapabilities(
                capabilities.GetProperty("protocol_revision").GetUInt16(),
                capabilities.GetProperty("search").GetBoolean(),
                capabilities.GetProperty("suggest").GetBoolean(),
                capabilities.GetProperty("incoming_references").GetBoolean(),
                capabilities.GetProperty("outgoing_references").GetBoolean(),
                capabilities.GetProperty("filesystem_reindex").GetBoolean(),
                capabilities.GetProperty("reindex_lifecycle").GetBoolean(),
                capabilities.GetProperty("background_reindex_discovery").GetBoolean(),
                capabilities.GetProperty("graceful_shutdown").GetBoolean());
        }

        internal static ApiErrorCode ReadApiErrorCode(JsonElement code, string path)
        {
            return ParseApiErrorCode(StrictJson.String(code, path));
        }

        internal static void ValidateResponseForRequest(ResponseEnvelopeV1 response, RequestEnvelopeV1 request)
        {
            if (response.IsError)
            {
                return;
            }

            JsonElement requestPayload = StrictJson.Required(request.Operation, "request", "request operation");
            JsonElement responsePayload = StrictJson.Required(response.Value, "response", "response operation");
            switch (request.OperationKind)
            {
                case "search":
                    string expectedQuery = StrictJson.String(
                        StrictJson.Required(requestPayload, "query", "search request"),
                        "search request.query");
                    string actualQuery = StrictJson.String(
                        StrictJson.Required(responsePayload, "query", "search response"),
                        "search response.query");
                    uint searchLimit = StrictJson.UInt32(
                        StrictJson.Required(requestPayload, "limit", "search request"),
                        "search request.limit");
                    uint returnedHits = StrictJson.UInt32(
                        StrictJson.Required(responsePayload, "returned_hits", "search response"),
                        "search response.returned_hits");
                    if (!string.Equals(expectedQuery, actualQuery, StringComparison.Ordinal)
                        || returnedHits > searchLimit
                        || checked((uint)responsePayload.GetProperty("hits").GetArrayLength()) > searchLimit)
                    {
                        throw new ProtocolValidationException("search response does not match request query and limit");
                    }
                    break;
                case "suggest":
                    string expectedPrefix = StrictJson.String(
                        StrictJson.Required(requestPayload, "prefix", "suggest request"),
                        "suggest request.prefix");
                    string actualPrefix = StrictJson.String(
                        StrictJson.Required(responsePayload, "prefix", "suggest response"),
                        "suggest response.prefix");
                    uint suggestionLimit = StrictJson.UInt32(
                        StrictJson.Required(requestPayload, "limit", "suggest request"),
                        "suggest request.limit");
                    if (!string.Equals(expectedPrefix, actualPrefix, StringComparison.Ordinal)
                        || checked((uint)responsePayload.GetProperty("suggestions").GetArrayLength()) > suggestionLimit)
                    {
                        throw new ProtocolValidationException("suggest response does not match request prefix and limit");
                    }
                    break;
                case "references":
                    JsonElement echoed = StrictJson.Required(responsePayload, "request", "references response");
                    if (!JsonEquivalent(echoed, requestPayload))
                    {
                        throw new ProtocolValidationException("references response request echo does not match request");
                    }
                    uint referenceLimit = StrictJson.UInt32(
                        StrictJson.Required(requestPayload, "limit", "references request"),
                        "references request.limit");
                    uint returnedReferences = StrictJson.UInt32(
                        StrictJson.Required(
                            StrictJson.Required(responsePayload, "coverage", "references response"),
                            "returned",
                            "references response.coverage"),
                        "references response.coverage.returned");
                    if (returnedReferences > referenceLimit
                        || checked((uint)responsePayload.GetProperty("hits").GetArrayLength()) > referenceLimit)
                    {
                        throw new ProtocolValidationException("references response exceeds request limit");
                    }
                    break;
                case "reindex_status":
                case "reindex_wait":
                case "reindex_cancel":
                    string requestedOperation = StrictJson.String(
                        StrictJson.Required(requestPayload, "operation_id", "reindex request"),
                        "reindex request.operation_id");
                    string returnedOperation = StrictJson.String(
                        StrictJson.Required(responsePayload, "operation_id", "reindex response"),
                        "reindex response.operation_id");
                    if (!string.Equals(requestedOperation, returnedOperation, StringComparison.Ordinal))
                    {
                        throw new ProtocolValidationException("response operation ID does not match request");
                    }
                    break;
            }
        }

        private static void ValidateRequestPayload(
            string kind,
            JsonElement request,
            string path,
            QueryPolicyId envelopePolicy)
        {
            switch (kind)
            {
                case "capabilities":
                case "status":
                    StrictJson.Properties(request, path);
                    return;
                case "search":
                    StrictJson.Properties(request, path, "query", "limit");
                    StrictJson.String(StrictJson.Required(request, "query", path), path + ".query", 4 * 1024);
                    RequireMaximum(StrictJson.UInt32(StrictJson.Required(request, "limit", path), path + ".limit"), 1000, path + ".limit");
                    return;
                case "suggest":
                    StrictJson.Properties(request, path, "prefix", "limit");
                    StrictJson.String(StrictJson.Required(request, "prefix", path), path + ".prefix", 4 * 1024);
                    uint suggestLimit = StrictJson.UInt32(
                        StrictJson.Required(request, "limit", path),
                        path + ".limit");
                    if (suggestLimit == 0 || suggestLimit > 50)
                    {
                        throw new ProtocolValidationException(path + ".limit must be in 1..=50");
                    }
                    return;
                case "references":
                    ValidateReferenceRequest(request, path, envelopePolicy);
                    return;
                case "reindex_admit":
                    StrictJson.Properties(request, path, "intent", "idempotency_key?");
                    ValidateReindexIntent(StrictJson.Required(request, "intent", path), path + ".intent");
                    if (StrictJson.Optional(request, "idempotency_key", out JsonElement key))
                    {
                        StrictJson.String(key, path + ".idempotency_key", 256, allowEmpty: false);
                    }
                    return;
                case "reindex_status":
                case "reindex_cancel":
                    StrictJson.Properties(request, path, "operation_id");
                    OperationId.Parse(StrictJson.String(StrictJson.Required(request, "operation_id", path), path + ".operation_id"));
                    return;
                case "reindex_wait":
                    StrictJson.Properties(request, path, "operation_id", "timeout_ms");
                    OperationId.Parse(StrictJson.String(StrictJson.Required(request, "operation_id", path), path + ".operation_id"));
                    uint timeout = StrictJson.UInt32(StrictJson.Required(request, "timeout_ms", path), path + ".timeout_ms");
                    if (timeout == 0 || timeout > 300_000)
                    {
                        throw new ProtocolValidationException(path + ".timeout_ms must be in 1..=300000");
                    }
                    return;
                case "shutdown":
                    StrictJson.Properties(request, path, "drain_timeout_ms");
                    RequireMaximum(
                        StrictJson.UInt32(StrictJson.Required(request, "drain_timeout_ms", path), path + ".drain_timeout_ms"),
                        60_000,
                        path + ".drain_timeout_ms");
                    return;
                default:
                    throw new ProtocolValidationException($"{path} has unsupported operation '{kind}'");
            }
        }

        private static void ValidateReferenceRequest(
            JsonElement request,
            string path,
            QueryPolicyId expectedPolicy)
        {
            StrictJson.Properties(request, path, "direction", "selector", "limit", "cursor?");
            StrictJson.Enum(StrictJson.Required(request, "direction", path), path + ".direction", "incoming", "outgoing");
            ValidateReferenceSelector(StrictJson.Required(request, "selector", path), path + ".selector");
            uint limit = StrictJson.UInt32(StrictJson.Required(request, "limit", path), path + ".limit");
            if (limit == 0 || limit > 500)
            {
                throw new ProtocolValidationException(path + ".limit must be in 1..=500");
            }
            if (StrictJson.Optional(request, "cursor", out JsonElement cursor))
            {
                ValidateReferenceCursor(
                    cursor,
                    path + ".cursor",
                    expectedPolicy,
                    ComputeReferenceQueryBinding(request, path));
            }
        }

        private static void ValidateReferenceSelector(JsonElement selector, string path)
        {
            string kind = StrictJson.String(StrictJson.Required(selector, "kind", path), path + ".kind", allowEmpty: false);
            if (kind == "guid")
            {
                StrictJson.Properties(selector, path, "kind", "guid", "file_id?");
                ValidateGuid(StrictJson.Required(selector, "guid", path), path + ".guid");
                if (StrictJson.Optional(selector, "file_id", out JsonElement fileId))
                {
                    StrictJson.Int64(fileId, path + ".file_id");
                }
                return;
            }
            if (kind == "object")
            {
                StrictJson.Properties(selector, path, "kind", "address");
                ValidateObjectAddress(StrictJson.Required(selector, "address", path), path + ".address");
                return;
            }
            throw new ProtocolValidationException($"{path}.kind contains unsupported value '{kind}'");
        }

        private static void ValidateReferenceCursor(
            JsonElement cursor,
            string path,
            QueryPolicyId expectedPolicy,
            string expectedQueryBinding)
        {
            StrictJson.Properties(cursor, path, "generation", "query_policy_id", "after_stable_id", "query_binding");
            ValidateDigest(StrictJson.Required(cursor, "generation", path), path + ".generation");
            QueryPolicyId actualPolicy = QueryPolicyId.Parse(
                StrictJson.String(
                    StrictJson.Required(cursor, "query_policy_id", path),
                    path + ".query_policy_id"));
            if (!actualPolicy.Equals(expectedPolicy))
            {
                throw new ProtocolValidationException(path + ".query_policy_id does not match the request envelope");
            }
            StrictJson.String(StrictJson.Required(cursor, "after_stable_id", path), path + ".after_stable_id", 256, allowEmpty: false);
            string actualQueryBinding = StrictJson.String(
                StrictJson.Required(cursor, "query_binding", path),
                path + ".query_binding",
                256,
                allowEmpty: false);
            StrictJson.FixedHex(actualQueryBinding, "reference-query-v2:", 64, path + ".query_binding", lowercaseOnly: true);
            if (!string.Equals(actualQueryBinding, expectedQueryBinding, StringComparison.Ordinal))
            {
                throw new ProtocolValidationException(path + ".query_binding does not match the reference request");
            }
        }

        private static void ValidateReindexIntent(JsonElement intent, string path)
        {
            StrictJson.Properties(intent, path, "protocol_revision", "scope");
            StrictJson.RequireRevision(StrictJson.Required(intent, "protocol_revision", path), path + ".protocol_revision");
            JsonElement scope = StrictJson.Required(intent, "scope", path);
            string kind = StrictJson.String(StrictJson.Required(scope, "kind", path + ".scope"), path + ".scope.kind", allowEmpty: false);
            if (kind == "full" || kind == "reconcile")
            {
                StrictJson.Properties(scope, path + ".scope", "kind");
                return;
            }
            if (kind != "changed_paths")
            {
                throw new ProtocolValidationException($"{path}.scope.kind contains unsupported value '{kind}'");
            }
            StrictJson.Properties(scope, path + ".scope", "kind", "paths");
            JsonElement[] paths = StrictJson.Array(
                StrictJson.Required(scope, "paths", path + ".scope"),
                path + ".scope.paths",
                maximum: 4096,
                allowEmpty: false);
            string? previous = null;
            for (int index = 0; index < paths.Length; index++)
            {
                StrictJson.PortablePath(paths[index], $"{path}.scope.paths[{index}]", requireRelative: true);
                string current = paths[index].GetString()!;
                StrictJson.ValidateUnicodeScalarString(current, $"{path}.scope.paths[{index}]");
                if (previous != null
                    && StrictJson.CompareUnicodeScalarOrdinal(previous, current, path + ".scope.paths") >= 0)
                {
                    throw new ProtocolValidationException(path + ".scope.paths must be strictly increasing");
                }
                previous = current;
            }
        }

        private static void ValidateResponsePayload(
            string kind,
            JsonElement response,
            string path,
            QueryPolicyId envelopePolicy)
        {
            switch (kind)
            {
                case "capabilities":
                    StrictJson.Properties(response, path, "daemon_version", "capabilities");
                    StrictJson.String(StrictJson.Required(response, "daemon_version", path), path + ".daemon_version", allowEmpty: false);
                    ValidateCapabilities(StrictJson.Required(response, "capabilities", path), path + ".capabilities");
                    return;
                case "status":
                    ValidateStatusResponse(response, path, envelopePolicy);
                    return;
                case "search":
                    ValidateSearchResponse(response, path, envelopePolicy);
                    return;
                case "suggest":
                    ValidateSuggestResponse(response, path, envelopePolicy);
                    return;
                case "references":
                    ValidateReferencesResponse(response, path, envelopePolicy);
                    return;
                case "reindex_admit":
                case "reindex_status":
                case "reindex_wait":
                    ValidateReindexOperationStatus(response, path, envelopePolicy);
                    return;
                case "reindex_cancel":
                    ValidateReindexCancelResponse(response, path);
                    return;
                case "shutdown":
                    StrictJson.Properties(response, path, "accepted");
                    StrictJson.Boolean(StrictJson.Required(response, "accepted", path), path + ".accepted");
                    return;
                default:
                    throw new ProtocolValidationException($"{path} has unsupported operation '{kind}'");
            }
        }

        private static void ValidateStatusResponse(JsonElement response, string path, QueryPolicyId envelopePolicy)
        {
            StrictJson.Properties(
                response,
                path,
                "protocol_revision",
                "daemon",
                "generation",
                "query_policy_id",
                "capabilities",
                "project_root",
                "generation_root",
                "scan_roots",
                "indexed_assets",
                "indexed_search_documents",
                "indexed_reference_facts",
                "incomplete_assets",
                "projection_truncations",
                "last_build_duration_ms?",
                "last_build_unix_ms?",
                "indexing");
            StrictJson.RequireRevision(StrictJson.Required(response, "protocol_revision", path), path + ".protocol_revision");
            JsonElement generation = StrictJson.Required(response, "generation", path);
            ValidateGenerationStatus(generation, path + ".generation");
            ValidateDaemonLifecycleStatus(
                StrictJson.Required(response, "daemon", path),
                generation,
                path + ".daemon");
            ValidatePolicyBinding(response, path, envelopePolicy);
            ValidateCapabilities(StrictJson.Required(response, "capabilities", path), path + ".capabilities");
            JsonElement projectRoot = StrictJson.Required(response, "project_root", path);
            JsonElement generationRoot = StrictJson.Required(response, "generation_root", path);
            JsonElement scanRoots = StrictJson.Required(response, "scan_roots", path);
            StrictJson.PortablePath(projectRoot, path + ".project_root");
            StrictJson.PortablePath(generationRoot, path + ".generation_root");
            JsonElement[] roots = StrictJson.Array(scanRoots, path + ".scan_roots", 64);
            for (int index = 0; index < roots.Length; index++)
            {
                StrictJson.PortablePath(roots[index], $"{path}.scan_roots[{index}]");
            }
            int encodedPathBytes = checked(
                Encoding.UTF8.GetByteCount(projectRoot.GetRawText())
                + Encoding.UTF8.GetByteCount(generationRoot.GetRawText())
                + Encoding.UTF8.GetByteCount(scanRoots.GetRawText()));
            if (encodedPathBytes > MaxStatusPathsJsonBytes)
            {
                throw new ProtocolValidationException(
                    $"{path} paths contain {encodedPathBytes} encoded JSON bytes; maximum is {MaxStatusPathsJsonBytes}");
            }
            StrictJson.UInt64(StrictJson.Required(response, "indexed_assets", path), path + ".indexed_assets");
            StrictJson.UInt64(StrictJson.Required(response, "indexed_search_documents", path), path + ".indexed_search_documents");
            StrictJson.UInt64(StrictJson.Required(response, "indexed_reference_facts", path), path + ".indexed_reference_facts");
            StrictJson.UInt64(StrictJson.Required(response, "incomplete_assets", path), path + ".incomplete_assets");
            StrictJson.UInt64(StrictJson.Required(response, "projection_truncations", path), path + ".projection_truncations");
            if (StrictJson.Optional(response, "last_build_duration_ms", out JsonElement duration))
            {
                StrictJson.UInt64(duration, path + ".last_build_duration_ms");
            }
            if (StrictJson.Optional(response, "last_build_unix_ms", out JsonElement timestamp))
            {
                StrictJson.UInt64(timestamp, path + ".last_build_unix_ms");
            }
            bool indexing = StrictJson.Boolean(
                StrictJson.Required(response, "indexing", path),
                path + ".indexing");
            if (!indexing && StrictJson.Optional(generation, "building_revision", out _))
            {
                throw new ProtocolValidationException(path + " has a building revision while indexing is false");
            }
        }

        private static void ValidateDaemonLifecycleStatus(
            JsonElement daemon,
            JsonElement generation,
            string path)
        {
            StrictJson.Properties(
                daemon,
                path,
                "lifecycle",
                "serving",
                "freshness",
                "freshness_maintenance",
                "reconcile",
                "generation_maintenance",
                "watcher",
                "timer",
                "background_reindex_operations");
            StrictJson.Enum(
                StrictJson.Required(daemon, "lifecycle", path),
                path + ".lifecycle",
                "booting",
                "serving",
                "draining");
            string serving = StrictJson.Enum(
                StrictJson.Required(daemon, "serving", path),
                path + ".serving",
                "unavailable",
                "queryable");
            string freshness = StrictJson.Enum(
                StrictJson.Required(daemon, "freshness", path),
                path + ".freshness",
                "absent",
                "stale",
                "current");
            string maintenance = StrictJson.Enum(
                StrictJson.Required(daemon, "freshness_maintenance", path),
                path + ".freshness_maintenance",
                "managed",
                "unmanaged");
            StrictJson.Enum(
                StrictJson.Required(daemon, "reconcile", path),
                path + ".reconcile",
                "idle",
                "queued",
                "running",
                "failed");

            JsonElement generationMaintenance = StrictJson.Required(daemon, "generation_maintenance", path);
            StrictJson.Properties(
                generationMaintenance,
                path + ".generation_maintenance",
                "state",
                "last_recovered_entries",
                "last_cleanup_failure?");
            string generationMaintenanceState = StrictJson.Enum(
                StrictJson.Required(generationMaintenance, "state", path + ".generation_maintenance"),
                path + ".generation_maintenance.state",
                "clean",
                "recovery_required");
            StrictJson.UInt64(
                StrictJson.Required(
                    generationMaintenance,
                    "last_recovered_entries",
                    path + ".generation_maintenance"),
                path + ".generation_maintenance.last_recovered_entries");
            bool hasCleanupFailure = StrictJson.Optional(
                generationMaintenance,
                "last_cleanup_failure",
                out JsonElement cleanupFailure);
            if (hasCleanupFailure)
            {
                StrictJson.String(
                    cleanupFailure,
                    path + ".generation_maintenance.last_cleanup_failure",
                    MaxErrorMessageBytes,
                    allowEmpty: false);
            }
            if ((generationMaintenanceState == "recovery_required") != hasCleanupFailure)
            {
                throw new ProtocolValidationException(
                    path + ".generation_maintenance cleanup failure evidence is inconsistent");
            }

            bool hasActive = StrictJson.Optional(generation, "active", out JsonElement active);
            string expectedServing = hasActive ? "queryable" : "unavailable";
            string expectedFreshness = !hasActive
                ? "absent"
                : StrictJson.Boolean(
                    StrictJson.Required(active, "stale", path + ".generation.active"),
                    path + ".generation.active.stale")
                    ? "stale"
                    : "current";
            if (!string.Equals(serving, expectedServing, StringComparison.Ordinal)
                || !string.Equals(freshness, expectedFreshness, StringComparison.Ordinal))
            {
                throw new ProtocolValidationException(path + " does not match generation availability and freshness");
            }

            JsonElement watcher = StrictJson.Required(daemon, "watcher", path);
            StrictJson.Properties(
                watcher,
                path + ".watcher",
                "state",
                "retry_count",
                "last_failure?",
                "next_retry_in_ms?");
            string watcherState = StrictJson.Enum(
                StrictJson.Required(watcher, "state", path + ".watcher"),
                path + ".watcher.state",
                "disabled",
                "starting",
                "healthy",
                "failed",
                "retrying",
                "stopped");
            StrictJson.UInt64(
                StrictJson.Required(watcher, "retry_count", path + ".watcher"),
                path + ".watcher.retry_count");
            bool watcherHasFailure = StrictJson.Optional(watcher, "last_failure", out JsonElement watcherFailure);
            if (watcherHasFailure)
            {
                StrictJson.String(watcherFailure, path + ".watcher.last_failure", MaxErrorMessageBytes, allowEmpty: false);
            }
            if ((watcherState == "failed" || watcherState == "retrying") && !watcherHasFailure)
            {
                throw new ProtocolValidationException(path + ".watcher requires failure evidence");
            }
            bool watcherHasRetryDeadline = StrictJson.Optional(
                watcher,
                "next_retry_in_ms",
                out JsonElement watcherRetryDeadline);
            if (watcherHasRetryDeadline)
            {
                StrictJson.UInt64(watcherRetryDeadline, path + ".watcher.next_retry_in_ms");
            }
            if ((watcherState == "retrying") != watcherHasRetryDeadline)
            {
                throw new ProtocolValidationException(path + ".watcher retry deadline is inconsistent");
            }

            JsonElement timer = StrictJson.Required(daemon, "timer", path);
            StrictJson.Properties(timer, path + ".timer", "state", "run_count", "last_failure?", "next_run_in_ms?");
            string timerState = StrictJson.Enum(
                StrictJson.Required(timer, "state", path + ".timer"),
                path + ".timer.state",
                "disabled",
                "scheduled",
                "running",
                "failed",
                "stopped");
            StrictJson.UInt64(
                StrictJson.Required(timer, "run_count", path + ".timer"),
                path + ".timer.run_count");
            bool timerHasFailure = StrictJson.Optional(timer, "last_failure", out JsonElement timerFailure);
            if (timerHasFailure)
            {
                StrictJson.String(timerFailure, path + ".timer.last_failure", MaxErrorMessageBytes, allowEmpty: false);
            }
            bool timerHasNextRun = StrictJson.Optional(timer, "next_run_in_ms", out JsonElement nextRun);
            if (timerHasNextRun)
            {
                StrictJson.UInt64(nextRun, path + ".timer.next_run_in_ms");
            }
            if (timerState == "failed" && !timerHasFailure)
            {
                throw new ProtocolValidationException(path + ".timer requires failure evidence");
            }
            if ((timerState == "disabled" || timerState == "stopped") && timerHasNextRun)
            {
                throw new ProtocolValidationException(path + ".timer cannot advertise a next run");
            }
            if (timerState == "scheduled" && !timerHasNextRun)
            {
                throw new ProtocolValidationException(path + ".timer must advertise its next run");
            }

            ValidateBackgroundReindexOperations(
                StrictJson.Required(daemon, "background_reindex_operations", path),
                path + ".background_reindex_operations");

            string expectedMaintenance = watcherState == "disabled" && timerState == "disabled"
                ? "unmanaged"
                : "managed";
            if (!string.Equals(maintenance, expectedMaintenance, StringComparison.Ordinal))
            {
                throw new ProtocolValidationException(path + ".freshness_maintenance is inconsistent");
            }
        }

        private static void ValidateSearchResponse(JsonElement response, string path, QueryPolicyId envelopePolicy)
        {
            StrictJson.Properties(
                response,
                path,
                "protocol_revision",
                "generation",
                "query_policy_id",
                "query",
                "took_ms",
                "match_count",
                "returned_hits",
                "request_limit_truncated",
                "fuzzy_work",
                "hits",
                "diagnostics",
                "fallback_used");
            StrictJson.RequireRevision(StrictJson.Required(response, "protocol_revision", path), path + ".protocol_revision");
            ValidateGenerationStamp(StrictJson.Required(response, "generation", path), path + ".generation");
            ValidatePolicyBinding(response, path, envelopePolicy);
            StrictJson.String(StrictJson.Required(response, "query", path), path + ".query", 4 * 1024);
            StrictJson.UInt64(StrictJson.Required(response, "took_ms", path), path + ".took_ms");

            JsonElement matchCount = StrictJson.Required(response, "match_count", path);
            StrictJson.Properties(matchCount, path + ".match_count", "value", "relation");
            ulong total = StrictJson.UInt64(StrictJson.Required(matchCount, "value", path + ".match_count"), path + ".match_count.value");
            StrictJson.Enum(StrictJson.Required(matchCount, "relation", path + ".match_count"), path + ".match_count.relation", "exact", "lower_bound");

            uint returned = StrictJson.UInt32(StrictJson.Required(response, "returned_hits", path), path + ".returned_hits");
            StrictJson.Boolean(StrictJson.Required(response, "request_limit_truncated", path), path + ".request_limit_truncated");
            ValidateFuzzyWork(StrictJson.Required(response, "fuzzy_work", path), path + ".fuzzy_work");
            JsonElement hitValues = StrictJson.Required(response, "hits", path);
            JsonElement[] hits = StrictJson.Array(hitValues, path + ".hits", 1000);
            if (returned != hits.Length || total < returned)
            {
                throw new ProtocolValidationException(path + " has inconsistent search hit counts");
            }
            for (int index = 0; index < hits.Length; index++)
            {
                ValidateSearchHit(hits[index], $"{path}.hits[{index}]", checked((uint)index + 1));
            }
            JsonElement diagnosticValues = StrictJson.Required(response, "diagnostics", path);
            JsonElement[] diagnosticEntries = StrictJson.Array(
                diagnosticValues,
                path + ".diagnostics",
                maximum: 4096);
            for (int index = 0; index < diagnosticEntries.Length; index++)
            {
                ValidateSearchDiagnostic(diagnosticEntries[index], $"{path}.diagnostics[{index}]");
            }
            StrictJson.Boolean(StrictJson.Required(response, "fallback_used", path), path + ".fallback_used");
            RequireJsonByteLimit(hitValues, path + ".hits", MaxSearchHitsJsonBytes);
            RequireJsonByteLimit(
                diagnosticValues,
                path + ".diagnostics",
                MaxSearchDiagnosticsJsonBytes);
            RequireJsonByteLimit(response, path, MaxSearchResponseJsonBytes);
        }

        private static void ValidateSuggestResponse(JsonElement response, string path, QueryPolicyId envelopePolicy)
        {
            StrictJson.Properties(
                response,
                path,
                "protocol_revision",
                "generation",
                "query_policy_id",
                "prefix",
                "took_ms",
                "suggestions");
            StrictJson.RequireRevision(StrictJson.Required(response, "protocol_revision", path), path + ".protocol_revision");
            ValidateGenerationStamp(StrictJson.Required(response, "generation", path), path + ".generation");
            ValidatePolicyBinding(response, path, envelopePolicy);
            StrictJson.String(StrictJson.Required(response, "prefix", path), path + ".prefix", 4 * 1024);
            StrictJson.UInt64(StrictJson.Required(response, "took_ms", path), path + ".took_ms");
            JsonElement suggestionValues = StrictJson.Required(response, "suggestions", path);
            JsonElement[] suggestions = StrictJson.Array(suggestionValues, path + ".suggestions", 50);
            for (int index = 0; index < suggestions.Length; index++)
            {
                StrictJson.String(
                    suggestions[index],
                    $"{path}.suggestions[{index}]",
                    32 * 1024,
                    allowEmpty: false);
            }
            if (Encoding.UTF8.GetByteCount(suggestionValues.GetRawText()) > 224 * 1024)
            {
                throw new ProtocolValidationException(path + ".suggestions exceeds the JSON byte limit");
            }
        }

        private static void ValidateSearchHit(JsonElement hit, string path, uint expectedRank)
        {
            StrictJson.Properties(
                hit,
                path,
                "rank",
                "guid?",
                "path",
                "name",
                "kind",
                "stable_id",
                "location",
                "ranking_signals",
                "match_kind",
                "explanation",
                "matched_hierarchy_paths",
                "matched_script_symbols",
                "highlight_path_ranges",
                "highlight_name_ranges");
            uint rank = StrictJson.UInt32(StrictJson.Required(hit, "rank", path), path + ".rank");
            if (rank != expectedRank)
            {
                throw new ProtocolValidationException(path + ".rank is not contiguous and one-based");
            }
            if (StrictJson.Optional(hit, "guid", out JsonElement guid))
            {
                ValidateGuid(guid, path + ".guid");
            }
            StrictJson.PortablePath(StrictJson.Required(hit, "path", path), path + ".path");
            StrictJson.String(StrictJson.Required(hit, "name", path), path + ".name");
            StrictJson.String(StrictJson.Required(hit, "kind", path), path + ".kind");
            StrictJson.String(StrictJson.Required(hit, "stable_id", path), path + ".stable_id");
            ValidateLocation(StrictJson.Required(hit, "location", path), path + ".location");
            ValidateRankingSignals(StrictJson.Required(hit, "ranking_signals", path), path + ".ranking_signals");
            ValidateMatchKind(StrictJson.Required(hit, "match_kind", path), path + ".match_kind");
            ValidateMatchExplanation(StrictJson.Required(hit, "explanation", path), path + ".explanation");
            ValidateStringArray(StrictJson.Required(hit, "matched_hierarchy_paths", path), path + ".matched_hierarchy_paths");
            ValidateStringArray(StrictJson.Required(hit, "matched_script_symbols", path), path + ".matched_script_symbols");
            ValidateRanges(StrictJson.Required(hit, "highlight_path_ranges", path), path + ".highlight_path_ranges");
            ValidateRanges(StrictJson.Required(hit, "highlight_name_ranges", path), path + ".highlight_name_ranges");
        }

        private static void ValidateRankingSignals(JsonElement signals, string path)
        {
            StrictJson.Properties(signals, path, "field_boost", "fuzzy_score", "retrieval_stage", "retrieval_score");
            StrictJson.UInt32(StrictJson.Required(signals, "field_boost", path), path + ".field_boost");
            StrictJson.Int64(StrictJson.Required(signals, "fuzzy_score", path), path + ".fuzzy_score");
            StrictJson.Enum(StrictJson.Required(signals, "retrieval_stage", path), path + ".retrieval_stage", "strict", "fuzzy_fallback");
            StrictJson.Int64(StrictJson.Required(signals, "retrieval_score", path), path + ".retrieval_score");
        }

        private static void ValidateMatchExplanation(JsonElement explanation, string path)
        {
            StrictJson.Properties(explanation, path, "terms", "fuzzy_fallback");
            JsonElement[] terms = StrictJson.Array(StrictJson.Required(explanation, "terms", path), path + ".terms");
            for (int index = 0; index < terms.Length; index++)
            {
                JsonElement term = terms[index];
                string termPath = $"{path}.terms[{index}]";
                StrictJson.Properties(term, termPath, "term", "quoted", "kind", "field");
                StrictJson.String(StrictJson.Required(term, "term", termPath), termPath + ".term");
                StrictJson.Boolean(StrictJson.Required(term, "quoted", termPath), termPath + ".quoted");
                ValidateMatchKind(StrictJson.Required(term, "kind", termPath), termPath + ".kind");
                StrictJson.Enum(StrictJson.Required(term, "field", termPath), termPath + ".field", "name", "path", "kind", "content");
            }
            StrictJson.Boolean(StrictJson.Required(explanation, "fuzzy_fallback", path), path + ".fuzzy_fallback");
        }

        private static void ValidateFuzzyWork(JsonElement usage, string path)
        {
            StrictJson.Properties(usage, path, "consumed", "limit", "exhausted");
            StrictJson.UInt64(StrictJson.Required(usage, "consumed", path), path + ".consumed");
            StrictJson.UInt64(StrictJson.Required(usage, "limit", path), path + ".limit");
            StrictJson.Boolean(StrictJson.Required(usage, "exhausted", path), path + ".exhausted");
        }

        private static void ValidateMatchKind(JsonElement element, string path)
        {
            StrictJson.Enum(element, path, "exact", "prefix", "token", "substring", "abbreviation", "fuzzy", "none");
        }

        private static void ValidateSearchDiagnostic(JsonElement diagnostic, string path)
        {
            string code = StrictJson.String(StrictJson.Required(diagnostic, "code", path), path + ".code", allowEmpty: false);
            switch (code)
            {
                case "empty_query":
                    StrictJson.Properties(diagnostic, path, "code");
                    return;
                case "unterminated_quote":
                case "empty_quoted_term":
                    StrictJson.Properties(diagnostic, path, "code", "byte_offset");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "byte_offset", path), path + ".byte_offset");
                    return;
                case "missing_filter_value":
                case "duplicate_filter":
                    StrictJson.Properties(diagnostic, path, "code", "field");
                    StrictJson.String(StrictJson.Required(diagnostic, "field", path), path + ".field");
                    return;
                case "unsupported_type_filter":
                    StrictJson.Properties(diagnostic, path, "code", "value");
                    StrictJson.String(StrictJson.Required(diagnostic, "value", path), path + ".value");
                    return;
                case "candidate_limit_exceeded":
                    StrictJson.Properties(diagnostic, path, "code", "stage", "provided", "limit");
                    StrictJson.Enum(StrictJson.Required(diagnostic, "stage", path), path + ".stage", "strict", "fuzzy_fallback");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "provided", path), path + ".provided");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "limit", path), path + ".limit");
                    return;
                case "query_byte_limit_exceeded":
                case "query_term_limit_exceeded":
                case "retrieval_term_limit_exceeded":
                case "candidate_evidence_limit_exceeded":
                    StrictJson.Properties(diagnostic, path, "code", "actual", "limit");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "actual", path), path + ".actual");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "limit", path), path + ".limit");
                    return;
                case "candidate_field_byte_limit_exceeded":
                    StrictJson.Properties(diagnostic, path, "code", "field", "actual", "limit");
                    StrictJson.Enum(
                        StrictJson.Required(diagnostic, "field", path),
                        path + ".field",
                        "stable_key",
                        "name",
                        "path",
                        "kind",
                        "guid",
                        "container_source_path");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "actual", path), path + ".actual");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "limit", path), path + ".limit");
                    return;
                case "candidate_total_byte_limit_exceeded":
                    StrictJson.Properties(diagnostic, path, "code", "consumed", "limit");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "consumed", path), path + ".consumed");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "limit", path), path + ".limit");
                    return;
                case "candidate_input_limit_exceeded":
                    StrictJson.Properties(diagnostic, path, "code", "limit");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "limit", path), path + ".limit");
                    return;
                case "fuzzy_work_limit_exceeded":
                    StrictJson.Properties(diagnostic, path, "code", "attempted", "limit");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "attempted", path), path + ".attempted");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "limit", path), path + ".limit");
                    return;
                case "invalid_retrieval_evidence":
                    StrictJson.Properties(diagnostic, path, "code", "term_index");
                    StrictJson.UInt64(StrictJson.Required(diagnostic, "term_index", path), path + ".term_index");
                    return;
                case "duplicate_candidate_key":
                    StrictJson.Properties(diagnostic, path, "code", "stable_key");
                    StrictJson.String(StrictJson.Required(diagnostic, "stable_key", path), path + ".stable_key");
                    return;
                default:
                    throw new ProtocolValidationException($"{path}.code contains unsupported value '{code}'");
            }
        }

        private static void ValidateReferencesResponse(JsonElement response, string path, QueryPolicyId envelopePolicy)
        {
            StrictJson.Properties(
                response,
                path,
                "protocol_revision",
                "generation",
                "query_policy_id",
                "request",
                "took_ms",
                "coverage",
                "hits",
                "diagnostics",
                "diagnostic_coverage");
            StrictJson.RequireRevision(StrictJson.Required(response, "protocol_revision", path), path + ".protocol_revision");
            JsonElement generation = StrictJson.Required(response, "generation", path);
            ValidateGenerationStamp(generation, path + ".generation");
            ValidatePolicyBinding(response, path, envelopePolicy);
            JsonElement referenceRequest = StrictJson.Required(response, "request", path);
            ValidateReferenceRequest(referenceRequest, path + ".request", envelopePolicy);
            string requestQueryBinding = ComputeReferenceQueryBinding(referenceRequest, path + ".request");
            StrictJson.UInt64(StrictJson.Required(response, "took_ms", path), path + ".took_ms");

            JsonElement coverage = StrictJson.Required(response, "coverage", path);
            uint returned = ValidateReferenceCoverage(
                coverage,
                path + ".coverage",
                envelopePolicy,
                requestQueryBinding);
            JsonElement[] hits = StrictJson.Array(StrictJson.Required(response, "hits", path), path + ".hits");
            if (returned != hits.Length)
            {
                throw new ProtocolValidationException(path + " has inconsistent reference hit counts");
            }
            for (int index = 0; index < hits.Length; index++)
            {
                ValidateReferenceHit(hits[index], $"{path}.hits[{index}]");
            }

            JsonElement diagnostics = StrictJson.Required(response, "diagnostics", path);
            JsonElement[] diagnosticEntries = StrictJson.Array(
                diagnostics,
                path + ".diagnostics",
                128);
            uint diagnosticCount = checked((uint)diagnosticEntries.Length);
            for (int index = 0; index < diagnosticEntries.Length; index++)
            {
                ValidateDiagnostic(diagnosticEntries[index], $"{path}.diagnostics[{index}]");
            }
            ValidateReferenceDiagnosticCoverage(
                StrictJson.Required(response, "diagnostic_coverage", path),
                path + ".diagnostic_coverage",
                diagnosticCount,
                checked((ulong)BootstrapCodec.Write(writer => diagnostics.WriteTo(writer)).Length));

            string responseGeneration = StrictJson.String(
                StrictJson.Required(generation, "generation", path + ".generation"),
                path + ".generation.generation");
            if (StrictJson.Optional(referenceRequest, "cursor", out JsonElement requestCursor))
            {
                string requestCursorGeneration = StrictJson.String(
                    StrictJson.Required(requestCursor, "generation", path + ".request.cursor"),
                    path + ".request.cursor.generation");
                if (!string.Equals(requestCursorGeneration, responseGeneration, StringComparison.Ordinal))
                {
                    throw new ProtocolValidationException(path + ".request.cursor generation binding mismatch");
                }
            }

            if (StrictJson.Optional(coverage, "next_cursor", out JsonElement cursor))
            {
                string cursorGeneration = StrictJson.String(
                    StrictJson.Required(cursor, "generation", path + ".coverage.next_cursor"),
                    path + ".coverage.next_cursor.generation");
                if (!string.Equals(cursorGeneration, responseGeneration, StringComparison.Ordinal))
                {
                    throw new ProtocolValidationException(path + ".coverage.next_cursor generation binding mismatch");
                }
                QueryPolicyId cursorPolicy = QueryPolicyId.Parse(
                    StrictJson.String(
                        StrictJson.Required(cursor, "query_policy_id", path + ".coverage.next_cursor"),
                        path + ".coverage.next_cursor.query_policy_id"));
                if (!cursorPolicy.Equals(envelopePolicy))
                {
                    throw new ProtocolValidationException(path + ".coverage.next_cursor query policy binding mismatch");
                }
            }
        }

        private static uint ValidateReferenceCoverage(
            JsonElement coverage,
            string path,
            QueryPolicyId expectedPolicy,
            string expectedQueryBinding)
        {
            StrictJson.Properties(coverage, path, "complete", "truncated", "returned", "total?", "next_cursor?");
            bool complete = StrictJson.Boolean(StrictJson.Required(coverage, "complete", path), path + ".complete");
            bool truncated = StrictJson.Boolean(StrictJson.Required(coverage, "truncated", path), path + ".truncated");
            uint returned = StrictJson.UInt32(StrictJson.Required(coverage, "returned", path), path + ".returned");
            bool hasTotal = StrictJson.Optional(coverage, "total", out JsonElement total);
            if (complete != hasTotal)
            {
                throw new ProtocolValidationException(path + ".total availability does not match complete");
            }
            if (!complete && !truncated)
            {
                throw new ProtocolValidationException(path + " must mark incomplete coverage as truncated");
            }
            if (hasTotal && StrictJson.UInt64(total, path + ".total") < returned)
            {
                throw new ProtocolValidationException(path + ".total is smaller than returned");
            }
            if (StrictJson.Optional(coverage, "next_cursor", out JsonElement cursor))
            {
                if (!truncated)
                {
                    throw new ProtocolValidationException(path + ".next_cursor requires truncated=true");
                }
                ValidateReferenceCursor(
                    cursor,
                    path + ".next_cursor",
                    expectedPolicy,
                    expectedQueryBinding);
            }
            return returned;
        }

        private static void ValidateReferenceDiagnosticCoverage(
            JsonElement coverage,
            string path,
            uint actualReturned,
            ulong actualSerializedBytes)
        {
            StrictJson.Properties(
                coverage,
                path,
                "returned",
                "truncated",
                "total?",
                "serialized_bytes",
                "max_count",
                "max_serialized_bytes");
            uint returned = StrictJson.UInt32(StrictJson.Required(coverage, "returned", path), path + ".returned");
            StrictJson.Boolean(StrictJson.Required(coverage, "truncated", path), path + ".truncated");
            ulong serialized = StrictJson.UInt64(StrictJson.Required(coverage, "serialized_bytes", path), path + ".serialized_bytes");
            uint maximumCount = StrictJson.UInt32(StrictJson.Required(coverage, "max_count", path), path + ".max_count");
            ulong maximumBytes = StrictJson.UInt64(StrictJson.Required(coverage, "max_serialized_bytes", path), path + ".max_serialized_bytes");
            if (returned != actualReturned
                || serialized != actualSerializedBytes
                || returned > maximumCount
                || maximumCount > 128
                || serialized > maximumBytes
                || maximumBytes > 256 * 1024)
            {
                throw new ProtocolValidationException(path + " contains inconsistent diagnostic limits");
            }
            if (StrictJson.Optional(coverage, "total", out JsonElement total)
                && StrictJson.UInt64(total, path + ".total") < returned)
            {
                throw new ProtocolValidationException(path + ".total is smaller than returned");
            }
        }

        private static void ValidateReferenceHit(JsonElement hit, string path)
        {
            StrictJson.Properties(
                hit,
                path,
                "source_path",
                "source_kind",
                "stable_id",
                "source_object",
                "location",
                "contexts",
                "objects");
            JsonElement sourcePathElement = StrictJson.Required(hit, "source_path", path);
            StrictJson.PortablePath(sourcePathElement, path + ".source_path");
            string sourcePath = StrictJson.String(sourcePathElement, path + ".source_path");
            StrictJson.String(StrictJson.Required(hit, "source_kind", path), path + ".source_kind");
            StrictJson.String(StrictJson.Required(hit, "stable_id", path), path + ".stable_id");
            long? sourceFileId = ValidateObjectAddress(
                StrictJson.Required(hit, "source_object", path),
                path + ".source_object");
            JsonElement location = StrictJson.Required(hit, "location", path);
            ValidateLocation(location, path + ".location");
            string locationPath = StrictJson.String(
                StrictJson.Required(location, "path", path + ".location"),
                path + ".location.path");
            if (!string.Equals(sourcePath, locationPath, StringComparison.Ordinal)
                || ReadOptionalInt64(location, "file_id", path + ".location") != sourceFileId)
            {
                throw new ProtocolValidationException(path + " contains inconsistent source identity");
            }
            JsonElement[] contexts = StrictJson.Array(
                StrictJson.Required(hit, "contexts", path),
                path + ".contexts");
            for (int index = 0; index < contexts.Length; index++)
            {
                string contextPath = $"{path}.contexts[{index}]";
                ValidateReferenceContext(contexts[index], contextPath);
                if (ReadOptionalInt64(contexts[index], "doc_file_id", contextPath) != sourceFileId)
                {
                    throw new ProtocolValidationException(path + " contains inconsistent source identity");
                }
            }
            JsonElement[] objects = StrictJson.Array(
                StrictJson.Required(hit, "objects", path),
                path + ".objects");
            for (int index = 0; index < objects.Length; index++)
            {
                ValidateReferenceObject(objects[index], $"{path}.objects[{index}]");
            }
        }

        private static void ValidateReferenceContext(JsonElement context, string path)
        {
            StrictJson.Properties(
                context,
                path,
                "doc_file_id?",
                "doc_class_id?",
                "object_name?",
                "hierarchy_path?",
                "field_hint?",
                "source_line?",
                "source_column?");
            ValidateOptionalInt64(context, "doc_file_id", path);
            ValidateOptionalInt32(context, "doc_class_id", path);
            ValidateOptionalString(context, "object_name", path);
            ValidateOptionalString(context, "hierarchy_path", path);
            ValidateOptionalString(context, "field_hint", path);
            ValidateOptionalUInt32(context, "source_line", path);
            ValidateOptionalUInt32(context, "source_column", path);
        }

        private static void ValidateReferenceObject(JsonElement value, string path)
        {
            StrictJson.Properties(
                value,
                path,
                "doc_file_id?",
                "doc_class_id?",
                "stable_id",
                "location",
                "object_name?",
                "hierarchy_path?",
                "field_hints");
            ValidateOptionalInt64(value, "doc_file_id", path);
            ValidateOptionalInt32(value, "doc_class_id", path);
            StrictJson.String(StrictJson.Required(value, "stable_id", path), path + ".stable_id");
            ValidateLocation(StrictJson.Required(value, "location", path), path + ".location");
            ValidateOptionalString(value, "object_name", path);
            ValidateOptionalString(value, "hierarchy_path", path);
            ValidateStringArray(StrictJson.Required(value, "field_hints", path), path + ".field_hints");
        }

        private static void ValidateReindexOperationStatus(JsonElement response, string path, QueryPolicyId envelopePolicy)
        {
            StrictJson.Properties(response, path, "operation_id", "state", "admission?", "completion?", "status?", "error?");
            OperationId.Parse(StrictJson.String(StrictJson.Required(response, "operation_id", path), path + ".operation_id"));
            ReindexOperationState state = ParseReindexOperationState(
                StrictJson.String(
                    StrictJson.Required(response, "state", path),
                    path + ".state"));
            bool hasAdmission = StrictJson.Optional(response, "admission", out JsonElement admission);
            bool hasCompletion = StrictJson.Optional(response, "completion", out JsonElement completion);
            bool hasStatus = StrictJson.Optional(response, "status", out JsonElement status);
            bool hasError = StrictJson.Optional(response, "error", out JsonElement error);
            if (hasAdmission)
            {
                ValidateReindexReceipt(admission, path + ".admission");
            }
            if (hasCompletion)
            {
                ValidateReindexReceipt(completion, path + ".completion");
            }
            if (hasStatus)
            {
                ValidateStatusResponse(status, path + ".status", envelopePolicy);
            }
            if (hasError)
            {
                ValidateApiError(error, path + ".error", envelopePolicy);
            }

            bool valid;
            switch (state)
            {
                case ReindexOperationState.Queued:
                case ReindexOperationState.Coalesced:
                case ReindexOperationState.Running:
                    valid = !hasCompletion && !hasStatus && !hasError;
                    break;
                case ReindexOperationState.Succeeded:
                    valid = hasCompletion && hasStatus && !hasError;
                    break;
                case ReindexOperationState.Failed:
                    valid = !hasCompletion && !hasStatus && hasError;
                    break;
                default:
                    valid = !hasCompletion && !hasStatus && !hasError;
                    break;
            }
            if (!valid)
            {
                throw new ProtocolValidationException(path + " has fields inconsistent with its lifecycle state");
            }
            if (state == ReindexOperationState.Succeeded)
            {
                string completionDisposition = StrictJson.String(
                    StrictJson.Required(completion, "disposition", path + ".completion"),
                    path + ".completion.disposition");
                string? completionGeneration = OptionalReceiptGeneration(completion);
                string? statusGeneration = OptionalActiveGeneration(status);
                bool indexing = StrictJson.Boolean(
                    StrictJson.Required(status, "indexing", path + ".status"),
                    path + ".status.indexing");
                JsonElement generationStatus = StrictJson.Required(status, "generation", path + ".status");
                if ((completionDisposition != "applied" && completionDisposition != "already_applied")
                    || indexing
                    || StrictJson.Optional(generationStatus, "building_revision", out _)
                    || completionGeneration == null
                    || statusGeneration == null
                    || !string.Equals(completionGeneration, statusGeneration, StringComparison.Ordinal))
                {
                    throw new ProtocolValidationException(path + " has an inconsistent succeeded lifecycle state");
                }
            }
        }

        private static void ValidateReindexCancelResponse(JsonElement response, string path)
        {
            StrictJson.Properties(response, path, "operation_id", "state", "cancelled");
            OperationId.Parse(StrictJson.String(StrictJson.Required(response, "operation_id", path), path + ".operation_id"));
            ReindexOperationState state = ParseReindexOperationState(
                StrictJson.String(
                    StrictJson.Required(response, "state", path),
                    path + ".state"));
            bool cancelled = StrictJson.Boolean(StrictJson.Required(response, "cancelled", path), path + ".cancelled");
            if (cancelled != (state == ReindexOperationState.Cancelled))
            {
                throw new ProtocolValidationException(path + " has an inconsistent cancellation result");
            }
        }

        private static void ValidateReindexReceipt(JsonElement receipt, string path)
        {
            StrictJson.Properties(
                receipt,
                path,
                "protocol_revision",
                "disposition",
                "transaction?",
                "target_revision?",
                "generation?",
                "evidence");
            StrictJson.RequireRevision(StrictJson.Required(receipt, "protocol_revision", path), path + ".protocol_revision");
            StrictJson.Enum(
                StrictJson.Required(receipt, "disposition", path),
                path + ".disposition",
                "applied",
                "already_applied",
                "coalesced",
                "queued");
            if (StrictJson.Optional(receipt, "transaction", out JsonElement transaction))
            {
                ValidateDigest(transaction, path + ".transaction");
            }
            if (StrictJson.Optional(receipt, "target_revision", out JsonElement target))
            {
                ValidateDigest(target, path + ".target_revision");
            }
            if (StrictJson.Optional(receipt, "generation", out JsonElement generation))
            {
                ValidateGenerationStamp(generation, path + ".generation");
            }
            ValidateReindexEvidence(StrictJson.Required(receipt, "evidence", path), path + ".evidence");
        }

        private static void ValidateReindexEvidence(JsonElement evidence, string path)
        {
            StrictJson.Properties(
                evidence,
                path,
                "forced_full_scan",
                "forced_full_analysis",
                "full_dependency_scan",
                "dependency_candidate_assets",
                "dependency_closure_assets",
                "analysis",
                "disk_estimate?",
                "publish_warnings");
            StrictJson.Boolean(StrictJson.Required(evidence, "forced_full_scan", path), path + ".forced_full_scan");
            StrictJson.Boolean(StrictJson.Required(evidence, "forced_full_analysis", path), path + ".forced_full_analysis");
            StrictJson.Boolean(StrictJson.Required(evidence, "full_dependency_scan", path), path + ".full_dependency_scan");
            StrictJson.UInt64(StrictJson.Required(evidence, "dependency_candidate_assets", path), path + ".dependency_candidate_assets");
            StrictJson.UInt64(StrictJson.Required(evidence, "dependency_closure_assets", path), path + ".dependency_closure_assets");
            ValidateReindexAnalysis(StrictJson.Required(evidence, "analysis", path), path + ".analysis");
            if (StrictJson.Optional(evidence, "disk_estimate", out JsonElement estimate))
            {
                ValidateDiskEstimate(estimate, path + ".disk_estimate");
            }
            JsonElement publishWarnings = StrictJson.Required(evidence, "publish_warnings", path);
            JsonElement[] warningValues = StrictJson.Array(
                publishWarnings,
                path + ".publish_warnings",
                MaxReindexPublishWarnings);
            for (int index = 0; index < warningValues.Length; index++)
            {
                StrictJson.String(
                    warningValues[index],
                    $"{path}.publish_warnings[{index}]",
                    MaxReindexPublishWarningBytes,
                    allowEmpty: false);
            }
            RequireJsonByteLimit(
                publishWarnings,
                path + ".publish_warnings",
                MaxReindexPublishWarningsJsonBytes);
        }

        private static void ValidateReindexAnalysis(JsonElement analysis, string path)
        {
            string[] fields =
            {
                "assets_visited",
                "assets_analyzed",
                "source_opens",
                "source_bytes_read",
                "text_sources",
                "text_bytes_scanned",
                "yaml_documents",
                "binary_objects",
                "unity_values_visited",
                "references_emitted",
                "container_entries_emitted",
                "truncations_emitted",
                "diagnostics_emitted",
            };
            StrictJson.Properties(analysis, path, fields);
            foreach (string field in fields)
            {
                StrictJson.UInt64(StrictJson.Required(analysis, field, path), path + "." + field);
            }
        }

        private static void ValidateDiskEstimate(JsonElement estimate, string path)
        {
            string[] fields =
            {
                "existing_generation_bytes",
                "old_active_generation_bytes",
                "new_generation_bytes",
                "publish_peak_bytes",
                "retained_bytes_after_publish",
                "reclaimable_bytes_after_publish",
            };
            StrictJson.Properties(estimate, path, fields);
            foreach (string field in fields)
            {
                StrictJson.UInt64(StrictJson.Required(estimate, field, path), path + "." + field);
            }
        }

        private static void ValidateApiError(JsonElement error, string path, QueryPolicyId envelopePolicy)
        {
            StrictJson.Properties(
                error,
                path,
                "protocol_revision",
                "code",
                "message",
                "retryable",
                "generation?",
                "query_policy_id?",
                "details");
            StrictJson.RequireRevision(StrictJson.Required(error, "protocol_revision", path), path + ".protocol_revision");
            ReadApiErrorCode(StrictJson.Required(error, "code", path), path + ".code");
            StrictJson.String(
                StrictJson.Required(error, "message", path),
                path + ".message",
                MaxErrorMessageBytes,
                allowEmpty: false);
            StrictJson.Boolean(StrictJson.Required(error, "retryable", path), path + ".retryable");
            if (StrictJson.Optional(error, "generation", out JsonElement generation))
            {
                ValidateGenerationStamp(generation, path + ".generation");
            }
            if (StrictJson.Optional(error, "query_policy_id", out JsonElement queryPolicy))
            {
                QueryPolicyId policy = QueryPolicyId.Parse(StrictJson.String(queryPolicy, path + ".query_policy_id"));
                if (!policy.Equals(envelopePolicy))
                {
                    throw new ProtocolValidationException(path + ".query_policy_id does not match response envelope");
                }
            }
            JsonElement details = StrictJson.Required(error, "details", path);
            StrictJson.RequireKind(details, JsonValueKind.Object, path + ".details");
            JsonProperty[] entries = details.EnumerateObject().ToArray();
            if (entries.Length > 64)
            {
                throw new ProtocolValidationException(path + ".details exceeds 64 entries");
            }
            string? previous = null;
            foreach (JsonProperty entry in entries)
            {
                if (entry.Name.Length == 0 || Encoding.UTF8.GetByteCount(entry.Name) > 256)
                {
                    throw new ProtocolValidationException(path + ".details has an invalid key");
                }
                StrictJson.ValidateUnicodeScalarString(entry.Name, path + ".details key");
                if (previous != null
                    && StrictJson.CompareUnicodeScalarOrdinal(previous, entry.Name, path + ".details") >= 0)
                {
                    throw new ProtocolValidationException(path + ".details keys must use canonical sorted order");
                }
                StrictJson.String(entry.Value, path + ".details." + entry.Name, 4 * 1024);
                previous = entry.Name;
            }
            RequireJsonByteLimit(error, path, MaxApiErrorJsonBytes);
        }

        private static void ValidateGenerationStamp(JsonElement generation, string path)
        {
            StrictJson.Properties(
                generation,
                path,
                "protocol_revision",
                "generation",
                "workspace",
                "actual_revision",
                "desired_revision",
                "semantics_current",
                "configuration_current",
                "stale");
            StrictJson.RequireRevision(StrictJson.Required(generation, "protocol_revision", path), path + ".protocol_revision");
            ValidateDigest(StrictJson.Required(generation, "generation", path), path + ".generation");
            string workspace = StrictJson.String(StrictJson.Required(generation, "workspace", path), path + ".workspace");
            StrictJson.FixedHex(workspace, "workspace-v1:", 32, path + ".workspace", lowercaseOnly: true);
            ValidateDigest(StrictJson.Required(generation, "actual_revision", path), path + ".actual_revision");
            ValidateDigest(StrictJson.Required(generation, "desired_revision", path), path + ".desired_revision");
            string actual = StrictJson.Required(generation, "actual_revision", path).GetString()!;
            string desired = StrictJson.Required(generation, "desired_revision", path).GetString()!;
            bool semanticsCurrent = StrictJson.Boolean(
                StrictJson.Required(generation, "semantics_current", path),
                path + ".semantics_current");
            bool configurationCurrent = StrictJson.Boolean(
                StrictJson.Required(generation, "configuration_current", path),
                path + ".configuration_current");
            bool stale = StrictJson.Boolean(StrictJson.Required(generation, "stale", path), path + ".stale");
            if (stale != (!string.Equals(actual, desired, StringComparison.Ordinal)
                || !semanticsCurrent
                || !configurationCurrent))
            {
                throw new ProtocolValidationException(path + ".stale does not match revision and semantic freshness");
            }
        }

        private static void ValidateGenerationStatus(JsonElement status, string path)
        {
            StrictJson.Properties(status, path, "protocol_revision", "active?", "building_revision?", "last_failure?");
            StrictJson.RequireRevision(StrictJson.Required(status, "protocol_revision", path), path + ".protocol_revision");
            if (StrictJson.Optional(status, "active", out JsonElement active))
            {
                ValidateGenerationStamp(active, path + ".active");
            }
            if (StrictJson.Optional(status, "building_revision", out JsonElement building))
            {
                ValidateDigest(building, path + ".building_revision");
            }
            if (StrictJson.Optional(status, "last_failure", out JsonElement failure))
            {
                ValidateGenerationFailure(failure, path + ".last_failure");
            }
        }

        private static void ValidateGenerationFailure(JsonElement failure, string path)
        {
            StrictJson.Properties(failure, path, "code", "message", "retryable", "failed_unix_ms", "desired_revision?");
            StrictJson.String(StrictJson.Required(failure, "code", path), path + ".code", allowEmpty: false);
            StrictJson.String(
                StrictJson.Required(failure, "message", path),
                path + ".message",
                MaxErrorMessageBytes,
                allowEmpty: false);
            StrictJson.Boolean(StrictJson.Required(failure, "retryable", path), path + ".retryable");
            StrictJson.UInt64(StrictJson.Required(failure, "failed_unix_ms", path), path + ".failed_unix_ms");
            if (StrictJson.Optional(failure, "desired_revision", out JsonElement desired))
            {
                ValidateDigest(desired, path + ".desired_revision");
            }
        }

        private static void RequireJsonByteLimit(JsonElement value, string path, int maximum)
        {
            int actual = Encoding.UTF8.GetByteCount(value.GetRawText());
            if (actual > maximum)
            {
                throw new ProtocolValidationException(
                    $"{path} contains {actual} encoded JSON bytes; maximum is {maximum}");
            }
        }

        private static void ValidateCapabilities(JsonElement capabilities, string path)
        {
            StrictJson.Properties(
                capabilities,
                path,
                "protocol_revision",
                "search",
                "suggest",
                "incoming_references",
                "outgoing_references",
                "filesystem_reindex",
                "reindex_lifecycle",
                "background_reindex_discovery",
                "graceful_shutdown");
            StrictJson.RequireRevision(StrictJson.Required(capabilities, "protocol_revision", path), path + ".protocol_revision");
            string[] flags =
            {
                "search",
                "suggest",
                "incoming_references",
                "outgoing_references",
                "filesystem_reindex",
                "reindex_lifecycle",
                "background_reindex_discovery",
                "graceful_shutdown",
            };
            foreach (string flag in flags)
            {
                StrictJson.Boolean(StrictJson.Required(capabilities, flag, path), path + "." + flag);
            }
        }

        private static void ValidateBackgroundReindexOperations(JsonElement operations, string path)
        {
            StrictJson.RequireKind(operations, JsonValueKind.Array, path);
            int count = operations.GetArrayLength();
            if (count > ProtocolConstants.MaxBackgroundReindexOperations)
            {
                throw new ProtocolValidationException(
                    $"{path} exceeds {ProtocolConstants.MaxBackgroundReindexOperations} entries");
            }

            int previousOrigin = -1;
            int index = 0;
            foreach (JsonElement operation in operations.EnumerateArray())
            {
                string operationPath = $"{path}[{index}]";
                StrictJson.Properties(operation, operationPath, BackgroundReindexOperationProperties);

                BackgroundReindexOrigin origin = ParseBackgroundReindexOrigin(
                    StrictJson.String(
                        StrictJson.Required(operation, "origin", operationPath),
                        operationPath + ".origin"));
                int originIndex = (int)origin;
                if (originIndex <= previousOrigin)
                {
                    throw new ProtocolValidationException(path + " origins must be strictly increasing");
                }
                previousOrigin = originIndex;

                string operationId = OperationId.Parse(
                    StrictJson.String(
                        StrictJson.Required(operation, "operation_id", operationPath),
                        operationPath + ".operation_id")).Value;
                int previousIndex = 0;
                foreach (JsonElement previousOperation in operations.EnumerateArray())
                {
                    if (previousIndex == index)
                    {
                        break;
                    }
                    if (previousOperation.GetProperty("operation_id").ValueEquals(operationId))
                    {
                        throw new ProtocolValidationException(path + " operation IDs must be unique");
                    }
                    previousIndex++;
                }

                ReindexOperationState state = ParseReindexOperationState(
                    StrictJson.String(
                        StrictJson.Required(operation, "state", operationPath),
                        operationPath + ".state"));
                if (state == ReindexOperationState.Lost)
                {
                    throw new ProtocolValidationException(
                        operationPath + ".state cannot be lost for a discoverable background operation");
                }
                index++;
            }
        }

        private static BackgroundReindexOrigin ParseBackgroundReindexOrigin(string value)
        {
            return value switch
            {
                "startup" => BackgroundReindexOrigin.Startup,
                "watcher" => BackgroundReindexOrigin.Watcher,
                "watcher_overflow" => BackgroundReindexOrigin.WatcherOverflow,
                "timer" => BackgroundReindexOrigin.Timer,
                "semantic_upgrade" => BackgroundReindexOrigin.SemanticUpgrade,
                _ => throw new ProtocolValidationException(
                    $"background reindex origin contains unsupported value '{value}'"),
            };
        }

        private static ReindexOperationState ParseReindexOperationState(string value)
        {
            return value switch
            {
                "queued" => ReindexOperationState.Queued,
                "coalesced" => ReindexOperationState.Coalesced,
                "running" => ReindexOperationState.Running,
                "succeeded" => ReindexOperationState.Succeeded,
                "failed" => ReindexOperationState.Failed,
                "cancelled" => ReindexOperationState.Cancelled,
                "expired" => ReindexOperationState.Expired,
                "lost" => ReindexOperationState.Lost,
                _ => throw new ProtocolValidationException(
                    $"reindex operation state contains unsupported value '{value}'"),
            };
        }

        private static ApiErrorCode ParseApiErrorCode(string value)
        {
            return value switch
            {
                "invalid_request" => ApiErrorCode.InvalidRequest,
                "invalid_cursor" => ApiErrorCode.InvalidCursor,
                "stale_cursor" => ApiErrorCode.StaleCursor,
                "incompatible_protocol" => ApiErrorCode.IncompatibleProtocol,
                "peer_rejected" => ApiErrorCode.PeerRejected,
                "busy" => ApiErrorCode.Busy,
                "not_ready" => ApiErrorCode.NotReady,
                "revision_mismatch" => ApiErrorCode.RevisionMismatch,
                "index_build_failed" => ApiErrorCode.IndexBuildFailed,
                "idempotency_conflict" => ApiErrorCode.IdempotencyConflict,
                "operation_not_found" => ApiErrorCode.OperationNotFound,
                "operation_control_forbidden" => ApiErrorCode.OperationControlForbidden,
                "internal" => ApiErrorCode.Internal,
                _ => throw new ProtocolValidationException(
                    $"API error code contains unsupported value '{value}'"),
            };
        }

        private static void ValidateLocation(JsonElement location, string path)
        {
            StrictJson.Properties(location, path, "path", "guid?", "file_id?", "class_id?");
            StrictJson.PortablePath(StrictJson.Required(location, "path", path), path + ".path");
            if (StrictJson.Optional(location, "guid", out JsonElement guid))
            {
                ValidateGuid(guid, path + ".guid");
            }
            ValidateOptionalInt64(location, "file_id", path);
            ValidateOptionalInt32(location, "class_id", path);
        }

        private static void ValidateDiagnostic(JsonElement diagnostic, string path)
        {
            StrictJson.Properties(diagnostic, path, "version", "severity", "code", "message", "address", "field_path");
            if (StrictJson.UInt32(StrictJson.Required(diagnostic, "version", path), path + ".version") != ProtocolConstants.CoreDiagnosticVersion)
            {
                throw new ProtocolValidationException(
                    $"{path}.version must be {ProtocolConstants.CoreDiagnosticVersion}");
            }
            StrictJson.Enum(StrictJson.Required(diagnostic, "severity", path), path + ".severity", "error", "warning", "info");
            string code = StrictJson.String(StrictJson.Required(diagnostic, "code", path), path + ".code", 128, allowEmpty: false);
            if (code.Any(character => !(character >= 'A' && character <= 'Z') && !(character >= '0' && character <= '9') && character != '_'))
            {
                throw new ProtocolValidationException(path + ".code must use ASCII uppercase letters, digits, or underscore");
            }
            StrictJson.String(StrictJson.Required(diagnostic, "message", path), path + ".message", 64 * 1024, allowEmpty: false);
            JsonElement address = StrictJson.Required(diagnostic, "address", path);
            if (address.ValueKind != JsonValueKind.Null)
            {
                ValidateObjectAddress(address, path + ".address");
            }
            JsonElement fieldPath = StrictJson.Required(diagnostic, "field_path", path);
            if (fieldPath.ValueKind != JsonValueKind.Null)
            {
                ValidateFieldPath(fieldPath, path + ".field_path");
            }
        }

        private static void ValidateFieldPath(JsonElement fieldPath, string path)
        {
            JsonElement[] segments = StrictJson.Array(fieldPath, path, 512);
            for (int index = 0; index < segments.Length; index++)
            {
                JsonElement segment = segments[index];
                string segmentPath = $"{path}[{index}]";
                string kind = StrictJson.String(StrictJson.Required(segment, "kind", segmentPath), segmentPath + ".kind");
                if (kind == "field")
                {
                    StrictJson.Properties(segment, segmentPath, "kind", "name");
                    string name = StrictJson.String(StrictJson.Required(segment, "name", segmentPath), segmentPath + ".name", 64 * 1024, allowEmpty: false);
                    if (name.IndexOf('\0') >= 0)
                    {
                        throw new ProtocolValidationException(segmentPath + ".name contains NUL");
                    }
                }
                else if (kind == "index")
                {
                    StrictJson.Properties(segment, segmentPath, "kind", "index");
                    StrictJson.UInt32(StrictJson.Required(segment, "index", segmentPath), segmentPath + ".index");
                }
                else
                {
                    throw new ProtocolValidationException($"{segmentPath}.kind contains unsupported value '{kind}'");
                }
            }
        }

        private static long? ValidateObjectAddress(JsonElement address, string path)
        {
            string kind = StrictJson.String(StrictJson.Required(address, "kind", path), path + ".kind", allowEmpty: false);
            if (kind == "binary_direct" || kind == "binary_bundle_member")
            {
                StrictJson.Properties(address, path, "kind", "version", "source", "path_id");
                RequireVersion(StrictJson.Required(address, "version", path), path + ".version", 1);
                JsonElement source = StrictJson.Required(address, "source", path);
                ValidateSourceLocator(source, path + ".source");
                long pathId = StrictJson.Int64(
                    StrictJson.Required(address, "path_id", path),
                    path + ".path_id");
                if (pathId == 0)
                {
                    throw new ProtocolValidationException(path + ".path_id must not be zero");
                }
                string? lastContainer = LastContainmentKind(source);
                if (kind == "binary_direct" && lastContainer == "bundle")
                {
                    throw new ProtocolValidationException(path + " direct address cannot target a bundle member");
                }
                if (kind == "binary_bundle_member" && lastContainer != "bundle")
                {
                    throw new ProtocolValidationException(path + " bundle address requires a bundle member");
                }
                return pathId;
            }
            if (kind == "yaml")
            {
                StrictJson.Properties(address, path, "kind", "version", "source", "selector");
                RequireVersion(StrictJson.Required(address, "version", path), path + ".version", 2);
                ValidateSourceLocator(StrictJson.Required(address, "source", path), path + ".source");
                return ValidateYamlSelector(
                    StrictJson.Required(address, "selector", path),
                    path + ".selector");
            }
            throw new ProtocolValidationException($"{path}.kind contains unsupported value '{kind}'");
        }

        private static void ValidateSourceLocator(JsonElement locator, string path)
        {
            StrictJson.Properties(locator, path, "version", "outer_path", "members");
            RequireVersion(StrictJson.Required(locator, "version", path), path + ".version", 1);
            StrictJson.PortablePath(
                StrictJson.Required(locator, "outer_path", path),
                path + ".outer_path",
                requireRelative: true,
                maximumUtf8Bytes: 64 * 1024,
                rejectControlCharacters: true);
            JsonElement[] members = StrictJson.Array(StrictJson.Required(locator, "members", path), path + ".members", 64);
            int totalBytes = Encoding.UTF8.GetByteCount(locator.GetProperty("outer_path").GetString()!);
            for (int index = 0; index < members.Length; index++)
            {
                JsonElement step = members[index];
                string stepPath = $"{path}.members[{index}]";
                StrictJson.Properties(step, stepPath, "container", "member");
                StrictJson.Enum(StrictJson.Required(step, "container", stepPath), stepPath + ".container", "archive", "web_file", "bundle", "companion");
                JsonElement member = StrictJson.Required(step, "member", stepPath);
                StrictJson.Properties(member, stepPath + ".member", "name", "same_name_occurrence");
                StrictJson.PortablePath(
                    StrictJson.Required(member, "name", stepPath + ".member"),
                    stepPath + ".member.name",
                    requireRelative: true,
                    maximumUtf8Bytes: 16 * 1024,
                    rejectControlCharacters: true);
                totalBytes = checked(totalBytes + Encoding.UTF8.GetByteCount(member.GetProperty("name").GetString()!));
                StrictJson.UInt32(StrictJson.Required(member, "same_name_occurrence", stepPath + ".member"), stepPath + ".member.same_name_occurrence");
            }
            if (totalBytes > 96 * 1024)
            {
                throw new ProtocolValidationException(path + " exceeds the source locator text limit");
            }
        }

        private static long? ValidateYamlSelector(JsonElement selector, string path)
        {
            string kind = StrictJson.String(StrictJson.Required(selector, "kind", path), path + ".kind", allowEmpty: false);
            if (kind == "file_id")
            {
                StrictJson.Properties(selector, path, "kind", "file_id");
                long fileId = StrictJson.Int64(
                    StrictJson.Required(selector, "file_id", path),
                    path + ".file_id");
                if (fileId == 0)
                {
                    throw new ProtocolValidationException(path + ".file_id must not be zero");
                }
                return fileId;
            }
            if (kind == "unanchored")
            {
                StrictJson.Properties(selector, path, "kind", "document_index");
                StrictJson.UInt32(StrictJson.Required(selector, "document_index", path), path + ".document_index");
                return null;
            }
            throw new ProtocolValidationException($"{path}.kind contains unsupported value '{kind}'");
        }

        private static void ValidatePolicyBinding(JsonElement value, string path, QueryPolicyId expected)
        {
            QueryPolicyId actual = QueryPolicyId.Parse(
                StrictJson.String(StrictJson.Required(value, "query_policy_id", path), path + ".query_policy_id"));
            if (!actual.Equals(expected))
            {
                throw new ProtocolValidationException(path + ".query_policy_id does not match response envelope");
            }
        }

        private static void ValidateDigest(JsonElement element, string path)
        {
            string value = StrictJson.String(element, path);
            StrictJson.FixedHex(value, "blake3-v1:", 64, path, lowercaseOnly: true);
        }

        private static string ComputeReferenceQueryBinding(JsonElement request, string path)
        {
            string direction = StrictJson.Enum(
                StrictJson.Required(request, "direction", path),
                path + ".direction",
                "incoming",
                "outgoing");
            JsonElement selector = StrictJson.Required(request, "selector", path);
            byte[] domain = Encoding.UTF8.GetBytes("unity-asset:reference-query:cursor-binding:v2\0");
            byte[] selectorJson = BootstrapCodec.Write(writer => selector.WriteTo(writer));
            var input = new byte[checked(domain.Length + 1 + selectorJson.Length)];
            Buffer.BlockCopy(domain, 0, input, 0, domain.Length);
            input[domain.Length] = direction == "incoming" ? (byte)0 : (byte)1;
            Buffer.BlockCopy(selectorJson, 0, input, domain.Length + 1, selectorJson.Length);

            byte[] digest;
            using (SHA256 sha256 = SHA256.Create())
            {
                digest = sha256.ComputeHash(input);
            }
            var binding = new StringBuilder("reference-query-v2:", "reference-query-v2:".Length + 64);
            foreach (byte value in digest)
            {
                binding.Append(value.ToString("x2", System.Globalization.CultureInfo.InvariantCulture));
            }
            return binding.ToString();
        }

        private static void ValidateGuid(JsonElement element, string path)
        {
            string value = StrictJson.String(element, path, 32, allowEmpty: false);
            StrictJson.FixedHex(value, string.Empty, 32, path, lowercaseOnly: true);
        }

        private static void RequireVersion(JsonElement element, string path, uint expected)
        {
            if (StrictJson.UInt32(element, path) != expected)
            {
                throw new ProtocolValidationException($"{path} must be {expected}");
            }
        }

        private static void RequireMaximum(uint actual, uint maximum, string path)
        {
            if (actual > maximum)
            {
                throw new ProtocolValidationException($"{path} exceeds {maximum}");
            }
        }

        private static void ValidateOptionalString(JsonElement owner, string name, string path)
        {
            if (StrictJson.Optional(owner, name, out JsonElement value))
            {
                StrictJson.String(value, path + "." + name);
            }
        }

        private static void ValidateOptionalInt64(JsonElement owner, string name, string path)
        {
            ReadOptionalInt64(owner, name, path);
        }

        private static long? ReadOptionalInt64(JsonElement owner, string name, string path)
        {
            return StrictJson.Optional(owner, name, out JsonElement value)
                ? StrictJson.Int64(value, path + "." + name)
                : null;
        }

        private static void ValidateOptionalInt32(JsonElement owner, string name, string path)
        {
            if (StrictJson.Optional(owner, name, out JsonElement value))
            {
                long number = StrictJson.Int64(value, path + "." + name);
                if (number < int.MinValue || number > int.MaxValue)
                {
                    throw new ProtocolValidationException(path + "." + name + " must be a signed 32-bit integer");
                }
            }
        }

        private static void ValidateOptionalUInt32(JsonElement owner, string name, string path)
        {
            if (StrictJson.Optional(owner, name, out JsonElement value))
            {
                StrictJson.UInt32(value, path + "." + name);
            }
        }

        private static void ValidateStringArray(JsonElement array, string path)
        {
            JsonElement[] values = StrictJson.Array(array, path);
            for (int index = 0; index < values.Length; index++)
            {
                StrictJson.String(values[index], $"{path}[{index}]");
            }
        }

        private static void ValidateRanges(JsonElement ranges, string path)
        {
            JsonElement[] values = StrictJson.Array(ranges, path);
            for (int index = 0; index < values.Length; index++)
            {
                JsonElement range = values[index];
                string rangePath = $"{path}[{index}]";
                StrictJson.Properties(range, rangePath, "start", "end");
                StrictJson.UInt32(StrictJson.Required(range, "start", rangePath), rangePath + ".start");
                StrictJson.UInt32(StrictJson.Required(range, "end", rangePath), rangePath + ".end");
            }
        }

        private static string? OptionalReceiptGeneration(JsonElement receipt)
        {
            if (!StrictJson.Optional(receipt, "generation", out JsonElement generation))
            {
                return null;
            }
            return StrictJson.String(
                StrictJson.Required(generation, "generation", "reindex completion generation"),
                "reindex completion generation.generation");
        }

        private static string? OptionalActiveGeneration(JsonElement status)
        {
            JsonElement generationStatus = StrictJson.Required(status, "generation", "reindex status");
            if (!StrictJson.Optional(generationStatus, "active", out JsonElement active))
            {
                return null;
            }
            return StrictJson.String(
                StrictJson.Required(active, "generation", "reindex status active generation"),
                "reindex status active generation.generation");
        }

        private static string? LastContainmentKind(JsonElement locator)
        {
            JsonElement members = locator.GetProperty("members");
            int count = members.GetArrayLength();
            return count == 0 ? null : members[count - 1].GetProperty("container").GetString();
        }

        private static bool JsonEquivalent(JsonElement left, JsonElement right)
        {
            if (left.ValueKind != right.ValueKind)
            {
                return false;
            }
            switch (left.ValueKind)
            {
                case JsonValueKind.Object:
                    JsonProperty[] leftProperties = left.EnumerateObject().ToArray();
                    JsonProperty[] rightProperties = right.EnumerateObject().ToArray();
                    if (leftProperties.Length != rightProperties.Length)
                    {
                        return false;
                    }
                    for (int index = 0; index < leftProperties.Length; index++)
                    {
                        if (!string.Equals(leftProperties[index].Name, rightProperties[index].Name, StringComparison.Ordinal)
                            || !JsonEquivalent(leftProperties[index].Value, rightProperties[index].Value))
                        {
                            return false;
                        }
                    }
                    return true;
                case JsonValueKind.Array:
                    JsonElement[] leftItems = left.EnumerateArray().ToArray();
                    JsonElement[] rightItems = right.EnumerateArray().ToArray();
                    if (leftItems.Length != rightItems.Length)
                    {
                        return false;
                    }
                    for (int index = 0; index < leftItems.Length; index++)
                    {
                        if (!JsonEquivalent(leftItems[index], rightItems[index]))
                        {
                            return false;
                        }
                    }
                    return true;
                case JsonValueKind.String:
                    return string.Equals(left.GetString(), right.GetString(), StringComparison.Ordinal);
                case JsonValueKind.Number:
                    return string.Equals(left.GetRawText(), right.GetRawText(), StringComparison.Ordinal);
                case JsonValueKind.True:
                case JsonValueKind.False:
                case JsonValueKind.Null:
                    return true;
                default:
                    return false;
            }
        }
    }
}
