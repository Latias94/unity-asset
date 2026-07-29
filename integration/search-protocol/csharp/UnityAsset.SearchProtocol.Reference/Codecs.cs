using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text.Json;

namespace UnityAsset.SearchProtocol.Reference
{
    public static class BootstrapCodec
    {
        private static readonly string[] RejectionCodes =
        {
            "project_mismatch",
            "instance_mismatch",
            "no_common_revision",
        };

        public static BootstrapHelloV1 DecodeHello(byte[] utf8Json)
        {
            StrictJson.RequireEncodedLimit(utf8Json, FrameLimits.BootstrapMaxEncodedBytes, "bootstrap hello");
            JsonElement root = StrictJson.ParseObject(utf8Json, "bootstrap hello");
            StrictJson.Properties(
                root,
                "bootstrap hello",
                "bootstrap_version",
                "project_id",
                "daemon_instance_id",
                "supported_revisions");
            ushort version = StrictJson.UInt16(
                StrictJson.Required(root, "bootstrap_version", "bootstrap hello"),
                "bootstrap hello.bootstrap_version");
            RequireBootstrapVersion(version, "bootstrap hello.bootstrap_version");
            ProjectId projectId = ProjectId.Parse(
                StrictJson.String(StrictJson.Required(root, "project_id", "bootstrap hello"), "bootstrap hello.project_id"));
            DaemonInstanceId instanceId = DaemonInstanceId.Parse(
                StrictJson.String(
                    StrictJson.Required(root, "daemon_instance_id", "bootstrap hello"),
                    "bootstrap hello.daemon_instance_id"));
            JsonElement[] revisions = StrictJson.Array(
                StrictJson.Required(root, "supported_revisions", "bootstrap hello"),
                "bootstrap hello.supported_revisions",
                maximum: 16,
                allowEmpty: false);
            var values = new ushort[revisions.Length];
            ushort previous = 0;
            for (int index = 0; index < revisions.Length; index++)
            {
                ushort revision = StrictJson.UInt16(revisions[index], $"bootstrap hello.supported_revisions[{index}]");
                if (revision == 0 || (index > 0 && revision <= previous))
                {
                    throw new ProtocolValidationException("bootstrap hello.supported_revisions must be non-zero and strictly increasing");
                }
                values[index] = revision;
                previous = revision;
            }
            return new BootstrapHelloV1(version, projectId, instanceId, values);
        }

        public static byte[] EncodeHello(BootstrapHelloV1 hello)
        {
            ValidateHello(hello);
            return Write(writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("bootstrap_version", hello.BootstrapVersion);
                writer.WriteString("project_id", hello.ProjectId.Value);
                writer.WriteString("daemon_instance_id", hello.DaemonInstanceId.Value);
                writer.WritePropertyName("supported_revisions");
                writer.WriteStartArray();
                foreach (ushort revision in hello.SupportedRevisions)
                {
                    writer.WriteNumberValue(revision);
                }
                writer.WriteEndArray();
                writer.WriteEndObject();
            });
        }

