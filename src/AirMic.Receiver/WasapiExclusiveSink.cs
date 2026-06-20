using System.Runtime.Versioning;
using NAudio.CoreAudioApi;
using NAudio.Wave;

namespace AirMic.Receiver;

/// <summary>
/// Windows-specific audio sink that targets Virtual Audio Cable using WASAPI Exclusive Mode
/// to minimize audio buffer delays and bypass the Windows audio session mixer.
/// </summary>
[SupportedOSPlatform("windows")]
public class WasapiExclusiveSink : IAudioBufferSink
{
    private WasapiOut? _wasapiOut;
    private BufferedWaveProvider? _bufferProvider;
    private double _maxBacklogSeconds = 0.200;
    private double _targetCushionSeconds = 0.120;

    public void Initialize(int sampleRate, int bitsPerSample, int channels, string? targetDeviceId = null, bool useExclusiveMode = true)
    {
        var format = new WaveFormat(sampleRate, bitsPerSample, channels);
        
        // Use a buffered wave provider, configured to discard frames on overflow to prevent accumulation of lag.
        // Cap the max buffer duration to 300ms to allow healthy jitter headroom.
        _bufferProvider = new BufferedWaveProvider(format)
        {
            DiscardOnBufferOverflow = true,
            BufferDuration = TimeSpan.FromMilliseconds(500)
        };

        using var enumerator = new MMDeviceEnumerator();
        MMDevice? vacDevice = null;

        if (!string.IsNullOrEmpty(targetDeviceId))
        {
            try
            {
                vacDevice = enumerator.GetDevice(targetDeviceId);
            }
            catch (Exception ex)
            {
                Console.WriteLine($"[!] Warning: Failed to locate device by ID {targetDeviceId}: {ex.Message}. Falling back to default search.");
            }
        }

        if (vacDevice == null)
        {
            var devices = enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active);

            foreach (var device in devices)
            {
                string name = device.FriendlyName.ToLower();
                if (name.Contains("cable input") || name.Contains("vb-cable") || name.Contains("virtual audio cable"))
                {
                    vacDevice = device;
                    break;
                }
            }
        }

        if (vacDevice == null)
        {
            throw new InvalidOperationException(
                "Virtual Audio Cable endpoint ('Cable Input', 'VB-Cable', or 'Virtual Audio Cable') could not be automatically found. " +
                "Please ensure VB-Audio Virtual Cable or Virtual Audio Cable is installed and active."
            );
        }

        var shareMode = useExclusiveMode ? AudioClientShareMode.Exclusive : AudioClientShareMode.Shared;
        try
        {
            _wasapiOut = new WasapiOut(vacDevice, shareMode, true, 15);
            _wasapiOut.Init(_bufferProvider);
            FileLogger.Log($"[+] WASAPI Target Device: {vacDevice.FriendlyName} ({(useExclusiveMode ? "Exclusive" : "Shared")} Mode)");
        }
        catch (Exception ex) when (useExclusiveMode)
        {
            Console.ForegroundColor = ConsoleColor.Yellow;
            Console.WriteLine($"[!] Warning: Failed to initialize WASAPI in Exclusive Mode: {ex.Message}");
            Console.WriteLine("[*] Falling back to Shared Mode for compatibility...");
            Console.ResetColor();

            shareMode = AudioClientShareMode.Shared;
            _wasapiOut = new WasapiOut(vacDevice, shareMode, true, 15);
            _wasapiOut.Init(_bufferProvider);
            FileLogger.Log($"[+] WASAPI Target Device: {vacDevice.FriendlyName} (Shared Mode)");
        }
    }

    public void ConfigureBufferThresholds(double maxBacklogSeconds, double targetCushionSeconds)
    {
        _maxBacklogSeconds = maxBacklogSeconds;
        _targetCushionSeconds = targetCushionSeconds;
        FileLogger.Log($"[WasapiExclusiveSink] Configured buffer thresholds: Max Backlog = {maxBacklogSeconds * 1000}ms, Cushion = {targetCushionSeconds * 1000}ms");
    }

    public void Write(byte[] pcmData)
    {
        if (_bufferProvider == null)
        {
            throw new InvalidOperationException("Sink is not initialized. Call Initialize() before writing audio data.");
        }

        // To prevent latency buildup from temporary network freezes or scheduling delays,
        // dynamically discard oldest buffered samples if the backlog exceeds the configured threshold.
        int maxBufferedBytes = (int)(_bufferProvider.WaveFormat.AverageBytesPerSecond * _maxBacklogSeconds);
        if (_bufferProvider.BufferedBytes > maxBufferedBytes)
        {
            int targetBytes = (int)(_bufferProvider.WaveFormat.AverageBytesPerSecond * _targetCushionSeconds);
            int bytesToDiscard = _bufferProvider.BufferedBytes - targetBytes;
            int blockAlign = _bufferProvider.WaveFormat.BlockAlign;
            bytesToDiscard = (bytesToDiscard / blockAlign) * blockAlign;

            if (bytesToDiscard > 0)
            {
                byte[] discardBuffer = new byte[bytesToDiscard];
                _bufferProvider.Read(discardBuffer, 0, bytesToDiscard);
            }
        }

        _bufferProvider.AddSamples(pcmData, 0, pcmData.Length);
    }

    public void Start()
    {
        if (_wasapiOut == null)
        {
            throw new InvalidOperationException("Sink is not initialized.");
        }

        _wasapiOut.Play();
        FileLogger.Log("[*] WASAPI playback thread started.");
    }

    public void Stop()
    {
        _wasapiOut?.Stop();
        FileLogger.Log("[*] WASAPI playback thread stopped.");
    }

    public void Dispose()
    {
        _wasapiOut?.Dispose();
        _wasapiOut = null;
        _bufferProvider = null;
        GC.SuppressFinalize(this);
    }
}
