using System;
using System.Collections.Generic;
using System.Text.Json;

namespace UnityAsset.SearchProtocol.Reference
{
    public static class ProtocolConstants
    {
        public const ushort BusinessProtocolRevision = 5;
        public const uint CoreDiagnosticVersion = 2;
        public const int MaxBackgroundReindexOperations = 5;
    }

    public enum ReindexOperationState
    {
        Queued,
        Coalesced,
        Running,
        Succeeded,
        Failed,
        Cancelled,
        Expired,
        Lost,
    }

    public enum BackgroundReindexOrigin
    {
        Startup,
        Watcher,
        WatcherOverflow,
        Timer,
        SemanticUpgrade,
    }

    public enum DaemonProcessComponent
    {
        ReindexCoordinator,
        FilesystemWatcher,
        ReconcileTimer,
    }

    public sealed class DaemonProcessFailure
    {
        internal DaemonProcessFailure(DaemonProcessComponent component, string cause)
        {
            Component = component;
            Cause = cause ?? throw new ArgumentNullException(nameof(cause));
        }

        public DaemonProcessComponent Component { get; }

        public string Cause { get; }
    }

    public enum ApiErrorCode
    {
        InvalidRequest,
        InvalidCursor,
        StaleCursor,
        IncompatibleProtocol,
        PeerRejected,
        Busy,
        NotReady,
        RevisionMismatch,
        IndexBuildFailed,
        IdempotencyConflict,
        OperationNotFound,
        OperationControlForbidden,
        Internal,
    }

    public sealed class BackgroundReindexOperation
    {
        internal BackgroundReindexOperation(
            BackgroundReindexOrigin origin,
            OperationId operationId,
            ReindexOperationState state)
        {
            Origin = origin;
            OperationId = operationId ?? throw new ArgumentNullException(nameof(operationId));
            State = state;
        }

        public BackgroundReindexOrigin Origin { get; }

        public OperationId OperationId { get; }

        public ReindexOperationState State { get; }
    }

    public sealed class SearchCapabilities
    {
        internal SearchCapabilities(
            ushort protocolRevision,
            bool search,
            bool suggest,
            bool incomingReferences,
            bool outgoingReferences,
            bool filesystemReindex,
            bool reindexLifecycle,
            bool backgroundReindexDiscovery,
            bool gracefulShutdown)
        {
            ProtocolRevision = protocolRevision;
            Search = search;
            Suggest = suggest;
            IncomingReferences = incomingReferences;
            OutgoingReferences = outgoingReferences;
            FilesystemReindex = filesystemReindex;
            ReindexLifecycle = reindexLifecycle;
            BackgroundReindexDiscovery = backgroundReindexDiscovery;
            GracefulShutdown = gracefulShutdown;
        }

        public ushort ProtocolRevision { get; }

        public bool Search { get; }

        public bool Suggest { get; }

        public bool IncomingReferences { get; }

        public bool OutgoingReferences { get; }

        public bool FilesystemReindex { get; }

        public bool ReindexLifecycle { get; }

        public bool BackgroundReindexDiscovery { get; }

        public bool GracefulShutdown { get; }
    }

    public sealed class ProtocolValidationException : Exception
    {
        public ProtocolValidationException(string message)
            : base(message)
        {
        }

        public ProtocolValidationException(string message, Exception innerException)
            : base(message, innerException)
        {
        }
    }

    public sealed class ProtocolBinding
    {
        public ProtocolBinding(
            ushort protocolRevision,
            ProjectId projectId,
            DaemonInstanceId daemonInstanceId,
            QueryPolicyId queryPolicyId)
        {
            if (protocolRevision != ProtocolConstants.BusinessProtocolRevision)
            {
                throw new ProtocolValidationException(
                    $"protocol revision mismatch: expected {ProtocolConstants.BusinessProtocolRevision}, got {protocolRevision}");
            }
            ProtocolRevision = protocolRevision;
            ProjectId = projectId ?? throw new ArgumentNullException(nameof(projectId));
            DaemonInstanceId = daemonInstanceId ?? throw new ArgumentNullException(nameof(daemonInstanceId));
            QueryPolicyId = queryPolicyId ?? throw new ArgumentNullException(nameof(queryPolicyId));
        }

        public ushort ProtocolRevision { get; }

        public ProjectId ProjectId { get; }

        public DaemonInstanceId DaemonInstanceId { get; }

        public QueryPolicyId QueryPolicyId { get; }
    }

    public sealed class RequestEnvelopeV1
    {
        internal RequestEnvelopeV1(
            ushort protocolRevision,
            RequestId requestId,
            ProjectId projectId,
            DaemonInstanceId daemonInstanceId,
            QueryPolicyId queryPolicyId,
            string operationKind,
            JsonElement operation)
        {
            ProtocolRevision = protocolRevision;
            RequestId = requestId;
            ProjectId = projectId;
            DaemonInstanceId = daemonInstanceId;
            QueryPolicyId = queryPolicyId;
            OperationKind = operationKind;
            Operation = operation;
        }

        public ushort ProtocolRevision { get; }

        public RequestId RequestId { get; }

        public ProjectId ProjectId { get; }

        public DaemonInstanceId DaemonInstanceId { get; }

        public QueryPolicyId QueryPolicyId { get; }

        public string OperationKind { get; }

        internal JsonElement Operation { get; }