        public static BootstrapReplyV1 DecodeReply(byte[] utf8Json)
        {
            StrictJson.RequireEncodedLimit(utf8Json, FrameLimits.BootstrapMaxEncodedBytes, "bootstrap reply");
            JsonElement root = StrictJson.ParseObject(utf8Json, "bootstrap reply");
            string result = StrictJson.String(
                StrictJson.Required(root, "result", "bootstrap reply"),
                "bootstrap reply.result",
                allowEmpty: false);
            if (result == "accepted")
            {
                StrictJson.Properties(
                    root,
                    "bootstrap reply",
                    "result",
                    "bootstrap_version",
                    "project_id",
                    "daemon_instance_id",
                    "selected_revision");
                ushort version = StrictJson.UInt16(root.GetProperty("bootstrap_version"), "bootstrap reply.bootstrap_version");
                RequireBootstrapVersion(version, "bootstrap reply.bootstrap_version");
                ProjectId projectId = ProjectId.Parse(StrictJson.String(root.GetProperty("project_id"), "bootstrap reply.project_id"));
                DaemonInstanceId instanceId = DaemonInstanceId.Parse(
                    StrictJson.String(root.GetProperty("daemon_instance_id"), "bootstrap reply.daemon_instance_id"));
                ushort revision = StrictJson.UInt16(root.GetProperty("selected_revision"), "bootstrap reply.selected_revision");
                if (revision == 0)
                {
                    throw new ProtocolValidationException("bootstrap reply.selected_revision must not be zero");
                }
                return new BootstrapAcceptedV1(version, projectId, instanceId, revision);
            }
            if (result == "rejected")
            {
                StrictJson.Properties(root, "bootstrap reply", "result", "bootstrap_version", "code");
                ushort version = StrictJson.UInt16(root.GetProperty("bootstrap_version"), "bootstrap reply.bootstrap_version");
                RequireBootstrapVersion(version, "bootstrap reply.bootstrap_version");
                string code = StrictJson.Enum(root.GetProperty("code"), "bootstrap reply.code", RejectionCodes);
                return new BootstrapRejectedV1(version, code);
            }
            throw new ProtocolValidationException($"bootstrap reply.result contains unsupported value '{result}'");
        }

