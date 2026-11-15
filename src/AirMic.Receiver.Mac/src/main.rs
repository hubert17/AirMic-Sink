pub mod audio;
pub mod logging;
pub mod signaling;
pub mod ui;
pub mod webrtc_engine;

use crate::ui::{AirMicApp, EndpointPreset};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedConfig {
    pub endpoint_preset: EndpointPreset,
    pub custom_url: String,
    pub auto_reconnect: bool,
    pub has_saved_routing: bool,
    pub output_sink_uid: Option<String>,
    pub input_capture_uid: Option<String>,
    pub output_gain_pct: f32,
    pub logs_expanded: bool,
}

impl Default for PersistedConfig {
    fn default() -> Self {
        Self {
            endpoint_preset: EndpointPreset::LocalDev,
            custom_url: "wss://localhost:7133/ws".to_string(),
            auto_reconnect: true,
            has_saved_routing: false,
            output_sink_uid: None,
            input_capture_uid: None,
            output_gain_pct: 100.0,
            logs_expanded: true,
        }
    }
}

pub fn app_support_dir() -> PathBuf {
    if let Some(base) = BaseDirs::new() {
        let dir = base
            .home_dir()
            .join("Library")
            .join("Application Support")
            .join("AirMic.Receiver");
        let _ = fs::create_dir_all(&dir);
        return dir;
    }
    let fallback = std::env::temp_dir().join("AirMic.Receiver");
    let _ = fs::create_dir_all(&fallback);
    fallback
}

pub fn load_persisted_config() -> PersistedConfig {
    let cfg_path = app_support_dir().join("config.json");
    match fs::read_to_string(&cfg_path) {
        Ok(raw) => serde_json::from_str::<PersistedConfig>(&raw).unwrap_or_default(),
        Err(_) => PersistedConfig::default(),
    }
}

pub fn save_persisted_config(cfg: &PersistedConfig) {
    let cfg_path = app_support_dir().join("config.json");
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(cfg_path, json);
    }
}

/// Loads Stream Key from `AIRMIC_SECRET` env var or local `0600` file in App Support (no Keychain prompt).
pub fn load_stream_secret() -> String {
    if let Ok(env_sec) = std::env::var("AIRMIC_SECRET") {
        if !env_sec.trim().is_empty() {
            return env_sec;
        }
    }

    let secret_path = app_support_dir().join(".stream_secret");
    fs::read_to_string(secret_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Saves Stream Key to a local `0600`-permission file in App Support (no Keychain prompt).
pub fn save_stream_secret(secret: &str) {
    let trimmed = secret.trim();
    let secret_path = app_support_dir().join(".stream_secret");

    if trimmed.is_empty() {
        let _ = fs::remove_file(secret_path);
        return;
    }

    if fs::write(&secret_path, trimmed.as_bytes()).is_ok() {
        let _ = fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600));
    }
}

fn main() -> eframe::Result<()> {
    signaling::ensure_rustls_crypto_provider();
    let (log_buffer, _log_guard, logs_dir) = logging::init_logging();

    tracing::info!(
        "AirMic.Receiver v{} starting on macOS ({}) — logs: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        logs_dir.display()
    );

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("airmic-tokio")
        .build()
        .expect("Failed to initialize Tokio async runtime");
    let tokio_handle = tokio_runtime.handle().clone();

    let persisted = load_persisted_config();
    let initial_secret = load_stream_secret();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("AirMic Receiver")
        .with_app_id("com.airmic.receiver")
        .with_inner_size([780.0, 620.0])
        .with_min_inner_size([660.0, 480.0]);

    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../resources/AppIcon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let native_options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "AirMic Receiver",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(AirMicApp::new(
                cc,
                tokio_handle,
                log_buffer,
                logs_dir,
                persisted,
                initial_secret,
            )))
        }),
    )
}
