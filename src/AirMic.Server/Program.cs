using System.Collections.Concurrent;
using System.Net.WebSockets;

var builder = WebApplication.CreateBuilder(args);
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

// Stream Secret configuration via environment variable or default
string streamSecret = Environment.GetEnvironmentVariable("STREAM_SECRET") ?? "MySuperSecretKey123";
var peers = new ConcurrentDictionary<string, WebSocket>();

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
    if (string.IsNullOrEmpty(secret) || secret != streamSecret)
    {
        Console.WriteLine($"[!] Blocked unauthorized connection attempt from {context.Connection.RemoteIpAddress}.");
        context.Response.StatusCode = StatusCodes.Status401Unauthorized;
        return;
    }

    // Role check
    if (role != "mobile" && role != "receiver")
    {
        Console.WriteLine($"[!] Connection rejected: Invalid role '{role}' specified.");
        context.Response.StatusCode = StatusCodes.Status400BadRequest;
        return;
    }

    // Accept WebSocket connection
    using WebSocket webSocket = await context.WebSockets.AcceptWebSocketAsync();
    
    // Register the socket, replacing any existing stale peer slot
    peers[role] = webSocket;
    Console.WriteLine($"[+] {role.ToUpper()} client connected from {context.Connection.RemoteIpAddress}.");

    string targetRole = (role == "mobile") ? "receiver" : "mobile";
    var buffer = new byte[1024 * 32]; // 32KB buffer for signaling messages (SDP offers can be large)

    try
    {
        while (webSocket.State == WebSocketState.Open)
        {
            var result = await webSocket.ReceiveAsync(new ArraySegment<byte>(buffer), CancellationToken.None);

            if (result.MessageType == WebSocketMessageType.Close)
            {
                Console.WriteLine($"[-] {role.ToUpper()} client requested closure.");
                break;
            }

            // Relay message to the target peer if connected and open
            if (peers.TryGetValue(targetRole, out var targetSocket) && targetSocket.State == WebSocketState.Open)
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
        Console.WriteLine($"[!] WebSocket connection error on {role.ToUpper()}: {wsex.Message}");
    }
    catch (Exception ex)
    {
        Console.WriteLine($"[!] Error processing signaling on {role.ToUpper()}: {ex.Message}");
    }
    finally
    {
        // Try clean removal of the peer
        if (peers.TryGetValue(role, out var registeredSocket) && registeredSocket == webSocket)
        {
            peers.TryRemove(role, out _);
        }
        Console.WriteLine($"[-] {role.ToUpper()} client disconnected.");
    }
});

// Serve the Blazor Client fallback page for client-side routing
app.MapFallbackToFile("index.html");

// Configure listener port (if PORT env var is present, typically in Docker/containers)
string? portStr = Environment.GetEnvironmentVariable("PORT");
if (!string.IsNullOrEmpty(portStr) && int.TryParse(portStr, out int port))
{
    Console.WriteLine($"🚀 AirMic-Sink Server starting on port {port} (Docker/Container mode)...");
    app.Run($"http://0.0.0.0:{port}");
}
else
{
    Console.WriteLine("🚀 AirMic-Sink Server starting with default hosting configuration (IIS/Development mode)...");
    app.Run();
}
