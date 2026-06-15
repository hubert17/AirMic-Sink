using System;
using System.Collections.Generic;
using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using SIPSorcery.Net;
using Concentus.Structs;
using AirMic.Contracts;
using Microsoft.Extensions.Logging;
using System.Text.RegularExpressions;

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
    
    private ClientWebSocket? _webSocket;
    private RTCPeerConnection? _peerConnection;
    private OpusDecoder? _opusDecoder;
    private CancellationTokenSource? _cts;
    
    private readonly short[] _pcmBuffer = new short[5760]; // Max Opus frame duration is 120ms (5760 samples @ 48kHz)

    public event Action<string>? OnDisconnected;

    public WebRtcReceiver(string signalingUrl, string streamSecret, IAudioBufferSink audioSink)
    {
        _signalingUrl = signalingUrl;
        _streamSecret = streamSecret;
        _audioSink = audioSink;
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
                }
                else if (state == RTCPeerConnectionState.closed || state == RTCPeerConnectionState.failed)
                {
                    Console.ForegroundColor = ConsoleColor.Red;
                    Console.WriteLine("[-] WebRTC Audio connection lost.");
                    Console.ResetColor();
                    OnDisconnected?.Invoke("WebRTC connection lost.");
                }
            };

            _peerConnection.onicecandidate += async (candidate) =>
            {
                if (candidate != null && !string.IsNullOrEmpty(candidate.candidate))
                {
                    Console.WriteLine($"[DEBUG] Gathered local ICE candidate: {candidate.candidate}");
                    
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
            var audioFormat = new SDPAudioVideoMediaFormat(
                SDPMediaTypesEnum.audio, 
                111, 
                "OPUS", 
                48000, 
                2, 
                "minptime=10;useinbandfec=1;stereo=0;sprop-stereo=0"
            );
            var audioTrack = new MediaStreamTrack(SDPMediaTypesEnum.audio, false, new List<SDPAudioVideoMediaFormat> { audioFormat }, MediaStreamStatusEnum.RecvOnly);
            _peerConnection.addTrack(audioTrack);

            // Set Remote SDP Offer
            var sdpInit = new RTCSessionDescriptionInit
            {
                type = RTCSdpType.offer,
                sdp = message.Sdp
            };
            
            Console.WriteLine($"[DEBUG] Remote SDP Offer:\n{message.Sdp}\n");

            var result = _peerConnection.setRemoteDescription(sdpInit);
            if (result != SetDescriptionResultEnum.OK)
            {
                Console.WriteLine($"[!] Failed to set remote description: {result}");
                return;
            }

            // Create Local SDP Answer
            var answer = _peerConnection.createAnswer();
            await _peerConnection.setLocalDescription(answer);

            Console.WriteLine($"[DEBUG] Local SDP Answer:\n{answer.sdp}\n");

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
                Console.WriteLine($"[DEBUG] Received remote ICE Candidate: {message.Candidate.Candidate}");
                
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
                _audioSink.Write(pcmBytes);
            }
        }
        catch (Exception ex)
        {
            // Log occasionally or drop silently to avoid console flooding during high packet rates
            Console.WriteLine($"[!] Opus Decoding Error: {ex.Message}");
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

    public void Dispose()
    {
        _cts?.Cancel();
        _cts?.Dispose();
        
        _peerConnection?.Close("disposing");
        _peerConnection?.Dispose();
        
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
        // Only show debug/warning/error logs of SIPSorcery to avoid cluttering the screen with info traces
        if (logLevel >= LogLevel.Debug)
        {
            var color = Console.ForegroundColor;
            if (logLevel == LogLevel.Error || logLevel == LogLevel.Critical) Console.ForegroundColor = ConsoleColor.Red;
            else if (logLevel == LogLevel.Warning) Console.ForegroundColor = ConsoleColor.Yellow;
            else Console.ForegroundColor = ConsoleColor.DarkGray;

            Console.WriteLine($"[SIPSorcery:{logLevel}] {message}");
            if (exception != null)
            {
                Console.WriteLine(exception.ToString());
            }
            Console.ForegroundColor = color;
        }
    }
}

