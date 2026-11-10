//! Structured harness bridges (Hydra's Pi extension) reached through the verified SSH session.
//!
//! A bridge registers itself under `~/.hydra/bridges/<id>/` on the host: `target.json` (what it is
//! bound to), `endpoint.json` (its Unix socket) and `bridge-token`. The engine reads that registry
//! with one fixed command, opens the socket with `direct-streamlocal`, and authenticates. The token
//! never leaves this crate; JS only sees newline-JSON frames of an authenticated connection.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::{EngineError, Events, ssh::SshTransport};

/// The only command run for bridges. It takes no input from JS. It has no single quotes or
/// backslashes, so it survives a non-POSIX login shell that only passes the quoted script to `sh`.
pub const REGISTRY_COMMAND: &str = concat!(
    "sh -c '",
    "cd \"$HOME/.hydra/bridges\" 2>/dev/null || exit 0; ",
    "for d in */; do ",
    "[ -S \"$d/bridge.sock\" ] && [ -f \"$d/target.json\" ] && [ -f \"$d/endpoint.json\" ] && [ -f \"$d/bridge-token\" ] || continue; ",
    "echo hydra-bridge; cat \"$d/target.json\"; echo; cat \"$d/endpoint.json\"; echo; cat \"$d/bridge-token\"; echo; ",
    "done'"
);
const MARKER: &str = "hydra-bridge";

/// Registry output cap; each entry is three small single-line files.
pub const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
/// The bridge answers with at most one 12 MiB frame (a snapshot or an artifact).
pub const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024 + 4096;
/// The bridge drops a client whose unterminated input exceeds 256 KiB.
pub const MAX_SEND_BYTES: usize = 192 * 1024;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// `sun_path` is 104 bytes on macOS and 108 on Linux.
const MAX_SOCKET_PATH: usize = 103;

