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
        return BusinessCodec.DecodeRequest(
            BusinessCodec.EncodeRequest(request),
            binding);
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
        ProtocolHttpClient client,
        RequestEnvelopeV1 request,
        CancellationToken cancellationToken)
    {
        if (client == null)
        {
            throw new ArgumentNullException(nameof(client));
        }
        ResponseEnvelopeV1 response = await client.ExchangeAsync(request, cancellationToken)
            .ConfigureAwait(false);
        response.ValidateFor(request);
        return response.Value.Clone();
    }

    public static ProtocolHttpClient OpenHttpClient(
        Func<byte[]?> readCurrentCanonicalDescriptor,
        ProjectId expectedProjectId,
        QueryPolicyId expectedQueryPolicyId)
    {
        LoopbackEndpointDescriptor endpoint = LoopbackEndpointDescriptor.ReadFromSource(
            readCurrentCanonicalDescriptor,
            expectedProjectId,
            expectedQueryPolicyId);
        return ProtocolHttpClient.Open(endpoint);
    }

    public static OperationId ParseOperationId(string operationId)
    {
        return OperationId.Parse(operationId);
    }

    public static OperationId ReadReindexOperationId(ResponseEnvelopeV1 response)
    {
        return response.ReadReindexOperationId();
    }

    public static SearchCapabilities ReadSearchCapabilities(ResponseEnvelopeV1 response)
    {
        return response.ReadSearchCapabilities();
    }

    public static IReadOnlyList<BackgroundReindexOperation> ReadBackgroundReindexOperations(
        ResponseEnvelopeV1 response)
    {
        return response.ReadBackgroundReindexOperations();
    }

    public static DaemonProcessFailure? ReadDaemonProcessFailure(ResponseEnvelopeV1 response)
    {
        return response.ReadDaemonProcessFailure();
    }

    public static ApiErrorCode ReadApiErrorCode(ResponseEnvelopeV1 response)
    {
        return response.ReadApiErrorCode();
    }

    private static ResponseEnvelopeV1 RoundTripResponse(
        RequestEnvelopeV1 request,
        ResponseEnvelopeV1 response)
    {
        response.ValidateFor(request);
        ResponseEnvelopeV1 decoded = BusinessCodec.DecodeResponse(BusinessCodec.EncodeResponse(response));
        decoded.ValidateFor(request);
        return decoded;
    }
}
