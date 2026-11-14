use crate::audio::coreaudio::{
    enumerate_virtual_devices, CoreAudioVirtualSink, CoreAudioVirtualSource,
};
use crate::audio::{
    compute_rms_i16, convert_captured_stereo_f32_to_i16, AudioRuntimeControls, AudioSink,
    AudioSource, MUSIC_MAX_BACKLOG_SECS, MUSIC_TARGET_CUSHION_SECS, OPUS_FRAME_SAMPLES_PER_CH,
    TARGET_SAMPLE_RATE, VOICE_MAX_BACKLOG_SECS, VOICE_TARGET_CUSHION_SECS,
};
use crate::signaling::{
    ensure_candidate_prefix, IceCandidatePayload, SignalingClient, SignalingEvent,
    SignalingMessage,
};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use opus::{Application as OpusApp, Bitrate as OpusBitrate, Channels as OpusChannels};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

pub const VOICE_OPUS_FMTP: &str = "minptime=20;useinbandfec=1;stereo=0;sprop-stereo=0;usedtx=0;maxaveragebitrate=24000;maxplaybackrate=48000;sprop-maxcapturerate=48000;ptime=20";
pub const MUSIC_OPUS_FMTP: &str = "minptime=20;useinbandfec=1;stereo=1;sprop-stereo=1;usedtx=0;maxaveragebitrate=64000;maxplaybackrate=48000;sprop-maxcapturerate=48000;ptime=20";

const SILENCE_THRESHOLD: f64 = 0.015;
const SILENCE_DURATION_LIMIT_SECS: f64 = 1.2;
const MAX_TEST_RECORD_SECONDS: usize = 30;

const SILENT_OPUS_MONO: [u8; 3] = [0xf8, 0xff, 0xfe];
const SILENT_OPUS_STEREO: [u8; 3] = [0xfc, 0xff, 0xfe];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    WaitingForClient,
    ConnectedVoice,
    ConnectedMusic,
}

impl ConnectionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::WaitingForClient => "Waiting for Client",
            Self::ConnectedVoice => "Connected · Voice",
            Self::ConnectedMusic => "Connected · Music",
        }
    }

    pub fn is_streaming(self) -> bool {
        matches!(self, Self::ConnectedVoice | Self::ConnectedMusic)
    }

    pub fn is_active_session(self) -> bool {
        !matches!(self, Self::Disconnected)
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub signaling_url: String,
    pub stream_secret: String,
    pub output_sink_uid: String,
    pub input_capture_uid: Option<String>,
    pub auto_reconnect: bool,
    pub logs_dir: PathBuf,
}

/// Rewrites (or inserts) the `a=fmtp:<pt>` line for `opus/48000/2` in the SDP Answer
/// to enforce exact Opus bitrate, stereo/mono, FEC, DTX, and 20ms ptime settings.
pub fn force_sdp_opus_fmtp(sdp: &str, optimize_for_voice: bool) -> String {
    let opus_params = if optimize_for_voice {
        VOICE_OPUS_FMTP
    } else {
        MUSIC_OPUS_FMTP
    };

    let crlf: String = [char::from(13), char::from(10)].into_iter().collect();
    let raw_lines: Vec<&str> = sdp.lines().collect();

    // Locate payload type in `a=rtpmap:<pt> opus/48000/2`
    let mut opus_pt: Option<&str> = None;
    for line in &raw_lines {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("a=rtpmap:") {
            let mut parts = rest.split_whitespace();
            if let (Some(pt), Some(codec)) = (parts.next(), parts.next()) {
                if codec.eq_ignore_ascii_case("opus/48000/2") {
                    opus_pt = Some(pt);
                    break;
                }
            }
        }
    }

    let Some(pt) = opus_pt else {
        return sdp.to_string();
    };

    let fmtp_prefix = format!("a=fmtp:{} ", pt);
    let target_fmtp = format!("a=fmtp:{} {}", pt, opus_params);
    let has_existing_fmtp = raw_lines
        .iter()
        .any(|l| l.trim().starts_with(&fmtp_prefix));

    let rtpmap_prefix = format!("a=rtpmap:{} ", pt);
    let mut out_lines = Vec::with_capacity(raw_lines.len() + 1);

    for line in raw_lines {
        let trimmed = line.trim();
        if has_existing_fmtp && trimmed.starts_with(&fmtp_prefix) {
            out_lines.push(target_fmtp.clone());
        } else {
            out_lines.push(trimmed.to_string());
            if !has_existing_fmtp && trimmed.starts_with(&rtpmap_prefix) {
                out_lines.push(target_fmtp.clone());
            }
        }
    }

    let mut joined = out_lines.join(&crlf);
    joined.push_str(&crlf);
    joined
}

struct TestLoopbackState {
    is_recording: bool,
    last_active_instant: Instant,
    recorded_samples: Vec<i16>,
    is_playing_back: Arc<AtomicBool>,
    playback_cancel: Option<watch::Sender<bool>>,
}

impl TestLoopbackState {
    fn new() -> Self {
        Self {
            is_recording: false,
            last_active_instant: Instant::now(),
            recorded_samples: Vec::new(),
            is_playing_back: Arc::new(AtomicBool::new(false)),
            playback_cancel: None,
        }
    }

    fn cancel_playback(&mut self) {
        if let Some(tx) = self.playback_cancel.take() {
            let _ = tx.send(true);
        }
        self.is_playing_back.store(false, Ordering::Relaxed);
    }
}

struct ActivePeerSession {
    peer_connection: Arc<RTCPeerConnection>,
    stop_tx: watch::Sender<bool>,
    pending_candidates: Vec<RTCIceCandidateInit>,
    remote_desc_ready: bool,
}

impl ActivePeerSession {
    async fn shutdown(self) {
        let _ = self.stop_tx.send(true);
        let _ = self.peer_connection.close().await;
    }
}

