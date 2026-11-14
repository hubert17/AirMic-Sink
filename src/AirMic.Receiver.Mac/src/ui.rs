use crate::audio::coreaudio::{
    enumerate_virtual_devices, is_same_virtual_bus, meeting_app_counterpart_name,
    sanitize_routing_selection, select_default_routing, DeviceChangeWatcher, VirtualAudioDevice,
};
use crate::audio::{AudioRuntimeControls, SineWaveTestRunner};
use crate::logging::{LogBuffer, UiLogLevel};
use crate::webrtc_engine::{ConnectionStatus, ReceiverSessionHandle, SessionConfig};
use crate::PersistedConfig;
use egui::{Color32, CornerRadius, FontId, RichText, ScrollArea, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointPreset {
    LocalDev,
    ProductionPrimary,
    ProductionIis,
    Custom,
}

impl EndpointPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::LocalDev => "Local Dev (wss://localhost:7133/ws)",
            Self::ProductionPrimary => "Production (wss://airmic.bernardgabon.com/ws)",
            Self::ProductionIis => "Production IIS (wss://airmic-iis.bernardgabon.com/ws)",
            Self::Custom => "Custom URL",
        }
    }

    pub fn url(self, custom: &str) -> String {
        match self {
            Self::LocalDev => "wss://localhost:7133/ws".to_string(),
            Self::ProductionPrimary => "wss://airmic.bernardgabon.com/ws".to_string(),
            Self::ProductionIis => "wss://airmic-iis.bernardgabon.com/ws".to_string(),
            Self::Custom => custom.trim().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFilter {
    All,
    Info,
    Warn,
    Error,
}

impl LogFilter {
    fn to_level(self) -> Option<UiLogLevel> {
        match self {
            Self::All => None,
            Self::Info => Some(UiLogLevel::Info),
            Self::Warn => Some(UiLogLevel::Warn),
            Self::Error => Some(UiLogLevel::Error),
        }
    }
}

pub struct AirMicApp {
    tokio_handle: tokio::runtime::Handle,
    logs: LogBuffer,
    logs_dir: PathBuf,
    controls: Arc<AudioRuntimeControls>,
    device_watcher: DeviceChangeWatcher,

    // Virtual audio devices & routing
    virtual_devices: Vec<VirtualAudioDevice>,
    selected_output_uid: Option<String>,
    selected_input_uid: Option<String>,

    // Session & signaling settings
    preset: EndpointPreset,
    custom_url: String,
    stream_secret: String,
    show_secret: bool,
    auto_reconnect: bool,

    // Audio controls
    mute_incoming: bool,
    output_gain_pct: f32,

    // Active workers
    session_handle: Option<ReceiverSessionHandle>,
    sine_test: Option<SineWaveTestRunner>,
    last_observed_status: ConnectionStatus,

    // Logs panel UI state
    logs_expanded: bool,
    log_filter: LogFilter,
    auto_scroll_logs: bool,

    // Config persistence callback
    config_dirty: bool,
    secret_dirty: bool,
}

impl AirMicApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tokio_handle: tokio::runtime::Handle,
        logs: LogBuffer,
        logs_dir: PathBuf,
        persisted: PersistedConfig,
        initial_secret: String,
    ) -> Self {
        configure_macos_dark_theme(&cc.egui_ctx);

        let controls = Arc::new(AudioRuntimeControls::new());
        let gain_pct = persisted.output_gain_pct.clamp(0.0, 150.0);
        controls.set_output_gain(gain_pct / 100.0);

        let virtual_devices = enumerate_virtual_devices();
        let routing = if persisted.has_saved_routing {
            sanitize_routing_selection(
                &virtual_devices,
                persisted.output_sink_uid.as_deref(),
                persisted.input_capture_uid.as_deref(),
            )
        } else {
            select_default_routing(&virtual_devices)
        };

        if virtual_devices.is_empty() {
            tracing::warn!(
                "[!] No virtual audio cable detected. Install BlackHole (`brew install --cask blackhole-2ch`) to enable routing."
            );
        } else {
            let names: Vec<&str> = virtual_devices.iter().map(|d| d.name.as_str()).collect();
            tracing::info!(
                "[*] Detected {} virtual audio bus(es): {}",
                virtual_devices.len(),
                names.join(", ")
            );
        }

        Self {
            tokio_handle,
            logs,
            logs_dir,
            controls,
            device_watcher: DeviceChangeWatcher::new(),
            virtual_devices,
            selected_output_uid: routing.output_sink_uid,
            selected_input_uid: routing.input_capture_uid,
            preset: persisted.endpoint_preset,
            custom_url: persisted.custom_url,
            stream_secret: initial_secret,
            show_secret: false,
            auto_reconnect: persisted.auto_reconnect,
            mute_incoming: false,
            output_gain_pct: gain_pct,
            session_handle: None,
            sine_test: None,
            last_observed_status: ConnectionStatus::Disconnected,
            logs_expanded: persisted.logs_expanded,
            log_filter: LogFilter::All,
            auto_scroll_logs: true,
            config_dirty: false,
            secret_dirty: false,
        }
    }

    fn refresh_virtual_devices(&mut self) {
        let updated = enumerate_virtual_devices();
        let sanitized = sanitize_routing_selection(
            &updated,
            self.selected_output_uid.as_deref(),
            self.selected_input_uid.as_deref(),
        );
        // If previously had 0 devices and user just installed BlackHole, apply smart defaults
        let final_routing = if self.virtual_devices.is_empty() && !updated.is_empty() {
            select_default_routing(&updated)
        } else {
            sanitized
        };

        self.virtual_devices = updated;
        self.selected_output_uid = final_routing.output_sink_uid;
        self.selected_input_uid = final_routing.input_capture_uid;
        self.config_dirty = true;
    }

    fn current_status(&mut self) -> ConnectionStatus {
        let status = match &self.session_handle {
            Some(handle) => handle.status(),
            None => ConnectionStatus::Disconnected,
        };

        // Clean up finished handle if supervisor exited without auto-reconnect
        if status == ConnectionStatus::Disconnected
            && self.last_observed_status != ConnectionStatus::Disconnected
        {
            self.session_handle = None;
        }
        self.last_observed_status = status;
        status
    }

    fn flush_persistence_if_needed(&mut self) {
        if self.config_dirty {
            self.config_dirty = false;
            let cfg = PersistedConfig {
                endpoint_preset: self.preset,
                custom_url: self.custom_url.clone(),
                auto_reconnect: self.auto_reconnect,
                has_saved_routing: true,
                output_sink_uid: self.selected_output_uid.clone(),
                input_capture_uid: self.selected_input_uid.clone(),
                output_gain_pct: self.output_gain_pct,
                logs_expanded: self.logs_expanded,
            };
            crate::save_persisted_config(&cfg);
        }

        if self.secret_dirty {
            self.secret_dirty = false;
            crate::save_stream_secret(&self.stream_secret);
        }
    }

    fn start_connection(&mut self, egui_ctx: &egui::Context) {
        if let Some(mut test) = self.sine_test.take() {
            test.stop();
        }

        let Some(output_uid) = self.selected_output_uid.clone() else {
            tracing::error!("[!] Cannot connect: No Virtual Output Sink selected.");
            return;
        };

        let signaling_url = self.preset.url(&self.custom_url);
        if signaling_url.trim().is_empty() {
            tracing::error!("[!] Cannot connect: Signaling URL is empty.");
            return;
        }
        if self.stream_secret.trim().is_empty() {
            tracing::error!("[!] Cannot connect: Stream Key is required.");
            return;
        }

        self.flush_persistence_if_needed();

        let cfg = SessionConfig {
            signaling_url,
            stream_secret: self.stream_secret.clone(),
            output_sink_uid: output_uid,
            input_capture_uid: self.selected_input_uid.clone(),
            auto_reconnect: self.auto_reconnect,
            logs_dir: self.logs_dir.clone(),
        };

        let handle = ReceiverSessionHandle::start(
            &self.tokio_handle,
            cfg,
            self.controls.clone(),
            Some(egui_ctx.clone()),
        );
        self.last_observed_status = ConnectionStatus::Connecting;
        self.session_handle = Some(handle);
    }

    fn stop_connection(&mut self) {
        if let Some(handle) = self.session_handle.take() {
            handle.stop();
        }
        self.last_observed_status = ConnectionStatus::Disconnected;
        self.controls.reset_meters();
    }

    fn toggle_440hz_test(&mut self) {
        if let Some(mut active) = self.sine_test.take() {
            active.stop();
            return;
        }

        let Some(ref out_uid) = self.selected_output_uid else {
            tracing::error!("[!] Cannot start 440 Hz test: No virtual output sink available.");
            return;
        };

        match SineWaveTestRunner::start(out_uid, self.controls.clone()) {
            Ok(runner) => {
                self.sine_test = Some(runner);
            }
            Err(err) => {
                tracing::error!("[!] Failed to start 440 Hz Sink Test: {}", err);
            }
        }
    }

    fn render_top_session_bar(&mut self, ui: &mut egui::Ui, status: ConnectionStatus) {
        let is_busy = status.is_active_session();
        let has_virtual_sink = self.selected_output_uid.is_some();

        egui::Frame::new()
            .fill(Color32::from_rgb(24, 26, 34))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 48, 62)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(14.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("AirMic Receiver")
                            .font(FontId::proportional(18.0))
                            .strong()
                            .color(Color32::from_rgb(235, 240, 255)),
                    );
                    ui.label(
                        RichText::new("macOS Apple Silicon · Virtual Audio Bridge")
                            .size(12.0)
                            .color(Color32::from_rgb(138, 146, 168)),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        render_status_pill(ui, status, self.sine_test.is_some());
                    });
                });

                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!is_busy, |ui| {
                        ui.label(RichText::new("Endpoint:").color(Color32::from_rgb(180, 186, 204)));
                        egui::ComboBox::from_id_salt("endpoint_preset_combo")
                            .selected_text(self.preset.label())
                            .width(275.0)
                            .show_ui(ui, |ui| {
                                for item in [
                                    EndpointPreset::LocalDev,
                                    EndpointPreset::ProductionPrimary,
                                    EndpointPreset::ProductionIis,
                                    EndpointPreset::Custom,
                                ] {
                                    if ui
                                        .selectable_value(&mut self.preset, item, item.label())
                                        .changed()
                                    {
                                        self.config_dirty = true;
                                    }
                                }
                            });

                        if self.preset == EndpointPreset::Custom {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut self.custom_url)
                                    .desired_width(210.0)
                                    .hint_text("wss://signaling.yourdomain.com/ws"),
                            );
                            if resp.changed() {
                                self.config_dirty = true;
                            }
                        }
                    });
                });

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!is_busy, |ui| {
                        ui.label(RichText::new("Stream Key:").color(Color32::from_rgb(180, 186, 204)));
                        let key_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.stream_secret)
                                .password(!self.show_secret)
                                .desired_width(220.0)
                                .hint_text("Enter shared stream secret"),
                        );
                        if key_resp.changed() {
                            self.secret_dirty = true;
                        }

                        let toggle_label = if self.show_secret { "Hide" } else { "Show" };
                        if ui.button(toggle_label).clicked() {
                            self.show_secret = !self.show_secret;
                        }

                        if ui
                            .checkbox(&mut self.auto_reconnect, "Auto-Reconnect")
                            .changed()
                        {
                            self.config_dirty = true;
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !is_busy {
                            let can_connect =
                                has_virtual_sink && !self.stream_secret.trim().is_empty();
                            let connect_btn = egui::Button::new(
                                RichText::new("  Connect  ")
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(if can_connect {
                                Color32::from_rgb(59, 130, 246)
                            } else {
                                Color32::from_rgb(55, 62, 78)
                            })
                            .min_size(Vec2::new(118.0, 30.0));

                            let resp = ui.add_enabled(can_connect, connect_btn);
                            if !has_virtual_sink {
                                resp.on_disabled_hover_text(
                                    "Install a virtual audio device (e.g. BlackHole 2ch) to connect.",
                                );
                            } else if self.stream_secret.trim().is_empty() {
                                resp.on_disabled_hover_text("Enter your Stream Key first.");
                            } else if resp.clicked() {
                                let ctx = ui.ctx().clone();
                                self.start_connection(&ctx);
                            }
                        } else {
                            let disc_btn = egui::Button::new(
                                RichText::new("Disconnect")
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(220, 62, 66))
                            .min_size(Vec2::new(118.0, 30.0));

                            if ui.add(disc_btn).clicked() {
                                self.stop_connection();
                            }
                        }
                    });
                });
            });
    }

    fn render_missing_virtual_device_banner(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(Color32::from_rgb(48, 32, 20))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(217, 119, 6)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Virtual Audio Driver Required (BlackHole)")
                            .strong()
                            .color(Color32::from_rgb(253, 186, 116)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Rescan Audio Buses").clicked() {
                            self.refresh_virtual_devices();
                        }
                    });
                });

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "AirMic routes strictly through virtual audio cables (never physical speakers/mics). Install free BlackHole via Homebrew:",
                    )
                    .size(12.5)
                    .color(Color32::from_rgb(235, 220, 205)),
                );
                ui.add_space(6.0);

                let cmd_2ch = "brew install --cask blackhole-2ch";
                ui.horizontal(|ui| {
                    ui.code(cmd_2ch);
                    ui.label(
                        RichText::new("(Required — Mic-only / RecvOnly)")
                            .size(12.0)
                            .color(Color32::from_rgb(190, 180, 170)),
                    );
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(cmd_2ch.to_string());
                    }
                });

                let cmd_16ch = "brew install --cask blackhole-16ch";
                ui.horizontal(|ui| {
                    ui.code(cmd_16ch);
                    ui.label(
                        RichText::new("(Optional 2nd bus — Bidirectional call audio)")
                            .size(12.0)
                            .color(Color32::from_rgb(190, 180, 170)),
                    );
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(cmd_16ch.to_string());
                    }
                });
            });
    }

    fn render_audio_routing_and_controls(&mut self, ui: &mut egui::Ui, status: ConnectionStatus) {
        let routing_locked = status.is_active_session() || self.sine_test.is_some();

        egui::Frame::new()
            .fill(Color32::from_rgb(24, 26, 34))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 48, 62)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(14.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Virtual Audio Routing & Playout Controls")
                            .strong()
                            .size(14.5)
                            .color(Color32::from_rgb(225, 232, 248)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!routing_locked, egui::Button::new("Refresh Buses"))
                            .clicked()
                        {
                            self.refresh_virtual_devices();
                        }
                    });
                });

                ui.add_space(8.0);

                let current_out_dev = self
                    .selected_output_uid
                    .as_deref()
                    .and_then(|uid| self.virtual_devices.iter().find(|d| d.uid == uid))
                    .cloned();

                let current_in_dev = self
                    .selected_input_uid
                    .as_deref()
                    .and_then(|uid| self.virtual_devices.iter().find(|d| d.uid == uid))
                    .cloned();

                // Dropdowns grid
                egui::Grid::new("virtual_routing_grid")
                    .num_columns(2)
                    .spacing([14.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Virtual Output Sink (Mobile Mic -> Meeting Mic):")
                                .color(Color32::from_rgb(180, 186, 204)),
                        );

                        ui.add_enabled_ui(!routing_locked && !self.virtual_devices.is_empty(), |ui| {
                            let out_label = current_out_dev
                                .as_ref()
                                .map(|d| format!("{} ({} ch · {} Hz)", d.name, d.output_channels, d.sample_rate))
                                .unwrap_or_else(|| "No virtual output bus detected".to_string());

                            egui::ComboBox::from_id_salt("virtual_output_sink_combo")
                                .selected_text(out_label)
                                .width(310.0)
                                .show_ui(ui, |ui| {
                                    for dev in self.virtual_devices.iter().filter(|d| d.has_output()) {
                                        let label = format!(
                                            "{} ({} ch · {} Hz)",
                                            dev.name, dev.output_channels, dev.sample_rate
                                        );
                                        let selected =
                                            self.selected_output_uid.as_deref() == Some(dev.uid.as_str());
                                        if ui.selectable_label(selected, label).clicked() {
                                            self.selected_output_uid = Some(dev.uid.clone());
                                            // Enforce same-bus loopback prevention immediately
                                            if let Some(ref in_uid) = self.selected_input_uid {
                                                if let Some(in_dev) =
                                                    self.virtual_devices.iter().find(|d| &d.uid == in_uid)
                                                {
                                                    if is_same_virtual_bus(dev, in_dev) {
                                                        self.selected_input_uid = None;
                                                        tracing::info!(
                                                            "[*] Reset Virtual Input Capture to None (RecvOnly) to prevent looping '{}' into itself.",
                                                            dev.name
                                                        );
                                                    }
                                                }
                                            }
                                            self.config_dirty = true;
                                        }
                                    }
                                });
                        });
                        ui.end_row();

                        ui.label(
                            RichText::new("Virtual Input Capture (Meeting Speaker -> Mobile):")
                                .color(Color32::from_rgb(180, 186, 204)),
                        );

                        ui.add_enabled_ui(!routing_locked && !self.virtual_devices.is_empty(), |ui| {
                            let in_label = current_in_dev
                                .as_ref()
                                .map(|d| format!("{} ({} ch · SendRecv)", d.name, d.input_channels))
                                .unwrap_or_else(|| "None (RecvOnly — Mic Only)".to_string());

                            egui::ComboBox::from_id_salt("virtual_input_capture_combo")
                                .selected_text(in_label)
                                .width(310.0)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(
                                            self.selected_input_uid.is_none(),
                                            "None (RecvOnly — Mic Only)",
                                        )
                                        .clicked()
                                    {
                                        self.selected_input_uid = None;
                                        self.config_dirty = true;
                                    }

                                    for dev in self.virtual_devices.iter().filter(|d| d.has_input()) {
                                        let same_as_sink = current_out_dev
                                            .as_ref()
                                            .map(|out| is_same_virtual_bus(out, dev))
                                            .unwrap_or(false);

                                        if same_as_sink {
                                            ui.add_enabled_ui(false, |ui| {
                                                let _ = ui.selectable_label(
                                                    false,
                                                    format!("{} (in use as Output Sink)", dev.name),
                                                );
                                            });
                                        } else {
                                            let selected = self.selected_input_uid.as_deref()
                                                == Some(dev.uid.as_str());
                                            let label = format!(
                                                "{} ({} ch · {} Hz)",
                                                dev.name, dev.input_channels, dev.sample_rate
                                            );
                                            if ui.selectable_label(selected, label).clicked() {
                                                self.selected_input_uid = Some(dev.uid.clone());
                                                self.config_dirty = true;
                                            }
                                        }
                                    }
                                });
                        });
                        ui.end_row();
                    });

                // Counterpart Meeting App Helper Banner
                if let Some(ref out_dev) = current_out_dev {
                    ui.add_space(10.0);
                    egui::Frame::new()
                        .fill(Color32::from_rgb(19, 34, 48))
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(38, 92, 142)))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            let mic_target = meeting_app_counterpart_name(Some(out_dev));
                            let spk_target = meeting_app_counterpart_name(current_in_dev.as_ref());
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new("Set Zoom / Teams Mic:")
                                        .size(12.5)
                                        .color(Color32::from_rgb(147, 197, 253)),
                                );
                                ui.label(
                                    RichText::new(&mic_target)
                                        .size(12.5)
                                        .strong()
                                        .color(Color32::WHITE),
                                );
                                ui.separator();
                                ui.label(
                                    RichText::new("Set Zoom / Teams Speaker:")
                                        .size(12.5)
                                        .color(Color32::from_rgb(147, 197, 253)),
                                );
                                ui.label(
                                    RichText::new(&spk_target)
                                        .size(12.5)
                                        .strong()
                                        .color(Color32::WHITE),
                                );
                            });
                        });
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);

                // Playout Controls: Mute Incoming, Output Gain (0%–150%), 1-click 440 Hz Sink Test
                ui.horizontal(|ui| {
                    let mute_btn = egui::Button::new(
                        RichText::new(if self.mute_incoming {
                            "Muted Incoming"
                        } else {
                            "Mute Incoming"
                        })
                        .color(if self.mute_incoming {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(215, 220, 235)
                        }),
                    )
                    .fill(if self.mute_incoming {
                        Color32::from_rgb(202, 58, 62)
                    } else {
                        Color32::from_rgb(42, 46, 58)
                    });

                    if ui.add(mute_btn).clicked() {
                        self.mute_incoming = !self.mute_incoming;
                        self.controls.set_mute_incoming(self.mute_incoming);
                    }

                    ui.add_space(10.0);
                    ui.label(RichText::new("Output Gain:").color(Color32::from_rgb(180, 186, 204)));
                    let slider = egui::Slider::new(&mut self.output_gain_pct, 0.0..=150.0)
                        .suffix("%")
                        .fixed_decimals(0);
                    let slider_resp = ui.add(slider);
                    if slider_resp.double_clicked() {
                        self.output_gain_pct = 100.0;
                        self.controls.set_output_gain(1.0);
                        self.config_dirty = true;
                    } else if slider_resp.changed() {
                        self.controls.set_output_gain(self.output_gain_pct / 100.0);
                        self.config_dirty = true;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let test_running = self.sine_test.is_some();
                        let can_test =
                            self.selected_output_uid.is_some() && !status.is_active_session();
                        let test_btn = egui::Button::new(
                            RichText::new(if test_running {
                                "Stop 440 Hz Sink Test"
                            } else {
                                "440 Hz Sink Test"
                            })
                            .strong()
                            .color(Color32::WHITE),
                        )
                        .fill(if test_running {
                            Color32::from_rgb(217, 119, 6)
                        } else if can_test {
                            Color32::from_rgb(79, 70, 229)
                        } else {
                            Color32::from_rgb(52, 56, 70)
                        });

                        if ui.add_enabled(can_test || test_running, test_btn).clicked() {
                            self.toggle_440hz_test();
                        }
                    });
                });

                ui.add_space(10.0);

                // Live Telemetry Strip: Incoming & Outgoing VU meters + Jitter Buffer Fill (ms)
                let in_vu = self.controls.incoming_vu();
                let out_vu = self.controls.outgoing_vu();
                let buf_ms = self.controls.buffer_fill_ms();

                egui::Frame::new()
                    .fill(Color32::from_rgb(17, 19, 25))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Incoming VU:")
                                    .size(12.0)
                                    .color(Color32::from_rgb(165, 172, 190)),
                            );
                            draw_vu_bar(ui, in_vu, 150.0, self.mute_incoming);

                            ui.add_space(12.0);
                            ui.label(
                                RichText::new("Outgoing VU:")
                                    .size(12.0)
                                    .color(Color32::from_rgb(165, 172, 190)),
                            );
                            if self.selected_input_uid.is_some() || status.is_streaming() {
                                draw_vu_bar(ui, out_vu, 150.0, false);
                            } else {
                                ui.label(
                                    RichText::new("RecvOnly")
                                        .size(11.5)
                                        .color(Color32::from_rgb(110, 118, 138)),
                                );
                            }

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let buf_color = if buf_ms > 190.0 {
                                        Color32::from_rgb(251, 191, 36)
                                    } else if buf_ms > 5.0 {
                                        Color32::from_rgb(52, 211, 153)
                                    } else {
                                        Color32::from_rgb(148, 163, 184)
                                    };
                                    ui.label(
                                        RichText::new(format!("Buffer: {:.0} ms", buf_ms))
                                            .monospace()
                                            .size(12.0)
                                            .color(buf_color),
                                    );
                                },
                            );
                        });
                    });
            });
    }

    fn render_bottom_logs_panel(&mut self, ctx: &egui::Context) {
        let panel = egui::TopBottomPanel::bottom("bottom_logs_panel")
            .resizable(self.logs_expanded)
            .min_height(if self.logs_expanded { 125.0 } else { 36.0 })
            .default_height(195.0)
            .max_height(420.0);

        panel.show(ctx, |ui| {
            let entries = self.logs.snapshot(self.log_filter.to_level());

            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let arrow = if self.logs_expanded { "▾" } else { "▸" };
                if ui
                    .button(
                        RichText::new(format!("{} Logs ({})", arrow, entries.len()))
                            .strong()
                            .size(12.5),
                    )
                    .clicked()
                {
                    self.logs_expanded = !self.logs_expanded;
                    self.config_dirty = true;
                }

                ui.separator();

                for (flt, text) in [
                    (LogFilter::All, "ALL"),
                    (LogFilter::Info, "INFO"),
                    (LogFilter::Warn, "WARN"),
                    (LogFilter::Error, "ERROR"),
                ] {
                    let active = self.log_filter == flt;
                    if ui.selectable_label(active, text).clicked() {
                        self.log_filter = flt;
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        self.logs.clear();
                    }
                    if ui.small_button("Copy Logs").clicked() {
                        let sep = String::from(char::from(10));
                        let joined = entries
                            .iter()
                            .map(|e| e.format_line())
                            .collect::<Vec<_>>()
                            .join(&sep);
                        ui.ctx().copy_text(joined);
                    }
                    ui.checkbox(&mut self.auto_scroll_logs, "Auto-scroll");
                });
            });

            if !self.logs_expanded {
                return;
            }

            ui.separator();

            let row_height = 17.0;
            let total_rows = entries.len();

            ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(self.auto_scroll_logs)
                .show_rows(ui, row_height, total_rows, |ui, row_range| {
                    for idx in row_range {
                        let entry = &entries[idx];
                        let color = match entry.level {
                            UiLogLevel::Info => Color32::from_rgb(203, 213, 225),
                            UiLogLevel::Warn => Color32::from_rgb(251, 191, 36),
                            UiLogLevel::Error => Color32::from_rgb(248, 113, 113),
                        };
                        ui.label(
                            RichText::new(entry.format_line())
                                .monospace()
                                .size(11.5)
                                .color(color),
                        );
                    }
                });
        });
    }
}

