use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};
use url::Url;

static CRYPTO_INIT: Once = Once::new();

pub fn ensure_rustls_crypto_provider() {
    CRYPTO_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Exact Rust mirror of `AirMic.Contracts.SignalingMessage` (`SignalingMessages.cs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SignalingMessage {
    #[serde(rename = "type", alias = "Type", skip_serializing_if = "Option::is_none")]
    pub msg_type: Option<String>,

    #[serde(alias = "Sdp", skip_serializing_if = "Option::is_none")]
    pub sdp: Option<String>,

    #[serde(alias = "Candidate", skip_serializing_if = "Option::is_none")]
    pub candidate: Option<IceCandidatePayload>,

    #[serde(alias = "IsTest", skip_serializing_if = "Option::is_none")]
    pub is_test: Option<bool>,

    #[serde(alias = "OptimizeForVoice", skip_serializing_if = "Option::is_none")]
    pub optimize_for_voice: Option<bool>,
}

/// Exact Rust mirror of `AirMic.Contracts.IceCandidatePayload` (`SignalingMessages.cs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidatePayload {
    #[serde(alias = "Candidate", skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,

    #[serde(alias = "SdpMid", skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,

    #[serde(alias = "SdpMLineIndex", skip_serializing_if = "Option::is_none")]
    pub sdp_m_line_index: Option<u16>,
}

/// Ensures outgoing Trickle ICE candidates carry the `candidate:` prefix expected by browsers.
pub fn ensure_candidate_prefix(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with("candidate:") {
        trimmed.to_string()
    } else {
        format!("candidate:{}", trimmed)
    }
}

/// Builds `<signaling_url>?role=receiver&secret=<url_encoded_secret>` matching `WebRtcReceiver.cs`.
pub fn build_signaling_ws_url(raw_url: &str, stream_secret: &str) -> Result<Url> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        bail!("Signaling URL cannot be empty.");
    }
    if stream_secret.trim().is_empty() {
        bail!("Stream Key cannot be empty.");
    }

    let normalized = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{}", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{}", rest)
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        format!("wss://{}", trimmed)
    };

    let mut url = Url::parse(&normalized)
        .with_context(|| format!("Invalid signaling endpoint URL: '{}'", trimmed))?;

    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/ws");
    }

    // Preserve any existing non-(role/secret) query parameters, then append role=receiver & secret=...
    let preserved_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "role" && k != "secret")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    url.set_query(None);
    {
        let mut qp = url.query_pairs_mut();
        for (k, v) in preserved_pairs {
            qp.append_pair(&k, &v);
        }
        qp.append_pair("role", "receiver");
        qp.append_pair("secret", stream_secret);
    }

    Ok(url)
}

/// Formats the URL without query parameters so logs never leak the stream secret.
pub fn redacted_endpoint_display(url: &Url) -> String {
    match url.port() {
        Some(port) => format!(
            "{}://{}:{}{}",
            url.scheme(),
            url.host_str().unwrap_or("unknown"),
            port,
            url.path()
        ),
        None => format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or("unknown"),
            url.path()
        ),
    }
}

pub fn is_local_dev_host(url: &Url) -> bool {
    url.scheme().eq_ignore_ascii_case("wss")
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
}

/// Accepts self-signed ASP.NET Core / dev TLS certificates ONLY when connecting to localhost/127.0.0.1.
#[derive(Debug)]
struct LocalDevCertVerifier;

