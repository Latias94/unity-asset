namespace UnityAsset.SearchProtocol.Reference
{
    /// <summary>
    /// Defines the encoded JSON limits shared by the business codec and loopback HTTP client.
    /// </summary>
    public static class ProtocolLimits
    {
        public const int RequestEnvelopeMaxEncodedBytes = 512 * 1024;
        public const int ResponseEnvelopeMaxEncodedBytes = 16 * 1024 * 1024;

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
                    return ResponseEnvelopeMaxEncodedBytes;
                case "capabilities":
                case "status":
                case "suggest":
                case "reindex_cancel":
                case "shutdown":
                    return 256 * 1024;
                default:
                    throw new ProtocolValidationException($"unknown response operation '{operationKind}'");
            }
        }
    }
}