impl eframe::App for AirMicApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.device_watcher.take_dirty() {
            self.refresh_virtual_devices();
        }

        let status = self.current_status();

        self.render_bottom_logs_panel(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.render_top_session_bar(ui, status);
                    ui.add_space(10.0);

                    if self.virtual_devices.is_empty() {
                        self.render_missing_virtual_device_banner(ui);
                        ui.add_space(10.0);
                    }

                    self.render_audio_routing_and_controls(ui, status);
                });
        });

        self.flush_persistence_if_needed();

        // Throttle live VU/buffer meter repaints to ~25 Hz (40ms) strictly while streaming
        // or running the 440 Hz test AND while the window is visible/unminimized.
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        let meters_active = status.is_streaming() || self.sine_test.is_some();
        if meters_active && !is_minimized {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
    }

    fn on_exit(&mut self) {
        if let Some(mut test) = self.sine_test.take() {
            test.stop();
        }
        self.stop_connection();
        self.flush_persistence_if_needed();
    }
}

fn render_status_pill(ui: &mut egui::Ui, status: ConnectionStatus, test_tone_active: bool) {
    let (label, bg, fg) = if test_tone_active && status == ConnectionStatus::Disconnected {
        (
            "Testing · 440 Hz Tone",
            Color32::from_rgb(92, 54, 14),
            Color32::from_rgb(253, 186, 116),
        )
    } else {
        match status {
            ConnectionStatus::Disconnected => (
                status.label(),
                Color32::from_rgb(42, 46, 58),
                Color32::from_rgb(172, 179, 196),
            ),
            ConnectionStatus::Connecting => (
                status.label(),
                Color32::from_rgb(88, 56, 16),
                Color32::from_rgb(252, 211, 77),
            ),
            ConnectionStatus::WaitingForClient => (
                status.label(),
                Color32::from_rgb(22, 62, 104),
                Color32::from_rgb(147, 197, 253),
            ),
            ConnectionStatus::ConnectedVoice => (
                status.label(),
                Color32::from_rgb(16, 78, 54),
                Color32::from_rgb(110, 231, 183),
            ),
            ConnectionStatus::ConnectedMusic => (
                status.label(),
                Color32::from_rgb(56, 38, 108),
                Color32::from_rgb(196, 181, 253),
            ),
        }
    };

    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(12.0).strong().color(fg));
        });
}

fn draw_vu_bar(ui: &mut egui::Ui, rms: f32, width: f32, muted: bool) {
    // Map normalized RMS [0.0..0.5] nonlinearly with sqrt for responsive visual speech bounce
    let level = if muted {
        0.0
    } else {
        (rms * 2.2).sqrt().clamp(0.0, 1.0)
    };

    let (rect, _resp) = ui.allocate_exact_size(Vec2::new(width, 12.0), egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, CornerRadius::same(4), Color32::from_rgb(34, 38, 48));

    if level > 0.005 {
        let mut fill_rect = rect;
        fill_rect.set_width((rect.width() * level).max(2.0));
        let bar_color = if level > 0.88 {
            Color32::from_rgb(239, 68, 68)
        } else if level > 0.68 {
            Color32::from_rgb(245, 158, 11)
        } else {
            Color32::from_rgb(16, 185, 129)
        };
        painter.rect_filled(fill_rect, CornerRadius::same(4), bar_color);
    }
}

fn configure_macos_dark_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(15, 17, 23);
    visuals.window_fill = Color32::from_rgb(20, 22, 30);
    visuals.extreme_bg_color = Color32::from_rgb(11, 13, 18);
    ctx.set_visuals(visuals);
}
