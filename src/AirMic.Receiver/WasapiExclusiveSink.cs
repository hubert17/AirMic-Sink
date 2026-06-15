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

    public void Initialize(int sampleRate, int bitsPerSample, int channels)
    {
        var format = new WaveFormat(sampleRate, bitsPerSample, channels);
        
        // Use a buffered wave provider, configured to discard frames on overflow to prevent accumulation of lag.
        _bufferProvider = new BufferedWaveProvider(format)
        {
            DiscardOnBufferOverflow = true
        };

        // Find the Virtual Audio Cable endpoint
        using var enumerator = new MMDeviceEnumerator();
        MMDevice? vacDevice = null;
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

        if (vacDevice == null)
        {
            throw new InvalidOperationException(
                "Virtual Audio Cable endpoint ('Cable Input', 'VB-Cable', or 'Virtual Audio Cable') could not be automatically found. " +
                "Please ensure VB-Audio Virtual Cable or Virtual Audio Cable is installed and active."
            );
        }

        Console.WriteLine($"[+] WASAPI Target Device: {vacDevice.FriendlyName}");

        // Initialize WASAPI in Exclusive mode with event synchronization and a target latency of 20ms.
        _wasapiOut = new WasapiOut(vacDevice, AudioClientShareMode.Exclusive, true, 20);
        _wasapiOut.Init(_bufferProvider);
    }

    public void Write(byte[] pcmData)
    {
        if (_bufferProvider == null)
        {
            throw new InvalidOperationException("Sink is not initialized. Call Initialize() before writing audio data.");
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
        Console.WriteLine("[*] WASAPI playback thread started.");
    }

    public void Stop()
    {
        _wasapiOut?.Stop();
        Console.WriteLine("[*] WASAPI playback thread stopped.");
    }

    public void Dispose()
    {
        _wasapiOut?.Dispose();
        _wasapiOut = null;
        _bufferProvider = null;
        GC.SuppressFinalize(this);
    }
}
