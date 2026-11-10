use std::sync::{Arc, Mutex};
use std::time::Duration;

use herdr_classic_control::{ControlError, ControlTransport, MAX_BYTES, Request, decode, lost};
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::{ConnectionConfig, EngineError, Events};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct SshTransport {
    handle: Arc<Handle<Client>>,
    events: Events,
    session_id: String,
}

pub struct Client {
    expected: Option<String>,
    presented: Arc<Mutex<Option<HostKey>>>,
}

#[derive(Clone, Debug)]
pub struct HostKey {
    pub algorithm: String,
    pub fingerprint: String,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKey) -> Result<bool, Self::Error> {
        let presented = HostKey {
            algorithm: key.algorithm().to_string(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        };
        if let Ok(mut slot) = self.presented.lock() {
            *slot = Some(presented.clone());
        }
        Ok(self.expected.as_deref() == Some(presented.fingerprint.as_str()))
    }
}

pub async fn connect(
    config: &ConnectionConfig,
    events: Events,
    session_id: String,
) -> Result<(SshTransport, HostKey), EngineError> {
    validate_config(config)?;
    let presented = Arc::new(Mutex::new(None));
    let client = Client { expected: config.expected_fingerprint.clone(), presented: presented.clone() };
    let ssh_config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let connection =
        tokio::time::timeout(CONNECT_TIMEOUT, client::connect(ssh_config, (config.host.as_str(), config.port), client))
            .await;
    let mut handle = match connection {
        Ok(Ok(handle)) => handle,
        Ok(Err(_)) | Err(_) => {
            let key = presented.lock().ok().and_then(|mut slot| slot.take());
            return Err(connect_failure(key, config.expected_fingerprint.as_deref(), connection.is_err()));
        }
    };

    // Parsed only after the host key was verified; ssh-key zeroizes the decoded key material on drop.
    let key = PrivateKey::from_openssh(&config.private_key).map_err(|_| {
        EngineError::new(
            "invalid_private_key",
            "The private key could not be parsed",
            "use an unencrypted OpenSSH private key; encrypted keys are not supported yet",
        )
    })?;
    let authenticated = tokio::time::timeout(AUTH_TIMEOUT, async {
        let hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
        handle.authenticate_publickey(&config.username, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await
    })
    .await
    .map_err(|_| {
        EngineError::new("ssh_timeout", "SSH authentication timed out", "check the network, then connect again")
    })?
    .map_err(|_| {
        EngineError::new(
            "ssh_auth_failed",
            "SSH authentication ended before completion",
            "check the user and authorized key",
        )
    })?;
    if !authenticated.success() {
        return Err(EngineError::new(
            "ssh_auth_failed",
            "The host rejected this key",
            "check the user and authorized key",
        ));
    }
    let host_key = presented.lock().ok().and_then(|slot| slot.clone()).ok_or_else(|| {
        EngineError::new(
            "ssh_protocol",
            "SSH completed without a verified host key",
            "reconnect and verify the host independently",
        )
    })?;
    Ok((SshTransport { handle: Arc::new(handle), events, session_id }, host_key))
}

/// Classifies a failed SSH handshake. Host-key errors are reported only when the presented key
/// was actually rejected (unknown or different); a failure after a matching key is not an attack signal.
fn connect_failure(presented: Option<HostKey>, expected: Option<&str>, timed_out: bool) -> EngineError {
    match (presented, expected) {
        (Some(key), None) => EngineError::host_key(key, false, None),
        (Some(key), Some(expected)) if key.fingerprint != expected => {
            EngineError::host_key(key, true, Some(expected.to_owned()))
        }
        (_, _) if timed_out => EngineError::new(
            "ssh_timeout",
            "The SSH handshake did not finish within 10 s",
            "check the address, port and network, then connect again",
        ),
        (Some(_), Some(_)) => EngineError::new(
            "ssh_handshake_failed",
            "The SSH connection failed after the trusted host key was verified",
            "the host key matched; check the host's SSH service, then connect again",
        ),
        (None, _) => EngineError::new(
            "ssh_unreachable",
            "Could not establish an SSH connection",
            "check the address, port and network",
        ),
    }
}

fn validate_config(config: &ConnectionConfig) -> Result<(), EngineError> {
    if config.host.is_empty()
        || config.host.chars().any(|c| c.is_whitespace() || c.is_control())
        || config.username.is_empty()
        || config.username.len() > 128
        || config.username.chars().any(char::is_control)
    {
        return Err(EngineError::new(
            "invalid_params",
            "Invalid SSH host or user",
            "enter a single host address and SSH user",
        ));
    }
    herdr_classic_control::check_session(&config.session).map_err(EngineError::from)
}

/// Why an exchange failed: only `Transport` means the SSH connection or channel is unusable.
enum Failure {
    Transport,
    Control(ControlError),
}

impl ControlTransport for SshTransport {
    async fn exchange(&self, session: &str, request: Request) -> Result<Value, ControlError> {
        let mutation = request.mutation;
        let bytes = request.encode()?;
        let result = tokio::time::timeout(EXCHANGE_TIMEOUT, self.exchange_inner(session, bytes)).await;
        match result.unwrap_or(Err(Failure::Transport)) {
            Ok(value) => Ok(value),
            Err(Failure::Transport) => {
                self.transport_lost(mutation);
                Err(lost(mutation))
            }
            // Remote errors are definite; any other failure after a mutation was sent is an unknown outcome.
            Err(Failure::Control(error)) if mutation && error.kind != "classic_remote" => Err(lost(true)),
            Err(Failure::Control(error)) => Err(error),
        }
    }
}

impl SshTransport {
    fn transport_lost(&self, mutation: bool) {
        self.events.control(json!({"kind":"transportLost","sessionId":self.session_id,"mutation":mutation}));
    }

    async fn exchange_inner(&self, session: &str, request: Vec<u8>) -> Result<Value, Failure> {
        let mut channel = self.handle.channel_open_session().await.map_err(|_| Failure::Transport)?;
        channel
            .exec(true, format!("herdr --session {session} remote-api-bridge"))
            .await
            .map_err(|_| Failure::Transport)?;
        channel.data_bytes(request).await.map_err(|_| Failure::Transport)?;
        let mut response = Vec::new();
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    if response.len().saturating_add(data.len()) > MAX_BYTES {
                        channel.close().await.ok();
                        return Err(Failure::Control(ControlError::new(
                            "classic_protocol",
                            "classic response exceeds 1 MiB",
                            "supported endpoint: classic Herdr speaking JSON API protocol 22",
                        )));
                    }
                    response.extend_from_slice(&data);
                    if let Some(newline) = response.iter().position(|byte| *byte == b'\n') {
                        channel.close().await.ok();
                        if newline + 1 != response.len() {
                            return Err(Failure::Control(ControlError::new(
                                "classic_protocol",
                                "classic returned more than one response",
                                "one request must have exactly one bounded response",
                            )));
                        }
                        return decode(&response).map_err(Failure::Control);
                    }
                }
                ChannelMsg::Close | ChannelMsg::Eof => break,
                _ => {}
            }
        }
        Err(Failure::Transport)
    }

    /// Opens the allowlisted classic stable-surface bridge as an ordered, lossless byte channel.
    /// Exactly one `channelClosed {channelId, reason}` event is emitted when it ends.
    pub async fn open_stable_channel(
        &self,
        session: &str,
        channel_id: String,
        stop: CancellationToken,
    ) -> Result<mpsc::Sender<ChannelCommand>, EngineError> {
        let opened = async {
            let channel = self.handle.channel_open_session().await.ok()?;
            channel.exec(true, format!("herdr --session {session} remote-client-bridge")).await.ok()?;
            Some(channel)
        };
        let Some(mut channel) = tokio::time::timeout(EXCHANGE_TIMEOUT, opened).await.ok().flatten() else {
            self.transport_lost(false);
            return Err(EngineError::transport("Could not open the classic stable bridge channel"));
        };
        let (tx, mut rx) = mpsc::channel::<ChannelCommand>(16);
        let events = self.events.clone();
        tokio::spawn(async move {
            use base64::Engine as _;
            let reason = loop {
                tokio::select! {
                    () = stop.cancelled() => {
                        channel.close().await.ok();
                        break "shutdown";
                    }
                    message = channel.wait() => match message {
                        Some(ChannelMsg::Data { data }) => {
                            let data = base64::engine::general_purpose::STANDARD.encode(data);
                            let event = json!({"kind":"channelData","channelId":channel_id,"data":data});
                            let delivered = tokio::select! {
                                delivered = events.data(event) => delivered,
                                () = stop.cancelled() => false,
                            };
                            if !delivered {
                                break "shutdown";
                            }
                        }
                        Some(ChannelMsg::Close) | Some(ChannelMsg::Eof) | None => break "remote",
                        _ => {}
                    },
                    command = rx.recv() => match command {
                        Some(ChannelCommand::Write(bytes)) => {
                            if channel.data_bytes(bytes).await.is_err() {
                                break "write_failed";
                            }
                        }
                        Some(ChannelCommand::Close) | None => {
                            channel.close().await.ok();
                            break "closed";
                        }
                    }
                }
            };
            events.control(json!({"kind":"channelClosed","channelId":channel_id,"reason":reason}));
        });
        Ok(tx)
    }

    pub async fn disconnect(&self) {
        self.handle.disconnect(Disconnect::ByApplication, "", "English").await.ok();
    }
}

