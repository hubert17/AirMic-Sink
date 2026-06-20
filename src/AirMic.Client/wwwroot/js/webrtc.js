window.airMic = {
    websocket: null,
    peerConnection: null,
    localStream: null,
    dotNetRef: null,
    audioElement: null,
    remoteCandidatesQueue: [],
    wakeLock: null,
    silentAudioCtx: null,
    silentAudioSource: null,
    inactivityTimer: null,
    activityListener: null,
    isOverlayActive: false,
    lastInteractionTime: 0,
    noSleepVideo: null,
    wakeLockRequestPromise: null,

    async startStreaming(signalingUrl, streamSecret, bypassHardware, selectedDeviceId, optimizeForVoice, isTestMode, dotNetRef) {
        this.dotNetRef = dotNetRef;
        this.optimizeForVoice = optimizeForVoice;
        this.isTestMode = isTestMode;
        console.log("[JS] Starting stream: bypassHardware =", bypassHardware, "selectedDeviceId =", selectedDeviceId, "optimizeForVoice =", optimizeForVoice, "isTestMode =", isTestMode);
        
        // Request Wake Lock and start silent audio immediately within user gesture context
        await this.requestWakeLock();
        this.startSilentAudio();
        this.startVideoWakeLock();

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
            this.peerConnection.onconnectionstatechange = async () => {
                const state = this.peerConnection.connectionState;
                console.log("[JS] WebRTC connection state changed:", state);
                this.dotNetRef.invokeMethodAsync("OnWebRtcStateChanged", state);
                
                if (state === "connected") {
                    // Re-request and start just in case they were released during a brief disconnect
                    await this.requestWakeLock();
                    this.startSilentAudio();
                    this.startVideoWakeLock();
                    this.startInactivityTracker();
                } else if (state === "disconnected" || state === "failed" || state === "closed") {
                    await this.releaseWakeLock();
                    this.stopSilentAudio();
                    this.stopVideoWakeLock();
                    this.stopInactivityTracker();
                }
                
                if (state === "failed" || state === "closed" || state === "disconnected") {
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
                sdp: offer.sdp,
                isTest: this.isTestMode
            };
            this.websocket.send(JSON.stringify(msg));
            console.log("[JS] SDP Offer sent (isTest = " + this.isTestMode + ").");
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
        
        // Release Wake Lock, stop silent audio, stop video lock, and stop inactivity tracker
        this.releaseWakeLock();
        this.stopSilentAudio();
        this.stopVideoWakeLock();
        this.stopInactivityTracker();

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
                this.audioElement.src = "";
                this.audioElement.removeAttribute("src");
                try {
                    this.audioElement.load();
                } catch (e) {}
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

    initialize(dotNetRef) {
        this.dotNetRef = dotNetRef;
        
        // Register device change listener
        this.registerDeviceChangeListener(dotNetRef);
        
        if (this.dotNetRef) {
            this.dotNetRef.invokeMethodAsync("SetDeviceStatus", this.isMobile());
        }
        
        // Setup event delegation for direct user gesture capture
        document.addEventListener('click', async (event) => {
            const btn = event.target.closest('#start-stream-btn');
            if (btn) {
                console.log("[JS] Start button click captured via delegation. Requesting Wake Lock and Silent Audio...");
                await this.requestWakeLock();
                this.startSilentAudio();
                this.startVideoWakeLock();
            }
        });
    },

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
    },

    isMobile() {
        return /Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(navigator.userAgent);
    },

    async requestWakeLock() {
        if (!this.isMobile()) return;
        if (!('wakeLock' in navigator)) {
            console.log("[JS] Screen Wake Lock API not supported on this browser.");
            return;
        }
        if (this.wakeLock) {
            console.log("[JS] Wake Lock already active, skipping request.");
            return;
        }
        if (this.wakeLockRequestPromise) {
            console.log("[JS] Wake Lock request already in progress, joining existing promise.");
            return this.wakeLockRequestPromise;
        }
        try {
            console.log("[JS] Requesting Screen Wake Lock...");
            this.wakeLockRequestPromise = navigator.wakeLock.request('screen');
            const lock = await this.wakeLockRequestPromise;
            this.wakeLock = lock;
            console.log("[JS] Screen Wake Lock acquired successfully.");
            if (this.dotNetRef) this.dotNetRef.invokeMethodAsync("SetWakeLockStatus", true);
            
            this.wakeLock.addEventListener('release', () => {
                console.log("[JS] Screen Wake Lock was released.");
                this.wakeLock = null;
                if (this.dotNetRef) this.dotNetRef.invokeMethodAsync("SetWakeLockStatus", false);
            });
        } catch (err) {
            console.warn("[JS] Failed to acquire Screen Wake Lock:", err);
        } finally {
            this.wakeLockRequestPromise = null;
        }
    },

    async releaseWakeLock() {
        if (this.wakeLock) {
            try {
                await this.wakeLock.release();
                console.log("[JS] Screen Wake Lock released manually.");
            } catch (err) {
                console.error("[JS] Error releasing Screen Wake Lock:", err);
            }
            this.wakeLock = null;
            if (this.dotNetRef) this.dotNetRef.invokeMethodAsync("SetWakeLockStatus", false);
        }
    },

    startSilentAudio() {
        if (!this.isMobile()) return;
        try {
            const AudioContextClass = window.AudioContext || window.webkitAudioContext;
            if (!AudioContextClass) {
                console.warn("[JS] AudioContext not supported on this browser.");
                return;
            }
            
            if (!this.silentAudioCtx) {
                this.silentAudioCtx = new AudioContextClass();
                console.log("[JS] Silent AudioContext created.");
            }

            if (!this.silentAudioSource) {
                // Create 2 seconds of silent buffer
                const bufferSize = this.silentAudioCtx.sampleRate * 2;
                const buffer = this.silentAudioCtx.createBuffer(1, bufferSize, this.silentAudioCtx.sampleRate);

                this.silentAudioSource = this.silentAudioCtx.createBufferSource();
                this.silentAudioSource.buffer = buffer;
                this.silentAudioSource.loop = true;

                const gainNode = this.silentAudioCtx.createGain();
                gainNode.gain.value = 0.0; // absolute digital silence

                this.silentAudioSource.connect(gainNode);
                gainNode.connect(this.silentAudioCtx.destination);

                this.silentAudioSource.start();
                console.log("[JS] Silent audio source started playing loop.");
            }
        } catch (err) {
            console.warn("[JS] Failed to start silent background audio loop:", err);
        }
    },

    stopSilentAudio() {
        try {
            if (this.silentAudioSource) {
                this.silentAudioSource.stop();
                this.silentAudioSource = null;
            }
            if (this.silentAudioCtx) {
                this.silentAudioCtx.close();
                this.silentAudioCtx = null;
            }
            console.log("[JS] Silent audio stopped and context closed.");
        } catch (err) {
            console.error("[JS] Error stopping silent audio:", err);
        }
    },

    startVideoWakeLock() {
        if (!this.isMobile()) return;
        try {
            if (!this.noSleepVideo) {
                this.noSleepVideo = document.createElement("video");
                this.noSleepVideo.setAttribute("title", "AirMic Wake Lock");
                this.noSleepVideo.setAttribute("playsinline", "");
                this.noSleepVideo.setAttribute("webkit-playsinline", "");
                this.noSleepVideo.muted = true;
                this.noSleepVideo.autoplay = true;
                this.noSleepVideo.loop = true;
                this.noSleepVideo.style.position = "absolute";
                this.noSleepVideo.style.width = "1px";
                this.noSleepVideo.style.height = "1px";
                this.noSleepVideo.style.opacity = "0.01";
                this.noSleepVideo.style.pointerEvents = "none";
                this.noSleepVideo.style.top = "0";
                
                const mp4Source = document.createElement("source");
                mp4Source.src = "data:video/mp4;base64,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";
                mp4Source.type = "video/mp4";
                this.noSleepVideo.appendChild(mp4Source);
                
                document.body.appendChild(this.noSleepVideo);
                console.log("[JS] Invisible NoSleep video element appended to DOM.");
            }
            
            const playPromise = this.noSleepVideo.play();
            if (playPromise !== undefined) {
                playPromise.then(() => {
                    console.log("[JS] Invisible looping video started playing. Screen wake lock forced.");
                }).catch(err => {
                    console.warn("[JS] Failed to play background video wake lock:", err);
                });
            }
        } catch (err) {
            console.warn("[JS] Error starting video wake lock:", err);
        }
    },

    stopVideoWakeLock() {
        if (this.noSleepVideo) {
            try {
                this.noSleepVideo.pause();
                this.noSleepVideo.src = "";
                this.noSleepVideo.removeAttribute("src");
                try {
                    this.noSleepVideo.load();
                } catch (e) {}
                this.noSleepVideo.remove();
                console.log("[JS] Invisible video wake lock stopped and removed.");
            } catch (err) {
                console.error("[JS] Error stopping video wake lock:", err);
            }
            this.noSleepVideo = null;
        }
    },

    startInactivityTracker() {
        if (!this.isMobile()) return;
        this.stopInactivityTracker();
        console.log("[JS] Starting inactivity tracker...");
        
        this.lastInteractionTime = Date.now();
        this.activityListener = () => {
            const now = Date.now();
            if (now - this.lastInteractionTime > 1000) {
                this.lastInteractionTime = now;
                this.resetInactivityTimer();
            }
        };
        
        const events = ['mousedown', 'mousemove', 'keypress', 'scroll', 'touchstart'];
        events.forEach(name => {
            document.addEventListener(name, this.activityListener, { passive: true });
        });
        
        this.resetInactivityTimer();
    },

    stopInactivityTracker() {
        console.log("[JS] Stopping inactivity tracker...");
        if (this.inactivityTimer) {
            clearTimeout(this.inactivityTimer);
            this.inactivityTimer = null;
        }
        if (this.activityListener) {
            const events = ['mousedown', 'mousemove', 'keypress', 'scroll', 'touchstart'];
            events.forEach(name => {
                document.removeEventListener(name, this.activityListener);
            });
            this.activityListener = null;
        }
        this.setOverlayVisible(false);
    },

    resetInactivityTimer() {
        if (this.inactivityTimer) {
            clearTimeout(this.inactivityTimer);
        }
        
        if (this.isOverlayActive) {
            this.setOverlayVisible(false);
        }
        
        this.inactivityTimer = setTimeout(() => {
            this.setOverlayVisible(true);
        }, 10000);
    },

    setOverlayVisible(visible) {
        if (this.isOverlayActive === visible) return;
        this.isOverlayActive = visible;
        console.log("[JS] OLED Screen Saver visibility changed to:", visible);
        if (this.dotNetRef) {
            this.dotNetRef.invokeMethodAsync("SetOledOverlayVisible", visible);
        }
    }
};

document.addEventListener('visibilitychange', async () => {
    if (document.visibilityState === 'visible') {
        if (window.airMic && window.airMic.peerConnection) {
            console.log("[JS] Page became visible. Re-requesting wake lock...");
            await window.airMic.requestWakeLock();
            
            if (window.airMic.silentAudioCtx && window.airMic.silentAudioCtx.state === 'suspended') {
                console.log("[JS] Resuming silent AudioContext...");
                try {
                    await window.airMic.silentAudioCtx.resume();
                } catch (err) {
                    console.warn("[JS] Failed to resume silent AudioContext:", err);
                }
            }
            
            if (window.airMic.noSleepVideo && window.airMic.noSleepVideo.paused) {
                console.log("[JS] Resuming silent video wake lock...");
                try {
                    await window.airMic.noSleepVideo.play();
                } catch (err) {
                    console.warn("[JS] Failed to resume video wake lock:", err);
                }
            }
        }
    }
});
