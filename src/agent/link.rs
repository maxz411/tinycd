//! How an agent reaches its hub: a channel pair inside one process, or a
//! WebSocket to another. Remote links pin the hub's certificate by the
//! fingerprint carried in the join token, so a self-signed hub is as safe
//! as one behind a real certificate.

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

use super::Agent;
use crate::{
    protocol::{AGENT_PATH, Envelope, ToAgent, ToHub},
    util::{self, Error, say, say_err},
};

/// Drive an agent from a channel inside the hub's own process.
pub async fn run_local(agent: Arc<Agent>, mut orders: mpsc::UnboundedReceiver<ToAgent>) {
    while let Some(message) = orders.recv().await {
        agent.handle(message).await;
    }
}

/// What a remote agent remembers between runs, at <data>/agent.json.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentFile {
    pub url: String,
    pub token: String,
    pub fingerprint: Option<String>,
    pub name: String,
    #[serde(default)]
    pub joined: bool,
}

impl AgentFile {
    pub fn path(data: &std::path::Path) -> PathBuf {
        data.join("agent.json")
    }

    pub fn load(data: &std::path::Path) -> Option<Self> {
        let bytes = std::fs::read(Self::path(data)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, data: &std::path::Path) -> Result<(), Error> {
        std::fs::create_dir_all(data)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        util::write_atomic(&Self::path(data), &bytes, 0o600)?;
        Ok(())
    }
}

/// A join token: `tcj1.<certificate fingerprint>.<secret>`; the fingerprint
/// is empty when the hub speaks plain HTTP.
pub fn parse_join_token(token: &str) -> Option<(Option<String>, String)> {
    let mut parts = token.trim().splitn(3, '.');
    let (Some("tcj1"), Some(fingerprint), Some(secret)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if secret.len() < 32 {
        return None;
    }
    let fingerprint = (!fingerprint.is_empty()).then(|| fingerprint.to_owned());
    Some((fingerprint, secret.to_owned()))
}

/// Keep an agent connected to a remote hub, reconnecting with backoff.
pub async fn run_remote(
    agent: Arc<Agent>,
    data: PathBuf,
    mut file: AgentFile,
    mut outbox: mpsc::UnboundedReceiver<ToHub>,
    insecure: bool,
) -> Result<(), Error> {
    let mut delay = Duration::from_secs(1);
    loop {
        match connect(&file, insecure).await {
            Ok(mut stream) => {
                say(format!("connected to {}", file.url));
                delay = Duration::from_secs(1);
                // Whatever queued up while offline is stale; the resync
                // below sends fresh state.
                while outbox.try_recv().is_ok() {}
                let register =
                    serde_json::to_string(&Envelope::new(agent.register_message(&file.name)))?;
                if let Err(error) = stream.send(Message::Text(register.into())).await {
                    say_err(format!("failed to register: {error}"));
                    continue;
                }
                agent.resync().await;

                loop {
                    tokio::select! {
                        outgoing = outbox.recv() => {
                            let Some(message) = outgoing else { return Ok(()) };
                            let text = serde_json::to_string(&Envelope::new(message))?;
                            if let Err(error) = stream.send(Message::Text(text.into())).await {
                                say_err(format!("link to the hub failed: {error}"));
                                break;
                            }
                        }
                        incoming = stream.next() => {
                            match incoming {
                                Some(Ok(Message::Text(text))) => {
                                    match serde_json::from_str::<Envelope<ToAgent>>(&text) {
                                        Ok(envelope) => {
                                            if let ToAgent::Welcome { token, .. } = &envelope.message {
                                                let changed = !file.joined || token.as_ref().is_some_and(|token| *token != file.token);
                                                if let Some(token) = token { file.token = token.clone(); }
                                                file.joined = true;
                                                if changed {
                                                    if let Err(error) = file.save(&data) {
                                                        say_err(format!("failed to save the host token: {error}"));
                                                    } else {
                                                        say("joined the hub; credentials saved");
                                                    }
                                                }
                                            }
                                            agent.handle(envelope.message).await;
                                        }
                                        Err(error) => say_err(format!("ignoring an unreadable message from the hub: {error}")),
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    say_err("the hub closed the link");
                                    break;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(error)) => {
                                    say_err(format!("link to the hub failed: {error}"));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(error) => say_err(format!("cannot reach {}: {error}", file.url)),
        }
        say(format!("reconnecting in {}s", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

type Stream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(file: &AgentFile, insecure: bool) -> Result<Stream, Error> {
    let (secure, rest) = match file.url.trim_end_matches('/') {
        url if url.starts_with("https://") => (true, url.trim_start_matches("https://")),
        url if url.starts_with("wss://") => (true, url.trim_start_matches("wss://")),
        url if url.starts_with("http://") => (false, url.trim_start_matches("http://")),
        url if url.starts_with("ws://") => (false, url.trim_start_matches("ws://")),
        url => {
            return Err(
                format!("the hub URL must start with https:// or http://, not {url}").into(),
            );
        }
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let host_only = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let loopback = matches!(host_only, "localhost" | "127.0.0.1" | "[::1]" | "::1");
    if !secure && !loopback && !insecure {
        return Err(
            "refusing to send the host token over plain HTTP to a non-local hub; use https:// or pass --insecure"
                .into(),
        );
    }

    let scheme = if secure { "wss" } else { "ws" };
    let mut request = format!("{scheme}://{rest}{AGENT_PATH}").into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {}", file.token).parse()?);

    let connector = match (secure, &file.fingerprint) {
        (true, Some(fingerprint)) => {
            let pin = util::decode_hex32(fingerprint)
                .ok_or("the saved certificate fingerprint is not valid hex")?;
            util::init_crypto();
            let config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Pinned(pin)))
                .with_no_client_auth();
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
        }
        _ => None,
    };
    let (stream, _) = tokio::time::timeout(
        Duration::from_secs(20),
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector),
    )
    .await
    .map_err(|_| "timed out connecting to the hub")??;
    Ok(stream)
}

/// Accept exactly one certificate: the one whose SHA-256 the join token
/// carried. Names and dates are irrelevant when the certificate itself is
/// the identity.
#[derive(Debug)]
struct Pinned([u8; 32]);

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use subtle::ConstantTimeEq;

        let presented = util::sha256(end_entity.as_ref());
        if bool::from(presented.ct_eq(&self.0)) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the hub's certificate does not match the fingerprint in the join token".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_tokens_parse() {
        let secret = "a".repeat(64);
        let (fingerprint, parsed) = parse_join_token(&format!("tcj1.ff00.{secret}")).unwrap();
        assert_eq!(fingerprint.as_deref(), Some("ff00"));
        assert_eq!(parsed, secret);
        let (fingerprint, _) = parse_join_token(&format!("tcj1..{secret}")).unwrap();
        assert!(fingerprint.is_none());
        assert!(parse_join_token("tcj1.ff.short").is_none());
        assert!(parse_join_token("nope").is_none());
    }
}
