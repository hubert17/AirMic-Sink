namespace AirMic.Contracts;

/// <summary>
/// Container for WebRTC signaling messages sent over the WebSocket hub.
/// </summary>
public class SignalingMessage
{
    /// <summary>
    /// Message type: "offer", "answer", or "candidate".
    /// </summary>
    public string? Type { get; set; }

    /// <summary>
    /// Session Description Protocol (SDP) string for "offer" and "answer" types.
    /// </summary>
    public string? Sdp { get; set; }

    /// <summary>
    /// Interactive Connectivity Establishment (ICE) candidate payload for "candidate" types.
    /// </summary>
    public IceCandidatePayload? Candidate { get; set; }
}

/// <summary>
/// Represents a WebRTC ICE candidate payload.
/// </summary>
public class IceCandidatePayload
{
    /// <summary>
    /// The candidate string (e.g. "candidate:842163049 1 udp 16777215 ...").
    /// </summary>
    public string? Candidate { get; set; }

    /// <summary>
    /// If present, the identifier of the "media stream component" in the template.
    /// </summary>
    public string? SdpMid { get; set; }

    /// <summary>
    /// If present, the index (starting at zero) of the media description in the SDP.
    /// </summary>
    public int? SdpMLineIndex { get; set; }
}
