//! Transport-neutral client for classic Herdr's newline JSON control API (protocol 22).
//! Native IDs never enter modern Core's terminal registry or address rewriter.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_BYTES: usize = 1024 * 1024;
pub const PROTOCOL: u64 = 22;
pub const REQUEST_ID: &str = "hc-classic";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    pub kind: String,
    pub message: String,
    pub hint: String,
}

impl ControlError {
    pub fn new(kind: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), hint: hint.into() }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new("invalid_params", message, "check the classic control parameters")
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ControlError {}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: &'static str,
    pub params: Value,
    pub mutation: bool,
}

impl Request {
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        let mut bytes = serde_json::to_vec(&json!({"id": REQUEST_ID, "method": self.method, "params": self.params}))
            .map_err(|_| protocol("could not encode classic request"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BYTES {
            return Err(ControlError::invalid_params("classic request exceeds 1 MiB"));
        }
        Ok(bytes)
    }
}

/// Implemented by desktop subprocess SSH and the embedded mobile SSH transport.
/// Implementations must send exactly one encoded request and return one decoded JSON value.
pub trait ControlTransport: Send + Sync {
    fn exchange(
        &self,
        session: &str,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Value, ControlError>> + Send;
}

pub fn unsupported(operation: &str) -> ControlError {
    ControlError::new(
        "unsupported_backend_capability",
        format!("classic does not support Core {operation}"),
        "use classic.list/read/send; no raw streams, run/wait, file operations or remote Core installation",
    )
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub panes: Vec<Pane>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub read: Text,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    /// Classic accepted input; not a claim that the shell executed it successfully.
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendInput<'a> {
    pub pane_id: &'a str,
    pub terminal_id: &'a str,
    pub text: &'a str,
    pub keys: &'a [String],
}

fn protocol(message: &str) -> ControlError {
    ControlError::new("classic_protocol", message, "supported endpoint: classic Herdr speaking JSON API protocol 22")
}

pub fn lost(mutation: bool) -> ControlError {
    ControlError::new(
        if mutation { "classic_outcome_unknown" } else { "classic_transport" },
        "classic connection ended, timed out, or returned an invalid response",
        if mutation {
            "input may have been delivered; inspect the pane before deciding whether to retry"
        } else {
            "check SSH access and the explicitly named classic session"
        },
    )
}

pub fn check_session(session: &str) -> Result<(), ControlError> {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.');
    let starts = session.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    let id_shaped =
        session.strip_prefix('t').is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
    if !starts || session.len() > 32 || !session.chars().all(allowed) || id_shaped {
        return Err(ControlError::invalid_params(
            "bad session name: use 1-32 of a-z 0-9 - _ . starting with a letter or digit, not shaped like an id (t7)",
        ));
    }
    Ok(())
}

pub fn decode(bytes: &[u8]) -> Result<Value, ControlError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| protocol("invalid JSON response"))?;
    if v["id"] != REQUEST_ID || v.get("result").is_some() == v.get("error").is_some() {
        return Err(protocol("uncorrelated or malformed classic response"));
    }
    if let Some(e) = v.get("error") {
        let code = e["code"].as_str().ok_or_else(|| protocol("invalid classic error"))?;
        let message = e["message"].as_str().ok_or_else(|| protocol("invalid classic error"))?;
        return Err(ControlError::new("classic_remote", format!("{code}: {message}"), "native classic API error"));
    }
    Ok(v["result"].clone())
}

/// Checks that a classic session answers with a protocol we speak; reports its version.
pub async fn probe<T: ControlTransport + ?Sized>(transport: &T, session: &str) -> Result<String, ControlError> {
    check_session(session)?;
    let v = transport.exchange(session, Request { method: "ping", params: json!({}), mutation: false }).await?;
    match (v["type"] == "pong", v["protocol"].as_u64(), v["version"].as_str()) {
        (true, Some(PROTOCOL), Some(version)) => Ok(version.to_string()),
        _ => Err(protocol("unsupported classic protocol; no upgrade attempted")),
    }
}

fn inventory(v: Value) -> Result<Inventory, ControlError> {
    serde_json::from_value(v).map_err(|_| protocol("invalid native pane inventory"))
}

pub async fn list<T: ControlTransport + ?Sized>(transport: &T, session: &str) -> Result<Inventory, ControlError> {
    check_session(session)?;
    inventory(transport.exchange(session, Request { method: "pane.list", params: json!({}), mutation: false }).await?)
}

fn correlate(panes: &Inventory, pane: &str, terminal: &str) -> Result<(), ControlError> {
    if !panes.panes.iter().any(|p| p.pane_id == pane && p.terminal_id == terminal) {
        return Err(ControlError::new(
            "classic_stale_identity",
            "native pane/runtime association no longer matches",
            "discover again with classic.list; no input was sent",
        ));
    }
    Ok(())
}

pub async fn read<T: ControlTransport + ?Sized>(
    transport: &T,
    session: &str,
    pane_id: &str,
    terminal_id: &str,
    lines: u32,
) -> Result<ReadResult, ControlError> {
    if !(1..=1000).contains(&lines) {
        return Err(ControlError::invalid_params("lines must be 1..1000"));
    }
    correlate(&list(transport, session).await?, pane_id, terminal_id)?;
    let v = transport
        .exchange(
            session,
            Request {
                method: "pane.read",
                params: json!({"pane_id": pane_id, "source":"recent", "format":"text", "strip_ansi":true, "lines":lines}),
                mutation: false,
            },
        )
        .await?;
    if v["type"] != "pane_read" || v["read"]["pane_id"] != pane_id {
        return Err(protocol("invalid native read response"));
    }
    serde_json::from_value(v).map_err(|_| protocol("invalid native text"))
}

pub async fn send<T: ControlTransport + ?Sized>(
    transport: &T,
    session: &str,
    input: SendInput<'_>,
) -> Result<SendResult, ControlError> {
    if input.text.is_empty() && input.keys.is_empty() {
        return Err(ControlError::invalid_params("nothing to send: give text, key or both"));
    }
    if input.text.len() > 65536 || input.keys.len() > 64 || input.keys.iter().any(|k| k.len() > 64) {
        return Err(ControlError::invalid_params("classic input limit: 64 KiB text, 64 keys of at most 64 bytes"));
    }
    correlate(&list(transport, session).await?, input.pane_id, input.terminal_id)?;
    let v = transport
        .exchange(
            session,
            Request {
                method: "pane.send_input",
                params: json!({"pane_id": input.pane_id, "text":input.text, "keys":input.keys}),
                mutation: true,
            },
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

    #[test]
    fn requests_are_bounded_and_session_names_are_not_shell_input() {
        let request = Request { method: "ping", params: json!({}), mutation: false };
        assert!(request.encode().unwrap().ends_with(b"\n"));
        assert!(check_session("work").is_ok());
        assert!(check_session("work; touch nope").is_err());
        assert!(check_session("t7").is_err());
        let oversized =
            Request { method: "pane.send_input", params: json!({"text": "x".repeat(MAX_BYTES)}), mutation: true };
        assert!(oversized.encode().is_err());
    }

    #[test]
    fn unsupported_and_ambiguous_are_explicit() {
        assert_eq!(unsupported("terminal.attach").kind, "unsupported_backend_capability");
        assert_eq!(lost(true).kind, "classic_outcome_unknown");
        assert_eq!(lost(false).kind, "classic_transport");
    }
}