/// Manages the background Tokio signaling + WebRTC session lifecycle for the desktop UI.
pub struct ReceiverSessionHandle {
    cancel_tx: watch::Sender<bool>,
    status_rx: watch::Receiver<ConnectionStatus>,
}

impl ReceiverSessionHandle {
    pub fn start(
        runtime: &tokio::runtime::Handle,
        config: SessionConfig,
        controls: Arc<AudioRuntimeControls>,
        ui_repaint: Option<egui::Context>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (status_tx, status_rx) = watch::channel(ConnectionStatus::Connecting);

        runtime.spawn(async move {
            run_supervisor_loop(config, controls, cancel_rx, status_tx, ui_repaint).await;
        });

        Self {
            cancel_tx,
            status_rx,
        }
    }

    pub fn status(&self) -> ConnectionStatus {
        *self.status_rx.borrow()
    }

    pub fn stop(&self) {
        let _ = self.cancel_tx.send(true);
    }
}

impl Drop for ReceiverSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn update_status(
    tx: &watch::Sender<ConnectionStatus>,
    repaint: &Option<egui::Context>,
    next: ConnectionStatus,
) {
    if *tx.borrow() != next {
        let _ = tx.send(next);
        if let Some(ctx) = repaint {
            ctx.request_repaint();
        }
    }
}

async fn run_supervisor_loop(
    config: SessionConfig,
    controls: Arc<AudioRuntimeControls>,
    mut cancel_rx: watch::Receiver<bool>,
    status_tx: watch::Sender<ConnectionStatus>,
    ui_repaint: Option<egui::Context>,
) {
    let mut first_run = true;

    while !*cancel_rx.borrow() {
        if !first_run {
            if !config.auto_reconnect {
                break;
            }
            tracing::warn!(
                "[!] Automatically restarting connection using selected virtual audio configuration..."
            );
            update_status(&status_tx, &ui_repaint, ConnectionStatus::Connecting);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        break;
                    }
                }
            }
        }
        first_run = false;

        update_status(&status_tx, &ui_repaint, ConnectionStatus::Connecting);

        match run_single_signaling_cycle(
            &config,
            controls.clone(),
            cancel_rx.clone(),
            &status_tx,
            &ui_repaint,
        )
        .await
        {
            Ok(()) => {
                if *cancel_rx.borrow() || !config.auto_reconnect {
                    break;
                }
                tracing::warn!("[-] Session closed. Automatically reconnecting in 2 seconds...");
            }
            Err(err) => {
                if *cancel_rx.borrow() {
                    break;
                }
                tracing::error!("[!] Receiver Error: {}", err);
                if !config.auto_reconnect {
                    break;
                }
                tracing::warn!("Retrying connection in 5 seconds...");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        }
    }

    controls.reset_meters();
    update_status(&status_tx, &ui_repaint, ConnectionStatus::Disconnected);
    tracing::info!("[*] Engine shut down. Virtual audio resources released.");
}

