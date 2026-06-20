#pragma warning disable CA1416
using System;
using System.Collections.Generic;
using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using SIPSorcery.Net;
using Concentus.Enums;
using Concentus.Structs;
using AirMic.Contracts;
using Microsoft.Extensions.Logging;
using System.Text.RegularExpressions;
using NAudio.CoreAudioApi;
using NAudio.Wave;
using System.IO;

namespace AirMic.Receiver;

/// <summary>
/// Handles the WebRTC connection state machine, signaling exchange,
/// and incoming Opus audio decoding using Concentus.
/// </summary>
public class WebRtcReceiver : IDisposable
{
    private static readonly JsonSerializerOptions _jsonOptions = new JsonSerializerOptions
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        PropertyNameCaseInsensitive = true
    };

    private readonly string _signalingUrl;
    private readonly string _streamSecret;
    private readonly IAudioBufferSink _audioSink;
    private readonly string? _captureDeviceId;
    
    private ClientWebSocket? _webSocket;
    private RTCPeerConnection? _peerConnection;
    private OpusDecoder? _opusDecoder;
    private OpusEncoder? _opusEncoder;
    private WasapiCapture? _wasapiCapture;
    private CancellationTokenSource? _cts;
    
    private readonly short[] _pcmBuffer = new short[5760]; // Max Opus frame duration is 120ms (5760 samples @ 48kHz)
    private readonly Queue<short> _capturePcmQueue = new Queue<short>();
    private readonly object _captureLock = new object();

    private bool _isTestMode = false;
    private bool _isRecording = false;
    private readonly List<byte> _recordedDataList = new List<byte>();
    private readonly object _recordingLock = new object();

    // Silence detection parameters
    private const double SilenceThreshold = 0.015; // Amplitude threshold (0.0 to 1.0)
    private const double SilenceDurationLimitSeconds = 1.2; // Silence duration to trigger playback
    private DateTime _lastActiveTime = DateTime.MinValue;

    private CancellationTokenSource? _playbackCts;
    private Task? _playbackTask;
    private volatile bool _isPlayingBack = false;
    private IAudioBufferSink? _testPlaybackSink;

    public event Action<string>? OnDisconnected;

    public WebRtcReceiver(string signalingUrl, string streamSecret, IAudioBufferSink audioSink, string? captureDeviceId = null)
    {
        _signalingUrl = signalingUrl;
        _streamSecret = streamSecret;
        _audioSink = audioSink;
        _captureDeviceId = captureDeviceId;
    }

    /// <summary>
    /// Starts the receiver: connects to the signaling hub and awaits incoming WebRTC connections.
    /// </summary>
    public async Task StartAsync()
    {
        // Register verbose console logger for SIPSorcery internals
        var loggerFactory = new SimpleConsoleLoggerFactory();
        loggerFactory.AddProvider(new SimpleConsoleLoggerProvider());
        SIPSorcery.LogFactory.Set(loggerFactory);

        _cts = new CancellationTokenSource();
        _opusDecoder = new OpusDecoder(48000, 1); // WebRTC voice streams default to 48kHz Mono
        _opusEncoder = new OpusEncoder(48000, 1, OpusApplication.OPUS_APPLICATION_VOIP);
        _opusEncoder.Bitrate = 16000;
        
        // Build signaling WebSocket URL with parameters
        var wsUriBuilder = new UriBuilder(_signalingUrl);
        var query = $"role=receiver&secret={Uri.EscapeDataString(_streamSecret)}";
        
        // Append or replace query string
        if (wsUriBuilder.Query.Length > 1)
        {
            wsUriBuilder.Query = wsUriBuilder.Query.Substring(1) + "&" + query;
        }
        else
        {
            wsUriBuilder.Query = query;
        }

        Uri uri = wsUriBuilder.Uri;
        Console.WriteLine($"[*] Connecting to signaling server: {uri.Scheme}://{uri.Host}:{uri.Port}{uri.PathAndQuery.Split('?')[0]}");

        var createWebSocket = () =>
        {
            var ws = new ClientWebSocket();
            if (uri.Scheme == "wss" && (uri.Host == "localhost" || uri.Host == "127.0.0.1"))
            {
                ws.Options.RemoteCertificateValidationCallback = (sender, cert, chain, sslErrors) => true;
            }
            return ws;
        };

        _webSocket = createWebSocket();
        int maxRetries = 10;
        int retryCount = 0;
        while (true)
        {
            try
            {
                await _webSocket.ConnectAsync(uri, _cts.Token);
                break;
            }
            catch (Exception ex) when (retryCount < maxRetries && !_cts.Token.IsCancellationRequested)
            {
                retryCount++;
                Console.ForegroundColor = ConsoleColor.Yellow;
                Console.WriteLine($"[!] Connection attempt {retryCount}/{maxRetries} failed: {ex.Message}. Retrying in 2 seconds...");
                Console.ResetColor();
                await Task.Delay(2000, _cts.Token);
                _webSocket.Dispose();
                _webSocket = createWebSocket();
            }
        }
        Console.WriteLine("[+] Connected to signaling server.");

        // Start listening loop in the background
        _ = Task.Run(() => ReceiveSignalingLoopAsync(_cts.Token));
    }

    private async Task ReceiveSignalingLoopAsync(CancellationToken cancellationToken)
    {
        var buffer = new byte[1024 * 32];

        try
        {
            while (_webSocket != null && _webSocket.State == WebSocketState.Open && !cancellationToken.IsCancellationRequested)
            {
                using var ms = new System.IO.MemoryStream();
                WebSocketReceiveResult result;
                do
                {
                    result = await _webSocket.ReceiveAsync(new ArraySegment<byte>(buffer), cancellationToken);
                    ms.Write(buffer, 0, result.Count);
                } while (!result.EndOfMessage);

                if (result.MessageType == WebSocketMessageType.Close)
                {
                    Console.WriteLine("[-] Signaling WebSocket closed by remote server.");
                    OnDisconnected?.Invoke("Signaling WebSocket closed.");
                    break;
                }

                ms.Seek(0, System.IO.SeekOrigin.Begin);
                using var reader = new System.IO.StreamReader(ms, Encoding.UTF8);
                string messageJson = await reader.ReadToEndAsync();

                try
                {
                    var message = JsonSerializer.Deserialize<SignalingMessage>(messageJson, _jsonOptions);

                    if (message != null)
                    {
                        await HandleSignalingMessageAsync(message);
                    }
                }
                catch (Exception ex)
                {
                    Console.WriteLine($"[!] Error parsing signaling message: {ex.Message}");
                }
            }
        }
        catch (Exception ex)
        {
            if (!cancellationToken.IsCancellationRequested)
            {
                Console.WriteLine($"[!] Exception in signaling loop: {ex.Message}");
                OnDisconnected?.Invoke($"Signaling loop exception: {ex.Message}");
            }
        }
    }

    private async Task HandleSignalingMessageAsync(SignalingMessage message)
    {
        if (message.Type == "offer")
        {
            Console.WriteLine("[*] Received SDP Offer. Preparing WebRTC connection...");
            _isTestMode = message.IsTest == true;
            _testPlaybackSink?.Dispose();
            _testPlaybackSink = null;
            if (_isTestMode)
            {
                Console.ForegroundColor = ConsoleColor.Magenta;
                Console.WriteLine("[*] Test Mode activated for this WebRTC session.");
                Console.ResetColor();

                // Initialize counterpart rendering sink if virtual cable matches
                if (!string.IsNullOrEmpty(_captureDeviceId))
                {
                    string? counterpartId = GetCounterpartRenderDeviceId(_captureDeviceId);
                    if (counterpartId != null)
                    {
                        try
                        {
                            var tempSink = new WasapiExclusiveSink();
                            tempSink.Initialize(48000, 16, 1, counterpartId);
                            tempSink.Start();
                            _testPlaybackSink = tempSink;
                            Console.ForegroundColor = ConsoleColor.Magenta;
                            Console.WriteLine($"[*] Test Loopback: Created matching output sink for counterpart rendering device.");
                            Console.ResetColor();
                        }
                        catch (Exception ex)
                        {
                            Console.ForegroundColor = ConsoleColor.Yellow;
                            Console.WriteLine($"[!] Warning: Failed to create test counterpart sink: {ex.Message}. Falling back to default output sink.");
                            Console.ResetColor();
                        }
                    }
                }
            }

            // Cleanup any stale connection
            _peerConnection?.Close("reconnecting");
            _peerConnection?.Dispose();

            // Set up PeerConnection configuration (use Google's public STUN for fallback NAT traversal)
            var config = new RTCConfiguration
            {
                iceServers = new List<RTCIceServer>
                {
                    new RTCIceServer { urls = "stun:stun.l.google.com:19302" }
                },
                X_UseRtpFeedbackProfile = true, // Forces SAVPF feedback profile required for WebRTC browsers
                X_BindAddress = System.Net.IPAddress.Any // Force standard IPv4 binding
            };

            _peerConnection = new RTCPeerConnection(config);

            // Register events
            _peerConnection.OnRtpPacketReceived += (remoteEndPoint, mediaType, rtpPacket) =>
            {
                if (mediaType == SDPMediaTypesEnum.audio)
                {
                    ProcessIncomingAudio(rtpPacket.Payload);
                }
            };

            _peerConnection.onconnectionstatechange += (state) =>
            {
                Console.WriteLine($"[WebRTC] Connection state: {state}");
                if (state == RTCPeerConnectionState.connected)
                {
                    Console.ForegroundColor = ConsoleColor.Green;
                    Console.WriteLine("[+] WebRTC Audio Stream established successfully!");
                    Console.ResetColor();
                    StartLoopbackCapture();
                }
                else if (state == RTCPeerConnectionState.closed || state == RTCPeerConnectionState.failed)
                {
                    Console.ForegroundColor = ConsoleColor.Red;
                    Console.WriteLine("[-] WebRTC Audio connection lost.");
                    Console.ResetColor();
                    StopLoopbackCapture();
                    OnDisconnected?.Invoke("WebRTC connection lost.");
                }
            };

            _peerConnection.onicecandidate += async (candidate) =>
            {
                if (candidate != null && !string.IsNullOrEmpty(candidate.candidate))
                {
                    FileLogger.Log($"Gathered local ICE candidate: {candidate.candidate}", "DEBUG");
                    
                    // Prepend required prefix if missing to satisfy WebRTC specifications
                    string candidateFormat = candidate.candidate;
                    if (!candidateFormat.StartsWith("candidate:"))
                    {
                        candidateFormat = "candidate:" + candidateFormat;
                    }

                    var candMsg = new SignalingMessage
                    {
                        Type = "candidate",
                        Candidate = new IceCandidatePayload
                        {
                            Candidate = candidateFormat,
                            SdpMid = candidate.sdpMid,
                            SdpMLineIndex = candidate.sdpMLineIndex
                        }
                    };
                    await SendSignalingMessageAsync(candMsg);
                }
            };

            // Add audio track to peer connection configured for real-time low-latency voice:
            // - minptime=10: sets frame size to 10ms (lowest latency)
            // - useinbandfec=1: enables forward error correction to prevent jitter from packet loss
            // - stereo=0: forces mono to focus bandwidth on voice quality
            // - usedtx=1: enables DTX to save network traffic during silence
            // - maxaveragebitrate=16000: caps voice stream to 16 kbps
            // - maxplaybackrate/sprop-maxcapturerate=16000: limits to 16kHz wideband voice frequencies
            // - ptime=10: requests 10ms packets to minimize packetization latency
            var audioFormat = new SDPAudioVideoMediaFormat(
                SDPMediaTypesEnum.audio, 
                111, 
                "OPUS", 
                48000, 
                2, 
                "minptime=10;useinbandfec=1;stereo=0;sprop-stereo=0;usedtx=1;maxaveragebitrate=24000;maxplaybackrate=16000;sprop-maxcapturerate=16000;ptime=10"
            );
            var audioTrack = new MediaStreamTrack(SDPMediaTypesEnum.audio, false, new List<SDPAudioVideoMediaFormat> { audioFormat }, MediaStreamStatusEnum.SendRecv);
            _peerConnection.addTrack(audioTrack);

            // Set Remote SDP Offer
            var sdpInit = new RTCSessionDescriptionInit
            {
                type = RTCSdpType.offer,
                sdp = message.Sdp
            };
            
            FileLogger.Log($"Remote SDP Offer:\n{message.Sdp}", "DEBUG");

            var result = _peerConnection.setRemoteDescription(sdpInit);
            if (result != SetDescriptionResultEnum.OK)
            {
                Console.WriteLine($"[!] Failed to set remote description: {result}");
                return;
            }

            // Create Local SDP Answer
            var answer = _peerConnection.createAnswer();
            await _peerConnection.setLocalDescription(answer);

            FileLogger.Log($"Local SDP Answer:\n{answer.sdp}", "DEBUG");

            // Send SDP Answer back to signaling channel
            var ansMsg = new SignalingMessage
            {
                Type = "answer",
                Sdp = answer.sdp
            };

            await SendSignalingMessageAsync(ansMsg);
            Console.WriteLine("[*] Sent SDP Answer to signaling client.");
        }
        else if (message.Type == "candidate")
        {
            if (_peerConnection != null && message.Candidate != null)
            {
                FileLogger.Log($"Received remote ICE Candidate: {message.Candidate.Candidate}", "DEBUG");
                
                _peerConnection.addIceCandidate(new RTCIceCandidateInit
                {
                    candidate = message.Candidate.Candidate,
                    sdpMid = message.Candidate.SdpMid,
                    sdpMLineIndex = (ushort)(message.Candidate.SdpMLineIndex ?? 0)
                });
            }
        }
    }

    private void ProcessIncomingAudio(byte[] opusData)
    {
        if (_opusDecoder == null) return;

        try
        {
            // Determine size of frame dynamically
            int frameSize = 960; // Default to 20ms (960 samples @ 48kHz)
            try
            {
                frameSize = OpusPacketInfo.GetNumSamples(_opusDecoder, opusData, 0, opusData.Length);
            }
            catch
            {
                // Fallback to 960 if packet parsing fails
            }

            int decodedSamples = _opusDecoder.Decode(opusData, 0, opusData.Length, _pcmBuffer, 0, frameSize, false);
            if (decodedSamples > 0)
            {
                int byteCount = decodedSamples * 2; // 16-bit short = 2 bytes
                byte[] pcmBytes = new byte[byteCount];
                Buffer.BlockCopy(_pcmBuffer, 0, pcmBytes, 0, byteCount);
                
                // Write to WASAPI virtual audio device
                if (!_isTestMode || !_isPlayingBack)
                {
                    if (_testPlaybackSink != null)
                    {
                        _testPlaybackSink.Write(pcmBytes);
                    }
                    else
                    {
                        _audioSink.Write(pcmBytes);
                    }
                }

                if (_isTestMode)
                {
                    // Calculate RMS of incoming audio frame
                    double sum = 0;
                    for (int i = 0; i < decodedSamples; i++)
                    {
                        double val = _pcmBuffer[i] / 32768.0;
                        sum += val * val;
                    }
                    double rms = Math.Sqrt(sum / decodedSamples);
                    bool isSilentFrame = rms < SilenceThreshold;

                    if (!isSilentFrame)
                    {
                        _lastActiveTime = DateTime.UtcNow;

                        if (!_isRecording)
                        {
                            Console.ForegroundColor = ConsoleColor.Magenta;
                            Console.WriteLine("[*] Test Loopback: Client speech detected. Recording started.");
                            Console.ResetColor();

                            _isPlayingBack = false;

                            // Cancel any running playback immediately when client starts speaking
                            _playbackCts?.Cancel();
                            _playbackCts?.Dispose();
                            _playbackCts = null;

                            lock (_recordingLock)
                            {
                                _recordedDataList.Clear();
                            }
                            _isRecording = true;
                        }
                    }
                }
            }
        }
        catch (Exception ex)
        {
            // Log occasionally or drop silently to avoid console flooding during high packet rates
            FileLogger.Log($"Opus Decoding Error: {ex.Message}", "ERROR");
        }
    }

    private async Task PlaybackRecordedBufferAsync(byte[] recordedData, CancellationToken ct)
    {
        Console.ForegroundColor = ConsoleColor.Magenta;
        Console.WriteLine($"[*] Test Loopback: Playback started. Total bytes to stream back: {recordedData.Length}");
        Console.ResetColor();
        FileLogger.Log($"Test Loopback: Playback task started. Total bytes to stream back: {recordedData.Length}");

        try
        {
            // Convert recordedData (PCM bytes) to PCM shorts
            int sampleCount = recordedData.Length / 2;
            short[] pcmShorts = new short[sampleCount];
            Buffer.BlockCopy(recordedData, 0, pcmShorts, 0, recordedData.Length);

            int frameSize = 960; // 20ms of 48kHz PCM mono
            int offset = 0;
            int framesSent = 0;

            byte[] outputBuffer = new byte[1275];
            short[] pcmFrame = new short[frameSize];

            while (offset < pcmShorts.Length && !ct.IsCancellationRequested)
            {
                int samplesToCopy = Math.Min(frameSize, pcmShorts.Length - offset);
                
                // If the last frame is partial, pad it with silence (zeros)
                if (samplesToCopy < frameSize)
                {
                    Array.Clear(pcmFrame, 0, frameSize);
                }
                Array.Copy(pcmShorts, offset, pcmFrame, 0, samplesToCopy);

                // 1. Play back locally to receiver speaker output (if any)
                byte[] chunk = new byte[samplesToCopy * 2];
                Buffer.BlockCopy(pcmFrame, 0, chunk, 0, chunk.Length);
                _audioSink.Write(chunk);

                // 2. Encode and stream back to the remote client over WebRTC
                int bytesEncoded = 0;
                try
                {
                    var encoder = _opusEncoder;
                    if (encoder != null)
                    {
                        lock (encoder)
                        {
                            bytesEncoded = encoder.Encode(pcmFrame, 0, frameSize, outputBuffer, 0, outputBuffer.Length);
                        }
                    }
                }
                catch (Exception encEx)
                {
                    FileLogger.Log($"Opus Encoding Error during loopback playback: {encEx.Message}", "ERROR");
                }

                if (bytesEncoded > 0 && _peerConnection != null)
                {
                    byte[] opusPacket = new byte[bytesEncoded];
                    Buffer.BlockCopy(outputBuffer, 0, opusPacket, 0, bytesEncoded);

                    _peerConnection.SendAudio(960, opusPacket);
                    framesSent++;
                }

                offset += frameSize;

                // Wait 20ms to match real-time audio playback speed
                await Task.Delay(20, ct);
            }

            if (!ct.IsCancellationRequested)
            {
                Console.ForegroundColor = ConsoleColor.Magenta;
                Console.WriteLine($"[*] Test Loopback: Playback finished. Sent {framesSent} audio frames.");
                Console.ResetColor();
                FileLogger.Log($"Test Loopback: Playback finished. Sent {framesSent} audio frames.");
            }
        }
        catch (OperationCanceledException)
        {
            Console.ForegroundColor = ConsoleColor.Magenta;
            Console.WriteLine("[*] Test Loopback: Playback cancelled.");
            Console.ResetColor();
            FileLogger.Log("Test Loopback: Playback cancelled.");
        }
        catch (Exception ex)
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine($"[!] Test Loopback Playback Error: {ex.Message}");
            Console.ResetColor();
            FileLogger.Log($"Test Loopback Playback Error: {ex.Message}", "ERROR", ex);
        }
        finally
        {
            _isPlayingBack = false;
        }
    }

    private async Task SendSignalingMessageAsync(SignalingMessage message)
    {
        if (_webSocket == null || _webSocket.State != WebSocketState.Open) return;

        try
        {
            string json = JsonSerializer.Serialize(message, _jsonOptions);
            byte[] bytes = Encoding.UTF8.GetBytes(json);
            
            await _webSocket.SendAsync(
                new ArraySegment<byte>(bytes),
                WebSocketMessageType.Text,
                true,
                CancellationToken.None
            );
        }
        catch (Exception ex)
        {
            Console.WriteLine($"[!] Failed to send signaling message: {ex.Message}");
        }
    }

    private void StartLoopbackCapture()
    {
        if (string.IsNullOrEmpty(_captureDeviceId))
        {
            Console.WriteLine("[*] Loopback Capture: No input device ID provided. Loopback stream will not start.");
            return;
        }

        try
        {
            StopLoopbackCapture();

            using var enumerator = new MMDeviceEnumerator();
            var captureDevice = enumerator.GetDevice(_captureDeviceId);
            
            Console.WriteLine($"[*] Initializing loopback capture device: {captureDevice.FriendlyName}");
            
            _wasapiCapture = new WasapiCapture(captureDevice);
            _wasapiCapture.DataAvailable += WasapiCapture_DataAvailable;
            _wasapiCapture.StartRecording();
            
            Console.ForegroundColor = ConsoleColor.Green;
            Console.WriteLine($"[+] Loopback Capture: Recording started successfully on {captureDevice.FriendlyName}");
            Console.ResetColor();
        }
        catch (Exception ex)
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine($"[!] Error starting loopback capture: {ex.Message}");
            Console.ResetColor();
        }
    }

    private void StopLoopbackCapture()
    {
        lock (_captureLock)
        {
            try
            {
                if (_wasapiCapture != null)
                {
                    Console.WriteLine("[*] Stopping loopback capture device...");
                    _wasapiCapture.StopRecording();
                    _wasapiCapture.DataAvailable -= WasapiCapture_DataAvailable;
                    _wasapiCapture.Dispose();
                    _wasapiCapture = null;
                    Console.WriteLine("[*] Loopback capture stopped and resources released.");
                }
            }
            catch (Exception ex)
            {
                Console.WriteLine($"[!] Error stopping loopback capture: {ex.Message}");
            }
            _capturePcmQueue.Clear();
        }
    }

    private void WasapiCapture_DataAvailable(object? sender, WaveInEventArgs e)
    {
        if (e.BytesRecorded == 0 || _opusEncoder == null || _peerConnection == null) return;

        if (_isTestMode && _isRecording)
        {
            var silenceElapsed = (DateTime.UtcNow - _lastActiveTime).TotalSeconds;
            if (silenceElapsed >= SilenceDurationLimitSeconds)
            {
                _isRecording = false;
                byte[] recordedBytes;
                lock (_recordingLock)
                {
                    recordedBytes = _recordedDataList.ToArray();
                    _recordedDataList.Clear();
                }

                if (recordedBytes.Length > 0)
                {
                    try
                    {
                        string filepath = Path.Combine(Directory.GetCurrentDirectory(), "loopback_test.wav");
                        using (var writer = new WaveFileWriter(filepath, new WaveFormat(48000, 16, 1)))
                        {
                            writer.Write(recordedBytes, 0, recordedBytes.Length);
                        }
                        Console.ForegroundColor = ConsoleColor.Magenta;
                        Console.WriteLine($"[*] Test Loopback: Silence detected ({silenceElapsed:F2}s). Recording stopped. Audio saved to: {filepath}");
                        Console.ResetColor();

                        // Trigger audio loopback playback
                        _playbackCts?.Cancel();
                        _playbackCts?.Dispose();
                        _isPlayingBack = true;
                        _playbackCts = new CancellationTokenSource();
                        FileLogger.Log($"Test Loopback: Silence detected ({silenceElapsed:F2}s). Starting loopback playback task with {recordedBytes.Length} bytes.");
                        _playbackTask = Task.Run(() => PlaybackRecordedBufferAsync(recordedBytes, _playbackCts.Token));
                    }
                    catch (Exception ex)
                    {
                        Console.ForegroundColor = ConsoleColor.Red;
                        Console.WriteLine($"[!] Failed to save or play loopback audio: {ex.Message}");
                        Console.ResetColor();
                    }
                }
                return;
            }
        }

        var waveFormat = _wasapiCapture?.WaveFormat;
        if (waveFormat == null) return;

        // Convert raw bytes to float samples based on bits per sample
        float[] floatSamples;
        int bitsPerSample = waveFormat.BitsPerSample;
        int channels = waveFormat.Channels;
        int sampleRate = waveFormat.SampleRate;

        if (bitsPerSample == 32)
        {
            int sampleCount = e.BytesRecorded / 4;
            floatSamples = new float[sampleCount];
            for (int i = 0; i < sampleCount; i++)
            {
                floatSamples[i] = BitConverter.ToSingle(e.Buffer, i * 4);
            }
        }
        else if (bitsPerSample == 16)
        {
            int sampleCount = e.BytesRecorded / 2;
            floatSamples = new float[sampleCount];
            for (int i = 0; i < sampleCount; i++)
            {
                floatSamples[i] = BitConverter.ToInt16(e.Buffer, i * 2) / 32768f;
            }
        }
        else
        {
            // Unsupported format
            return;
        }

        // Downmix to Mono if Stereo/Multi-channel
        float[] monoSamples;
        if (channels > 1)
        {
            int monoCount = floatSamples.Length / channels;
            monoSamples = new float[monoCount];
            for (int i = 0; i < monoCount; i++)
            {
                float sum = 0;
                for (int c = 0; c < channels; c++)
                {
                    sum += floatSamples[i * channels + c];
                }
                monoSamples[i] = sum / channels;
            }
        }
        else
        {
            monoSamples = floatSamples;
        }

        // Resample to 48000Hz via linear interpolation if necessary
        float[] resampledSamples;
        if (sampleRate != 48000)
        {
            double ratio = 48000.0 / sampleRate;
            int targetLength = (int)(monoSamples.Length * ratio);
            if (targetLength == 0) return;
            
            resampledSamples = new float[targetLength];
            for (int i = 0; i < targetLength; i++)
            {
                double sourceIndex = i / ratio;
                int idx1 = (int)Math.Floor(sourceIndex);
                int idx2 = idx1 + 1;
                if (idx1 >= monoSamples.Length) idx1 = monoSamples.Length - 1;
                if (idx2 >= monoSamples.Length) idx2 = monoSamples.Length - 1;
                double weight = sourceIndex - idx1;
                resampledSamples[i] = (float)((1.0 - weight) * monoSamples[idx1] + weight * monoSamples[idx2]);
            }
        }
        else
        {
            resampledSamples = monoSamples;
        }

        // Convert float samples to 16-bit PCM shorts
        short[] pcmShorts = new short[resampledSamples.Length];
        for (int i = 0; i < resampledSamples.Length; i++)
        {
            float val = resampledSamples[i];
            if (val > 1.0f) val = 1.0f;
            if (val < -1.0f) val = -1.0f;
            pcmShorts[i] = (short)(val * short.MaxValue);
        }

        if (_isTestMode)
        {
            if (_isRecording)
            {
                lock (_recordingLock)
                {
                    byte[] pcmBytes = new byte[pcmShorts.Length * 2];
                    Buffer.BlockCopy(pcmShorts, 0, pcmBytes, 0, pcmBytes.Length);
                    _recordedDataList.AddRange(pcmBytes);

                    // Limit recording to 30 seconds max to prevent memory leakage
                    if (_recordedDataList.Count >= 48000 * 2 * 30)
                    {
                        _isRecording = false;
                        byte[] recordedBytes = _recordedDataList.ToArray();
                        _recordedDataList.Clear();

                        try
                        {
                            string filepath = Path.Combine(Directory.GetCurrentDirectory(), "loopback_test.wav");
                            using (var writer = new WaveFileWriter(filepath, new WaveFormat(48000, 16, 1)))
                            {
                                writer.Write(recordedBytes, 0, recordedBytes.Length);
                            }
                            Console.ForegroundColor = ConsoleColor.Magenta;
                            Console.WriteLine($"[*] Test Loopback: Maximum recording duration reached (30s). Recording stopped. Audio saved to: {filepath}");
                            Console.ResetColor();

                            // Trigger audio loopback playback
                            _playbackCts?.Cancel();
                            _playbackCts?.Dispose();
                            _isPlayingBack = true;
                            _playbackCts = new CancellationTokenSource();
                            FileLogger.Log($"Test Loopback: Maximum recording duration reached (30s). Starting loopback playback task with {recordedBytes.Length} bytes.");
                            _playbackTask = Task.Run(() => PlaybackRecordedBufferAsync(recordedBytes, _playbackCts.Token));
                        }
                        catch (Exception ex)
                        {
                            Console.ForegroundColor = ConsoleColor.Red;
                            Console.WriteLine($"[!] Failed to save or play loopback audio: {ex.Message}");
                            Console.ResetColor();
                        }
                    }
                }
            }

            // Send silent Opus packets back to the client browser while NOT playing back
            // to keep the WebRTC connection active and prevent ICE/DTLS timeouts.
            if (!_isPlayingBack)
            {
                lock (_captureLock)
                {
                    foreach (var sample in pcmShorts)
                    {
                        _capturePcmQueue.Enqueue(sample);
                    }

                    int frameSize = 960;
                    byte[] silentOpusPacket = new byte[] { 0xf8, 0xff, 0xfe };

                    while (_capturePcmQueue.Count >= frameSize)
                    {
                        for (int i = 0; i < frameSize; i++)
                        {
                            _capturePcmQueue.Dequeue();
                        }
                        _peerConnection?.SendAudio(960, silentOpusPacket);
                    }
                }
            }
            else
            {
                lock (_captureLock)
                {
                    _capturePcmQueue.Clear();
                }
            }
            return; // Skip streaming back to the client during test loopback
        }

        // Enqueue PCM short samples and feed the encoder in 20ms chunks (960 samples @ 48kHz Mono)
        lock (_captureLock)
        {
            // Safeguard against accumulation of lag
            if (_capturePcmQueue.Count > 48000)
            {
                _capturePcmQueue.Clear();
            }

            foreach (var sample in pcmShorts)
            {
                _capturePcmQueue.Enqueue(sample);
            }

            int frameSize = 960;
            byte[] outputBuffer = new byte[1275];
            short[] pcmFrame = new short[frameSize];

            while (_capturePcmQueue.Count >= frameSize)
            {
                for (int i = 0; i < frameSize; i++)
                {
                    pcmFrame[i] = _capturePcmQueue.Dequeue();
                }

                try
                {
                    int bytesEncoded = _opusEncoder.Encode(pcmFrame, 0, frameSize, outputBuffer, 0, outputBuffer.Length);
                    if (bytesEncoded > 0)
                    {
                        byte[] opusPacket = new byte[bytesEncoded];
                        Buffer.BlockCopy(outputBuffer, 0, opusPacket, 0, bytesEncoded);

                        // Stream audio back on the connection
                        _peerConnection?.SendAudio(960, opusPacket);
                    }
                }
                catch (Exception ex)
                {
                    Console.WriteLine($"[!] Opus Encoding / Loopback capture error: {ex.Message}");
                }
            }
        }
    }

    public void Dispose()
    {
        _testPlaybackSink?.Dispose();
        _testPlaybackSink = null;

        _playbackCts?.Cancel();
        _playbackCts?.Dispose();

        _cts?.Cancel();
        _cts?.Dispose();
        
        _peerConnection?.Close("disposing");
        _peerConnection?.Dispose();
        
        StopLoopbackCapture();
        _opusEncoder = null;
        
        if (_webSocket != null)
        {
            try
            {
                if (_webSocket.State == WebSocketState.Open)
                {
                    _webSocket.CloseAsync(WebSocketCloseStatus.NormalClosure, "Disposing", CancellationToken.None).Wait(2000);
                }
            }
            catch
            {
                // Ignore socket close exceptions during cleanup
            }
            _webSocket.Dispose();
        }
        
        GC.SuppressFinalize(this);
    }

    private string? GetCounterpartRenderDeviceId(string captureDeviceId)
    {
        try
        {
            using var enumerator = new MMDeviceEnumerator();
            var captureDevice = enumerator.GetDevice(captureDeviceId);
            string captureName = captureDevice.FriendlyName;

            string targetNamePart = "";
            if (captureName.Contains("CABLE-B Output", StringComparison.OrdinalIgnoreCase)) 
                targetNamePart = "CABLE-B Input";
            else if (captureName.Contains("CABLE Output", StringComparison.OrdinalIgnoreCase)) 
                targetNamePart = "CABLE Input";
            else 
                return null;

            var renderDevices = enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active);
            foreach (var dev in renderDevices)
            {
                if (dev.FriendlyName.Contains(targetNamePart, StringComparison.OrdinalIgnoreCase))
                {
                    return dev.ID;
                }
            }
        }
        catch {}
        return null;
    }
}

