using System.Text.Json;
using UnityAsset.SearchProtocol.Reference;

namespace UnityAsset.SearchProtocol.ExternalConsumer;

public static class ExternalResponseValueConsumer
{
    public static JsonElement Read(ResponseEnvelopeV1 response)
    {
        return response.Value;
    }
}
