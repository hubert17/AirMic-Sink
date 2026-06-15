using System;
using System.Threading;
using System.Threading.Tasks;

namespace AirMic.Receiver;

class Program
{
    static async Task Main(string[] args)
    {
        Console.WriteLine("=== AirMic-Sink Receiver Engine ===");

        // Default run mode is signaling mode, but check if user explicitly requested test-sink
        bool testMode = false;
        if (args.Length > 0)
        {
            if (args[0].ToLower() == "--mode" && args.Length > 1 && args[1].ToLower() == "test-sink")
            {
                testMode = true;
            }
        }

        if (testMode)
        {
            RunLocalSinkTest();
        }
        else
        {
            await RunSignalingModeAsync(args);
        }
    }

    private static void RunLocalSinkTest()
    {
        Console.WriteLine("\n[*] Launching Local Ingestion Sink Test (Phase 1)...");

        if (!OperatingSystem.IsWindows())
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine("[!] Error: WASAPI Exclusive Mode is only supported on Windows.");
            Console.WriteLine("    Future versions will support macOS BlackHole and CoreAudio.");
            Console.ResetColor();
            return;
        }

        IAudioBufferSink? sink = null;
        bool isRunning = true;

        try
        {
            // Instantiate Windows-specific WASAPI sink
            sink = new WasapiExclusiveSink();

            // WebRTC VoIP default formatting: 48000Hz, 16-bit, Mono (1 channel)
            int sampleRate = 48000;
            int bitsPerSample = 16;
            int channels = 1;

            Console.WriteLine("[*] Initializing audio sink device (Exclusive Mode)...");
            sink.Initialize(sampleRate, bitsPerSample, channels);

            // Audio generation variables
            double frequency = 440.0; // Standard concert pitch A
            double amplitude = 0.15;  // Comfortable volume level
            double phase = 0.0;
            
            // Generate 10ms frames of audio (480 samples per frame at 48kHz)
            int samplesPerFrame = sampleRate / 100; // 480
            int bytesPerSample = bitsPerSample / 8; // 2
            int frameBytes = samplesPerFrame * bytesPerSample * channels; // 960 bytes

            byte[] frameBuffer = new byte[frameBytes];

            // Thread to continuously feed raw PCM buffers to the sink
            Thread feedThread = new Thread(() =>
            {
                Console.WriteLine("[*] Audio feed worker loop running.");
                while (isRunning)
                {
                    for (int i = 0; i < samplesPerFrame; i++)
                    {
                        double sample = amplitude * Math.Sin(phase);
                        phase += 2 * Math.PI * frequency / sampleRate;
                        if (phase > 2 * Math.PI)
                        {
                            phase -= 2 * Math.PI;
                        }

                        // Convert floating-point sample to 16-bit Signed PCM short integer
                        short pcmValue = (short)(sample * short.MaxValue);

                        // Split into little-endian byte array
                        int index = i * bytesPerSample;
                        frameBuffer[index] = (byte)(pcmValue & 0xFF);
                        frameBuffer[index + 1] = (byte)((pcmValue >> 8) & 0xFF);
                    }

                    try
                    {
                        // Write block to play queue
                        sink.Write(frameBuffer);
                    }
                    catch (Exception ex)
                    {
                        Console.ForegroundColor = ConsoleColor.Red;
                        Console.WriteLine($"\n[!] Error pushing samples to audio client: {ex.Message}");
                        Console.ResetColor();
                        break;
                    }

                    // Throttle block writing to match the 10ms frame clock rate
                    Thread.Sleep(10);
                }
            });

            sink.Start();
            feedThread.Start();

            Console.ForegroundColor = ConsoleColor.Green;
            Console.WriteLine("\n[+] SUCCESS: Feeding continuous 440Hz tone to Virtual Audio Cable!");
            Console.ResetColor();
            Console.WriteLine("---------------------------------------------------------------");
            Console.WriteLine("--> ACTION REQUIRED: ");
            Console.WriteLine("    1. Open Teams, Zoom, or Windows Audio settings.");
            Console.WriteLine("    2. Set your microphone source to 'Cable Output' or 'VB-Cable'.");
            Console.WriteLine("    3. Test the microphone loopback or make a test call.");
            Console.WriteLine("    4. Confirm you hear a clean, pitch-stable 440Hz wave.");
            Console.WriteLine("---------------------------------------------------------------");
            Console.WriteLine("\nPress any key to terminate the sink test and release the hardware client...");
            Console.ReadKey();

            isRunning = false;
            feedThread.Join();
            sink.Stop();
        }
        catch (Exception ex)
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine($"\n[!] Critical Ingestion Error: {ex.Message}");
            Console.ResetColor();
        }
        finally
        {
            sink?.Dispose();
            Console.WriteLine("\n[*] Cleanup completed. Sink released.");
        }
    }

    private static async Task RunSignalingModeAsync(string[] args)
    {
        Console.WriteLine("\n[*] Launching in Signaling / WebRTC Receiver Mode (Phase 3)...");

        if (!OperatingSystem.IsWindows())
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine("[!] Error: WASAPI Exclusive Mode is only supported on Windows.");
            Console.WriteLine("    Future versions will support macOS BlackHole and CoreAudio.");
            Console.ResetColor();
            return;
        }

        // Configure signaling parameters from environment variables or defaults
        string signalingUrl = Environment.GetEnvironmentVariable("SIGNALING_URL") ?? "wss://localhost:7133/ws";
        string streamSecret = Environment.GetEnvironmentVariable("STREAM_SECRET") ?? "MySuperSecretKey123";

        Console.ForegroundColor = ConsoleColor.Cyan;
        Console.WriteLine("=== Select Signaling Endpoint ===");
        Console.WriteLine($"1. Local Development  : wss://localhost:7133/ws (Default)");
        Console.WriteLine($"2. Docker Production : wss://airmic.bernardgabon.com/ws");
        Console.WriteLine($"3. IIS Production    : wss://air-mic.bernardgabon.com/ws");
        Console.Write("Enter choice (1-3) or custom URL [Default: 1]: ");
        Console.ResetColor();

        string? inputUrl = Console.ReadLine()?.Trim();
        if (string.IsNullOrEmpty(inputUrl) || inputUrl == "1")
        {
            // Keep default signalingUrl (local wss://localhost:7133/ws)
            signalingUrl = "wss://localhost:7133/ws";
        }
        else if (inputUrl == "2")
        {
            signalingUrl = "wss://airmic.bernardgabon.com/ws";
        }
        else if (inputUrl == "3")
        {
            signalingUrl = "wss://air-mic.bernardgabon.com/ws";
        }
        else
        {
            // Custom URL entered directly
            signalingUrl = inputUrl;
        }

        Console.ForegroundColor = ConsoleColor.Cyan;
        Console.Write($"Enter stream secret [Default: {streamSecret}]: ");
        Console.ResetColor();
        string? inputSecret = Console.ReadLine();
        if (!string.IsNullOrWhiteSpace(inputSecret))
        {
            streamSecret = inputSecret.Trim();
        }
        Console.WriteLine();

        IAudioBufferSink? sink = null;
        WebRtcReceiver? webRtc = null;

        try
        {
            // 1. Initialize WASAPI Exclusive Mode target device
            sink = new WasapiExclusiveSink();

            int sampleRate = 48000;
            int bitsPerSample = 16;
            int channels = 1;

            Console.WriteLine("[*] Initializing audio sink device (Exclusive Mode)...");
            sink.Initialize(sampleRate, bitsPerSample, channels);
            sink.Start();

            // 2. Instantiate and start WebRTC receiver
            Console.WriteLine("[*] Starting WebRTC receiver connection...");
            webRtc = new WebRtcReceiver(signalingUrl, streamSecret, sink);
            await webRtc.StartAsync();

            Console.ForegroundColor = ConsoleColor.Green;
            Console.WriteLine("\n[+] SUCCESS: WebRTC Receiver is running!");
            Console.ResetColor();
            Console.WriteLine("---------------------------------------------------------------");
            Console.WriteLine($"Signaling Host : {signalingUrl}");
            Console.WriteLine("Waiting for remote mobile client to connect and stream audio...");
            Console.WriteLine("---------------------------------------------------------------");
            var tcs = new TaskCompletionSource();
            Console.CancelKeyPress += (sender, eventArgs) =>
            {
                Console.WriteLine("\n[*] Shutdown requested. Exiting gracefully...");
                eventArgs.Cancel = true;
                tcs.TrySetResult();
            };

            Console.WriteLine("\nPress Ctrl+C to stop the receiver and terminate the program...");
            
            // Wait for Ctrl+C event
            await tcs.Task;
        }
        catch (Exception ex)
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine($"\n[!] Receiver Error: {ex.Message}");
            Console.ResetColor();
        }
        finally
        {
            webRtc?.Dispose();
            sink?.Dispose();
            Console.WriteLine("\n[*] Engine shut down. Resources released.");
        }
    }
}