        public void ValidateBinding(ProtocolBinding binding)
        {
            if (binding == null)
            {
                throw new ArgumentNullException(nameof(binding));
            }
            if (ProtocolRevision != binding.ProtocolRevision)
            {
                throw new ProtocolValidationException(
                    $"protocol revision mismatch: expected {binding.ProtocolRevision}, got {ProtocolRevision}");
            }
            if (!ProjectId.Equals(binding.ProjectId))
            {
                throw new ProtocolValidationException("project binding mismatch");
            }
            if (!DaemonInstanceId.Equals(binding.DaemonInstanceId))
            {
                throw new ProtocolValidationException("daemon instance binding mismatch");
            }
            if (!QueryPolicyId.Equals(binding.QueryPolicyId))
            {
                throw new ProtocolValidationException("query policy binding mismatch");
            }
        }
    }

    public sealed class ResponseEnvelopeV1
    {
        internal ResponseEnvelopeV1(
            ushort protocolRevision,
            RequestId requestId,
            ProjectId projectId,
            DaemonInstanceId daemonInstanceId,
            QueryPolicyId queryPolicyId,
            bool isError,
            string? operationKind,
            JsonElement value,
            int encodedLength)
        {
            ProtocolRevision = protocolRevision;
            RequestId = requestId;
            ProjectId = projectId;
            DaemonInstanceId = daemonInstanceId;
            QueryPolicyId = queryPolicyId;
            IsError = isError;
            OperationKind = operationKind;
            Value = value;
            EncodedLength = encodedLength;
        }

        public ushort ProtocolRevision { get; }

        public RequestId RequestId { get; }

        public ProjectId ProjectId { get; }

        public DaemonInstanceId DaemonInstanceId { get; }

        public QueryPolicyId QueryPolicyId { get; }

        public bool IsError { get; }

        public string? OperationKind { get; }

        /// <summary>
        /// Gets the schema-validated operation result or structured error payload.
        /// </summary>
        public JsonElement Value { get; }

        /// <summary>
        /// Reads the closed capability set from a successful capabilities or status response.
        /// </summary>
        public SearchCapabilities ReadSearchCapabilities()
        {
            if (IsError
                || (!string.Equals(OperationKind, "capabilities", StringComparison.Ordinal)
                    && !string.Equals(OperationKind, "status", StringComparison.Ordinal)))
            {
                throw new ProtocolValidationException(
                    "search capabilities are available only on a successful capabilities or status response");
            }
            JsonElement response = Value.GetProperty("response");
            return ContractValidator.MaterializeSearchCapabilities(response.GetProperty("capabilities"));
        }

        /// <summary>
        /// Reads the bounded background reindex summary from a successful status response.
        /// </summary>
        public IReadOnlyList<BackgroundReindexOperation> ReadBackgroundReindexOperations()
        {
            if (IsError || !string.Equals(OperationKind, "status", StringComparison.Ordinal))
            {
                throw new ProtocolValidationException(
                    "background reindex operations are available only on a successful status response");
            }
            JsonElement response = Value.GetProperty("response");
            JsonElement daemon = response.GetProperty("daemon");
            return ContractValidator.MaterializeBackgroundReindexOperations(
                daemon.GetProperty("background_reindex_operations"));
        }

        /// <summary>
        /// Reads the process-lifetime task failure from a successful status response, if present.
        /// </summary>
        public DaemonProcessFailure? ReadDaemonProcessFailure()
        {
            if (IsError || !string.Equals(OperationKind, "status", StringComparison.Ordinal))
            {
                throw new ProtocolValidationException(
                    "daemon process failure is available only on a successful status response");
            }
            JsonElement failure = Value.GetProperty("response").GetProperty("daemon")
                .GetProperty("process_failure");
            return ContractValidator.MaterializeDaemonProcessFailure(failure);
        }

        /// <summary>
        /// Reads the operation identifier from a successful reindex lifecycle response.
        /// </summary>
        public OperationId ReadReindexOperationId()
        {
            if (IsError || OperationKind is null || !OperationKind.StartsWith("reindex_", StringComparison.Ordinal))
            {
                throw new ProtocolValidationException(
                    "a reindex operation ID is available only on a successful reindex lifecycle response");
            }
            return OperationId.Parse(
                Value.GetProperty("response").GetProperty("operation_id").GetString()!);
        }

        /// <summary>
        /// Reads the closed error code from a structured error response.
        /// </summary>
        public ApiErrorCode ReadApiErrorCode()
        {
            if (!IsError)
            {
                throw new ProtocolValidationException(
                    "an API error code is available only on an error response");
            }
            return ContractValidator.ReadApiErrorCode(
                Value.GetProperty("code"),
                "response envelope.value.code");
        }

        internal int EncodedLength { get; }

        public void ValidateFor(RequestEnvelopeV1 request)
        {
            if (request == null)
            {
                throw new ArgumentNullException(nameof(request));
            }
            if (ProtocolRevision != request.ProtocolRevision)
            {
                throw new ProtocolValidationException("response protocol revision does not match request");
            }
            if (!RequestId.Equals(request.RequestId))
            {
                throw new ProtocolValidationException("response request ID does not match request");
            }
            if (!ProjectId.Equals(request.ProjectId))
            {
                throw new ProtocolValidationException("response project does not match request");
            }
            if (!DaemonInstanceId.Equals(request.DaemonInstanceId))
            {
                throw new ProtocolValidationException("response daemon instance does not match request");
            }
            if (!QueryPolicyId.Equals(request.QueryPolicyId))
            {
                throw new ProtocolValidationException("response query policy does not match request");
            }
            if (!IsError && !string.Equals(OperationKind, request.OperationKind, StringComparison.Ordinal))
            {
                throw new ProtocolValidationException("response operation kind does not match request");
            }
            int maximum = ProtocolLimits.ForResponse(request.OperationKind);
            if (EncodedLength > maximum)
            {
                throw new ProtocolValidationException(
                    $"{request.OperationKind} response contains {EncodedLength} encoded bytes; maximum is {maximum}");
            }

            ContractValidator.ValidateResponseForRequest(this, request);
        }
    }
}