/// <summary>
/// Simple zero-dependency logger factory to print SIPSorcery logs to console.
/// </summary>
public class SimpleConsoleLoggerFactory : ILoggerFactory
{
    private readonly List<ILoggerProvider> _providers = new List<ILoggerProvider>();

    public void AddProvider(ILoggerProvider provider)
    {
        _providers.Add(provider);
    }

    public ILogger CreateLogger(string categoryName)
    {
        return new SimpleConsoleLogger(categoryName);
    }

    public void Dispose()
    {
        foreach (var provider in _providers)
        {
            provider.Dispose();
        }
    }
}

/// <summary>
/// Simple zero-dependency logger provider to print SIPSorcery logs to console.
/// </summary>
public class SimpleConsoleLoggerProvider : ILoggerProvider
{
    public ILogger CreateLogger(string categoryName) => new SimpleConsoleLogger(categoryName);
    public void Dispose() {}
}

public class SimpleConsoleLogger : ILogger
{
    private readonly string _categoryName;
    public SimpleConsoleLogger(string categoryName) => _categoryName = categoryName;

    public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;
    public bool IsEnabled(LogLevel logLevel) => true;

    public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception, Func<TState, Exception?, string> formatter)
    {
        var message = formatter(state, exception);
        // Write SIPSorcery logs to the log file instead of cluttering the console
        if (logLevel >= LogLevel.Debug)
        {
            FileLogger.Log(message, $"SIPSorcery:{logLevel}", exception);
        }
    }
}