pub struct Registration {
    pub pane_id: String,
    pub conversation_id: String,
    pub worker_epoch: String,
    pub instance_id: String,
    pub actor_id: String,
    pub target: Value,
    socket_path: String,
    token: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl Registration {
    /// What JS may see: identity and binding, never the token or the socket path.
    pub fn public(&self) -> Value {
        json!({
            "paneId": self.pane_id,
            "conversationId": self.conversation_id,
            "workerEpoch": self.worker_epoch,
            "instanceId": self.instance_id,
            "actorId": self.actor_id,
            "target": self.target,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetFile {
    actor_id: String,
    target: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndpointFile {
    protocol: String,
    major: u64,
    socket_path: String,
    worker_epoch: String,
    instance_id: String,
}

fn plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

/// Parses the registry listing. Entries that are malformed, bound to another classic session, or
/// internally inconsistent are skipped: the registry is account-writable configuration, and a
/// bridge is only trusted after it authenticates and its hello matches (checked by the caller).
pub fn parse_registry(output: &[u8], session: &str) -> Vec<Registration> {
    let Ok(text) = std::str::from_utf8(output) else { return Vec::new() };
    let mut found: Vec<Registration> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line != MARKER {
            continue;
        }
        let mut entry = Vec::new();
        while let Some(next) = lines.peek() {
            if *next == MARKER {
                break;
            }
            let next = lines.next().unwrap_or_default();
            if !next.is_empty() {
                entry.push(next);
            }
        }
        let [target, endpoint, token] = entry.as_slice() else { continue };
        let Ok(target) = serde_json::from_str::<TargetFile>(target) else { continue };
        let Ok(endpoint) = serde_json::from_str::<EndpointFile>(endpoint) else { continue };
        let field = |name: &str| target.target.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (pane_id, conversation_id, worker_epoch) = (field("paneId"), field("conversationId"), field("workerEpoch"));
        let valid = endpoint.protocol == "hydra.bridge"
            && endpoint.major == 1
            && field("remoteSessionId") == session
            && endpoint.worker_epoch == worker_epoch
            && endpoint.socket_path.starts_with('/')
            && plain(&endpoint.socket_path, MAX_SOCKET_PATH)
            && plain(&pane_id, 128)
            && plain(&conversation_id, 256)
            && plain(&worker_epoch, 256)
            && plain(&endpoint.instance_id, 128)
            && plain(&target.actor_id, 256)
            && plain(token, 256);
        // Two live registrations for one pane are ambiguous; neither is offered.
        if let Some(index) = found.iter().position(|other| other.pane_id == pane_id) {
            found.remove(index);
            continue;
        }
        if valid {
            found.push(Registration {
                pane_id,
                conversation_id,
                worker_epoch,
                instance_id: endpoint.instance_id,
                actor_id: target.actor_id,
                target: target.target,
                socket_path: endpoint.socket_path,
                token: (*token).to_owned(),
            });
        }
    }
    found
}

pub async fn registry(transport: &SshTransport, session: &str) -> Result<Vec<Registration>, EngineError> {
    let output = transport.exec_fixed(REGISTRY_COMMAND, MAX_REGISTRY_BYTES).await?;
    Ok(parse_registry(&output, session))
}

pub enum BridgeCommand {
    Send(String),
    Close,
}

pub struct Bridge {
    pub session_id: String,
    pub sender: mpsc::Sender<BridgeCommand>,
    pub stop: CancellationToken,
}

fn unavailable(message: &str) -> EngineError {
    EngineError::new("bridge_unavailable", message, "the pane may have no structured bridge; use the pane text view")
}

/// Takes one newline-terminated frame out of `buffer`, if a whole one is there.
fn take_line(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let newline = buffer.iter().position(|byte| *byte == b'\n')?;
    let mut line: Vec<u8> = buffer.drain(..=newline).collect();
    line.pop();
    Some(line)
}

/// Opens and authenticates the bridge of `registration`, then forwards frames until it ends.
/// Frames are lossless (`Events::data`); exactly one `closed` event follows the last frame.
pub async fn open(
    transport: &SshTransport,
    registration: Registration,
    bridge_id: String,
    events: Events,
    stop: CancellationToken,
) -> Result<mpsc::Sender<BridgeCommand>, EngineError> {
    let stream = transport.open_unix_stream(&registration.socket_path).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buffer = Vec::new();

    let handshake = async {
        let mut auth = serde_json::to_vec(&json!({"type":"hydra.auth","token":registration.token})).ok()?;
        auth.push(b'\n');
        let written = writer.write_all(&auth).await;
        auth.zeroize();
        written.ok()?;
        writer.flush().await.ok()?;
        loop {
            if let Some(line) = take_line(&mut buffer) {
                return Some(line);
            }
            if buffer.len() > 4096 || reader.read_buf(&mut buffer).await.ok()? == 0 {
                return None;
            }
        }
    };
    let reply = tokio::time::timeout(AUTH_TIMEOUT, handshake).await.ok().flatten();
    let authenticated = reply.and_then(|line| serde_json::from_slice::<Value>(&line).ok()).is_some_and(|frame| {
        frame["type"] == "hydra.authenticated" && frame["protocol"] == "hydra.bridge" && frame["major"] == 1
    });
    if !authenticated {
        return Err(unavailable("The bridge did not accept this connection"));
    }
    drop(registration);

    let (tx, mut rx) = mpsc::channel::<BridgeCommand>(16);
    tokio::spawn(async move {
        let reason = 'run: loop {
            while let Some(line) = take_line(&mut buffer) {
                if line.is_empty() {
                    continue;
                }
                let Ok(frame) = String::from_utf8(line) else { break 'run "protocol" };
                let event = json!({"kind":"bridge","bridgeId":bridge_id,"type":"frame","frame":frame});
                let delivered = tokio::select! {
                    delivered = events.data(event) => delivered,
                    () = stop.cancelled() => false,
                };
                if !delivered {
                    break 'run "closed";
                }
            }
            if buffer.len() > MAX_FRAME_BYTES {
                break "frame_too_large";
            }
            tokio::select! {
                () = stop.cancelled() => break "closed",
                read = reader.read_buf(&mut buffer) => match read {
                    Ok(0) | Err(_) => break "remote",
                    Ok(_) => {}
                },
                command = rx.recv() => match command {
                    Some(BridgeCommand::Send(mut line)) => {
                        line.push('\n');
                        if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                            break "write_failed";
                        }
                    }
                    Some(BridgeCommand::Close) | None => break "closed",
                },
            }
        };
        writer.shutdown().await.ok();
        // Lossless like frames: JS must learn that in-flight requests will never be answered.
        events.data(json!({"kind":"bridge","bridgeId":bridge_id,"type":"closed","reason":reason})).await;
    });
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pane: &str, session: &str, socket: &str, epoch: (&str, &str)) -> String {
        let target = json!({"piSessionId":"s","actorId":"host","target":{"paneId":pane,"conversationId":"c","workerEpoch":epoch.0,"remoteSessionId":session}});
        let endpoint = json!({"protocol":"hydra.bridge","major":1,"minor":1,"socketPath":socket,"workerEpoch":epoch.1,"instanceId":"i"});
        format!("{MARKER}\n{target}\n{endpoint}\nsecret-token\n")
    }

    #[test]
    fn registry_lists_only_coherent_bridges_of_this_session() {
        let listing = [
            entry("w1:p1", "work", "/home/u/.hydra/bridges/a/bridge.sock", ("e", "e")),
            entry("w1:p2", "other", "/s", ("e", "e")),
            entry("w1:p3", "work", "relative.sock", ("e", "e")),
            entry("w1:p4", "work", "/s", ("e", "stale")),
            format!("{MARKER}\nnot json\n{{}}\ntoken\n"),
            format!("{MARKER}\n{{}}\n"),
        ]
        .concat();
        let found = parse_registry(listing.as_bytes(), "work");
        assert_eq!(found.iter().map(|r| r.pane_id.as_str()).collect::<Vec<_>>(), ["w1:p1"]);
        let public = found[0].public().to_string();
        assert!(!public.contains("secret-token") && !public.contains("bridge.sock"), "{public}");
        assert_eq!(found[0].public()["target"]["paneId"], "w1:p1");
    }

    #[test]
    fn two_registrations_for_one_pane_are_not_offered() {
        let listing = [entry("w1:p1", "work", "/a", ("e", "e")), entry("w1:p1", "work", "/b", ("e", "e"))].concat();
        assert!(parse_registry(listing.as_bytes(), "work").is_empty());
        assert!(parse_registry(b"", "work").is_empty());
        assert!(parse_registry(&[0xff, 0xfe], "work").is_empty());
    }

    #[test]
    fn the_registry_command_is_fixed_and_shell_neutral() {
        let script = REGISTRY_COMMAND.strip_prefix("sh -c '").and_then(|rest| rest.strip_suffix('\'')).unwrap();
        assert!(!script.contains('\'') && !script.contains('\\'));
    }

    #[test]
    fn frames_are_split_on_newlines_only() {
        let mut buffer = b"{\"a\":1}\n\n{\"b\"".to_vec();
        assert_eq!(take_line(&mut buffer).unwrap(), b"{\"a\":1}");
        assert_eq!(take_line(&mut buffer).unwrap(), b"");
        assert!(take_line(&mut buffer).is_none());
        assert_eq!(buffer, b"{\"b\"");
    }
}
