# AirMic-Sink

An ultra-low latency, zero-compromise Remote Microphone Streaming Pipeline. This monorepo implements a real-time WebRTC audio relay engine designed to capture broadcast-grade voice from an edge mobile client, traverse restrictive international networks, and inject the stream directly into a native Virtual Audio Cable (VAC) on a home base machine.

The resulting virtual device serves as an enterprise-grade hardware microphone source for platforms like Microsoft Teams, Zoom, and Webex, completely bypassing high-overhead, generic commercial streaming utilities.

---

## 🌌 The Mission & Architectural Intent

When working remotely across international boundaries—often bound to unpredictable hotel Wi-Fi, complex symmetric NATs, and high-jitter residential ISP routings—joining critical corporate meetings with stable audio is a massive hurdle. Generic streaming apps favor heavy media buffering (introducing high latency) or introduce massive CPU/network overhead.

`AirMic-Sink` solves this by stripping away everything except the raw essentials needed for high-velocity voice processing.

### Project Pillars

* **Zero-Overhead Plumbing:** Pure .NET stack using minimal execution runtimes to cut down scheduling jitter.
* **Direct Hardware Injection:** Bypasses the Windows System Mixer via **WASAPI Exclusive Mode** (`IAudioClient3`) to eliminate internal OS audio latency down to the millisecond.
* **Resilient Network Traversal:** Built specifically to survive aggressive hospitality firewalls via strategic STUN/TURN UDP fallbacks and lightweight signaling.
* **Antigravity 2.0 Realization:** Fully realized and optimized using **Antigravity 2.0** principles to maintain absolute structural performance, lightweight data state transitions, and extreme resilience against network throughput degradation.

---

## 🏗️ System Architecture

The pipeline is organized as a unified .NET Monorepo split into highly specialized, isolated boundaries:

```text
📁 AirMic-Sink/
│
├── 📁 src/
│   ├── 📁 AirMic.Server/          # ASP.NET Core Minimal API WebSockets Hub (Docker/IIS-ready)
│   ├── 📁 AirMic.Receiver/        # Desktop Engine (SIPSorcery WebRTC State Machine + WASAPI Sink)
│   └── 📁 AirMic.Client/          # Mobile Capture Edge (.NET MAUI / Blazor Hybrid)
│
├── 📁 shared/
│   └── 📁 AirMic.Contracts/       # Zero-dependency POCOs for SDP and ICE Candidate routing
│
└── 📄 AirMic-Sink.sln             # Master Solution

```

### The Data Flow Pipeline

```text
 [ Mobile Mic ] ──(Low-Delay WebRTC / Opus)──> [ Cloud / Home Lab Signaling ]
                                                       │
                                           (P2P WebRTC / UDP Stream)
                                                       │
                                                       ▼
 [ MS Teams / Zoom ] <─── [ Cable Output ] <─── [ WASAPI Exclusive Sink ]

```

---

## 🏎️ Low-Latency Engineering Mandate

To preserve deterministic real-time communication across continents, the engine enforces strict low-latency constraints across all layers:

### 1. Audio Processing & Codec Blueprint

* **Codec Selection:** Explicitly locked to the **Opus** codec configured for **VoIP/Low Delay** optimization modes.
* **Packetization Rate (ptime):** Hard-capped at **10ms to 20ms** frames to maximize fast delivery while avoiding excessive IP packet header overhead.
* **Hardware Bypassing:** Mobile client disables native hardware processing variations (Echo Cancellation, Automatic Gain Control, Noise Suppression) at the edge capture point. This moves raw processing out of high-overhead mobile loops and defers advanced filtering downstream to Microsoft Teams/Zoom.

### 2. Network & Traversal Strategy

* **Signaling Channel:** Stateless **C# Minimal API WebSockets** router handling pure SDP Offer/Answer exchanges and immediate ICE candidate forwarding.
* **Security Context:** Implements a lean, high-velocity pre-shared **"Stream Secret Key"** parameter validation on the WebSockets upgrade pathway to instantly drop unauthenticated probes.
* **Transport Layer:** Prioritizes raw peer-to-peer **UDP** streams to eliminate TCP Head-of-Line blocking over jitter-prone hospitality Wi-Fi.

### 3. Native OS Ingestion (Windows Host)

* **WASAPI Exclusive Mode:** Targets the Virtual Audio Cable input buffer using `AudioClientShareMode.Exclusive`. This grants `AirMic.Receiver` direct, uninterrupted access to the hardware device driver data ring, dodging the latency penalty of the Windows audio rendering subsystem.
* **Cross-Platform Forward Compatibility:** Architected via interface abstraction (`IAudioBufferSink`). The receiver easily adapts to macOS in the future by hot-swapping the Windows WASAPI layer for a **Mac CoreAudio / BlackHole Virtual Loopback** engine without touching the network or signaling infrastructure.

---

## 🛠️ Quick Start & Verification Blueprint

### Prerequisites

1. **VB-Audio Virtual Cable** or **Virtual Audio Cable (VAC)** installed on the target Windows Receiver machine.
2. A functional **Cloudflare Tunnel** (or similar reverse proxy) exposing the signaling hub securely over `HTTPS`/`WSS` to satisfy mobile browser and native application microphone permission restrictions.

### Phase 1: Local Sink Ingestion Test

Before launching network code, verify your local audio routing matrix. Run the local test fixture in `AirMic.Receiver` to generate an isolated, continuous 440Hz wave directly into your Virtual Audio Cable:

```bash
dotnet run --project src/AirMic.Receiver --mode test-sink

```

* **Validation:** Open your meeting software (Teams/Zoom), switch the microphone input source to **Cable Output**, and execute an audio loopback check. If you observe a crisp, uncorrupted synthetic tone free of crackle or distortion, the WASAPI injection loop is validated.

### Phase 2: Spin Up the .NET Signaling Hub

Deploy the signaling hub directly to Docker inside your homelab environment:

```bash
docker build -t airmic-server -f src/AirMic.Server/Dockerfile .
docker run -d -p 8443:8443 -e PORT=8443 -e STREAM_SECRET="YourSecretKeyHere" airmic-server

```

Hook up your Cloudflare Tunnel to map an external secure endpoint straight to internal port `8443`.