async fn run_single_signaling_cycle(
    config: &SessionConfig,
    controls: Arc<AudioRuntimeControls>,
    mut cancel_rx: watch::Receiver<bool>,
    status_tx: &watch::Sender<ConnectionStatus>,
    ui_repaint: &Option<egui::Context>,
) -> Result<()> {
    // 1. Initialize CoreAudio Virtual Output Sink before connecting signaling
    let mut sink = CoreAudioVirtualSink::new(controls.clone());
    sink.initialize(TARGET_SAMPLE_RATE, 2, Some(&config.output_sink_uid))?;
    sink.configure_buffer_thresholds(VOICE_MAX_BACKLOG_SECS, VOICE_TARGET_CUSHION_SECS);
    sink.start()?;
    tracing::info!(
        "[*] CoreAudio Virtual Sink active: '{}'",
        sink.device_name()
    );

    let shared_sink = Arc::new(Mutex::new(sink));

    // 2. Connect to WebSocket signaling server
    let mut sig_client = SignalingClient::connect_with_retries(
        &config.signaling_url,
        &config.stream_secret,
        cancel_rx.clone(),
    )
    .await?;

    let sig_tx = sig_client.sender();
    update_status(status_tx, ui_repaint, ConnectionStatus::WaitingForClient);
    tracing::info!("[+] Waiting for remote mobile client to connect and stream audio...");

    let (peer_lost_tx, mut peer_lost_rx) = mpsc::unbounded_channel::<String>();
    let mut active_peer: Option<ActivePeerSession> = None;

    let cycle_result = loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    break Ok(());
                }
            }
            Some(reason) = peer_lost_rx.recv() => {
                tracing::warn!("[-] {}", reason);
                if let Some(peer) = active_peer.take() {
                    peer.shutdown().await;
                }
                controls.reset_meters();
                // Mirror Program.cs: exit cycle on WebRTC disconnect so supervisor cleanly reconnects
                break Ok(());
            }
            sig_ev = sig_client.next_event() => {
                match sig_ev {
                    SignalingEvent::Disconnected(reason) => {
                        tracing::warn!("[-] {}", reason);
                        break Ok(());
                    }
                    SignalingEvent::Message(msg) => {
                        match msg.msg_type.as_deref() {
                            Some("offer") => {
                                if let Some(prev) = active_peer.take() {
                                    prev.shutdown().await;
                                }
                                match handle_incoming_offer(
                                    msg,
                                    OfferSessionContext {
                                        config: config.clone(),
                                        shared_sink: shared_sink.clone(),
                                        controls: controls.clone(),
                                        sig_tx: sig_tx.clone(),
                                        peer_lost_tx: peer_lost_tx.clone(),
                                        status_tx: status_tx.clone(),
                                        ui_repaint: ui_repaint.clone(),
                                    },
                                ).await {
                                    Ok(new_peer) => {
                                        active_peer = Some(new_peer);
                                    }
                                    Err(err) => {
                                        tracing::error!("[!] Failed handling SDP Offer: {}", err);
                                    }
                                }
                            }
                            Some("candidate") => {
                                if let (Some(peer), Some(cand_payload)) = (active_peer.as_mut(), msg.candidate) {
                                    if let Some(cand_str) = cand_payload.candidate {
                                        if !cand_str.trim().is_empty() {
                                            tracing::debug!("Received remote ICE Candidate: {}", cand_str);
                                            let init = RTCIceCandidateInit {
                                                candidate: cand_str,
                                                sdp_mid: cand_payload.sdp_mid,
                                                sdp_mline_index: cand_payload.sdp_m_line_index,
                                                username_fragment: None,
                                            };
                                            if peer.remote_desc_ready {
                                                if let Err(err) = peer.peer_connection.add_ice_candidate(init).await {
                                                    tracing::debug!("Failed adding remote ICE candidate: {}", err);
                                                }
                                            } else {
                                                peer.pending_candidates.push(init);
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    };

    if let Some(peer) = active_peer.take() {
        peer.shutdown().await;
    }
    shared_sink.lock().stop();
    controls.reset_meters();

    cycle_result
}

struct OfferSessionContext {
    config: SessionConfig,
    shared_sink: Arc<Mutex<CoreAudioVirtualSink>>,
    controls: Arc<AudioRuntimeControls>,
    sig_tx: mpsc::UnboundedSender<SignalingMessage>,
    peer_lost_tx: mpsc::UnboundedSender<String>,
    status_tx: watch::Sender<ConnectionStatus>,
    ui_repaint: Option<egui::Context>,
}

async fn handle_incoming_offer(
    offer_msg: SignalingMessage,
    ctx: OfferSessionContext,
) -> Result<ActivePeerSession> {
    let OfferSessionContext {
        config,
        shared_sink,
        controls,
        sig_tx,
        peer_lost_tx,
        status_tx,
        ui_repaint,
    } = ctx;
    let offer_sdp = offer_msg
        .sdp
        .ok_or_else(|| anyhow!("Received SDP offer message with empty SDP"))?;

    let is_test_mode = offer_msg.is_test.unwrap_or(false);
    let optimize_for_voice = offer_msg.optimize_for_voice.unwrap_or(true);
    let channels: u16 = if optimize_for_voice { 1 } else { 2 };

    tracing::debug!(
        "Received SDP Offer: optimizeForVoice={}, channels={}, isTest={}",
        optimize_for_voice,
        channels,
        is_test_mode
    );

    // 1. Apply exact dynamic buffer hysteresis thresholds (`WasapiExclusiveSink.cs` parity)
    {
        let sink_guard = shared_sink.lock();
        if optimize_for_voice {
            sink_guard.configure_buffer_thresholds(
                VOICE_MAX_BACKLOG_SECS,
                VOICE_TARGET_CUSHION_SECS,
            );
        } else {
            sink_guard.configure_buffer_thresholds(
                MUSIC_MAX_BACKLOG_SECS,
                MUSIC_TARGET_CUSHION_SECS,
            );
        }
    }

    if is_test_mode {
        tracing::info!("[*] Test Mode activated for this WebRTC session.");
    }

    // 2. Configure MediaEngine & Opus codec parameters
    let opus_fmtp = if optimize_for_voice {
        VOICE_OPUS_FMTP
    } else {
        MUSIC_OPUS_FMTP
    };

    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: TARGET_SAMPLE_RATE,
                channels: 2,
                sdp_fmtp_line: opus_fmtp.to_owned(),
                rtcp_feedback: Vec::new(),
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let rtc_config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let peer_connection = Arc::new(api.new_peer_connection(rtc_config).await?);
    let (stop_tx, stop_rx) = watch::channel(false);

    // 3. Configure Track Direction per JSEP (RFC 8829):
    //    - `SendRecv`: calling `add_track` attaches our local Opus track onto the Offer's `m=audio` m-line (`a=sendrecv`).
    //    - `RecvOnly`: adding no local track causes `set_remote_description(offer)` to negotiate `a=recvonly`.
    let send_enabled = config.input_capture_uid.is_some() || is_test_mode;
    let local_track: Option<Arc<TrackLocalStaticSample>> = if send_enabled {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: TARGET_SAMPLE_RATE,
                channels: 2,
                sdp_fmtp_line: opus_fmtp.to_owned(),
                rtcp_feedback: Vec::new(),
            },
            "audio".to_owned(),
            "airmic-receiver".to_owned(),
        ));
        peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        Some(track)
    } else {
        None
    };

    // 4. Trickle ICE outgoing candidate handler (guarantee `candidate:` prefix)
    let sig_tx_cand = sig_tx.clone();
    peer_connection.on_ice_candidate(Box::new(move |cand: Option<RTCIceCandidate>| {
        let sig_tx_inner = sig_tx_cand.clone();
        Box::pin(async move {
            if let Some(c) = cand {
                if let Ok(json_cand) = c.to_json() {
                    if !json_cand.candidate.trim().is_empty() {
                        let formatted = ensure_candidate_prefix(&json_cand.candidate);
                        tracing::debug!("Gathered local ICE candidate: {}", formatted);
                        let msg = SignalingMessage {
                            msg_type: Some("candidate".to_string()),
                            sdp: None,
                            candidate: Some(IceCandidatePayload {
                                candidate: Some(formatted),
                                sdp_mid: json_cand.sdp_mid,
                                sdp_m_line_index: json_cand.sdp_mline_index,
                            }),
                            is_test: None,
                            optimize_for_voice: None,
                        };
                        let _ = sig_tx_inner.send(msg);
                    }
                }
            }
        })
    }));

    // 5. Shared `_isTestMode` state between incoming RTP decoder & loopback capture/playback worker
    let test_state = Arc::new(Mutex::new(TestLoopbackState::new()));

    // 6. Incoming RTP Opus audio handler
    let sink_for_rx = shared_sink.clone();
    let stop_rx_for_track = stop_rx.clone();
    let test_state_rx = test_state.clone();
    peer_connection.on_track(Box::new(move |track, _receiver, _transceiver| {
        let sink_clone = sink_for_rx.clone();
        let mut stop_track = stop_rx_for_track.clone();
        let test_state_inner = test_state_rx.clone();

        Box::pin(async move {
            if track.kind() != RTPCodecType::Audio {
                return;
            }

            let opus_channels = if channels == 1 {
                OpusChannels::Mono
            } else {
                OpusChannels::Stereo
            };

            let mut decoder = match opus::Decoder::new(TARGET_SAMPLE_RATE, opus_channels) {
                Ok(d) => d,
                Err(err) => {
                    tracing::error!("[!] Failed to initialize Opus decoder: {}", err);
                    return;
                }
            };

            // Max 120ms @ 48kHz stereo = 5760 * 2 = 11520 i16 samples (exact parity with C# `_pcmBuffer`)
            let mut pcm_buf = vec![0_i16; 11_520];
            let mut first_packet_logged = false;

            loop {
                tokio::select! {
                    _ = stop_track.changed() => {
                        if *stop_track.borrow() {
                            break;
                        }
                    }
                    read_res = track.read_rtp() => {
                        let Ok((rtp_packet, _attrs)) = read_res else {
                            break;
                        };
                        if rtp_packet.payload.is_empty() {
                            continue;
                        }

                        if !first_packet_logged {
                            first_packet_logged = true;
                            tracing::info!("[*] WebRTC: First incoming audio packet received from client.");
                        }

                        let max_frames = pcm_buf.len() / (channels as usize);
                        match decoder.decode(&rtp_packet.payload, &mut pcm_buf[..max_frames * (channels as usize)], false) {
                            Ok(decoded_frames) if decoded_frames > 0 => {
                                let total_samples = decoded_frames * (channels as usize);
                                let decoded_slice = &pcm_buf[..total_samples];

                                let is_playing_back = is_test_mode && test_state_inner.lock().is_playing_back.load(Ordering::Relaxed);
                                if !is_test_mode || !is_playing_back {
                                    sink_clone.lock().write_pcm_i16(decoded_slice, channels, TARGET_SAMPLE_RATE);
                                }

                                if is_test_mode {
                                    let rms = compute_rms_i16(decoded_slice);
                                    if rms >= SILENCE_THRESHOLD {
                                        let mut ts = test_state_inner.lock();
                                        ts.last_active_instant = Instant::now();
                                        let currently_playing = ts.is_playing_back.load(Ordering::Relaxed);
                                        if !ts.is_recording && !currently_playing {
                                            tracing::info!("[*] Test Loopback: Client speech detected. Recording started.");
                                            ts.cancel_playback();
                                            ts.recorded_samples.clear();
                                            ts.is_recording = true;
                                        }
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(err) => {
                                tracing::debug!("Opus decode error: {}", err);
                            }
                        }
                    }
                }
            }
        })
    }));

    // 7. PeerConnection state machine transitions + upstream capture startup
    let capture_uid_cfg = config.input_capture_uid.clone();
    let output_sink_uid_cfg = config.output_sink_uid.clone();
    let logs_dir_cfg = config.logs_dir.clone();
    let controls_for_state = controls.clone();
    let sink_for_tx = shared_sink.clone();
    let stop_rx_for_state = stop_rx.clone();
    let capture_started = Arc::new(AtomicBool::new(false));

    peer_connection.on_peer_connection_state_change(Box::new(
        move |state: RTCPeerConnectionState| {
            tracing::debug!("[WebRTC] Connection state: {}", state);

            match state {
                RTCPeerConnectionState::Connected => {
                    tracing::info!("[+] WebRTC Audio Stream established successfully!");
                    let connected_status = if optimize_for_voice {
                        ConnectionStatus::ConnectedVoice
                    } else {
                        ConnectionStatus::ConnectedMusic
                    };
                    update_status(&status_tx, &ui_repaint, connected_status);

                    if let Some(track_out) = local_track.clone() {
                        if !capture_started.swap(true, Ordering::AcqRel) {
                            spawn_upstream_capture_worker(UpstreamWorkerArgs {
                                capture_uid: capture_uid_cfg.clone(),
                                output_sink_uid: output_sink_uid_cfg.clone(),
                                is_test_mode,
                                optimize_for_voice,
                                channels,
                                local_track: track_out,
                                shared_sink: sink_for_tx.clone(),
                                controls: controls_for_state.clone(),
                                test_state: test_state.clone(),
                                logs_dir: logs_dir_cfg.clone(),
                                stop_rx: stop_rx_for_state.clone(),
                            });
                        }
                    } else {
                        tracing::info!(
                            "[*] Upstream Capture: None (RecvOnly mode — mobile mic -> virtual sink only)."
                        );
                    }
                }
                RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected => {
                    let _ = peer_lost_tx.send(format!("WebRTC Audio connection {}.", state));
                }
                _ => {}
            }

            Box::pin(async {})
        },
    ));

    // 8. Apply Remote SDP Offer, create SDP Answer, set local description, and enforce Opus `a=fmtp`
    tracing::debug!("Remote SDP Offer: {}", offer_sdp);
    let remote_desc = RTCSessionDescription::offer(offer_sdp)?;
    peer_connection.set_remote_description(remote_desc).await?;

    let answer = peer_connection.create_answer(None).await?;
    let raw_answer_sdp = answer.sdp.clone();
    peer_connection.set_local_description(answer).await?;

    let final_answer_sdp = force_sdp_opus_fmtp(&raw_answer_sdp, optimize_for_voice);
    tracing::debug!("Local SDP Answer: {}", final_answer_sdp);

    let answer_msg = SignalingMessage {
        msg_type: Some("answer".to_string()),
        sdp: Some(final_answer_sdp),
        candidate: None,
        is_test: None,
        optimize_for_voice: None,
    };
    sig_tx
        .send(answer_msg)
        .context("Failed queueing SDP Answer to signaling channel")?;
    tracing::debug!("[*] Sent SDP Answer to signaling client.");

    Ok(ActivePeerSession {
        peer_connection,
        stop_tx,
        pending_candidates: Vec::new(),
        remote_desc_ready: true,
    })
}

struct UpstreamWorkerArgs {
    capture_uid: Option<String>,
    output_sink_uid: String,
    is_test_mode: bool,
    optimize_for_voice: bool,
    channels: u16,
    local_track: Arc<TrackLocalStaticSample>,
    shared_sink: Arc<Mutex<CoreAudioVirtualSink>>,
    controls: Arc<AudioRuntimeControls>,
    test_state: Arc<Mutex<TestLoopbackState>>,
    logs_dir: PathBuf,
    stop_rx: watch::Receiver<bool>,
}

struct TestPlaybackArgs {
    recorded_pcm: Vec<i16>,
    channels: u16,
    optimize_for_voice: bool,
    local_track: Arc<TrackLocalStaticSample>,
    shared_sink: Arc<Mutex<CoreAudioVirtualSink>>,
    is_playing_back: Arc<AtomicBool>,
    cancel_rx: watch::Receiver<bool>,
    session_stop_rx: watch::Receiver<bool>,
}

fn spawn_upstream_capture_worker(args: UpstreamWorkerArgs) {
    let UpstreamWorkerArgs {
        capture_uid,
        output_sink_uid,
        is_test_mode,
        optimize_for_voice,
        channels,
        local_track,
        shared_sink,
        controls,
        test_state,
        logs_dir,
        stop_rx,
    } = args;
    // Determine which virtual bus to capture from:
    // - In normal `SendRecv` mode: capture from `capture_uid` (e.g. `BlackHole 16ch`).
    // - In `_isTestMode`: capture from the virtual sink bus itself (since BlackHole / VB-Cable
    //   loops its output right back into its own input!) so the full CoreAudio kernel driver path is tested!
    let effective_capture_uid = if is_test_mode {
        let devs = enumerate_virtual_devices();
        if devs
            .iter()
            .any(|d| d.uid == output_sink_uid && d.has_input())
        {
            Some(output_sink_uid)
        } else {
            capture_uid
        }
    } else {
        capture_uid
    };

    let Some(target_uid) = effective_capture_uid else {
        return;
    };

    let runtime_handle = tokio::runtime::Handle::current();

    std::thread::Builder::new()
        .name("airmic-upstream-capture".to_string())
        .spawn(move || {
            let mut source = CoreAudioVirtualSource::new(controls.clone());
            if let Err(err) = source.initialize(&target_uid) {
                tracing::error!("[!] Failed to initialize virtual capture device: {}", err);
                return;
            }
            if let Err(err) = source.start() {
                tracing::error!("[!] Failed to start virtual capture device: {}", err);
                return;
            }

            tracing::info!(
                "[+] Upstream Capture active on '{}'",
                source.device_name()
            );

            let opus_channels = if channels == 1 {
                OpusChannels::Mono
            } else {
                OpusChannels::Stereo
            };
            let opus_app = if optimize_for_voice {
                OpusApp::Voip
            } else {
                OpusApp::Audio
            };
            let bitrate = if optimize_for_voice { 24_000 } else { 64_000 };

            let mut encoder = match opus::Encoder::new(TARGET_SAMPLE_RATE, opus_channels, opus_app)
            {
                Ok(mut enc) => {
                    let _ = enc.set_bitrate(OpusBitrate::Bits(bitrate));
                    enc
                }
                Err(err) => {
                    tracing::error!("[!] Failed to create Opus encoder: {}", err);
                    source.stop();
                    return;
                }
            };

            let src_rate = source.device_sample_rate();
            let frame_samples_total = OPUS_FRAME_SAMPLES_PER_CH * (channels as usize);
            let mut stereo_read_buf = vec![0.0_f32; 4096];
            let mut converted_i16 = Vec::<i16>::with_capacity(4096);
            let mut pcm_queue = VecDeque::<i16>::with_capacity(TARGET_SAMPLE_RATE as usize * 2);
            let mut pcm_frame = vec![0_i16; frame_samples_total];
            let mut opus_out = vec![0_u8; 1275];

            while !*stop_rx.borrow() {
                let n = source.read_stereo_f32(&mut stereo_read_buf);
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }

                convert_captured_stereo_f32_to_i16(
                    &stereo_read_buf[..n],
                    src_rate,
                    TARGET_SAMPLE_RATE,
                    channels,
                    &mut converted_i16,
                );

                if is_test_mode {
                    let mut trigger_playback: Option<(Vec<i16>, f64)> = None;
                    let is_playing_back;

                    {
                        let mut ts = test_state.lock();
                        is_playing_back = ts.is_playing_back.load(Ordering::Relaxed);

                        if ts.is_recording {
                            ts.recorded_samples.extend_from_slice(&converted_i16);
                            let max_samples = (TARGET_SAMPLE_RATE as usize)
                                * (channels as usize)
                                * MAX_TEST_RECORD_SECONDS;

                            let silence_elapsed = ts.last_active_instant.elapsed().as_secs_f64();
                            if ts.recorded_samples.len() >= max_samples {
                                ts.is_recording = false;
                                let data = std::mem::take(&mut ts.recorded_samples);
                                trigger_playback = Some((data, MAX_TEST_RECORD_SECONDS as f64));
                            } else if silence_elapsed >= SILENCE_DURATION_LIMIT_SECS {
                                ts.is_recording = false;
                                let data = std::mem::take(&mut ts.recorded_samples);
                                if !data.is_empty() {
                                    trigger_playback = Some((data, silence_elapsed));
                                }
                            }
                        }
                    }

                    if let Some((recorded_pcm, elapsed_secs)) = trigger_playback {
                        let wav_path = logs_dir.join("loopback_test.wav");
                        if let Err(err) = write_loopback_wav(&wav_path, &recorded_pcm, channels) {
                            tracing::warn!("Failed saving loopback_test.wav: {}", err);
                        } else {
                            tracing::info!(
                                "[*] Test Loopback: Silence detected ({:.2}s) or limit reached. Saved to {}",
                                elapsed_secs,
                                wav_path.display()
                            );
                        }

                        let (pb_cancel_tx, pb_cancel_rx) = watch::channel(false);
                        let pb_flag = {
                            let mut ts = test_state.lock();
                            ts.cancel_playback();
                            ts.is_playing_back.store(true, Ordering::Relaxed);
                            ts.playback_cancel = Some(pb_cancel_tx);
                            ts.is_playing_back.clone()
                        };

                        let track_pb = local_track.clone();
                        let sink_pb = shared_sink.clone();
                        let stop_pb = stop_rx.clone();
                        runtime_handle.spawn(async move {
                            run_test_loopback_playback(TestPlaybackArgs {
                                recorded_pcm,
                                channels,
                                optimize_for_voice,
                                local_track: track_pb,
                                shared_sink: sink_pb,
                                is_playing_back: pb_flag,
                                cancel_rx: pb_cancel_rx,
                                session_stop_rx: stop_pb,
                            })
                            .await;
                        });
                    }

                    // Send silent Opus keepalive packets while not playing back so ICE/DTLS stays alive
                    if !is_playing_back {
                        for &s in &converted_i16 {
                            pcm_queue.push_back(s);
                        }
                        let silent_pkt = if channels == 1 {
                            Bytes::from_static(&SILENT_OPUS_MONO)
                        } else {
                            Bytes::from_static(&SILENT_OPUS_STEREO)
                        };

                        while pcm_queue.len() >= frame_samples_total {
                            pcm_queue.drain(..frame_samples_total);
                            let sample = Sample {
                                data: silent_pkt.clone(),
                                duration: Duration::from_millis(20),
                                ..Default::default()
                            };
                            let track_ref = local_track.clone();
                            let _ = runtime_handle.block_on(track_ref.write_sample(&sample));
                        }
                    } else {
                        pcm_queue.clear();
                    }

                    continue;
                }

                // Normal upstream streaming mode (Meeting App Speaker -> Mobile Speaker)
                if pcm_queue.len() > (TARGET_SAMPLE_RATE as usize) * (channels as usize) {
                    pcm_queue.clear();
                }
                for &s in &converted_i16 {
                    pcm_queue.push_back(s);
                }

                while pcm_queue.len() >= frame_samples_total {
                    for slot in &mut pcm_frame {
                        *slot = pcm_queue.pop_front().unwrap_or(0);
                    }

                    match encoder.encode(&pcm_frame, &mut opus_out) {
                        Ok(encoded_bytes) if encoded_bytes > 0 => {
                            let sample = Sample {
                                data: Bytes::copy_from_slice(&opus_out[..encoded_bytes]),
                                duration: Duration::from_millis(20),
                                ..Default::default()
                            };
                            let track_ref = local_track.clone();
                            let _ = runtime_handle.block_on(track_ref.write_sample(&sample));
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::error!("[!] Opus upstream encoding error: {}", err);
                        }
                    }
                }
            }

            source.stop();
            controls.store_outgoing_vu(0.0);
            tracing::debug!("Upstream capture worker stopped.");
        })
        .ok();
}

async fn run_test_loopback_playback(args: TestPlaybackArgs) {
    let TestPlaybackArgs {
        recorded_pcm,
        channels,
        optimize_for_voice,
        local_track,
        shared_sink,
        is_playing_back,
        mut cancel_rx,
        mut session_stop_rx,
    } = args;
    tracing::info!(
        "[*] Test Loopback: Playback started ({} samples).",
        recorded_pcm.len()
    );

    let opus_channels = if channels == 1 {
        OpusChannels::Mono
    } else {
        OpusChannels::Stereo
    };
    let opus_app = if optimize_for_voice {
        OpusApp::Voip
    } else {
        OpusApp::Audio
    };
    let bitrate = if optimize_for_voice { 24_000 } else { 64_000 };

    let mut encoder = match opus::Encoder::new(TARGET_SAMPLE_RATE, opus_channels, opus_app) {
        Ok(mut enc) => {
            let _ = enc.set_bitrate(OpusBitrate::Bits(bitrate));
            enc
        }
        Err(err) => {
            tracing::error!("[!] Test Loopback encoder init failed: {}", err);
            is_playing_back.store(false, Ordering::Relaxed);
            return;
        }
    };

    let frame_samples = OPUS_FRAME_SAMPLES_PER_CH * (channels as usize);
    let mut pcm_frame = vec![0_i16; frame_samples];
    let mut opus_out = vec![0_u8; 1275];
    let mut offset = 0_usize;
    let mut frames_sent = 0_u64;
    let start = Instant::now();

    while offset < recorded_pcm.len() && !*cancel_rx.borrow() && !*session_stop_rx.borrow() {
        let to_copy = frame_samples.min(recorded_pcm.len() - offset);
        if to_copy < frame_samples {
            pcm_frame.fill(0);
        }
        pcm_frame[..to_copy].copy_from_slice(&recorded_pcm[offset..offset + to_copy]);

        shared_sink
            .lock()
            .write_pcm_i16(&pcm_frame[..to_copy], channels, TARGET_SAMPLE_RATE);

        if let Ok(encoded_len) = encoder.encode(&pcm_frame, &mut opus_out) {
            if encoded_len > 0 {
                let sample = Sample {
                    data: Bytes::copy_from_slice(&opus_out[..encoded_len]),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                };
                let _ = local_track.write_sample(&sample).await;
            }
        }

        frames_sent += 1;
        offset += frame_samples;

        let target_wake = Duration::from_millis(frames_sent * 20);
        let elapsed = start.elapsed();
        if target_wake > elapsed {
            tokio::select! {
                _ = tokio::time::sleep(target_wake - elapsed) => {}
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        break;
                    }
                }
                _ = session_stop_rx.changed() => {
                    if *session_stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
    }

    is_playing_back.store(false, Ordering::Relaxed);
    if *cancel_rx.borrow() || *session_stop_rx.borrow() {
        tracing::info!("[*] Test Loopback: Playback cancelled.");
    } else {
        tracing::info!(
            "[*] Test Loopback: Playback finished. Sent {} audio frames.",
            frames_sent
        );
    }
}

fn write_loopback_wav(path: &std::path::Path, samples: &[i16], channels: u16) -> Result<()> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_fmtp_rewrites_existing_and_inserts_when_missing() {
        let crlf: String = [char::from(13), char::from(10)].into_iter().collect();
        let raw_answer_with_fmtp = [
            "v=0",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111",
            "a=rtpmap:111 opus/48000/2",
            "a=fmtp:111 minptime=10;useinbandfec=1",
            "",
        ]
        .join(&crlf);

        let voice_sdp = force_sdp_opus_fmtp(&raw_answer_with_fmtp, true);
        assert!(voice_sdp.contains(&format!("a=fmtp:111 {}", VOICE_OPUS_FMTP)));
        assert!(!voice_sdp.contains("minptime=10"));

        let music_sdp = force_sdp_opus_fmtp(&raw_answer_with_fmtp, false);
        assert!(music_sdp.contains(&format!("a=fmtp:111 {}", MUSIC_OPUS_FMTP)));

        let raw_answer_no_fmtp = [
            "v=0",
            "m=audio 9 UDP/TLS/RTP/SAVPF 109",
            "a=rtpmap:109 opus/48000/2",
            "a=sendrecv",
            "",
        ]
        .join(&crlf);

        let injected = force_sdp_opus_fmtp(&raw_answer_no_fmtp, true);
        let expected = format!("a=rtpmap:109 opus/48000/2{}a=fmtp:109 {}", crlf, VOICE_OPUS_FMTP);
        assert!(injected.contains(&expected));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_signaling_webrtc_opus_to_coreaudio_virtual_sink() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::tungstenite::Message;

        crate::signaling::ensure_rustls_crypto_provider();
        let (test_logs, _guard, _) = crate::logging::init_logging();

        let virtual_devs = enumerate_virtual_devices();
        let Some(out_dev) = virtual_devs.iter().find(|d| d.has_output()).cloned() else {
            // Skip hardware leg only on CI runners with zero virtual audio drivers installed
            return;
        };

        // 1. Start an in-process WebSocket relay hub matching AirMic.Server (`/ws?role=...&secret=...`)
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (to_client_tx, mut to_client_rx) = mpsc::unbounded_channel::<String>();
        let (to_receiver_tx, mut to_receiver_rx) = mpsc::unbounded_channel::<String>();
        let (receiver_ready_tx, receiver_ready_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let mut ready_opt = Some(receiver_ready_tx);
            // Connection 0 is receiver, Connection 1 is sender
            for conn_idx in 0..2 {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let ws = accept_async(tcp).await.unwrap();

                let (mut w, mut r) = ws.split();
                if conn_idx == 0 {
                    if let Some(tx) = ready_opt.take() {
                        let _ = tx.send(());
                    }
                    let forward_tx = to_client_tx.clone();
                    tokio::spawn(async move {
                        while let Some(Ok(Message::Text(txt))) = r.next().await {
                            let _ = forward_tx.send(txt.to_string());
                        }
                    });
                    let mut rx_in = to_receiver_rx;
                    tokio::spawn(async move {
                        while let Some(msg) = rx_in.recv().await {
                            if w.send(Message::Text(msg.into())).await.is_err() {
                                break;
                            }
                        }
                    });
                    let (_unused_tx, unused_rx) = mpsc::unbounded_channel();
                    to_receiver_rx = unused_rx;
                } else {
                    let forward_tx = to_receiver_tx.clone();
                    tokio::spawn(async move {
                        while let Some(Ok(Message::Text(txt))) = r.next().await {
                            let _ = forward_tx.send(txt.to_string());
                        }
                    });
                    let mut rx_in = to_client_rx;
                    tokio::spawn(async move {
                        while let Some(msg) = rx_in.recv().await {
                            if w.send(Message::Text(msg.into())).await.is_err() {
                                break;
                            }
                        }
                    });
                    let (_unused_tx, unused_rx) = mpsc::unbounded_channel();
                    to_client_rx = unused_rx;
                }
            }
        });

        // 2. Start ReceiverSessionHandle targeting the real CoreAudio virtual sink
        let controls = Arc::new(AudioRuntimeControls::new());
        let session_cfg = SessionConfig {
            signaling_url: format!("ws://{}/ws", addr),
            stream_secret: "TestSecret#2026".to_string(),
            output_sink_uid: out_dev.uid.clone(),
            input_capture_uid: None,
            auto_reconnect: false,
            logs_dir: std::env::temp_dir(),
        };

        let rt_handle = tokio::runtime::Handle::current();
        let receiver_handle =
            ReceiverSessionHandle::start(&rt_handle, session_cfg, controls.clone(), None);

        tokio::time::timeout(Duration::from_secs(5), receiver_ready_rx)
            .await
            .expect("Receiver did not connect to WS hub in time")
            .unwrap();

        // 3. Connect simulated mobile WebRTC client (`role=sender`)
        let sender_url = format!("ws://{}/ws?role=sender&secret=TestSecret%232026", addr);
        let (sender_ws, _) = tokio_tungstenite::connect_async(&sender_url).await.unwrap();
        let (mut sender_ws_write, mut sender_ws_read) = sender_ws.split();

        let mut m = MediaEngine::default();
        m.register_default_codecs().unwrap();
        let mut reg = Registry::new();
        reg = register_default_interceptors(reg, &mut m).unwrap();
        let client_api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(reg)
            .build();

        let client_pc = Arc::new(
            client_api
                .new_peer_connection(RTCConfiguration::default())
                .await
                .unwrap(),
        );

        let client_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: Vec::new(),
            },
            "audio".to_owned(),
            "mobile-mic".to_owned(),
        ));
        client_pc
            .add_track(Arc::clone(&client_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .unwrap();

        let (cand_out_tx, mut cand_out_rx) = mpsc::unbounded_channel::<SignalingMessage>();
        client_pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let tx = cand_out_tx.clone();
            Box::pin(async move {
                if let Some(cand) = c {
                    if let Ok(j) = cand.to_json() {
                        let _ = tx.send(SignalingMessage {
                            msg_type: Some("candidate".to_string()),
                            sdp: None,
                            candidate: Some(IceCandidatePayload {
                                candidate: Some(ensure_candidate_prefix(&j.candidate)),
                                sdp_mid: j.sdp_mid,
                                sdp_m_line_index: j.sdp_mline_index,
                            }),
                            is_test: None,
                            optimize_for_voice: None,
                        });
                    }
                }
            })
        }));

        let offer = client_pc.create_offer(None).await.unwrap();
        client_pc.set_local_description(offer.clone()).await.unwrap();

        let offer_msg = SignalingMessage {
            msg_type: Some("offer".to_string()),
            sdp: Some(offer.sdp),
            candidate: None,
            is_test: Some(false),
            optimize_for_voice: Some(true),
        };
        sender_ws_write
            .send(Message::Text(serde_json::to_string(&offer_msg).unwrap().into()))
            .await
            .unwrap();

        tokio::spawn(async move {
            while let Some(cmsg) = cand_out_rx.recv().await {
                let txt = serde_json::to_string(&cmsg).unwrap();
                if sender_ws_write.send(Message::Text(txt.into())).await.is_err() {
                    break;
                }
            }
        });

        let client_pc_rx = client_pc.clone();
        let (answer_verified_tx, answer_verified_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut ans_tx = Some(answer_verified_tx);
            while let Some(Ok(Message::Text(txt))) = sender_ws_read.next().await {
                let msg: SignalingMessage = serde_json::from_str(&txt).unwrap();
                match msg.msg_type.as_deref() {
                    Some("answer") => {
                        let sdp = msg.sdp.unwrap();
                        assert!(sdp.contains(VOICE_OPUS_FMTP));
                        assert!(sdp.contains("a=recvonly"));
                        let desc = RTCSessionDescription::answer(sdp).unwrap();
                        client_pc_rx.set_remote_description(desc).await.unwrap();
                        if let Some(tx) = ans_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    Some("candidate") => {
                        let cp = msg.candidate.unwrap();
                        let cstr = cp.candidate.unwrap();
                        assert!(cstr.starts_with("candidate:"));
                        let _ = client_pc_rx
                            .add_ice_candidate(RTCIceCandidateInit {
                                candidate: cstr,
                                sdp_mid: cp.sdp_mid,
                                sdp_mline_index: cp.sdp_m_line_index,
                                username_fragment: None,
                            })
                            .await;
                    }
                    _ => {}
                }
            }
        });

        if tokio::time::timeout(Duration::from_secs(5), answer_verified_rx)
            .await
            .is_err()
        {
            for entry in test_logs.snapshot(None) {
                eprintln!("{}", entry.format_line());
            }
            panic!("Did not receive SDP Answer in time");
        }

        // Wait for ConnectedVoice state
        let start_wait = Instant::now();
        while receiver_handle.status() != ConnectionStatus::ConnectedVoice {
            assert!(
                start_wait.elapsed() < Duration::from_secs(5),
                "Timed out waiting for ConnectedVoice status"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Encode & transmit 25 frames (500ms) of 440 Hz mono Opus audio and verify CoreAudio sink VU rises
        let mut enc = opus::Encoder::new(48_000, OpusChannels::Mono, OpusApp::Voip).unwrap();
        let mut pcm = vec![0_i16; 960];
        let mut opus_pkt = vec![0_u8; 512];
        let mut phase = 0.0_f64;
        for _ in 0..25 {
            for sample in &mut pcm {
                *sample = ((0.35 * phase.sin()) * (i16::MAX as f64)) as i16;
                phase += std::f64::consts::TAU * 440.0 / 48_000.0;
            }
            let len = enc.encode(&pcm, &mut opus_pkt).unwrap();
            client_track
                .write_sample(&Sample {
                    data: Bytes::copy_from_slice(&opus_pkt[..len]),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            controls.incoming_vu() > 0.05,
            "Expected CoreAudio virtual sink incoming VU > 0.05, got {}",
            controls.incoming_vu()
        );

        receiver_handle.stop();
        let _ = client_pc.close().await;
    }
}