pub enum ChannelCommand {
    Write(Vec<u8>),
    Close,
}

impl Drop for ConnectionConfig {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(fingerprint: &str) -> Option<HostKey> {
        Some(HostKey { algorithm: "ssh-ed25519".into(), fingerprint: fingerprint.into() })
    }

    #[test]
    fn failure_after_a_matching_key_is_not_a_changed_key() {
        assert_eq!(connect_failure(key("SHA256:a"), Some("SHA256:a"), false).kind, "ssh_handshake_failed");
        assert_eq!(connect_failure(key("SHA256:a"), Some("SHA256:a"), true).kind, "ssh_timeout");
        assert!(connect_failure(key("SHA256:a"), Some("SHA256:a"), false).host_key.is_none());
    }

    #[test]
    fn only_rejected_keys_are_host_key_errors() {
        let changed = connect_failure(key("SHA256:b"), Some("SHA256:a"), false);
        assert_eq!(changed.kind, "host_key_changed");
        let challenge = changed.host_key.unwrap();
        assert_eq!(challenge.presented_key.fingerprint, "SHA256:b");
        assert_eq!(challenge.expected_fingerprint.as_deref(), Some("SHA256:a"));
        assert_eq!(connect_failure(key("SHA256:b"), Some("SHA256:a"), true).kind, "host_key_changed");
        assert_eq!(connect_failure(key("SHA256:b"), None, false).kind, "host_key_unverified");
        assert_eq!(connect_failure(None, Some("SHA256:a"), false).kind, "ssh_unreachable");
        assert_eq!(connect_failure(None, None, true).kind, "ssh_timeout");
    }
}
