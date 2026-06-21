using System.Collections.Concurrent;
using System.Net.WebSockets;
using System.Linq;
using Serilog;
using Serilog.Events;

var builder = WebApplication.CreateBuilder(args);

// Configure Serilog
Log.Logger = new LoggerConfiguration()
    .MinimumLevel.Information()
    .MinimumLevel.Override("Microsoft", LogEventLevel.Warning)
    .Enrich.FromLogContext()
    .WriteTo.Console(outputTemplate: "[{Timestamp:HH:mm:ss} {Level:u3}] {Message:lj}{NewLine}{Exception}")
    .WriteTo.File(
        path: Path.Combine(AppContext.BaseDirectory, "logs", "server-.log"),
        rollingInterval: RollingInterval.Day,
        outputTemplate: "{Timestamp:yyyy-MM-dd HH:mm:ss.fff zzz} [{Level:u3}] {Message:lj}{NewLine}{Exception}")
    .CreateLogger();

builder.Host.UseSerilog();

var app = builder.Build();

// Enable Blazor WebAssembly unified hosting files
if (app.Environment.IsDevelopment())
{
    app.UseWebAssemblyDebugging();
}
app.UseBlazorFrameworkFiles();
app.UseStaticFiles();

// Enable WebSockets middleware
app.UseWebSockets();

var privateMasterKeys = builder.Configuration.GetSection("PrivateMasterKeys")
    .GetChildren()
    .Select(c => c.Value)
    .Where(v => v != null)
    .Cast<string>()
    .ToList();

string? streamSecret = Environment.GetEnvironmentVariable("STREAM_SECRET") ?? builder.Configuration["StreamSecret"];
if (!string.IsNullOrEmpty(streamSecret) && !privateMasterKeys.Contains(streamSecret))
{
    privateMasterKeys.Add(streamSecret);
}

if (privateMasterKeys.Count == 0)
{
    privateMasterKeys.Add("MySuperSecretKey123");
}

int maxPublicSessions = 10;
if (int.TryParse(builder.Configuration["MaxPublicSessions"] ?? Environment.GetEnvironmentVariable("MAX_PUBLIC_SESSIONS"), out int parsedLimit))
{
    maxPublicSessions = parsedLimit;
}

// Key = secret, Value = Dictionary mapping "mobile"/"receiver" to WebSocket
var sessions = new ConcurrentDictionary<string, ConcurrentDictionary<string, WebSocket>>();

// WebSocket endpoint
app.Map("/ws", async (HttpContext context) =>
{
    if (!context.WebSockets.IsWebSocketRequest)
    {
        context.Response.StatusCode = StatusCodes.Status400BadRequest;
        await context.Response.WriteAsync("Only WebSocket connections are accepted at this endpoint.");
        return;
    }

    // Extract query parameters
    string? secret = context.Request.Query["secret"];
    string? role = context.Request.Query["role"]; // "mobile" or "receiver"

    // Primitive authentication check
    if (string.IsNullOrEmpty(secret))
    {
        app.Logger.LogWarning($"[!] Blocked unauthorized connection attempt from {context.Connection.RemoteIpAddress}: Missing secret.");
        context.Response.StatusCode = StatusCodes.Status401Unauthorized;
        return;
    }

    // Role check
    if (role != "mobile" && role != "receiver")
    {
        app.Logger.LogWarning($"[!] Connection rejected: Invalid role '{role}' specified.");
        context.Response.StatusCode = StatusCodes.Status400BadRequest;
        return;
    }

    // Verify session limit for public keys
    bool isPrivateSession = privateMasterKeys.Contains(secret);
    bool isNewSession = !sessions.ContainsKey(secret);

    if (!isPrivateSession && isNewSession)
    {
        int activePublicSessions = sessions.Keys.Count(k => !privateMasterKeys.Contains(k));
        if (activePublicSessions >= maxPublicSessions)
        {
            app.Logger.LogWarning($"[!] Blocked connection attempt from {context.Connection.RemoteIpAddress}: Max public sessions cap ({maxPublicSessions}) reached.");
            context.Response.StatusCode = StatusCodes.Status503ServiceUnavailable;
            await context.Response.WriteAsync("Signaling server is at capacity. Please try again later.");
            return;
        }
    }

    // Accept WebSocket connection
    using WebSocket webSocket = await context.WebSockets.AcceptWebSocketAsync();
    
    // Register the socket, replacing any existing stale peer slot in this session
    var sessionPeers = sessions.GetOrAdd(secret, _ => new ConcurrentDictionary<string, WebSocket>());
    sessionPeers[role] = webSocket;
    app.Logger.LogInformation($"[+] {role.ToUpper()} client connected to session '{(isPrivateSession ? "[PRIVATE]" : secret)}' from {context.Connection.RemoteIpAddress}.");

    string targetRole = (role == "mobile") ? "receiver" : "mobile";
    var buffer = new byte[1024 * 32]; // 32KB buffer for signaling messages (SDP offers can be large)

    try
    {
        while (webSocket.State == WebSocketState.Open)
        {
            var result = await webSocket.ReceiveAsync(new ArraySegment<byte>(buffer), CancellationToken.None);

            if (result.MessageType == WebSocketMessageType.Close)
            {
                app.Logger.LogInformation($"[-] {role.ToUpper()} client in session '{(isPrivateSession ? "[PRIVATE]" : secret)}' requested closure.");
                break;
            }

            // Relay message to the target peer if connected and open within this session
            if (sessionPeers.TryGetValue(targetRole, out var targetSocket) && targetSocket.State == WebSocketState.Open)
            {
                await targetSocket.SendAsync(
                    new ArraySegment<byte>(buffer, 0, result.Count),
                    result.MessageType,
                    result.EndOfMessage,
                    CancellationToken.None
                );
            }
        }
    }
    catch (WebSocketException wsex)
    {
        app.Logger.LogError(wsex, $"[!] WebSocket connection error on {role.ToUpper()} in session '{(isPrivateSession ? "[PRIVATE]" : secret)}'");
    }
    catch (Exception ex)
    {
        app.Logger.LogError(ex, $"[!] Error processing signaling on {role.ToUpper()} in session '{(isPrivateSession ? "[PRIVATE]" : secret)}'");
    }
    finally
    {
        // Try clean removal of the peer
        if (sessions.TryGetValue(secret, out var currentSession))
        {
            if (currentSession.TryGetValue(role, out var registeredSocket) && registeredSocket == webSocket)
            {
                currentSession.TryRemove(role, out _);
            }
            if (currentSession.IsEmpty)
            {
                sessions.TryRemove(secret, out _);
            }
        }
        app.Logger.LogInformation($"[-] {role.ToUpper()} client disconnected from session '{(isPrivateSession ? "[PRIVATE]" : secret)}'.");
    }
});

// Serve the Blazor Client fallback page for client-side routing
app.MapFallbackToFile("index.html");

try
{
    // Configure listener port (if PORT env var is present, typically in Docker/containers)
    string? portStr = Environment.GetEnvironmentVariable("PORT");
    if (!string.IsNullOrEmpty(portStr) && int.TryParse(portStr, out int port))
    {
        app.Logger.LogInformation($"🚀 AirMic-Sink Server starting on port {port} (Docker/Container mode)...");
        app.Run($"http://0.0.0.0:{port}");
    }
    else
    {
        app.Logger.LogInformation("🚀 AirMic-Sink Server starting with default hosting configuration (IIS/Development mode)...");
        app.Run();
    }
}
finally
{
    Log.CloseAndFlush();
}
