using System;
using System.Collections.Generic;
using System.Text.Json;

namespace UnityAsset.SearchProtocol.Reference
{
    public static class ProtocolConstants
    {
        public const ushort BootstrapVersion = 1;
        public const ushort BusinessProtocolRevision = 1;
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

    public sealed class BootstrapHelloV1
    {
        public BootstrapHelloV1(
            ushort bootstrapVersion,
            ProjectId projectId,
            DaemonInstanceId daemonInstanceId,
            IReadOnlyList<ushort> supportedRevisions)
        {
            BootstrapVersion = bootstrapVersion;
            ProjectId = projectId ?? throw new ArgumentNullException(nameof(projectId));
            DaemonInstanceId = daemonInstanceId ?? throw new ArgumentNullException(nameof(daemonInstanceId));
            SupportedRevisions = supportedRevisions ?? throw new ArgumentNullException(nameof(supportedRevisions));
        }

        public ushort BootstrapVersion { get; }

        public ProjectId ProjectId { get; }

        public DaemonInstanceId DaemonInstanceId { get; }

        public IReadOnlyList<ushort> SupportedRevisions { get; }
    }

    public abstract class BootstrapReplyV1
    {
        protected BootstrapReplyV1(string result, ushort bootstrapVersion)
        {
            Result = result;
            BootstrapVersion = bootstrapVersion;
        }

        public string Result { get; }

        public ushort BootstrapVersion { get; }
    }

    public sealed class BootstrapAcceptedV1 : BootstrapReplyV1
    {
        public BootstrapAcceptedV1(
            ushort bootstrapVersion,
            ProjectId projectId,
            DaemonInstanceId daemonInstanceId,
            ushort selectedRevision)
            : base("accepted", bootstrapVersion)
        {
            ProjectId = projectId ?? throw new ArgumentNullException(nameof(projectId));
            DaemonInstanceId = daemonInstanceId ?? throw new ArgumentNullException(nameof(daemonInstanceId));
            SelectedRevision = selectedRevision;
        }

        public ProjectId ProjectId { get; }

        public DaemonInstanceId DaemonInstanceId { get; }

        public ushort SelectedRevision { get; }
    }

    public sealed class BootstrapRejectedV1 : BootstrapReplyV1
    {
        public BootstrapRejectedV1(ushort bootstrapVersion, string code)
            : base("rejected", bootstrapVersion)
        {
            Code = code ?? throw new ArgumentNullException(nameof(code));
        }

        public string Code { get; }
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

        internal JsonElement Value { get; }

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
            int maximum = FrameLimits.ForResponse(request.OperationKind);
            if (EncodedLength > maximum)
            {
                throw new ProtocolValidationException(
                    $"{request.OperationKind} response contains {EncodedLength} encoded bytes; maximum is {maximum}");
            }

            ContractValidator.ValidateResponseForRequest(this, request);
        }
    }
}
