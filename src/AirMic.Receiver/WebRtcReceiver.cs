#pragma warning disable CA1416
using System;
using System.Collections.Concurrent;
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
using Serilog;

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
    
    private readonly short[] _pcmBuffer = new short[11520]; // Max Opus frame duration is 120ms (5760 samples @ 48kHz per channel)
    private readonly Queue<short> _capturePcmQueue = new Queue<short>();
    private readonly object _captureLock = new object();
    private int _channels = 1;
    private bool _optimizeForVoice = true;

    private BlockingCollection<byte[]>? _captureQueue;
    private Task? _captureProcessingTask;
    private CancellationTokenSource? _captureProcessingCts;

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
        _opusDecoder = new OpusDecoder(48000, _channels);
        _opusEncoder = new OpusEncoder(48000, _channels, OpusApplication.OPUS_APPLICATION_VOIP);
        _opusEncoder.Bitrate = 24000;
        
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
                string msg = $"[!] Connection attempt {retryCount}/{maxRetries} failed: {ex.Message}. Retrying in 2 seconds...";
                AppLogger.Warning(ex, msg);
                await Task.Delay(2000, _cts.Token);
                _webSocket.Dispose();
                _webSocket = createWebSocket();
            }
        }
        AppLogger.Success("[+] Connected to signaling server.");

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
                    AppLogger.Warning("[-] Signaling WebSocket closed by remote server.");
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
                    string msg = $"[!] Error parsing signaling message: {ex.Message}";
                    AppLogger.Error(ex, msg);
                }
            }
        }
        catch (Exception ex)
        {
            if (!cancellationToken.IsCancellationRequested)
            {
                string msg = $"[!] Exception in signaling loop: {ex.Message}";
                AppLogger.Error(ex, msg);
                OnDisconnected?.Invoke($"Signaling loop exception: {ex.Message}");
            }
        }
    }

    private async Task HandleSignalingMessageAsync(SignalingMessage message)
    {
        if (message.Type == "offer")
        {
            Log.Debug("[*] Received SDP Offer. Preparing WebRTC connection...");
            _isTestMode = message.IsTest == true;
            _optimizeForVoice = message.OptimizeForVoice ?? true;
            _channels = _optimizeForVoice ? 1 : 2;

            Log.Debug("Connection Audio Parameters: optimizeForVoice = {OptimizeForVoice}, channels = {Channels}", _optimizeForVoice, _channels);

            // Configure dynamic encoder/decoder settings
            _opusDecoder = new OpusDecoder(48000, _channels);
            _opusEncoder = new OpusEncoder(48000, _channels, _optimizeForVoice ? OpusApplication.OPUS_APPLICATION_VOIP : OpusApplication.OPUS_APPLICATION_AUDIO);
            _opusEncoder.Bitrate = _optimizeForVoice ? 24000 : 64000;

            // Apply dynamic buffer splits (latency budget thresholds) to WASAPI output sink
            if (_optimizeForVoice)
            {
                // Voice mode: Max backlog = 200ms, Cushion = 120ms (hysteresis buffering for jitter stability)
                _audioSink.ConfigureBufferThresholds(0.200, 0.120);
            }
            else
            {
                // Music mode: Max backlog = 300ms, Cushion = 135ms (prioritize stability / zero drops over latency)
                _audioSink.ConfigureBufferThresholds(0.300, 0.135);
            }

            _testPlaybackSink?.Dispose();
            _testPlaybackSink = null;
            if (_isTestMode)
            {
                AppLogger.Test("[*] Test Mode activated for this WebRTC session.");

                // Initialize counterpart rendering sink if virtual cable matches
                if (!string.IsNullOrEmpty(_captureDeviceId))
                {
                    string? counterpartId = GetCounterpartRenderDeviceId(_captureDeviceId);
                    if (counterpartId != null)
                    {
                        try
                        {
                            var tempSink = new WasapiExclusiveSink();
                            tempSink.Initialize(48000, 16, 2, counterpartId); // Initialize in stereo

                            // Apply matching buffer splits to the test loopback sink
                            if (_optimizeForVoice)
                            {
                                tempSink.ConfigureBufferThresholds(0.200, 0.120);
                            }
                            else
                            {
                                tempSink.ConfigureBufferThresholds(0.300, 0.135);
                            }

                            tempSink.Start();
                            _testPlaybackSink = tempSink;
                            Log.Debug("[*] Test Loopback: Created matching output sink for counterpart rendering device.");
                        }
                        catch (Exception ex)
                        {
                            string msg = $"[!] Warning: Failed to create test counterpart sink: {ex.Message}. Falling back to default output sink.";
                            AppLogger.Warning(ex, msg);
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
                Log.Debug("[WebRTC] Connection state: {State}", state);
                if (state == RTCPeerConnectionState.connected)
                {
                    AppLogger.Success("[+] WebRTC Audio Stream established successfully!");
                    if (_isTestMode)
                    {
                        StartLoopbackCapture();
                    }
                }
                else if (state == RTCPeerConnectionState.closed || state == RTCPeerConnectionState.failed)
                {
                    AppLogger.Error("[-] WebRTC Audio connection lost.");
                    StopLoopbackCapture();
                    OnDisconnected?.Invoke("WebRTC connection lost.");
                }
            };

            _peerConnection.onicecandidate += async (candidate) =>
            {
                if (candidate != null && !string.IsNullOrEmpty(candidate.candidate))
                {
                    Log.Debug("Gathered local ICE candidate: {Candidate}", candidate.candidate);
                    
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

            // Add audio track to peer connection configured with stable parameters to prevent jitter and choppiness.
            // - ptime=20, minptime=20: 20ms packet sizes for network stability (halves packet overhead, avoids jitter).
            // - usedtx=0: disables discontinuous transmission to prevent audio clipping/cutting out.
            // - useinbandfec=1: keeps forward error correction enabled.
            // - stereo/sprop-stereo: dynamically toggled for high quality music/voice versus mono.
            // - maxaveragebitrate: 32kbps for voice-optimized, 128kbps for stereo music.
            string opusParams = _optimizeForVoice 
                ? "minptime=20;useinbandfec=1;stereo=0;sprop-stereo=0;usedtx=0;maxaveragebitrate=24000;maxplaybackrate=48000;sprop-maxcapturerate=48000;ptime=20"
                : "minptime=20;useinbandfec=1;stereo=1;sprop-stereo=1;usedtx=0;maxaveragebitrate=64000;maxplaybackrate=48000;sprop-maxcapturerate=48000;ptime=20";

            var audioFormat = new SDPAudioVideoMediaFormat(
                SDPMediaTypesEnum.audio, 
                111, 
                "OPUS", 
                48000, 
                2, 
                opusParams
            );
            var audioTrack = new MediaStreamTrack(SDPMediaTypesEnum.audio, false, new List<SDPAudioVideoMediaFormat> { audioFormat }, MediaStreamStatusEnum.SendRecv);
            _peerConnection.addTrack(audioTrack);

            // Set Remote SDP Offer
            var sdpInit = new RTCSessionDescriptionInit
            {
                type = RTCSdpType.offer,
                sdp = message.Sdp
            };
            
            Log.Debug("Remote SDP Offer:\n{Sdp}", message.Sdp);

            var result = _peerConnection.setRemoteDescription(sdpInit);
            if (result != SetDescriptionResultEnum.OK)
            {
                string msg = $"[!] Failed to set remote description: {result}";
                Log.Error("{Message}", msg);
                return;
            }

            // Create Local SDP Answer
            var answer = _peerConnection.createAnswer();
            
            // Force dynamic Opus settings on the SDP Answer to negotiate correct encoder/decoder parameters
            string sdp = answer.sdp;
            var sdpMatch = Regex.Match(sdp, @"a=rtpmap:(\d+)\s+opus/48000/2", RegexOptions.IgnoreCase);
            if (sdpMatch.Success)
            {
                string pt = sdpMatch.Groups[1].Value;
                string targetFmtp = $"a=fmtp:{pt} {opusParams}";
                
                if (sdp.Contains($"a=fmtp:{pt}"))
                {
                    sdp = Regex.Replace(sdp, $@"a=fmtp:{pt}\s+.*", targetFmtp);
                }
                else
                {
                    sdp = sdp.Replace($"a=rtpmap:{pt} opus/48000/2", $"a=rtpmap:{pt} opus/48000/2\r\n{targetFmtp}");
                }
                Log.Debug("SDP Answer: Forced Opus payload {Pt} params -> {OpusParams}", pt, opusParams);
            }
            answer.sdp = sdp;

            await _peerConnection.setLocalDescription(answer);

            Log.Debug("Local SDP Answer:\n{Sdp}", answer.sdp);

            // Send SDP Answer back to signaling channel
            var ansMsg = new SignalingMessage
            {
                Type = "answer",
                Sdp = answer.sdp
            };

            await SendSignalingMessageAsync(ansMsg);
            Log.Debug("[*] Sent SDP Answer to signaling client.");
        }
        else if (message.Type == "candidate")
        {
            if (_peerConnection != null && message.Candidate != null)
            {
                Log.Debug("Received remote ICE Candidate: {Candidate}", message.Candidate.Candidate);
                
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
                int byteCount = decodedSamples * _channels * 2; // 16-bit short = 2 bytes
                byte[] pcmBytes = new byte[byteCount];
                Buffer.BlockCopy(_pcmBuffer, 0, pcmBytes, 0, byteCount);

                // Upmix Mono to Stereo if connection is Mono but target output device is Stereo
                if (_channels == 1)
                {
                    byte[] stereoBytes = new byte[pcmBytes.Length * 2];
                    for (int i = 0; i < pcmBytes.Length; i += 2)
                    {
                        stereoBytes[2 * i] = pcmBytes[i];
                        stereoBytes[2 * i + 1] = pcmBytes[i + 1];
                        stereoBytes[2 * i + 2] = pcmBytes[i];
                        stereoBytes[2 * i + 3] = pcmBytes[i + 1];
                    }
                    pcmBytes = stereoBytes;
                }
                
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

                        if (!_isRecording && !_isPlayingBack)
                        {
                            AppLogger.Test("[*] Test Loopback: Client speech detected. Recording started.");

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
            Log.Debug(ex, "Opus Decoding Error: {Message}", ex.Message);
        }
    }

    private async Task PlaybackRecordedBufferAsync(byte[] recordedData, CancellationToken ct)
    {
        AppLogger.Test($"[*] Test Loopback: Playback started. Total bytes to stream back: {recordedData.Length}");

        try
        {
            // Convert recordedData (PCM bytes) to PCM shorts
            int sampleCount = recordedData.Length / 2;
            short[] pcmShorts = new short[sampleCount];
            Buffer.BlockCopy(recordedData, 0, pcmShorts, 0, recordedData.Length);

            int frameSize = 960;
            int frameSamples = frameSize * _channels;
            int offset = 0;
            int framesSent = 0;

            byte[] outputBuffer = new byte[1275];
            short[] pcmFrame = new short[frameSamples];

            var stopwatch = System.Diagnostics.Stopwatch.StartNew();
            int frameDurationMs = 20;

            while (offset < pcmShorts.Length && !ct.IsCancellationRequested)
            {
                int samplesToCopy = Math.Min(frameSamples, pcmShorts.Length - offset);
                
                // If the last frame is partial, pad it with silence (zeros)
                if (samplesToCopy < frameSamples)
                {
                    Array.Clear(pcmFrame, 0, frameSamples);
                }
                Array.Copy(pcmShorts, offset, pcmFrame, 0, samplesToCopy);

                // 1. Play back locally to receiver speaker output (if any)
                byte[] chunk = new byte[samplesToCopy * 2];
                Buffer.BlockCopy(pcmFrame, 0, chunk, 0, chunk.Length);

                if (_channels == 1)
                {
                    byte[] stereoBytes = new byte[chunk.Length * 2];
                    for (int i = 0; i < chunk.Length; i += 2)
                    {
                        stereoBytes[2 * i] = chunk[i];
                        stereoBytes[2 * i + 1] = chunk[i + 1];
                        stereoBytes[2 * i + 2] = chunk[i];
                        stereoBytes[2 * i + 3] = chunk[i + 1];
                    }
                    chunk = stereoBytes;
                }
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
                    Log.Debug(encEx, "Opus Encoding Error during loopback playback: {Message}", encEx.Message);
                }

                if (bytesEncoded > 0 && _peerConnection != null)
                {
                    byte[] opusPacket = new byte[bytesEncoded];
                    Buffer.BlockCopy(outputBuffer, 0, opusPacket, 0, bytesEncoded);

                    _peerConnection.SendAudio(960, opusPacket);
                }

                framesSent++;
                offset += frameSamples;

                // Calculate next wake time using high-precision stopwatch timer
                double nextWakeTimeMs = framesSent * frameDurationMs;
                double elapsedMs = stopwatch.Elapsed.TotalMilliseconds;
                double sleepTimeMs = nextWakeTimeMs - elapsedMs;

                if (sleepTimeMs > 0)
                {
                    if (sleepTimeMs > 15)
                    {
                        await Task.Delay((int)sleepTimeMs, ct);
                    }

                    while (stopwatch.Elapsed.TotalMilliseconds < nextWakeTimeMs && !ct.IsCancellationRequested)
                    {
                        Thread.Yield();
                    }
                }
            }

            if (!ct.IsCancellationRequested)
            {
                AppLogger.Test($"[*] Test Loopback: Playback finished. Sent {framesSent} audio frames.");
            }
        }
        catch (OperationCanceledException)
        {
            AppLogger.Test("[*] Test Loopback: Playback cancelled.");
        }
        catch (Exception ex)
        {
            AppLogger.Error(ex, $"[!] Test Loopback Playback Error: {ex.Message}");
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
            string msg = $"[!] Failed to send signaling message: {ex.Message}";
            AppLogger.Error(ex, msg);
        }
    }

    private void StartLoopbackCapture()
    {
        if (string.IsNullOrEmpty(_captureDeviceId))
        {
            AppLogger.Warning("[*] Loopback Capture: No input device ID provided. Loopback stream will not start.");
            return;
        }

        try
        {
            StopLoopbackCapture();

            using var enumerator = new MMDeviceEnumerator();
            var captureDevice = enumerator.GetDevice(_captureDeviceId);
            
            Log.Debug("[*] Initializing loopback capture device: {CaptureDeviceName}", captureDevice.FriendlyName);
            
            _captureQueue = new BlockingCollection<byte[]>();
            _captureProcessingCts = new CancellationTokenSource();
            _captureProcessingTask = Task.Run(() => ProcessCaptureQueueLoop(_captureProcessingCts.Token));

            _wasapiCapture = new WasapiCapture(captureDevice);
            _wasapiCapture.DataAvailable += WasapiCapture_DataAvailable;
            _wasapiCapture.StartRecording();
            
            Log.Debug("[+] Loopback Capture: Recording started successfully on {CaptureDeviceName}", captureDevice.FriendlyName);
        }
        catch (Exception ex)
        {
            string msg = $"[!] Error starting loopback playback task: {ex.Message}";
            AppLogger.Error(ex, msg);
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
                    Log.Debug("[*] Stopping loopback capture device...");
                    _wasapiCapture.StopRecording();
                    _wasapiCapture.DataAvailable -= WasapiCapture_DataAvailable;
                    _wasapiCapture.Dispose();
                    _wasapiCapture = null;
                    Log.Debug("[*] Loopback capture stopped and resources released.");
                }
            }
            catch (Exception ex)
            {
                string msg = $"[!] Error stopping loopback capture: {ex.Message}";
                AppLogger.Error(ex, msg);
            }

            try
            {
                if (_captureProcessingCts != null)
                {
                    _captureProcessingCts.Cancel();
                    _captureProcessingCts.Dispose();
                    _captureProcessingCts = null;
                }
                if (_captureQueue != null)
                {
                    _captureQueue.CompleteAdding();
                    _captureQueue.Dispose();
                    _captureQueue = null;
                }
                if (_captureProcessingTask != null)
                {
                    _captureProcessingTask.Wait(1000);
                    _captureProcessingTask = null;
                }
            }
            catch (Exception ex)
            {
                string msg = $"[!] Error stopping loopback capture background loop: {ex.Message}";
                AppLogger.Error(ex, msg);
            }

            _capturePcmQueue.Clear();
        }
    }

    private void WasapiCapture_DataAvailable(object? sender, WaveInEventArgs e)
    {
        if (e.BytesRecorded == 0 || _captureQueue == null || _captureQueue.IsAddingCompleted) return;

        try
        {
            byte[] bufferCopy = new byte[e.BytesRecorded];
            Buffer.BlockCopy(e.Buffer, 0, bufferCopy, 0, e.BytesRecorded);
            _captureQueue.Add(bufferCopy);
        }
        catch
        {
            // Ignore queue addition errors during shutdown
        }
    }

    private async Task HandleLoopbackPlaybackTriggerAsync(byte[] recordedBytes, double silenceElapsed)
    {
        try
        {
            string filepath = Path.Combine(Directory.GetCurrentDirectory(), "loopback_test.wav");
            using (var writer = new WaveFileWriter(filepath, new WaveFormat(48000, 16, _channels)))
            {
                writer.Write(recordedBytes, 0, recordedBytes.Length);
            }
            Console.ForegroundColor = ConsoleColor.Magenta;
            AppLogger.Test($"[*] Test Loopback: Silence detected ({silenceElapsed:F2}s) or limit reached. Recording stopped. Audio saved to: {filepath}");
            Console.ResetColor();

            _playbackCts?.Cancel();
            _playbackCts?.Dispose();
            _isPlayingBack = true;
            _playbackCts = new CancellationTokenSource();
            Log.Debug("Test Loopback: Silence detected ({SilenceElapsed:F2}s) or limit reached. Starting loopback playback task with {RecordedBytesLength} bytes.", silenceElapsed, recordedBytes.Length);
            _playbackTask = Task.Run(() => PlaybackRecordedBufferAsync(recordedBytes, _playbackCts.Token));
        }
        catch (Exception ex)
        {
            string msg = $"[!] Failed to save or play loopback audio: {ex.Message}";
            AppLogger.Error(ex, msg);
        }
    }

    private void ProcessCaptureQueueLoop(CancellationToken ct)
    {
        var queue = _captureQueue;
        if (queue == null) return;

        try
        {
            foreach (var buffer in queue.GetConsumingEnumerable(ct))
            {
                if (ct.IsCancellationRequested || _opusEncoder == null || _peerConnection == null) continue;
                
                var waveFormat = _wasapiCapture?.WaveFormat;
                if (waveFormat == null) continue;

                int bitsPerSample = waveFormat.BitsPerSample;
                int channels = waveFormat.Channels;
                int sampleRate = waveFormat.SampleRate;

                float[] floatSamples;
                if (bitsPerSample == 32)
                {
                    int sampleCount = buffer.Length / 4;
                    floatSamples = new float[sampleCount];
                    for (int i = 0; i < sampleCount; i++)
                    {
                        floatSamples[i] = BitConverter.ToSingle(buffer, i * 4);
                    }
                }
                else if (bitsPerSample == 16)
                {
                    int sampleCount = buffer.Length / 2;
                    floatSamples = new float[sampleCount];
                    for (int i = 0; i < sampleCount; i++)
                    {
                        floatSamples[i] = BitConverter.ToInt16(buffer, i * 2) / 32768f;
                    }
                }
                else
                {
                    continue;
                }

                // Downmix/Upmix to _channels layout
                float[] targetChannelsSamples;
                if (channels != _channels)
                {
                    if (_channels == 1)
                    {
                        // Downmix to Mono
                        int monoCount = floatSamples.Length / channels;
                        targetChannelsSamples = new float[monoCount];
                        for (int i = 0; i < monoCount; i++)
                        {
                            float sum = 0;
                            for (int c = 0; c < channels; c++)
                            {
                                sum += floatSamples[i * channels + c];
                            }
                            targetChannelsSamples[i] = sum / channels;
                        }
                    }
                    else // _channels == 2
                    {
                        // Upmix Mono to Stereo
                        if (channels == 1)
                        {
                            targetChannelsSamples = new float[floatSamples.Length * 2];
                            for (int i = 0; i < floatSamples.Length; i++)
                            {
                                targetChannelsSamples[i * 2] = floatSamples[i];
                                targetChannelsSamples[i * 2 + 1] = floatSamples[i];
                            }
                        }
                        else
                        {
                            // Multi-channel downmix to stereo
                            int frameCount = floatSamples.Length / channels;
                            targetChannelsSamples = new float[frameCount * 2];
                            for (int i = 0; i < frameCount; i++)
                            {
                                targetChannelsSamples[i * 2] = floatSamples[i * channels];
                                targetChannelsSamples[i * 2 + 1] = floatSamples[i * channels + 1];
                            }
                        }
                    }
                }
                else
                {
                    targetChannelsSamples = floatSamples;
                }

                // Resample to 48000Hz via linear interpolation if necessary
                float[] resampledSamples;
                if (sampleRate != 48000)
                {
                    double ratio = 48000.0 / sampleRate;
                    int sourceFrames = targetChannelsSamples.Length / _channels;
                    int targetFrames = (int)(sourceFrames * ratio);
                    if (targetFrames == 0) continue;
                    
                    resampledSamples = new float[targetFrames * _channels];
                    for (int i = 0; i < targetFrames; i++)
                    {
                        double sourceFrameIndex = i / ratio;
                        int frame1 = (int)Math.Floor(sourceFrameIndex);
                        int frame2 = frame1 + 1;
                        if (frame1 >= sourceFrames) frame1 = sourceFrames - 1;
                        if (frame2 >= sourceFrames) frame2 = sourceFrames - 1;
                        double weight = sourceFrameIndex - frame1;
                        
                        for (int c = 0; c < _channels; c++)
                        {
                            float val1 = targetChannelsSamples[frame1 * _channels + c];
                            float val2 = targetChannelsSamples[frame2 * _channels + c];
                            resampledSamples[i * _channels + c] = (float)((1.0 - weight) * val1 + weight * val2);
                        }
                    }
                }
                else
                {
                    resampledSamples = targetChannelsSamples;
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
                            if (_recordedDataList.Count >= 48000 * _channels * 2 * 30)
                            {
                                _isRecording = false;
                                byte[] recordedBytes = _recordedDataList.ToArray();
                                _recordedDataList.Clear();

                                _ = Task.Run(() => HandleLoopbackPlaybackTriggerAsync(recordedBytes, 30.0));
                            }
                        }
                    }

                    // Check silence threshold to trigger loopback test playback
                    if (_isRecording)
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
                                _ = Task.Run(() => HandleLoopbackPlaybackTriggerAsync(recordedBytes, silenceElapsed));
                            }
                            continue;
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
                            int frameSamples = frameSize * _channels;
                            byte[] silentOpusPacket = _channels == 1 ? new byte[] { 0xf8, 0xff, 0xfe } : new byte[] { 0xfc, 0xff, 0xfe };

                            while (_capturePcmQueue.Count >= frameSamples)
                            {
                                for (int i = 0; i < frameSamples; i++)
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
                    continue;
                }

                // Enqueue PCM short samples and feed the encoder in 20ms chunks (960 samples @ 48kHz Mono/Stereo)
                lock (_captureLock)
                {
                    // Safeguard against accumulation of lag
                    if (_capturePcmQueue.Count > 48000 * _channels)
                    {
                        _capturePcmQueue.Clear();
                    }

                    foreach (var sample in pcmShorts)
                    {
                        _capturePcmQueue.Enqueue(sample);
                    }

                    int frameSize = 960;
                    int frameSamples = frameSize * _channels;
                    byte[] outputBuffer = new byte[1275];
                    short[] pcmFrame = new short[frameSamples];

                    while (_capturePcmQueue.Count >= frameSamples)
                    {
                        for (int i = 0; i < frameSamples; i++)
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
                                _peerConnection?.SendAudio(960, opusPacket);
                            }
                        }
                        catch (Exception ex)
                        {
                            string msg = $"[!] Opus Encoding / Loopback capture error: {ex.Message}";
                            AppLogger.Error(ex, msg);
                        }
                    }
                }
            }
        }
        catch (OperationCanceledException) {}
        catch (Exception ex)
        {
            Log.Error(ex, "Error in ProcessCaptureQueueLoop: {Message}", ex.Message);
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

    public Microsoft.Extensions.Logging.ILogger CreateLogger(string categoryName)
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
    public Microsoft.Extensions.Logging.ILogger CreateLogger(string categoryName) => new SimpleConsoleLogger(categoryName);
    public void Dispose() {}
}

public class SimpleConsoleLogger : Microsoft.Extensions.Logging.ILogger
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
            var serilogLevel = ConvertToSerilogLevel(logLevel);
            Serilog.Log.ForContext("SourceContext", _categoryName)
                       .Write(serilogLevel, exception, "{Message:lj}", message);
        }
    }

    private Serilog.Events.LogEventLevel ConvertToSerilogLevel(LogLevel logLevel)
    {
        return logLevel switch
        {
            LogLevel.Trace => Serilog.Events.LogEventLevel.Verbose,
            LogLevel.Debug => Serilog.Events.LogEventLevel.Debug,
            LogLevel.Information => Serilog.Events.LogEventLevel.Information,
            LogLevel.Warning => Serilog.Events.LogEventLevel.Warning,
            LogLevel.Error => Serilog.Events.LogEventLevel.Error,
            LogLevel.Critical => Serilog.Events.LogEventLevel.Fatal,
            _ => Serilog.Events.LogEventLevel.Information
        };
    }
}