impl ServerCertVerifier for LocalDevCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let host_ok = match server_name {
            ServerName::DnsName(dns) => dns.as_ref().eq_ignore_ascii_case("localhost"),
            ServerName::IpAddress(ip) => {
                std::net::IpAddr::from(*ip)
                    == std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
            }
            _ => false,
        };

        if host_ok {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(
                "LocalDevCertVerifier refused non-localhost server".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub struct SignalingClient {
    tx: mpsc::UnboundedSender<SignalingMessage>,
    rx: mpsc::UnboundedReceiver<SignalingMessage>,
    closed_rx: watch::Receiver<Option<String>>,
}

impl SignalingClient {
    /// Connects to `<signaling_url>?role=receiver&secret=<secret>` with up to 10 initial retries
    /// (2s backoff), matching `WebRtcReceiver.StartAsync` in `WebRtcReceiver.cs`.
    pub async fn connect_with_retries(
        raw_url: &str,
        stream_secret: &str,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<Self> {
        ensure_rustls_crypto_provider();

        let ws_url = build_signaling_ws_url(raw_url, stream_secret)?;
        let display_endpoint = redacted_endpoint_display(&ws_url);
        tracing::info!("[*] Connecting to signaling server: {}", display_endpoint);

        let connector = if is_local_dev_host(&ws_url) {
            let tls_cfg = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(LocalDevCertVerifier))
                .with_no_client_auth();
            Some(Connector::Rustls(Arc::new(tls_cfg)))
        } else {
            None
        };

        let max_retries: u32 = 10;
        let mut retry_count: u32 = 0;

        let ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>> = loop {
            if *cancel_rx.borrow() {
                bail!("Connection cancelled.");
            }

            let connect_fut =
                connect_async_tls_with_config(ws_url.as_str(), None, false, connector.clone());

            tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        bail!("Connection cancelled.");
                    }
                    continue;
                }
                res = connect_fut => {
                    match res {
                        Ok((stream, _response)) => break stream,
                        Err(err) => {
                            if retry_count >= max_retries {
                                return Err(anyhow!(
                                    "Failed to connect to {} after {} retries: {}",
                                    display_endpoint,
                                    max_retries,
                                    err
                                ));
                            }
                            retry_count += 1;
                            tracing::warn!(
                                "[!] Connection attempt {}/{} failed: {}. Retrying in 2 seconds...",
                                retry_count,
                                max_retries,
                                err
                            );
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                                _ = cancel_rx.changed() => {
                                    if *cancel_rx.borrow() {
                                        bail!("Connection cancelled.");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };

        tracing::info!("[+] Connected to signaling server.");

        let (mut ws_write, mut ws_read) = ws_stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalingMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<SignalingMessage>();
        let (closed_tx, closed_rx) = watch::channel::<Option<String>>(None);

        let mut writer_cancel = cancel_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_cancel.changed() => {
                        if *writer_cancel.borrow() {
                            let _ = ws_write.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    maybe_msg = out_rx.recv() => {
                        let Some(msg) = maybe_msg else {
                            let _ = ws_write.send(Message::Close(None)).await;
                            break;
                        };
                        match serde_json::to_string(&msg) {
                            Ok(json) => {
                                if let Err(err) = ws_write.send(Message::Text(json.into())).await {
                                    tracing::error!("[!] Failed to send signaling message: {}", err);
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::error!("[!] Failed to serialize signaling message: {}", err);
                            }
                        }
                    }
                }
            }
        });

        let closed_tx_reader = closed_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                    maybe_frame = ws_read.next() => {
                        match maybe_frame {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<SignalingMessage>(&text) {
                                    Ok(parsed) => {
                                        let _ = in_tx.send(parsed);
                                    }
                                    Err(err) => {
                                        tracing::error!("[!] Error parsing signaling message: {}", err);
                                    }
                                }
                            }
                            Some(Ok(Message::Binary(bin))) => {
                                if let Ok(text) = std::str::from_utf8(&bin) {
                                    if let Ok(parsed) = serde_json::from_str::<SignalingMessage>(text) {
                                        let _ = in_tx.send(parsed);
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::warn!("[-] Signaling WebSocket closed by remote server.");
                                let _ = closed_tx_reader.send(Some("Signaling WebSocket closed.".to_string()));
                                break;
                            }
                            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                            Some(Err(err)) => {
                                tracing::error!("[!] Exception in signaling loop: {}", err);
                                let _ = closed_tx_reader.send(Some(format!("Signaling loop error: {}", err)));
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            tx: out_tx,
            rx: in_rx,
            closed_rx,
        })
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<SignalingMessage> {
        self.tx.clone()
    }

    pub async fn next_event(&mut self) -> SignalingEvent {
        tokio::select! {
            maybe_msg = self.rx.recv() => {
                match maybe_msg {
                    Some(msg) => SignalingEvent::Message(msg),
                    None => {
                        let reason = self
                            .closed_rx
                            .borrow()
                            .clone()
                            .unwrap_or_else(|| "Signaling channel closed.".to_string());
                        SignalingEvent::Disconnected(reason)
                    }
                }
            }
            _ = self.closed_rx.changed() => {
                if let Some(reason) = self.closed_rx.borrow().clone() {
                    SignalingEvent::Disconnected(reason)
                } else {
                    SignalingEvent::Disconnected("Signaling channel closed.".to_string())
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum SignalingEvent {
    Message(SignalingMessage),
    Disconnected(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signaling_message_camel_case_parity() {
        let client_offer_json = serde_json::json!({
            "type": "offer",
            "sdp": "v=0",
            "isTest": true,
            "optimizeForVoice": false
        })
        .to_string();

        let parsed: SignalingMessage = serde_json::from_str(&client_offer_json).unwrap();
        assert_eq!(parsed.msg_type.as_deref(), Some("offer"));
        assert_eq!(parsed.is_test, Some(true));
        assert_eq!(parsed.optimize_for_voice, Some(false));

        let cand_msg = SignalingMessage {
            msg_type: Some("candidate".to_string()),
            sdp: None,
            candidate: Some(IceCandidatePayload {
                candidate: Some(ensure_candidate_prefix("842163049 1 udp 16777215 1.2.3.4 5000 typ srflx")),
                sdp_mid: Some("0".to_string()),
                sdp_m_line_index: Some(0),
            }),
            is_test: None,
            optimize_for_voice: None,
        };

        let serialized = serde_json::to_string(&cand_msg).unwrap();
        assert!(serialized.contains(r#""type":"candidate""#));
        assert!(serialized.contains(r#""candidate":"candidate:842163049"#));
        assert!(serialized.contains(r#""sdpMid":"0""#));
        assert!(serialized.contains(r#""sdpMLineIndex":0"#));
        assert!(!serialized.contains("isTest"));
    }

    #[test]
    fn ws_url_encodes_secret_and_detects_localhost() {
        let url = build_signaling_ws_url("wss://localhost:7133/ws", "MyKey+With Space&Symbols=1")
            .unwrap();
        assert!(is_local_dev_host(&url));
        assert_eq!(
            redacted_endpoint_display(&url),
            "wss://localhost:7133/ws"
        );
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("role".to_string(), "receiver".to_string()),
                ("secret".to_string(), "MyKey+With Space&Symbols=1".to_string())
            ]
        );

        let prod = build_signaling_ws_url("https://airmic.bernardgabon.com", "sec").unwrap();
        assert!(!is_local_dev_host(&prod));
        assert_eq!(prod.scheme(), "wss");
        assert_eq!(prod.path(), "/ws");
    }

    #[tokio::test]
    async fn live_dotnet_server_handshake_parity() {
        let Ok(server_url) = std::env::var("AIRMIC_TEST_SERVER_URL") else {
            return;
        };
        let secret = std::env::var("AIRMIC_TEST_SECRET")
            .unwrap_or_else(|_| "SuperSecretKey123".to_string());
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let client = SignalingClient::connect_with_retries(&server_url, &secret, cancel_rx)
            .await
            .expect("Failed connecting Rust SignalingClient to live .NET AirMic.Server");

        drop(client);
        let _ = cancel_tx.send(true);
    }
}
