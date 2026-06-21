using System;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NAudio.CoreAudioApi;

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
        string streamSecret = Environment.GetEnvironmentVariable("STREAM_SECRET") ?? TryGetSecretFromAppSettings() ?? "MySuperSecretKey123";

        Console.ForegroundColor = ConsoleColor.Cyan;
        Console.WriteLine("=== Select Signaling Endpoint ===");
        Console.WriteLine($"1. Local Development  : wss://localhost:7133/ws");
        Console.WriteLine($"2. Docker Production : wss://airmic.bernardgabon.com/ws");
        Console.WriteLine($"3. IIS Production    : wss://air-mic.bernardgabon.com/ws");
        Console.Write($"Enter choice (1-3) or custom URL [Default: {signalingUrl}]: ");
        Console.ResetColor();

        string? inputUrl = Console.ReadLine()?.Trim();
        if (!string.IsNullOrEmpty(inputUrl))
        {
            if (inputUrl == "1")
            {
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
                signalingUrl = inputUrl;
            }
        }

        Console.ForegroundColor = ConsoleColor.Cyan;
        Console.Write($"Enter stream key [Default: {streamSecret}]: ");
        Console.ResetColor();
        string? inputSecret = Console.ReadLine();
        if (!string.IsNullOrWhiteSpace(inputSecret))
        {
            streamSecret = inputSecret.Trim();
        }
        Console.WriteLine();

        // Enumerate and sort audio devices
        using var enumerator = new MMDeviceEnumerator();
        var rawInputs = enumerator.EnumerateAudioEndPoints(DataFlow.Capture, DeviceState.Active)
            .Where(d => !d.FriendlyName.ToLower().Contains("in 16ch"));
        var rawOutputs = enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active)
            .Where(d => !d.FriendlyName.ToLower().Contains("in 16ch"));

        Func<MMDevice, bool> isPrioritized = d => {
            string name = d.FriendlyName.ToLower();
            return name.Contains("virtual audio") || name.Contains("vac") || name.Contains("vb-cable");
        };

        var sortedInputs = rawInputs
            .OrderByDescending(isPrioritized)
            .ThenBy(d => d.FriendlyName)
            .ToList();

        var sortedOutputs = rawOutputs
            .OrderByDescending(isPrioritized)
            .ThenBy(d => d.FriendlyName)
            .ToList();

        MMDevice? selectedInputDevice = null;
        MMDevice? selectedOutputDevice = null;

        if (sortedOutputs.Count > 0)
        {
            Console.ForegroundColor = ConsoleColor.Cyan;
            Console.WriteLine("=== Select Audio Output Device (Speaker) ===");
            for (int i = 0; i < sortedOutputs.Count; i++)
            {
                Console.WriteLine($"{i + 1}. {sortedOutputs[i].FriendlyName}");
            }
            Console.Write($"Enter choice (1-{sortedOutputs.Count}) or leave blank for default [Default: 1]: ");
            Console.ResetColor();
            
            string? outputChoice = Console.ReadLine()?.Trim();
            int selectedOutputIndex = 0;
            if (!string.IsNullOrEmpty(outputChoice))
            {
                if (int.TryParse(outputChoice, out int parsedIndex) && parsedIndex >= 1 && parsedIndex <= sortedOutputs.Count)
                {
                    selectedOutputIndex = parsedIndex - 1;
                }
            }
            selectedOutputDevice = sortedOutputs[selectedOutputIndex];
            Console.WriteLine();

            if (sortedInputs.Count > 0)
            {
                int defaultInputIndex = 0;
                if ((string.IsNullOrEmpty(outputChoice) || outputChoice == "1") && sortedInputs.Count > 1)
                {
                    defaultInputIndex = 1;
                }
                Console.ForegroundColor = ConsoleColor.Cyan;
                Console.WriteLine("=== Select Audio Input Device (Microphone) ===");
                for (int i = 0; i < sortedInputs.Count; i++)
                {
                    Console.WriteLine($"{i + 1}. {sortedInputs[i].FriendlyName}");
                }
                Console.Write($"Enter choice (1-{sortedInputs.Count}) or leave blank for default [Default: {defaultInputIndex + 1}]: ");
                Console.ResetColor();

                string? inputChoice = Console.ReadLine()?.Trim();
                int selectedInputIndex = defaultInputIndex;
                if (!string.IsNullOrEmpty(inputChoice))
                {
                    if (int.TryParse(inputChoice, out int parsedIndex) && parsedIndex >= 1 && parsedIndex <= sortedInputs.Count)
                    {
                        selectedInputIndex = parsedIndex - 1;
                    }
                }
                selectedInputDevice = sortedInputs[selectedInputIndex];
            }
        }
        else
        {
            if (sortedInputs.Count > 0)
            {
                Console.ForegroundColor = ConsoleColor.Cyan;
                Console.WriteLine("=== Select Audio Input Device (Microphone) ===");
                for (int i = 0; i < sortedInputs.Count; i++)
                {
                    Console.WriteLine($"{i + 1}. {sortedInputs[i].FriendlyName}");
                }
                Console.Write($"Enter choice (1-{sortedInputs.Count}) or leave blank for default [Default: 1]: ");
                Console.ResetColor();

                string? inputChoice = Console.ReadLine()?.Trim();
                int selectedInputIndex = 0;
                if (!string.IsNullOrEmpty(inputChoice))
                {
                    if (int.TryParse(inputChoice, out int parsedIndex) && parsedIndex >= 1 && parsedIndex <= sortedInputs.Count)
                    {
                        selectedInputIndex = parsedIndex - 1;
                    }
                }
                selectedInputDevice = sortedInputs[selectedInputIndex];
            }
        }

        Console.WriteLine();

        bool exitRequested = false;
        bool firstRun = true;

        while (!exitRequested)
        {
            if (!firstRun)
            {
                Console.ForegroundColor = ConsoleColor.Yellow;
                Console.WriteLine("\n[!] Automatically restarting connection using previously selected configurations...");
                Console.ResetColor();
                // Wait 2 seconds to avoid rapid reconnection loops in case of network/server downtime
                await Task.Delay(2000);
            }
            firstRun = false;

            IAudioBufferSink? sink = null;
            WebRtcReceiver? webRtc = null;

            var shutdownTcs = new TaskCompletionSource();
            ConsoleCancelEventHandler cancelHandler = (sender, eventArgs) =>
            {
                Console.ForegroundColor = ConsoleColor.Yellow;
                Console.WriteLine("\n[*] Shutdown requested. Exiting gracefully...");
                Console.ResetColor();
                eventArgs.Cancel = true;
                shutdownTcs.TrySetResult();
            };

            try
            {
                Console.CancelKeyPress += cancelHandler;

                // 1. Initialize WASAPI Exclusive Mode target device
                sink = new WasapiExclusiveSink();

                int sampleRate = 48000;
                int bitsPerSample = 16;
                int channels = 2; // Initialize target device in stereo to maximize exclusive mode compatibility

                Console.WriteLine("[*] Initializing audio sink device (Exclusive Mode)...");
                sink.Initialize(sampleRate, bitsPerSample, channels, selectedOutputDevice?.ID);
                sink.Start();

                // 2. Instantiate and start WebRTC receiver
                Console.WriteLine("[*] Starting WebRTC receiver connection...");
                webRtc = new WebRtcReceiver(signalingUrl, streamSecret, sink, selectedInputDevice?.ID);

                var disconnectTcs = new TaskCompletionSource();
                webRtc.OnDisconnected += (reason) =>
                {
                    disconnectTcs.TrySetResult();
                };

                await webRtc.StartAsync();

                string teamsMicName = GetTeamsCounterpartName(selectedOutputDevice);
                string teamsSpkName = GetTeamsCounterpartName(selectedInputDevice);
                
                string title = "Set the following for your call/meeting app:";
                string spkText = $"  speaker out: {teamsSpkName}";
                string micText = $"  mic input:   {teamsMicName}";

                int boxWidth = Math.Max(55, Math.Max(micText.Length + 4, spkText.Length + 4));
                string borderLine = new string('─', boxWidth);

                Console.ForegroundColor = ConsoleColor.Yellow;
                Console.WriteLine("\n" + borderLine);
                Console.WriteLine(title);
                Console.ResetColor();

                Console.ForegroundColor = ConsoleColor.Cyan;
                Console.Write("  speaker out: ");
                Console.ResetColor();
                Console.WriteLine(teamsSpkName);

                Console.ForegroundColor = ConsoleColor.Cyan;
                Console.Write("  mic input:   ");
                Console.ResetColor();
                Console.WriteLine(teamsMicName);

                Console.ForegroundColor = ConsoleColor.Yellow;
                Console.WriteLine(borderLine);
                Console.ResetColor();

                Console.ForegroundColor = ConsoleColor.Green;
                Console.WriteLine("\n[+] SUCCESS: WebRTC Receiver is running!");
                Console.ResetColor();
                Console.WriteLine("---------------------------------------------------------------");
                Console.WriteLine($"Signaling Host : {signalingUrl}");
                Console.WriteLine("Waiting for remote mobile client to connect and stream audio...");
                Console.WriteLine("---------------------------------------------------------------");
                
                Console.WriteLine("\nPress Ctrl+C to stop the receiver and terminate the program...");
                
                // Wait for Ctrl+C event or client disconnect
                var completedTask = await Task.WhenAny(shutdownTcs.Task, disconnectTcs.Task);
                if (completedTask == shutdownTcs.Task)
                {
                    exitRequested = true;
                }
                else
                {
                    // Disconnected!
                    Console.ForegroundColor = ConsoleColor.Yellow;
                    Console.WriteLine("\n[-] WebRTC session closed. Automatically reconnecting in 2 seconds...");
                    Console.ResetColor();
                }
            }
            catch (Exception ex)
            {
                Console.ForegroundColor = ConsoleColor.Red;
                Console.WriteLine($"\n[!] Receiver Error: {ex.Message}");
                Console.ResetColor();
                
                // Wait a bit before retrying in case of network/audio device errors
                Console.WriteLine("Retrying connection in 5 seconds... Press Ctrl+C to exit.");
                var waitKeyOrShutdown = await Task.WhenAny(shutdownTcs.Task, Task.Delay(5000));
                if (waitKeyOrShutdown == shutdownTcs.Task)
                {
                    exitRequested = true;
                }
            }
            finally
            {
                Console.CancelKeyPress -= cancelHandler;
                webRtc?.Dispose();
                sink?.Dispose();
                Console.WriteLine("\n[*] Engine shut down. Resources released.");
            }
        }
    }

    private static string? TryGetSecretFromAppSettings()
    {
        string? env = Environment.GetEnvironmentVariable("ASPNETCORE_ENVIRONMENT") ?? Environment.GetEnvironmentVariable("DOTNET_ENVIRONMENT");
        var fileNames = new List<string>();
        if (!string.IsNullOrEmpty(env))
        {
            fileNames.Add($"appsettings.{env}.json");
        }
        fileNames.Add("appsettings.Development.json");
        fileNames.Add("appsettings.json");

        var dirsToSearch = new[]
        {
            System.IO.Directory.GetCurrentDirectory(),
            AppContext.BaseDirectory
        };

        foreach (var startDir in dirsToSearch)
        {
            var current = new System.IO.DirectoryInfo(startDir);
            while (current != null)
            {
                foreach (var fileName in fileNames)
                {
                    var pathsToCheck = new[]
                    {
                        System.IO.Path.Combine(current.FullName, "src", "AirMic.Server", fileName),
                        System.IO.Path.Combine(current.FullName, "AirMic.Server", fileName),
                        System.IO.Path.Combine(current.FullName, fileName)
                    };

                    foreach (var path in pathsToCheck)
                    {
                        if (System.IO.File.Exists(path))
                        {
                            try
                            {
                                var json = System.IO.File.ReadAllText(path);
                                using var doc = System.Text.Json.JsonDocument.Parse(json);
                                
                                if (doc.RootElement.TryGetProperty("PrivateMasterKeys", out var keysProp) && 
                                    keysProp.ValueKind == System.Text.Json.JsonValueKind.Array && 
                                    keysProp.GetArrayLength() > 0)
                                {
                                    var firstKey = keysProp[0].GetString();
                                    if (!string.IsNullOrEmpty(firstKey))
                                    {
                                        return firstKey;
                                    }
                                }

                                if (doc.RootElement.TryGetProperty("StreamSecret", out var prop))
                                {
                                    return prop.GetString();
                                }
                            }
                            catch
                            {
                                // Ignore and keep looking
                            }
                        }
                    }
                }

                current = current.Parent;
            }
        }

        return null;
    }

    private static string GetTeamsCounterpartName(MMDevice? device)
    {
        if (device == null) return "Default";
        string name = device.FriendlyName;
        if (name.Contains("CABLE Input")) return "CABLE Output (VB-Audio Virtual Cable)";
        if (name.Contains("CABLE Output")) return "CABLE Input (VB-Audio Virtual Cable)";
        if (name.Contains("CABLE-B Input")) return "CABLE-B Output (VB-Audio Virtual Cable B)";
        if (name.Contains("CABLE-B Output")) return "CABLE-B Input (VB-Audio Virtual Cable B)";
        return name;
    }
}
