using System.Text.Json;
using UnityAsset.SearchProtocol.Reference;

namespace UnityAsset.SearchProtocol.ExternalConsumer;

public static class ExternalProtocolConsumer
{
    public static ProtocolBinding CreateBinding(
        string projectId,
        string daemonInstanceId,
        string queryPolicyId)
    {
        return new ProtocolBinding(
            ProtocolConstants.BusinessProtocolRevision,
            ProjectId.Parse(projectId),
            DaemonInstanceId.Parse(daemonInstanceId),
            QueryPolicyId.Parse(queryPolicyId));
    }

    public static BootstrapReplyV2 RoundTripBootstrap(ProtocolBinding binding)
    {
        var hello = new BootstrapHelloV2(
            ProtocolConstants.BootstrapVersion,
            binding.ProjectId,
            binding.DaemonInstanceId,
            new[] { ProtocolConstants.BusinessProtocolRevision });
        BootstrapHelloV2 decodedHello = FrameCodec.DecodeBootstrapHello(
            FrameCodec.EncodeBootstrapHello(
                BootstrapCodec.DecodeHello(BootstrapCodec.EncodeHello(hello))));
        BootstrapReplyV2 reply = BootstrapNegotiator.Negotiate(
            decodedHello,
            binding.ProjectId,
            binding.DaemonInstanceId,
            binding.QueryPolicyId,
            new[] { ProtocolConstants.BusinessProtocolRevision });
        return FrameCodec.DecodeBootstrapReply(
            FrameCodec.EncodeBootstrapReply(
                BootstrapCodec.DecodeReply(BootstrapCodec.EncodeReply(reply))));
    }

    public static RequestEnvelopeV1 RoundTripRequest(
        ProtocolBinding binding,
        string requestId,
        string operationKind,
        byte[] requestPayloadJson)
    {
        RequestEnvelopeV1 request = BusinessCodec.CreateRequest(
            binding,
            RequestId.Parse(requestId),
            operationKind,
            requestPayloadJson);
        RequestEnvelopeV1 decoded = BusinessCodec.DecodeRequest(
            BusinessCodec.EncodeRequest(request),
            binding);
        return FrameCodec.DecodeRequest(FrameCodec.EncodeRequest(decoded), binding);
    }

    public static ResponseEnvelopeV1 RoundTripSuccess(
        RequestEnvelopeV1 request,
        byte[] responsePayloadJson)
    {
        return RoundTripResponse(
            request,
            BusinessCodec.CreateSuccessResponse(request, responsePayloadJson));
    }

    public static ResponseEnvelopeV1 RoundTripError(
        RequestEnvelopeV1 request,
        byte[] errorPayloadJson)
    {
        return RoundTripResponse(
            request,
            BusinessCodec.CreateErrorResponse(request, errorPayloadJson));
    }

    public static async Task<JsonElement> ExchangeAsync(
        IProtocolTransportAdapter transport,
        ProtocolBinding expectedBinding,
        RequestEnvelopeV1 request,
        CancellationToken cancellationToken)
    {
        using ProtocolSession session = await ProtocolSession.ConnectAsync(
            transport,
            expectedBinding.ProjectId,
            expectedBinding.DaemonInstanceId,
            cancellationToken).ConfigureAwait(false);
        ResponseEnvelopeV1 response = await session.ExchangeAsync(request, cancellationToken)
            .ConfigureAwait(false);
        response.ValidateFor(request);
        return response.Value.Clone();
    }

    public static OperationId ParseOperationId(string operationId)
    {
        return OperationId.Parse(operationId);
    }

    public static string ReadBootstrapRejectionCode(ProtocolBootstrapRejectedException error)
    {
        return error.Code;
    }

    private static ResponseEnvelopeV1 RoundTripResponse(
        RequestEnvelopeV1 request,
        ResponseEnvelopeV1 response)
    {
        response.ValidateFor(request);
        ResponseEnvelopeV1 decoded = BusinessCodec.DecodeResponse(
            BusinessCodec.EncodeResponse(response));
        return FrameCodec.DecodeResponse(FrameCodec.EncodeResponse(decoded, request), request);
    }
}

public sealed class DelegatingTransportAdapter : IProtocolTransportAdapter
{
    private readonly Func<CancellationToken, Task<Stream>> connect;

    public DelegatingTransportAdapter(Func<CancellationToken, Task<Stream>> connect)
    {
        this.connect = connect ?? throw new ArgumentNullException(nameof(connect));
    }

    public Task<Stream> ConnectAsync(CancellationToken cancellationToken)
    {
        return connect(cancellationToken);
    }
}
