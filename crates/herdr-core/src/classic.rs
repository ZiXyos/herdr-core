//! Fresh, narrow client for classic Herdr's newline JSON control API (protocol 22).
//! Native IDs never enter the modern terminal registry or address rewriter.

use crate::machines::{Backend, Machine, Reach};
use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: u64 = 22;
const MAX_BYTES: usize = 1024 * 1024;
const REQUEST_ID: &str = "hc-classic";

pub fn unsupported(operation: &str) -> RpcError {
    RpcError::new(
        "unsupported_backend_capability",
        format!("classic does not support Core {operation}"),
        "use classic.list/read/send; no raw streams, run/wait, file operations or remote Core installation",
    )
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Inventory {
    pub panes: Vec<Pane>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadResult {
    pub read: Text,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Text {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct SendResult {
    /// Classic accepted input; not a claim that the shell executed it successfully.
    pub accepted: bool,
}

fn protocol(message: &str) -> RpcError {
    RpcError::new("classic_protocol", message, "supported endpoint: classic Herdr speaking JSON API protocol 22")
}

fn lost(mutation: bool) -> RpcError {
    RpcError::new(
        if mutation { "classic_outcome_unknown" } else { "classic_transport" },
        "classic connection ended, timed out, or returned an invalid response",
        if mutation {
            "input may have been delivered; inspect the pane before deciding whether to retry"
        } else {
            "check SSH access and the explicitly named classic session"
        },
    )
}

fn decode(bytes: &[u8]) -> Result<Value, RpcError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| protocol("invalid JSON response"))?;
    if v["id"] != REQUEST_ID || v.get("result").is_some() == v.get("error").is_some() {
        return Err(protocol("uncorrelated or malformed classic response"));
    }
    if let Some(e) = v.get("error") {
        let code = e["code"].as_str().ok_or_else(|| protocol("invalid classic error"))?;
        let message = e["message"].as_str().ok_or_else(|| protocol("invalid classic error"))?;
        return Err(RpcError::new("classic_remote", format!("{code}: {message}"), "native classic API error"));
    }
    Ok(v["result"].clone())
}

async fn read_reply(read: impl AsyncRead + Unpin) -> Result<Value, RpcError> {
    let mut bytes = Vec::new();
    BufReader::new(read.take((MAX_BYTES + 1) as u64)).read_until(b'\n', &mut bytes).await.map_err(|_| lost(false))?;
    if bytes.len() > MAX_BYTES || bytes.last() != Some(&b'\n') {
        return Err(protocol("classic response exceeds 1 MiB or lacks newline"));
    }
    decode(&bytes)
}

/// One bounded SSH exchange. No retry, install, focus, layout or modern handshake.
async fn exchange(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: &str,
    method: &str,
    params: Value,
    mutation: bool,
) -> Result<Value, RpcError> {
    crate::machines::check_target(to)?;
    crate::addr::check_name(session)?; // makes the only interpolated shell token safe
    let request = format!("{}\n", json!({"id": REQUEST_ID, "method": method, "params": params}));
    if request.len() > MAX_BYTES {
        return Err(RpcError::invalid_params("classic request exceeds 1 MiB"));
    }
    let command = format!("herdr --session {session} remote-api-bridge");
    let mut cmd = tokio::process::Command::from(to.command(paths, &["BatchMode=yes", "ConnectTimeout=10"], &command));
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| RpcError::new("classic_transport", e.to_string(), "check SSH"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        stdin.write_all(request.as_bytes()).await.map_err(|_| lost(mutation))?;
        // Do not close stdin before the reply: the native bridge owns its lifetime.
        read_reply(stdout).await
    })
    .await
    .unwrap_or_else(|_| Err(lost(mutation)));
    let _ = child.kill().await;
    let _ = child.wait().await;
    result.map_err(|e| if mutation && e.kind != "classic_remote" { lost(true) } else { e })
}

/// Checks that a classic session answers with a protocol we speak; reports its version.
pub async fn probe(paths: &crate::paths::Paths, to: Reach<'_>, session: &str) -> Result<String, RpcError> {
    let v = exchange(paths, to, session, "ping", json!({}), false).await?;
    match (v["type"] == "pong", v["protocol"].as_u64(), v["version"].as_str()) {
        (true, Some(PROTOCOL), Some(version)) => Ok(version.to_string()),
        _ => Err(protocol("unsupported classic protocol; no upgrade attempted")),
    }
}

fn session(m: &Machine) -> Result<&str, RpcError> {
    if m.backend != Backend::Classic {
        return Err(RpcError::new(
            "unsupported_backend_capability",
            format!("{} is a modern machine, not a classic session", m.name),
            "classic.* only works on machines added with --backend classic",
        ));
    }
    m.session.as_deref().ok_or_else(|| RpcError::invalid_params("classic requires a named session"))
}

fn inventory(v: Value) -> Result<Inventory, RpcError> {
    serde_json::from_value(v).map_err(|_| protocol("invalid native pane inventory"))
}

pub async fn list(paths: &crate::paths::Paths, m: &Machine) -> Result<Inventory, RpcError> {
    inventory(exchange(paths, m.reach(), session(m)?, "pane.list", json!({}), false).await?)
}

fn correlate(panes: &Inventory, pane: &str, terminal: &str) -> Result<(), RpcError> {
    if !panes.panes.iter().any(|p| p.pane_id == pane && p.terminal_id == terminal) {
        return Err(RpcError::new(
            "classic_stale_identity",
            "native pane/runtime association no longer matches",
            "discover again with classic.list; no input was sent",
        ));
    }
    Ok(())
}

pub async fn read(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicReadParams,
) -> Result<ReadResult, RpcError> {
    if !(1..=1000).contains(&p.lines) {
        return Err(RpcError::invalid_params("lines must be 1..1000"));
    }
    correlate(&list(paths, m).await?, &p.pane_id, &p.terminal_id)?;
    let v = exchange(
        paths,
        m.reach(),
        session(m)?,
        "pane.read",
        json!({"pane_id": p.pane_id, "source":"recent", "format":"text", "strip_ansi":true, "lines":p.lines}),
        false,
    )
    .await?;
    if v["type"] != "pane_read" || v["read"]["pane_id"] != p.pane_id {
        return Err(protocol("invalid native read response"));
    }
    serde_json::from_value(v).map_err(|_| protocol("invalid native text"))
}

pub async fn send(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicSendParams,
) -> Result<SendResult, RpcError> {
    if p.text.is_empty() && p.keys.is_empty() {
        return Err(RpcError::invalid_params("nothing to send: give --text, --key or both"));
    }
    if p.text.len() > 65536 || p.keys.len() > 64 || p.keys.iter().any(|k| k.len() > 64) {
        return Err(RpcError::invalid_params("classic input limit: 64 KiB text, 64 keys of at most 64 bytes"));
    }
    correlate(&list(paths, m).await?, &p.pane_id, &p.terminal_id)?;
    let v = exchange(
        paths,
        m.reach(),
        session(m)?,
        "pane.send_input",
        json!({"pane_id": p.pane_id, "text":p.text, "keys":p.keys}),
        true,
    )
    .await?;
    if v["type"] != "ok" {
        return Err(lost(true));
    }
    Ok(SendResult { accepted: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_and_native_errors_are_not_modern_rpc_errors() {
        assert!(decode(br#"{"id":"other","result":{}}"#).is_err());
        assert!(decode(br#"{"id":"hc-classic","result":{},"error":{}}"#).is_err());
        let e = decode(br#"{"id":"hc-classic","error":{"code":"pane_not_found","message":"gone"}}"#).unwrap_err();
        assert_eq!(e.kind, "classic_remote");
        assert!(e.message.contains("pane_not_found"));
        assert!(decode(br#"{"id":"hc-classic"}"#).is_err());
    }

    #[test]
    fn discovery_drops_vendor_status_and_preserves_opaque_ids() {
        let panes = inventory(json!({"panes":[{
            "pane_id":"w1:p1", "terminal_id":"term_native", "workspace_id":"w1", "tab_id":"w1:t1",
            "agent_status":"working", "cwd":"/w1:p1"
        }]}))
        .unwrap();
        let v = serde_json::to_value(&panes).unwrap();
        assert_eq!(v["panes"][0]["pane_id"], "w1:p1");
        assert!(v["panes"][0].get("agent_status").is_none());
        assert!(correlate(&panes, "w1:p1", "term_native").is_ok());
        assert_eq!(correlate(&panes, "w1:p1", "term_reused").unwrap_err().kind, "classic_stale_identity");
    }

    #[tokio::test]
    async fn response_is_bounded_and_requires_a_complete_line() {
        assert!(read_reply(&b"{\"id\":\"hc-classic\",\"result\":{}}\n"[..]).await.is_ok());
        assert!(read_reply(&b"{\"id\":\"hc-classic\",\"result\":{}}"[..]).await.is_err());
        assert!(read_reply(&vec![b'x'; MAX_BYTES + 1][..]).await.is_err());
    }

    #[test]
    fn unsupported_and_ambiguous_are_explicit() {
        assert_eq!(unsupported("terminal.attach").kind, "unsupported_backend_capability");
        assert_eq!(lost(true).kind, "classic_outcome_unknown");
        assert_eq!(lost(false).kind, "classic_transport");
    }
}
