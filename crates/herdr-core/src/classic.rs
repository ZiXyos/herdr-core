//! Fresh, narrow client for classic Herdr's newline JSON control API (protocol 22).
//! Native IDs never enter the modern terminal registry or address rewriter.

use crate::machines::{Backend, Machine, Reach};
use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: u64 = 22;
const MAX_BYTES: usize = 1024 * 1024;
const REQUEST_ID: &str = "hc-classic";

pub fn unsupported(operation: &str) -> RpcError {
    RpcError::new(
        "unsupported_backend_capability",
        format!("classic does not support Core {operation}"),
        "use classic.list/read/send/create/start/inspect; no raw streams, run/wait, file operations or remote Core installation",
    )
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Opaque native metadata, including optional client launch correlation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: BTreeMap<String, String>,
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

#[derive(Serialize, JsonSchema)]
pub struct StartResult {
    /// Classic accepted input; not a claim that the requested process executed.
    pub accepted: bool,
    /// Authoritative foreground identity observed immediately before input was sent.
    pub prestart: ProcessInfo,
}

#[derive(Clone, Copy, Serialize, JsonSchema, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Correlation {
    Recorded,
    Unavailable,
    Unknown,
}

#[derive(Serialize, JsonSchema)]
pub struct CreateResult {
    pub pane: Pane,
    /// Whether native correlation metadata was acknowledged. Pane creation is confirmed in all cases.
    pub correlation: Correlation,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv0: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct ProcessInfo {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_process_group_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    #[serde(default)]
    pub foreground_processes: Vec<Process>,
}

#[derive(Serialize, JsonSchema)]
pub struct InspectResult {
    pub process_info: ProcessInfo,
}

const LAUNCH_ID_TOKEN: &str = "hydra_launch_id";
const CONFIGURATION_HASH_TOKEN: &str = "hydra_configuration_hash";
const LAUNCH_ID_ENV: &str = "HYDRA_LAUNCH_ID";
const CONFIGURATION_HASH_ENV: &str = "HYDRA_CONFIGURATION_HASH";

fn protocol(message: &str) -> RpcError {
    RpcError::new("classic_protocol", message, "supported endpoint: classic Herdr speaking JSON API protocol 22")
}

fn lost(mutation: bool) -> RpcError {
    RpcError::new(
        if mutation { "classic_outcome_unknown" } else { "classic_transport" },
        "classic connection ended, timed out, or returned an invalid response",
        if mutation {
            "the native mutation may have happened; reconcile authoritative state before deciding whether to retry"
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

fn opaque(id: &str, what: &str) -> Result<(), RpcError> {
    if id.is_empty() || id.len() > 256 || id.contains('\0') {
        Err(RpcError::invalid_params(format!("{what} must be 1..256 bytes without NUL")))
    } else {
        Ok(())
    }
}

fn correlation(launch_id: &str, configuration_hash: &str) -> Result<(), RpcError> {
    if launch_id.is_empty()
        || launch_id.len() > 64
        || !launch_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(RpcError::invalid_params("launch_id must be 1..64 ASCII letters, digits, _ or -"));
    }
    if configuration_hash.len() != 64
        || !configuration_hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(RpcError::invalid_params("configuration_hash must be a lowercase SHA-256 hex digest"));
    }
    Ok(())
}

/// Values typed into the pane's PTY. The tty line discipline and the shell's line
/// editor interpret control characters (DEL/^H erase, ^U/^W kill, ^C/^\ signals,
/// ^V literal-next, ^D EOF, Tab completion, ESC/meta sequences, CR/LF submission)
/// *before* the shell parses any quoting, so quoting alone cannot keep them literal.
/// Reject every Unicode control character (C0, DEL and C1) instead of escaping it.
fn typed_text(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

fn launch_env(
    entries: &[crate::api::Label],
    launch_id: &str,
    configuration_hash: &str,
) -> Result<BTreeMap<String, String>, RpcError> {
    correlation(launch_id, configuration_hash)?;
    if entries.len() > 64 {
        return Err(RpcError::invalid_params("at most 64 environment entries"));
    }
    let mut env = crate::api::labels(entries);
    for (key, value) in &env {
        if key.is_empty()
            || !key.bytes().enumerate().all(|(i, b)| {
                b.is_ascii_uppercase() || b.is_ascii_lowercase() || b == b'_' || (i > 0 && b.is_ascii_digit())
            })
            || !typed_text(value)
            || value.len() > 16384
        {
            return Err(RpcError::invalid_params(
                "environment keys must be POSIX names and values at most 16 KiB without control characters \
                 (tab, newline, DEL and other C0/C1 controls are rejected because they are typed into a terminal)",
            ));
        }
        if matches!(key.as_str(), LAUNCH_ID_ENV | CONFIGURATION_HASH_ENV) {
            return Err(RpcError::invalid_params("HYDRA launch correlation environment keys are reserved"));
        }
    }
    env.insert(LAUNCH_ID_ENV.into(), launch_id.into());
    env.insert(CONFIGURATION_HASH_ENV.into(), configuration_hash.into());
    Ok(env)
}

fn launch_cwd(cwd: Option<&str>) -> Result<Option<&str>, RpcError> {
    if cwd.is_some_and(|cwd| !cwd.starts_with('/') || cwd.len() > 4096 || !typed_text(cwd)) {
        return Err(RpcError::invalid_params(
            "cwd must be an absolute path of at most 4096 bytes without control characters; \
             the host resolves symlinks, so only a canonical path can match observed process evidence",
        ));
    }
    Ok(cwd)
}

/// Serialize a process configuration for the classic pane's interactive shell.
/// Every value is single-quoted. That only keeps *shell* metacharacters literal:
/// the text is typed into a PTY, so control characters are rejected separately by
/// `typed_text` before this point, and the shell is assumed to have default line
/// editing (no user `bind` macros or `stty` special characters on printable keys).
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn launch_command(p: &crate::api::ClassicStartParams) -> Result<String, RpcError> {
    if p.argv.is_empty() || p.argv.len() > 128 || p.argv[0].is_empty() {
        return Err(RpcError::invalid_params("argv needs a non-empty executable and at most 128 entries"));
    }
    if p.argv.iter().any(|arg| !typed_text(arg) || arg.len() > 65536) {
        return Err(RpcError::invalid_params(
            "argv entries must be at most 64 KiB without control characters \
             (tab, newline, DEL and other C0/C1 controls are rejected because they are typed into a terminal)",
        ));
    }
    let cwd = launch_cwd(p.cwd.as_deref())?;
    let env = launch_env(&p.env, &p.launch_id, &p.configuration_hash)?;
    let assignments =
        env.iter().map(|(key, value)| format!("{key}={}", shell_quote(value))).collect::<Vec<_>>().join(" ");
    let program = p.argv.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ");
    let run = if assignments.is_empty() { program } else { format!("{assignments} {program}") };
    let command = cwd.map_or(run.clone(), |cwd| format!("cd -- {} && {run}", shell_quote(cwd)));
    if command.len() > 65536 {
        return Err(RpcError::invalid_params("serialized launch exceeds the classic 64 KiB input limit"));
    }
    Ok(command)
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

pub async fn create(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicCreateParams,
) -> Result<CreateResult, RpcError> {
    opaque(&p.target_pane_id, "target_pane_id")?;
    opaque(&p.terminal_id, "terminal_id")?;
    let cwd = launch_cwd(p.cwd.as_deref())?;
    let env = launch_env(&p.env, &p.launch_id, &p.configuration_hash)?;
    correlate(&list(paths, m).await?, &p.target_pane_id, &p.terminal_id)?;
    let direction = match p.direction {
        crate::api::ClassicSplitDirection::Right => "right",
        crate::api::ClassicSplitDirection::Down => "down",
    };
    let v = exchange(
        paths,
        m.reach(),
        session(m)?,
        "pane.split",
        json!({"target_pane_id":p.target_pane_id,"direction":direction,"focus":false,"cwd":cwd,"env":env}),
        true,
    )
    .await?;
    if v["type"] != "pane_info" {
        return Err(lost(true));
    }
    let pane: Pane = serde_json::from_value(v["pane"].clone()).map_err(|_| lost(true))?;
    // Correlation is a second native effect. Its failure never erases the confirmed pane identity.
    let correlation = match exchange(
        paths,
        m.reach(),
        session(m)?,
        "pane.report_metadata",
        json!({"pane_id":pane.pane_id,"source":"hydra.launch","tokens":{
            LAUNCH_ID_TOKEN:p.launch_id, CONFIGURATION_HASH_TOKEN:p.configuration_hash
        }}),
        true,
    )
    .await
    {
        Ok(v) if v["type"] == "ok" => Correlation::Recorded,
        Ok(_) => Correlation::Unknown,
        Err(e) if e.kind == "classic_remote" => Correlation::Unavailable,
        Err(_) => Correlation::Unknown,
    };
    Ok(CreateResult { pane, correlation })
}

async fn process_info(
    paths: &crate::paths::Paths,
    m: &Machine,
    pane_id: &str,
    terminal_id: &str,
) -> Result<ProcessInfo, RpcError> {
    correlate(&list(paths, m).await?, pane_id, terminal_id)?;
    let v = exchange(paths, m.reach(), session(m)?, "pane.process_info", json!({"pane_id":pane_id}), false).await?;
    if v["type"] != "pane_process_info" {
        return Err(protocol("invalid native process-info response"));
    }
    let process_info: ProcessInfo =
        serde_json::from_value(v["process_info"].clone()).map_err(|_| protocol("invalid native process info"))?;
    if process_info.pane_id != pane_id {
        return Err(protocol("native process info is for another pane"));
    }
    Ok(process_info)
}

pub async fn inspect(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicInspectParams,
) -> Result<InspectResult, RpcError> {
    opaque(&p.pane_id, "pane_id")?;
    opaque(&p.terminal_id, "terminal_id")?;
    Ok(InspectResult { process_info: process_info(paths, m, &p.pane_id, &p.terminal_id).await? })
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

async fn send_input(
    paths: &crate::paths::Paths,
    m: &Machine,
    pane_id: &str,
    terminal_id: &str,
    text: &str,
    keys: &[String],
) -> Result<SendResult, RpcError> {
    if text.is_empty() && keys.is_empty() {
        return Err(RpcError::invalid_params("nothing to send: give text, a key or both"));
    }
    if text.len() > 65536 || keys.len() > 64 || keys.iter().any(|k| k.len() > 64) {
        return Err(RpcError::invalid_params("classic input limit: 64 KiB text, 64 keys of at most 64 bytes"));
    }
    correlate(&list(paths, m).await?, pane_id, terminal_id)?;
    let v = exchange(
        paths,
        m.reach(),
        session(m)?,
        "pane.send_input",
        json!({"pane_id":pane_id,"text":text,"keys":keys}),
        true,
    )
    .await?;
    if v["type"] != "ok" {
        return Err(lost(true));
    }
    Ok(SendResult { accepted: true })
}

pub async fn send(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicSendParams,
) -> Result<SendResult, RpcError> {
    send_input(paths, m, &p.pane_id, &p.terminal_id, &p.text, &p.keys).await
}

/// Shells without their own line editor read typed input through the tty's canonical
/// mode, which silently truncates a line at MAX_CANON (1024 bytes on macOS, 4095 on
/// Linux). A truncated launch leaves an unterminated quote at a PS2 prompt that later
/// input would complete, so long launches are refused before input for those shells.
const CANONICAL_LINE_LIMIT: usize = 1000;

fn shell_name(info: &ProcessInfo) -> Option<&str> {
    let executable = info.foreground_processes.first()?.argv.as_ref()?.first()?;
    let executable = executable.strip_prefix('-').unwrap_or(executable);
    Some(executable.rsplit('/').next().unwrap_or(executable))
}

fn interactive_posix_shell(info: &ProcessInfo) -> bool {
    let [process] = info.foreground_processes.as_slice() else { return false };
    if info.shell_pid != Some(process.pid) || info.foreground_process_group_id != Some(process.pid) {
        return false;
    }
    let Some(argv) = &process.argv else { return false };
    let Some(executable) = argv.first() else { return false };
    let executable = executable.strip_prefix('-').unwrap_or(executable);
    let name = executable.rsplit('/').next().unwrap_or(executable);
    if !matches!(name, "sh" | "bash" | "dash" | "ash" | "zsh" | "ksh" | "mksh" | "pdksh") {
        return false;
    }
    argv[1..].iter().all(|argument| {
        if argument == "--" {
            true
        } else if argument.starts_with("--") {
            // Long options are shell-specific and can consume command/input modes.
            false
        } else if let Some(flags) = argument.strip_prefix('-') {
            // -c supplies command text and -s reads standard input; neither is a
            // known idle interactive prompt. Other flag-only invocations are safe.
            !flags.contains('c') && !flags.contains('s')
        } else {
            // A non-option operand is a script path, not an interactive prompt.
            false
        }
    })
}

fn canonical_line_fits(prestart: &ProcessInfo, command: &str) -> Result<(), RpcError> {
    // bash (readline) and zsh (zle) edit lines in raw mode; every other supported
    // shell may be reading through the canonical tty buffer.
    if matches!(shell_name(prestart), Some("bash" | "zsh")) || command.len() < CANONICAL_LINE_LIMIT {
        return Ok(());
    }
    Err(RpcError::new(
        "classic_shell_not_ready",
        format!(
            "serialized launch is {} bytes; a shell without a line editor reads at most {CANONICAL_LINE_LIMIT} \
             bytes per typed line",
            command.len()
        ),
        "no input was sent; shorten the configuration or start from a bash/zsh prompt",
    ))
}

pub async fn start(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicStartParams,
) -> Result<StartResult, RpcError> {
    opaque(&p.pane_id, "pane_id")?;
    opaque(&p.terminal_id, "terminal_id")?;
    let command = launch_command(p)?;
    let prestart = process_info(paths, m, &p.pane_id, &p.terminal_id).await?;
    if !interactive_posix_shell(&prestart) {
        return Err(RpcError::new(
            "classic_shell_not_ready",
            "native pane is not at one observed interactive POSIX shell prompt",
            "no input was sent; return to a supported sh/bash/dash/ash/zsh/ksh prompt and retry explicitly",
        ));
    }
    canonical_line_fits(&prestart, &command)?;
    // Readiness and input are separate native requests, so a foreground change
    // between them remains an unavoidable race. The reply preserves the observed
    // prestart identity; callers must require new process evidence after the ack.
    send_input(paths, m, &p.pane_id, &p.terminal_id, &command, &["Enter".into()]).await?;
    Ok(StartResult { accepted: true, prestart })
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
    fn launch_serialization_quotes_shell_metacharacters_exactly() {
        let p: crate::api::ClassicStartParams = serde_json::from_value(json!({
            "machine":"native", "pane_id":"w1:p2", "terminal_id":"term_2",
            "argv":["python3", "-c", "print('ok')", "; touch /tmp/no; $(touch /tmp/no) \" '"],
            "cwd":"/tmp/work; touch /tmp/no", "env":{"FIXTURE_VALUE":"x;$(touch /tmp/no) \" '"},
            "launch_id":"launch-1", "configuration_hash":"a".repeat(64)
        }))
        .unwrap();
        let command = launch_command(&p).unwrap();
        assert_eq!(
            command,
            "cd -- '/tmp/work; touch /tmp/no' && FIXTURE_VALUE='x;$(touch /tmp/no) \" '\\''' HYDRA_CONFIGURATION_HASH='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' HYDRA_LAUNCH_ID='launch-1' 'python3' '-c' 'print('\\''ok'\\'')' '; touch /tmp/no; $(touch /tmp/no) \" '\\'''"
        );
        assert!(!command.contains("\nexec"));
        let mut invalid = serde_json::to_value(&p).unwrap();
        invalid["env"] = json!({"BAD=KEY":"x"});
        let invalid: crate::api::ClassicStartParams = serde_json::from_value(invalid).unwrap();
        assert_eq!(launch_command(&invalid).unwrap_err().kind, "invalid_params");
    }

    #[test]
    fn correlation_and_configuration_are_validated_before_effects() {
        let base = json!({
            "machine":"native", "pane_id":"w1:p2", "terminal_id":"term_2", "argv":["pi"],
            "launch_id":"launch-1", "configuration_hash":"a".repeat(64), "env":{}
        });
        for (field, replacement) in [
            ("launch_id", json!("bad id")),
            ("configuration_hash", json!("not-a-hash")),
            ("argv", json!([])),
            ("cwd", json!("bad\0path")),
        ] {
            let mut value = base.clone();
            value[field] = replacement;
            let p: crate::api::ClassicStartParams = serde_json::from_value(value).unwrap();
            assert_eq!(launch_command(&p).unwrap_err().kind, "invalid_params", "{field}");
        }
        let mut reserved = base;
        reserved["env"] = json!({"HYDRA_LAUNCH_ID":"other"});
        let p: crate::api::ClassicStartParams = serde_json::from_value(reserved).unwrap();
        assert_eq!(launch_command(&p).unwrap_err().kind, "invalid_params");
    }

    #[test]
    fn terminal_control_characters_are_rejected_before_any_request() {
        let base = json!({
            "machine":"native", "pane_id":"w1:p2", "terminal_id":"term_2", "argv":["/bin/echo", "ok"],
            "cwd":"/tmp", "launch_id":"launch-1", "configuration_hash":"a".repeat(64), "env":{"V":"ok"}
        });
        // Every C0 control, DEL and every C1 control (as UTF-8) in every typed field.
        let controls = (0u32..0x20).chain(0x7f..0xa0).map(|c| char::from_u32(c).unwrap());
        for control in controls {
            let value = format!("x{control}; touch /tmp/PWN #");
            for field in ["argv", "env", "cwd"] {
                let mut v = base.clone();
                match field {
                    "argv" => v["argv"][1] = json!(value),
                    "env" => v["env"]["V"] = json!(value),
                    _ => v["cwd"] = json!(format!("/{value}")),
                }
                let p: crate::api::ClassicStartParams = serde_json::from_value(v).unwrap();
                let error = launch_command(&p).unwrap_err();
                assert_eq!(error.kind, "invalid_params", "{field} U+{:04X}", control as u32);
                assert!(error.message.contains("control characters"), "{field}: {}", error.message);
            }
            let mut v = base.clone();
            v["argv"][0] = json!(format!("/bin/echo{control}"));
            let p: crate::api::ClassicStartParams = serde_json::from_value(v).unwrap();
            assert_eq!(launch_command(&p).unwrap_err().kind, "invalid_params", "executable U+{:04X}", control as u32);
        }
        // Benign UTF-8 and shell metacharacters stay literal (single-quoted, unchanged).
        let mut v = base;
        let literal = "café 日本 😀 \u{a0}\u{2028} $(id) `id` ;|&<>*?[]{}~!\\ \" '";
        v["argv"][1] = json!(literal);
        v["env"]["V"] = json!(literal);
        let p: crate::api::ClassicStartParams = serde_json::from_value(v).unwrap();
        let command = launch_command(&p).unwrap();
        assert!(command.contains(&shell_quote(literal)));
        assert!(!command.chars().any(char::is_control));
    }

    #[test]
    fn cwd_must_be_absolute() {
        for cwd in ["relative", "./x", "~/x", "", " /tmp"] {
            assert_eq!(launch_cwd(Some(cwd)).unwrap_err().kind, "invalid_params", "{cwd:?}");
        }
        assert_eq!(launch_cwd(Some("/tmp/x y")).unwrap(), Some("/tmp/x y"));
        assert_eq!(launch_cwd(None).unwrap(), None);
    }

    #[test]
    fn canonical_mode_shells_refuse_lines_they_would_truncate() {
        let shell = |name: &str| ProcessInfo {
            pane_id: "w1:p2".into(),
            shell_pid: Some(10),
            foreground_process_group_id: Some(10),
            tty: None,
            foreground_processes: vec![Process {
                pid: 10,
                name: name.into(),
                argv0: None,
                argv: Some(vec![format!("-{name}")]),
                cmdline: None,
                cwd: None,
            }],
        };
        let long = "x".repeat(CANONICAL_LINE_LIMIT);
        for name in ["sh", "dash", "ash", "ksh", "mksh"] {
            assert!(canonical_line_fits(&shell(name), &long[1..]).is_ok(), "{name}");
            assert_eq!(canonical_line_fits(&shell(name), &long).unwrap_err().kind, "classic_shell_not_ready", "{name}");
        }
        for name in ["bash", "zsh"] {
            assert!(canonical_line_fits(&shell(name), &"x".repeat(65536)).is_ok(), "{name}");
        }
    }

    #[test]
    fn start_readiness_requires_an_interactive_posix_shell() {
        let info = |argv: &[&str]| ProcessInfo {
            pane_id: "w1:p2".into(),
            shell_pid: Some(10),
            foreground_process_group_id: Some(10),
            tty: Some("/dev/ttys1".into()),
            foreground_processes: vec![Process {
                pid: 10,
                name: "shell".into(),
                argv0: None,
                argv: Some(argv.iter().map(|arg| arg.to_string()).collect()),
                cmdline: None,
                cwd: None,
            }],
        };
        for argv in [vec!["/bin/sh"], vec!["bash", "-li"], vec!["-zsh"], vec!["dash", "--"]] {
            assert!(interactive_posix_shell(&info(&argv)), "{argv:?}");
        }
        for argv in [
            vec!["fish"],
            vec!["sh", "-c", "echo occupied"],
            vec!["sh", "script.sh"],
            vec!["sh", "-s"],
            vec!["zsh", "--sh-in-stdin"],
            vec!["vim"],
        ] {
            assert!(!interactive_posix_shell(&info(&argv)), "{argv:?}");
        }
        let mut occupied = info(&["python3", "fixture.py"]);
        occupied.foreground_processes[0].pid = 11;
        occupied.foreground_process_group_id = Some(11);
        assert!(!interactive_posix_shell(&occupied));
        let mut unknown = info(&[]);
        unknown.foreground_processes[0].argv = None;
        assert!(!interactive_posix_shell(&unknown));
        unknown.foreground_processes.push(unknown.foreground_processes[0].clone());
        assert!(!interactive_posix_shell(&unknown));
    }

    #[test]
    fn unsupported_and_ambiguous_are_explicit() {
        assert_eq!(unsupported("terminal.attach").kind, "unsupported_backend_capability");
        assert_eq!(lost(true).kind, "classic_outcome_unknown");
        assert_eq!(lost(false).kind, "classic_transport");
    }
}
