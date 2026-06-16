window.airMic = {
    websocket: null,
    peerConnection: null,
    localStream: null,
    dotNetRef: null,
    audioElement: null,
    remoteCandidatesQueue: [],

    async startStreaming(signalingUrl, streamSecret, bypassHardware, selectedDeviceId, optimizeForVoice, dotNetRef) {
        this.dotNetRef = dotNetRef;
        this.optimizeForVoice = optimizeForVoice;
        console.log("[JS] Starting stream: bypassHardware =", bypassHardware, "selectedDeviceId =", selectedDeviceId, "optimizeForVoice =", optimizeForVoice);
        try {
            // 1. Acquire local audio stream with latency-optimized constraints
            const constraints = {
                audio: {
                    ...(selectedDeviceId ? { deviceId: { exact: selectedDeviceId } } : {}),
                    echoCancellation: false,
                    ...(optimizeForVoice ? {
                        noiseSuppression: true,
                        autoGainControl: true,
                        channelCount: { ideal: 1 },
                        sampleRate: { ideal: 16000 },
                        latency: 0
                    } : (bypassHardware ? {
                        noiseSuppression: false,
                        autoGainControl: false,
                        latency: 0
                    } : {}))
                }
            };
            
            this.localStream = await navigator.mediaDevices.getUserMedia(constraints);
            console.log("[JS] Local microphone captured successfully.");
            this.dotNetRef.invokeMethodAsync("OnMicCaptured", true);
        } catch (err) {
            console.error("[JS] Failed to capture microphone", err);
            this.dotNetRef.invokeMethodAsync("OnError", "Microphone access denied or failed: " + err.message);
            this.cleanup();
            return;
        }

        try {
            // 2. Establish signaling WebSocket
            const wsUrl = new URL(signalingUrl);
            wsUrl.searchParams.set("role", "mobile");
            wsUrl.searchParams.set("secret", streamSecret);

            console.log("[JS] Connecting to signaling server:", wsUrl.origin + wsUrl.pathname);
            this.websocket = new WebSocket(wsUrl.toString());

            this.websocket.onopen = async () => {
                console.log("[JS] Signaling WebSocket open.");
                this.dotNetRef.invokeMethodAsync("OnSignalingStateChanged", "Connected");
                await this.initiateWebRtc();
            };

            this.websocket.onclose = () => {
                console.log("[JS] Signaling WebSocket closed.");
                this.dotNetRef.invokeMethodAsync("OnSignalingStateChanged", "Disconnected");
                this.cleanup();
            };

            this.websocket.onerror = (err) => {
                console.error("[JS] Signaling WebSocket error", err);
                this.dotNetRef.invokeMethodAsync("OnError", "Signaling connection failed.");
            };

            this.websocket.onmessage = async (event) => {
                try {
                    const msg = JSON.parse(event.data);
                    await this.handleSignalingMessage(msg);
                } catch (err) {
                    console.error("[JS] Error processing signaling message", err);
                }
            };
        } catch (err) {
            console.error("[JS] Failed to establish signaling connection", err);
            this.dotNetRef.invokeMethodAsync("OnError", "Failed to start signaling: " + err.message);
            this.cleanup();
        }
    },

    async initiateWebRtc() {
        try {
            console.log("[JS] Initiating RTCPeerConnection...");
            const config = {
                iceServers: [
                    { urls: "stun:stun.l.google.com:19302" }
                ]
            };

            this.peerConnection = new RTCPeerConnection(config);

            // Add local track to peer connection
            this.localStream.getTracks().forEach(track => {
                this.peerConnection.addTrack(track, this.localStream);
            });

            // Handle connection state changes
            this.peerConnection.onconnectionstatechange = () => {
                const state = this.peerConnection.connectionState;
                console.log("[JS] WebRTC connection state changed:", state);
                this.dotNetRef.invokeMethodAsync("OnWebRtcStateChanged", state);
                if (state === "failed" || state === "closed") {
                    this.cleanup();
                }
            };

            // Handle ICE candidates
            this.peerConnection.onicecandidate = (event) => {
                if (event.candidate && this.websocket && this.websocket.readyState === WebSocket.OPEN) {
                    const msg = {
                        type: "candidate",
                        candidate: {
                            candidate: event.candidate.candidate,
                            sdpMid: event.candidate.sdpMid,
                            sdpMLineIndex: event.candidate.sdpMLineIndex
                        }
                    };
                    this.websocket.send(JSON.stringify(msg));
                }
            };

            // Handle incoming track (for receiver loopback feedback)
            this.peerConnection.ontrack = (event) => {
                console.log("[JS] Remote track received:", event);
                this.dotNetRef.invokeMethodAsync("OnLoopbackAudioReceived", true);

                if (!this.audioElement) {
                    this.audioElement = document.createElement("audio");
                    this.audioElement.autoplay = true;
                    this.audioElement.playsInline = true;
                    this.audioElement.controls = false;
                    document.body.appendChild(this.audioElement);
                }
                
                if (event.streams && event.streams[0]) {
                    this.audioElement.srcObject = event.streams[0];
                } else {
                    const newStream = new MediaStream([event.track]);
                    this.audioElement.srcObject = newStream;
                }
            };

            // Create Offer
            const offer = await this.peerConnection.createOffer();
            await this.peerConnection.setLocalDescription(offer);

            // Send Offer
            const msg = {
                type: "offer",
                sdp: offer.sdp
            };
            this.websocket.send(JSON.stringify(msg));
            console.log("[JS] SDP Offer sent.");
        } catch (err) {
            console.error("[JS] Error initiating WebRTC connection", err);
            this.dotNetRef.invokeMethodAsync("OnError", "WebRTC initialization failed: " + err.message);
            this.cleanup();
        }
    },

    async handleSignalingMessage(msg) {
        if (!this.peerConnection) return;

        if (msg.type === "answer") {
            console.log("[JS] Received SDP Answer. Setting remote description.");
            const answer = new RTCSessionDescription({
                type: "answer",
                sdp: msg.sdp
            });
            await this.peerConnection.setRemoteDescription(answer);

            // Apply voice optimizations to RTCRtpSender if active
            if (this.optimizeForVoice) {
                try {
                    const senders = this.peerConnection.getSenders();
                    const audioSender = senders.find(s => s.track && s.track.kind === 'audio');
                    if (audioSender) {
                        const params = audioSender.getParameters();
                        if (!params.encodings) {
                            params.encodings = [{}];
                        }
                        params.encodings[0].maxBitrate = 24000;
                        await audioSender.setParameters(params);
                        console.log("[JS] RTCRtpSender: Audio maxBitrate clamped to 24000 bps for voice.");
                    }
                } catch (err) {
                    console.warn("[JS] Failed to apply audio encoder parameters:", err);
                }
            }

            // Drain queued remote candidates
            console.log(`[JS] Draining ${this.remoteCandidatesQueue.length} queued remote candidates.`);
            while (this.remoteCandidatesQueue.length > 0) {
                const cand = this.remoteCandidatesQueue.shift();
                try {
                    await this.peerConnection.addIceCandidate(cand);
                    console.log("[JS] Successfully added queued remote candidate.");
                } catch (e) {
                    console.error("[JS] Failed to add queued remote candidate:", e);
                }
            }
        } else if (msg.type === "candidate") {
            if (msg.candidate) {
                const candidate = new RTCIceCandidate({
                    candidate: msg.candidate.candidate,
                    sdpMid: msg.candidate.sdpMid,
                    sdpMLineIndex: msg.candidate.sdpMLineIndex
                });

                if (this.peerConnection.remoteDescription && this.peerConnection.remoteDescription.type) {
                    console.log("[JS] Received remote ICE Candidate. Adding immediately.");
                    try {
                        await this.peerConnection.addIceCandidate(candidate);
                    } catch (e) {
                        console.error("[JS] Failed to add remote candidate immediately:", e);
                    }
                } else {
                    console.log("[JS] Received remote ICE Candidate. Queueing since remote description is not set yet.");
                    this.remoteCandidatesQueue.push(candidate);
                }
            }
        }
    },

    stopStreaming() {
        console.log("[JS] Stop streaming requested.");
        this.cleanup();
        if (this.dotNetRef) {
            this.dotNetRef.invokeMethodAsync("OnStreamStopped", true);
        }
    },

    cleanup() {
        console.log("[JS] Executing cleanup...");
        this.remoteCandidatesQueue = [];

        if (this.websocket) {
            try {
                this.websocket.close();
            } catch (e) {}
            this.websocket = null;
        }

        if (this.peerConnection) {
            try {
                this.peerConnection.close();
            } catch (e) {}
            this.peerConnection = null;
        }

        if (this.localStream) {
            try {
                this.localStream.getTracks().forEach(track => {
                    track.stop();
                });
            } catch (e) {}
            this.localStream = null;
        }

        if (this.audioElement) {
            try {
                this.audioElement.pause();
                this.audioElement.srcObject = null;
                this.audioElement.remove();
            } catch (e) {}
            this.audioElement = null;
        }

        if (this.dotNetRef) {
            this.dotNetRef.invokeMethodAsync("OnMicCaptured", false);
            this.dotNetRef.invokeMethodAsync("OnLoopbackAudioReceived", false);
        }
    },

    async getAudioDevices() {
        try {
            let devices = await navigator.mediaDevices.enumerateDevices();
            const hasLabels = devices.some(d => d.kind === 'audioinput' && d.label);
            
            if (!hasLabels) {
                try {
                    const tempStream = await navigator.mediaDevices.getUserMedia({ audio: true });
                    tempStream.getTracks().forEach(track => track.stop());
                    devices = await navigator.mediaDevices.enumerateDevices();
                } catch (permissionErr) {
                    console.warn("[JS] Microphone permission denied when trying to list devices:", permissionErr);
                }
            }
            
            return devices
                .filter(d => d.kind === 'audioinput')
                .map(d => ({
                    deviceId: d.deviceId,
                    label: d.label || `Microphone (${d.deviceId.slice(0, 5)}...)`
                }));
        } catch (err) {
            console.error("[JS] Error listing audio devices:", err);
            return [];
        }
    },

    deviceChangeListener: null,

    registerDeviceChangeListener(dotNetRef) {
        if (this.deviceChangeListener) {
            navigator.mediaDevices.removeEventListener('devicechange', this.deviceChangeListener);
        }
        this.deviceChangeListener = async () => {
            console.log("[JS] Media devices changed, updating device list.");
            const devices = await this.getAudioDevices();
            dotNetRef.invokeMethodAsync("OnDevicesChanged", devices);
        };
        navigator.mediaDevices.addEventListener('devicechange', this.deviceChangeListener);
    },

    async changeAudioDevice(selectedDeviceId, bypassHardware, optimizeForVoice) {
        console.log("[JS] Changing audio device to:", selectedDeviceId, "optimizeForVoice =", optimizeForVoice);
        if (!this.localStream) return;
        
        this.optimizeForVoice = optimizeForVoice;
        
        try {
            // Stop current local track(s)
            this.localStream.getTracks().forEach(track => track.stop());
            
            // Get new stream constraints
            const constraints = {
                audio: {
                    ...(selectedDeviceId ? { deviceId: { exact: selectedDeviceId } } : {}),
                    echoCancellation: false,
                    ...(optimizeForVoice ? {
                        noiseSuppression: true,
                        autoGainControl: true,
                        channelCount: { ideal: 1 },
                        sampleRate: { ideal: 16000 },
                        latency: 0
                    } : (bypassHardware ? {
                        noiseSuppression: false,
                        autoGainControl: false,
                        latency: 0
                    } : {}))
                }
            };
            
            this.localStream = await navigator.mediaDevices.getUserMedia(constraints);
            const newTrack = this.localStream.getAudioTracks()[0];
            
            // Replace the track in all active peer connection senders
            if (this.peerConnection) {
                const senders = this.peerConnection.getSenders();
                const audioSender = senders.find(s => s.track && s.track.kind === 'audio');
                if (audioSender) {
                    await audioSender.replaceTrack(newTrack);
                    console.log("[JS] Audio track successfully replaced on PeerConnection.");
                }
            }
        } catch (err) {
            console.error("[JS] Failed to switch audio device:", err);
            if (this.dotNetRef) {
                this.dotNetRef.invokeMethodAsync("OnError", "Failed to switch audio device: " + err.message);
            }
        }
    }
};
