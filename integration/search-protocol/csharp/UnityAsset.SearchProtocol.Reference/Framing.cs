using System;

namespace UnityAsset.SearchProtocol.Reference
{
    public static class FrameLimits
    {
        public const int BootstrapMaxEncodedBytes = 16 * 1024;
        public const int RequestEnvelopeMaxEncodedBytes = 512 * 1024;

        public static int ForRequest(string operationKind)
        {
            switch (operationKind)
            {
                case "reindex_admit":
                    return 512 * 1024;
                case "references":
                    return 64 * 1024;
                case "capabilities":
                case "status":
                case "search":
                case "suggest":
                case "reindex_status":
                case "reindex_wait":
                case "reindex_cancel":
                case "shutdown":
                    return 16 * 1024;
                default:
                    throw new ProtocolValidationException($"unknown request operation '{operationKind}'");
            }
        }

        public static int ForResponse(string operationKind)
        {
            switch (operationKind)
            {
                case "search":
                case "references":
                case "reindex_admit":
                case "reindex_status":
                case "reindex_wait":
                    return 16 * 1024 * 1024;
                case "capabilities":
                case "status":
                case "suggest":
                    return 256 * 1024;
                case "reindex_cancel":
                case "shutdown":
                    return 16 * 1024;
                default:
                    throw new ProtocolValidationException($"unknown response operation '{operationKind}'");
            }
        }
    }

    public static class FrameCodec
    {
        public static byte[] Encode(byte[] payload, int maximumEncodedBytes)
        {
            if (payload == null)
            {
                throw new ArgumentNullException(nameof(payload));
            }
            ValidateMaximum(maximumEncodedBytes);
            if (payload.Length > maximumEncodedBytes)
            {
                throw new ProtocolValidationException(
                    $"frame contains {payload.Length} encoded bytes; maximum is {maximumEncodedBytes}");
            }

            var frame = new byte[checked(payload.Length + 4)];
            uint length = checked((uint)payload.Length);
            frame[0] = (byte)(length >> 24);
            frame[1] = (byte)(length >> 16);
            frame[2] = (byte)(length >> 8);
            frame[3] = (byte)length;
            Buffer.BlockCopy(payload, 0, frame, 4, payload.Length);
            return frame;
        }

        public static byte[] Decode(byte[] frame, int maximumEncodedBytes)
        {
            if (frame == null)
            {
                throw new ArgumentNullException(nameof(frame));
            }
            ValidateMaximum(maximumEncodedBytes);
            if (frame.Length < 4)
            {
                throw new ProtocolValidationException("frame header is truncated");
            }
            uint declared = ReadLength(frame);
            if (declared > maximumEncodedBytes)
            {
                throw new ProtocolValidationException(
                    $"frame declares {declared} encoded bytes; maximum is {maximumEncodedBytes}");
            }
            int actual = frame.Length - 4;
            if (declared != actual)
            {
                throw new ProtocolValidationException(
                    $"frame declares {declared} encoded bytes but contains {actual}");
            }
            var payload = new byte[actual];
            Buffer.BlockCopy(frame, 4, payload, 0, actual);
            return payload;
        }

        public static byte[] EncodeBootstrapHello(BootstrapHelloV1 hello)
        {
            return Encode(BootstrapCodec.EncodeHello(hello), FrameLimits.BootstrapMaxEncodedBytes);
        }

        public static BootstrapHelloV1 DecodeBootstrapHello(byte[] frame)
        {
            return BootstrapCodec.DecodeHello(Decode(frame, FrameLimits.BootstrapMaxEncodedBytes));
        }

        public static byte[] EncodeBootstrapReply(BootstrapReplyV1 reply)
        {
            return Encode(BootstrapCodec.EncodeReply(reply), FrameLimits.BootstrapMaxEncodedBytes);
        }

        public static BootstrapReplyV1 DecodeBootstrapReply(byte[] frame)
        {
            return BootstrapCodec.DecodeReply(Decode(frame, FrameLimits.BootstrapMaxEncodedBytes));
        }

        public static byte[] EncodeRequest(RequestEnvelopeV1 request)
        {
            return Encode(BusinessCodec.EncodeRequest(request), FrameLimits.ForRequest(request.OperationKind));
        }

        public static RequestEnvelopeV1 DecodeRequest(byte[] frame, ProtocolBinding binding)
        {
            byte[] payload = Decode(frame, FrameLimits.RequestEnvelopeMaxEncodedBytes);
            return BusinessCodec.DecodeRequest(payload, binding);
        }

        public static byte[] EncodeResponse(ResponseEnvelopeV1 response, RequestEnvelopeV1 request)
        {
            response.ValidateFor(request);
            return Encode(BusinessCodec.EncodeResponse(response), FrameLimits.ForResponse(request.OperationKind));
        }

        public static ResponseEnvelopeV1 DecodeResponse(byte[] frame, RequestEnvelopeV1 request)
        {
            ResponseEnvelopeV1 response = BusinessCodec.DecodeResponse(
                Decode(frame, FrameLimits.ForResponse(request.OperationKind)));
            response.ValidateFor(request);
            return response;
        }

        internal static uint ReadLength(byte[] header)
        {
            return ((uint)header[0] << 24)
                | ((uint)header[1] << 16)
                | ((uint)header[2] << 8)
                | header[3];
        }

        internal static void ValidateMaximum(int maximumEncodedBytes)
        {
            if (maximumEncodedBytes < 0)
            {
                throw new ArgumentOutOfRangeException(nameof(maximumEncodedBytes));
            }
        }
    }
}
