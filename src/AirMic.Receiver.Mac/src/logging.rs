use chrono::Local;
use directories::BaseDirs;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::field::Visit;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub const MAX_UI_LOG_LINES: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiLogLevel {
    Info,
    Warn,
    Error,
}

impl UiLogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERR ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: UiLogLevel,
    pub message: String,
}

impl LogEntry {
    pub fn format_line(&self) -> String {
        format!("[{} {}] {}", self.timestamp, self.level.as_str(), self.message)
    }
}

#[derive(Clone, Default)]
pub struct LogBuffer {
    entries: Arc<Mutex<VecDeque<LogEntry>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_UI_LOG_LINES))),
        }
    }

    pub fn push(&self, entry: LogEntry) {
        let mut guard = self.entries.lock();
        if guard.len() >= MAX_UI_LOG_LINES {
            guard.pop_front();
        }
        guard.push_back(entry);
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    pub fn snapshot(&self, filter: Option<UiLogLevel>) -> Vec<LogEntry> {
        let guard = self.entries.lock();
        match filter {
            None => guard.iter().cloned().collect(),
            Some(level) => guard.iter().filter(|e| e.level == level).cloned().collect(),
        }
    }
}

struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: String::new(),
        }
    }

    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields
        } else {
            format!("{} {}", self.message, self.fields)
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{:?}", value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(&mut self.fields, "{}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(&mut self.fields, "{}={}", field.name(), value);
        }
    }
}

struct UiRingBufferLayer {
    buffer: LogBuffer,
}

impl<S: Subscriber> Layer<S> for UiRingBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let target = metadata.target();

        // Mirror AppLogger.cs behavior: keep external library internals (webrtc_ice IPv6 link-local
        // interface warnings, wgpu/TLS internals) in file logs only; show clean AirMic events in UI.
        let is_own_crate =
            target.starts_with("AirMicReceiver") || target.starts_with("airmic_receiver_mac");
        let level = *metadata.level();

        if !is_own_crate || level > Level::INFO {
            return;
        }

        let ui_level = match level {
            Level::ERROR => UiLogLevel::Error,
            Level::WARN => UiLogLevel::Warn,
            _ => UiLogLevel::Info,
        };

        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);
        let msg = visitor.finish();
        if msg.is_empty() {
            return;
        }

        self.buffer.push(LogEntry {
            timestamp: Local::now().format("%H:%M:%S").to_string(),
            level: ui_level,
            message: msg,
        });
    }
}

pub fn resolve_logs_dir() -> PathBuf {
    // Prefer ./logs when running from a writable project directory; fall back to
    // ~/Library/Application Support/AirMic.Receiver/logs when launched as a .app bundle (cwd == "/").
    if let Ok(cwd) = std::env::current_dir() {
        if cwd != Path::new("/") {
            let candidate = cwd.join("logs");
            if fs::create_dir_all(&candidate).is_ok() {
                return candidate;
            }
        }
    }

    if let Some(base) = BaseDirs::new() {
        let app_logs = base
            .home_dir()
            .join("Library")
            .join("Application Support")
            .join("AirMic.Receiver")
            .join("logs");
        let _ = fs::create_dir_all(&app_logs);
        return app_logs;
    }

    std::env::temp_dir().join("AirMic.Receiver").join("logs")
}

pub fn init_logging() -> (LogBuffer, WorkerGuard, PathBuf) {
    let logs_dir = resolve_logs_dir();
    let _ = fs::create_dir_all(&logs_dir);

    let file_appender = tracing_appender::rolling::daily(&logs_dir, "receiver.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("AirMicReceiver=debug,airmic_receiver_mac=debug,webrtc=warn,wgpu=warn,eframe=warn,info")
    });

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking)
        .with_target(true)
        .with_filter(file_filter);

    let ui_buffer = LogBuffer::new();
    let ui_layer = UiRingBufferLayer {
        buffer: ui_buffer.clone(),
    };

    tracing_subscriber::registry()
        .with(file_layer)
        .with(ui_layer)
        .init();

    (ui_buffer, guard, logs_dir)
}
