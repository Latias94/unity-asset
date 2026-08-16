using System;
using System.IO;
using System.Text.Json;

namespace UnityAsset.SearchProtocol.Reference
{
    internal static class CanonicalJson
    {
        internal static byte[] Write(Action<Utf8JsonWriter> write)
        {
            using var stream = new MemoryStream();
            using (var writer = new Utf8JsonWriter(stream, StrictJson.WriterOptions))
            {
                write(writer);
                writer.Flush();
            }
            return StrictJson.CanonicalizeContractWriterOutput(stream.ToArray());
        }
    }

    public static class BusinessCodec
    {
        public static RequestEnvelopeV1 DecodeRequest(byte[] utf8Json)
        {
            StrictJson.RequireEncodedLimit(utf8Json, ProtocolLimits.RequestEnvelopeMaxEncodedBytes, "request envelope");
            JsonElement root = StrictJson.ParseObject(utf8Json, "request envelope");
            string operationKind = ContractValidator.ValidateRequest(root);
            StrictJson.RequireEncodedLimit(utf8Json, ProtocolLimits.ForRequest(operationKind), operationKind + " request");
            return new RequestEnvelopeV1(
                StrictJson.UInt16(root.GetProperty("protocol_revision"), "request envelope.protocol_revision"),
                RequestId.Parse(root.GetProperty("request_id").GetString()!),
                ProjectId.Parse(root.GetProperty("project_id").GetString()!),
                DaemonInstanceId.Parse(root.GetProperty("daemon_instance_id").GetString()!),
                QueryPolicyId.Parse(root.GetProperty("query_policy_id").GetString()!),
                operationKind,
                root.GetProperty("operation").Clone());
        }

        public static RequestEnvelopeV1 DecodeRequest(byte[] utf8Json, ProtocolBinding binding)
        {
            RequestEnvelopeV1 request = DecodeRequest(utf8Json);
            request.ValidateBinding(binding);
            return request;
        }

        public static byte[] EncodeRequest(RequestEnvelopeV1 request)
        {
            if (request == null)
            {
                throw new ArgumentNullException(nameof(request));
            }
            return CanonicalJson.Write(writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("protocol_revision", request.ProtocolRevision);
                writer.WriteString("request_id", request.RequestId.Value);
                writer.WriteString("project_id", request.ProjectId.Value);
                writer.WriteString("daemon_instance_id", request.DaemonInstanceId.Value);
                writer.WriteString("query_policy_id", request.QueryPolicyId.Value);
                writer.WritePropertyName("operation");
                request.Operation.WriteTo(writer);
                writer.WriteEndObject();
            });
        }

        public static RequestEnvelopeV1 CreateRequest(
            ProtocolBinding binding,
            RequestId requestId,
            string operationKind,
            byte[] requestPayloadJson)
        {
            if (binding == null)
            {
                throw new ArgumentNullException(nameof(binding));
            }
            if (requestId == null)
            {
                throw new ArgumentNullException(nameof(requestId));
            }
            JsonElement payload = StrictJson.ParseObject(requestPayloadJson, "request payload");
            byte[] envelope = CanonicalJson.Write(writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("protocol_revision", binding.ProtocolRevision);
                writer.WriteString("request_id", requestId.Value);
                writer.WriteString("project_id", binding.ProjectId.Value);
                writer.WriteString("daemon_instance_id", binding.DaemonInstanceId.Value);
                writer.WriteString("query_policy_id", binding.QueryPolicyId.Value);
                writer.WritePropertyName("operation");
                writer.WriteStartObject();
                writer.WriteString("kind", operationKind);
                writer.WritePropertyName("request");
                payload.WriteTo(writer);
                writer.WriteEndObject();
                writer.WriteEndObject();
            });
            return DecodeRequest(envelope, binding);
        }

        public static ResponseEnvelopeV1 DecodeResponse(byte[] utf8Json)
        {
            StrictJson.RequireEncodedLimit(utf8Json, ProtocolLimits.ResponseEnvelopeMaxEncodedBytes, "response envelope");
            JsonElement root = StrictJson.ParseObject(utf8Json, "response envelope");
            string? operationKind = ContractValidator.ValidateResponse(root, out bool isError);
            return new ResponseEnvelopeV1(
                StrictJson.UInt16(root.GetProperty("protocol_revision"), "response envelope.protocol_revision"),
                RequestId.Parse(root.GetProperty("request_id").GetString()!),
                ProjectId.Parse(root.GetProperty("project_id").GetString()!),
                DaemonInstanceId.Parse(root.GetProperty("daemon_instance_id").GetString()!),
                QueryPolicyId.Parse(root.GetProperty("query_policy_id").GetString()!),
                isError,
                operationKind,
                root.GetProperty("value").Clone(),
                utf8Json.Length);
        }

        public static byte[] EncodeResponse(ResponseEnvelopeV1 response)
        {
            if (response == null)
            {
                throw new ArgumentNullException(nameof(response));
            }
            return CanonicalJson.Write(writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("protocol_revision", response.ProtocolRevision);
                writer.WriteString("request_id", response.RequestId.Value);
                writer.WriteString("project_id", response.ProjectId.Value);
                writer.WriteString("daemon_instance_id", response.DaemonInstanceId.Value);
                writer.WriteString("query_policy_id", response.QueryPolicyId.Value);
                writer.WriteString("outcome", response.IsError ? "error" : "success");
                writer.WritePropertyName("value");
                response.Value.WriteTo(writer);
                writer.WriteEndObject();
            });
        }

        public static ResponseEnvelopeV1 CreateSuccessResponse(RequestEnvelopeV1 request, byte[] responsePayloadJson)
        {
            return CreateResponse(request, responsePayloadJson, isError: false);
        }

        public static ResponseEnvelopeV1 CreateErrorResponse(RequestEnvelopeV1 request, byte[] errorPayloadJson)
        {
            return CreateResponse(request, errorPayloadJson, isError: true);
        }

        private static ResponseEnvelopeV1 CreateResponse(RequestEnvelopeV1 request, byte[] payloadJson, bool isError)
        {
            if (request == null)
            {
                throw new ArgumentNullException(nameof(request));
            }
            JsonElement payload = StrictJson.ParseObject(payloadJson, isError ? "error payload" : "response payload");
            byte[] envelope = CanonicalJson.Write(writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("protocol_revision", request.ProtocolRevision);
                writer.WriteString("request_id", request.RequestId.Value);
                writer.WriteString("project_id", request.ProjectId.Value);
                writer.WriteString("daemon_instance_id", request.DaemonInstanceId.Value);
                writer.WriteString("query_policy_id", request.QueryPolicyId.Value);
                writer.WriteString("outcome", isError ? "error" : "success");
                writer.WritePropertyName("value");
                if (isError)
                {
                    payload.WriteTo(writer);
                }
                else
                {
                    writer.WriteStartObject();
                    writer.WriteString("kind", request.OperationKind);
                    writer.WritePropertyName("response");
                    payload.WriteTo(writer);
                    writer.WriteEndObject();
                }
                writer.WriteEndObject();
            });
            ResponseEnvelopeV1 response = DecodeResponse(envelope);
            response.ValidateFor(request);
            return response;
        }

    }
}
