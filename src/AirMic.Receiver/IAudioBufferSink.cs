namespace AirMic.Receiver;

/// <summary>
/// Abstraction interface to decouple the audio rendering sink from native system API libraries.
/// This allows hot-swapping WASAPI (Windows) for CoreAudio (macOS) in the future.
/// </summary>
public interface IAudioBufferSink : IDisposable
{
    /// <summary>
    /// Initializes the audio buffer sink with the specified PCM formatting parameters.
    /// </summary>
    void Initialize(int sampleRate, int bitsPerSample, int channels, string? targetDeviceId = null, bool useExclusiveMode = true);

    /// <summary>
    /// Writes raw PCM frames received from the source to the playback buffer.
    /// </summary>
    void Write(byte[] pcmData);

    /// <summary>
    /// Starts the audio rendering device.
    /// </summary>
    void Start();

    /// <summary>
    /// Configures the dynamic latency/jitter buffer discard thresholds.
    /// </summary>
    void ConfigureBufferThresholds(double maxBacklogSeconds, double targetCushionSeconds);

    /// <summary>
    /// Stops the audio rendering device.
    /// </summary>
    void Stop();
}
