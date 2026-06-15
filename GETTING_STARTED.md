# Getting Started Guide - AirMic-Sink

This guide will walk you through setting up and running `AirMic-Sink` on your local environment to stream microphone audio from a client device (mobile/browser) to a virtual audio cable on a target machine (home base receiver).

---

## 🛠️ Prerequisites

1. **Virtual Audio Cable (VAC)**:
   Ensure you have [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) installed on the Windows Receiver machine. This acts as the physical recording device that MS Teams/Zoom will hook onto.
2. **.NET 10 SDK**:
   Ensure you have the .NET 10 SDK installed on the machines compiling/running the server and receiver.

---

## 🚀 Step-by-Step Setup

### Step 1: Start the Unified Server (Host)
The server acts as both the **WebSockets signaling hub** (for negotiation) and the **Blazor WebAssembly client web host**.

1. Open your terminal in the root of the `AirMic-Sink` workspace.
2. Start the server:
   ```bash
   dotnet run --project src/AirMic.Server
   ```
3. Keep this terminal open. The server will start and listen on port `8443`:
   ```text
   🚀 AirMic-Sink Server starting on port 8443...
   Now listening on: http://0.0.0.0:8443
   ```

---

### Step 2: Start the Receiver (Home Machine)
The receiver captures incoming WebRTC streams, decodes the Opus packets to raw PCM audio, and feeds it directly into your virtual audio cable's driver ring.

1. Open a second terminal window.
2. Run the receiver app pointing to your server's WebSocket endpoint:
   ```bash
   dotnet run --project src/AirMic.Receiver -- "ws://localhost:8443/ws" "MySuperSecretKey123"
   ```
   *(Replace `"MySuperSecretKey123"` with any custom secret key you want to secure the session).*
3. Verify that the receiver logs a successful connection to the server:
   ```text
   [*] Connecting to signaling server: ws://localhost:8443/ws
   [+] Connected to signaling server.
   [+] SUCCESS: WebRTC Receiver is running!
   Waiting for remote mobile client to connect and stream audio...
   ```

---

### Step 3: Stream Audio from the Client
1. Open your web browser (Chrome, Edge, or mobile Safari/Chrome if hosted externally) and go to:
   👉 **[http://127.0.0.1:8443/?secret=MySuperSecretKey123](http://127.0.0.1:8443/?secret=MySuperSecretKey123)**
2. The MudBlazor control panel will load with your secret pre-filled.
3. Click the **Start Audio Stream** button.
4. Accept the browser prompt requesting **Microphone Access**.
5. Once microphone access is granted, the signaling and WebRTC connection will initiate automatically:
   * The client connection dashboard will show **Signaling Channel: Connected** and **WebRTC Peer State: connected**.
   * The receiver console terminal will output:
     ```text
     [WebRTC] Connection state: connected
     [+] WebRTC Audio Stream established successfully!
     ```

---

### Step 4: Configure Your Meeting Software (Teams/Zoom)
Now that the receiver is playing the WebRTC audio directly into the virtual audio cable:

1. Open **Microsoft Teams**, **Zoom**, or **Webex**.
2. Go to **Settings** -> **Devices** (or Audio Settings).
3. Under **Microphone Input**, choose:
   🎤 **Cable Output (VB-Audio Virtual Cable)**
4. Make a test call or speak into your client device. Your voice will traverse the WebRTC connection, decode on the receiver, and inject seamlessly into the meeting call!

---

## 🔒 Production Hosting Tip (Symmetric NAT & TLS)
When deploying this across the internet (e.g., when traveling abroad):
* **Microphone Security**: Modern browsers block microphone access (`getUserMedia`) on insecure endpoints. The server *must* be served over HTTPS/TLS (e.g. via a Cloudflare Tunnel or local certificate).
* **Signaling URL**: Make sure to update the **Signaling Hub WebSockets URL** on the client to use `wss://` instead of `ws://` (e.g., `wss://yourserver.com/ws`).
* **TURN Server**: Standard STUN/TURN traversal may be needed if you are on restricted hotel Wi-Fi that utilizes symmetric NAT routers.

---

## 📦 Publishing & Deployment

The unified host (`AirMic.Server`) is designed to support both Docker containerization and IIS (Internet Information Services) publication out-of-the-box.

### 1. IIS Publication Setup
To host the application on IIS:
1. Open the project in Visual Studio, right-click `AirMic.Server`, and choose **Publish**.
2. Select **Folder** or **IIS** as the target.
3. Configure the publish settings (e.g., Target Runtime `Portable` or `win-x64`).
4. Publish the application. The output folder will contain a `web.config` file configured for the IIS ASP.NET Core Module (`AspNetCoreModuleV2`).
5. On your target IIS machine:
   * Install the [.NET 10 Hosting Bundle](https://dotnet.microsoft.com/download/dotnet/10.0).
   * Ensure the **WebSockets** protocol feature is enabled in IIS/Windows Features.
   * Map a new IIS Website to your published folder.
   * Ensure the IIS Application Pool is configured to use **No Managed Code**.
   * Note: The app will run dynamically under IIS port bindings; do not define a `PORT` environment variable.

### 2. Docker Deployment Setup
To host the application inside a container:
1. In the root directory, build the Docker image targeting the unified server:
   ```bash
   docker build -t airmic-server -f src/AirMic.Server/Dockerfile .
   ```
2. Run the Docker container, mapping port `8443` and configuring your custom stream secret:
   ```bash
    docker run -d -p 8443:8443 -e PORT=8443 -e STREAM_SECRET="MySuperSecretKey123" airmic-server
    ```
3. The server container will launch, configure itself to run on port `8443`, and start serving WebAssembly assets and the WS endpoint.