        public static byte[] EncodeReply(BootstrapReplyV1 reply)
        {
            if (reply == null)
            {
                throw new ArgumentNullException(nameof(reply));
            }
            RequireBootstrapVersion(reply.BootstrapVersion, "bootstrap reply.bootstrap_version");
            return Write(writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("result", reply.Result);
                writer.WriteNumber("bootstrap_version", reply.BootstrapVersion);
                if (reply is BootstrapAcceptedV1 accepted)
                {
                    if (accepted.SelectedRevision == 0)
                    {
                        throw new ProtocolValidationException("bootstrap reply.selected_revision must not be zero");
                    }
                    writer.WriteString("project_id", accepted.ProjectId.Value);
                    writer.WriteString("daemon_instance_id", accepted.DaemonInstanceId.Value);
                    writer.WriteNumber("selected_revision", accepted.SelectedRevision);
                }
                else if (reply is BootstrapRejectedV1 rejected)
                {
                    if (!RejectionCodes.Contains(rejected.Code, StringComparer.Ordinal))
                    {
                        throw new ProtocolValidationException($"bootstrap reply.code contains unsupported value '{rejected.Code}'");
                    }
                    writer.WriteString("code", rejected.Code);
                }
                else
                {
                    throw new ProtocolValidationException("bootstrap reply has an unsupported runtime type");
                }
                writer.WriteEndObject();
            });
        }

        public static void ValidateReplyFor(BootstrapReplyV1 reply, BootstrapHelloV1 hello)
        {
            if (reply == null)
            {
                throw new ArgumentNullException(nameof(reply));
            }
            if (hello == null)
            {
                throw new ArgumentNullException(nameof(hello));
            }
            DecodeHello(EncodeHello(hello));
            DecodeReply(EncodeReply(reply));
            if (reply.BootstrapVersion != hello.BootstrapVersion)
            {
                throw new ProtocolValidationException("bootstrap reply version does not match hello");
            }
            if (reply is not BootstrapAcceptedV1 accepted)
            {
                return;
            }
            if (!accepted.ProjectId.Equals(hello.ProjectId))
            {
                throw new ProtocolValidationException("bootstrap accepted project does not match hello");
            }
            if (!accepted.DaemonInstanceId.Equals(hello.DaemonInstanceId))
            {
                throw new ProtocolValidationException("bootstrap accepted daemon instance does not match hello");
            }
            if (!hello.SupportedRevisions.Contains(accepted.SelectedRevision))
            {
                throw new ProtocolValidationException("bootstrap selected revision was not offered by hello");
            }
        }

        private static void ValidateHello(BootstrapHelloV1 hello)
        {
            if (hello == null)
            {
                throw new ArgumentNullException(nameof(hello));
            }
            RequireBootstrapVersion(hello.BootstrapVersion, "bootstrap hello.bootstrap_version");
            if (hello.SupportedRevisions.Count == 0 || hello.SupportedRevisions.Count > 16)
            {
                throw new ProtocolValidationException("bootstrap hello.supported_revisions must contain 1..=16 entries");
            }
            ushort previous = 0;
            for (int index = 0; index < hello.SupportedRevisions.Count; index++)
            {
                ushort revision = hello.SupportedRevisions[index];
                if (revision == 0 || (index > 0 && revision <= previous))
                {
                    throw new ProtocolValidationException("bootstrap hello.supported_revisions must be non-zero and strictly increasing");
                }
                previous = revision;
            }
        }

        private static void RequireBootstrapVersion(ushort version, string path)
        {
            if (version != ProtocolConstants.BootstrapVersion)
            {
                throw new ProtocolValidationException(
                    $"{path} mismatch: expected {ProtocolConstants.BootstrapVersion}, got {version}");
            }
        }

        internal static byte[] Write(Action<Utf8JsonWriter> write)
        {
            using var stream = new MemoryStream();
            using (var writer = new Utf8JsonWriter(stream, StrictJson.WriterOptions))
            {
                write(writer);
                writer.Flush();
            }
            return stream.ToArray();
        }

    }

    public static class BootstrapNegotiator
    {
        public static BootstrapReplyV1 Negotiate(
            BootstrapHelloV1 hello,
            ProjectId expectedProjectId,
            DaemonInstanceId expectedDaemonInstanceId,
            IReadOnlyCollection<ushort> localRevisions)
        {
            if (hello == null)
            {
                throw new ArgumentNullException(nameof(hello));
            }
            if (expectedProjectId == null)
            {
                throw new ArgumentNullException(nameof(expectedProjectId));
            }
            if (expectedDaemonInstanceId == null)
            {
                throw new ArgumentNullException(nameof(expectedDaemonInstanceId));
            }
            if (localRevisions == null)
            {
                throw new ArgumentNullException(nameof(localRevisions));
            }

            BootstrapCodec.DecodeHello(BootstrapCodec.EncodeHello(hello));
            if (!hello.ProjectId.Equals(expectedProjectId))
            {
                return new BootstrapRejectedV1(ProtocolConstants.BootstrapVersion, "project_mismatch");
            }
            if (!hello.DaemonInstanceId.Equals(expectedDaemonInstanceId))
            {
                return new BootstrapRejectedV1(ProtocolConstants.BootstrapVersion, "instance_mismatch");
            }

            ushort selected = 0;
            var local = new HashSet<ushort>(localRevisions);
            foreach (ushort revision in hello.SupportedRevisions)
            {
                if (local.Contains(revision) && revision > selected)
                {
                    selected = revision;
                }
            }
            if (selected == 0)
            {
                return new BootstrapRejectedV1(ProtocolConstants.BootstrapVersion, "no_common_revision");
            }
            return new BootstrapAcceptedV1(
                ProtocolConstants.BootstrapVersion,
                hello.ProjectId,
                hello.DaemonInstanceId,
                selected);
        }
    }

    public static class BusinessCodec
    {
        public static RequestEnvelopeV1 DecodeRequest(byte[] utf8Json)
        {
            StrictJson.RequireEncodedLimit(utf8Json, FrameLimits.RequestEnvelopeMaxEncodedBytes, "request envelope");
            JsonElement root = StrictJson.ParseObject(utf8Json, "request envelope");
            string operationKind = ContractValidator.ValidateRequest(root);
            StrictJson.RequireEncodedLimit(utf8Json, FrameLimits.ForRequest(operationKind), operationKind + " request");
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
            return BootstrapCodec.Write(writer =>
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
            byte[] envelope = BootstrapCodec.Write(writer =>
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
            StrictJson.RequireEncodedLimit(utf8Json, 16 * 1024 * 1024, "response envelope");
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
            return BootstrapCodec.Write(writer =>
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
            byte[] envelope = BootstrapCodec.Write(writer =>
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
