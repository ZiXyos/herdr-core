//! Pane creation and process launch on the classic control API, shared by desktop `hc` and the
//! embedded mobile engine so both validate, serialize and report outcomes identically.

use crate::{ControlError, ControlTransport, Pane, Request, SendInput, correlate, list, lost, protocol, send};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const LAUNCH_ID_TOKEN: &str = "hydra_launch_id";
const CONFIGURATION_HASH_TOKEN: &str = "hydra_configuration_hash";
const LAUNCH_ID_ENV: &str = "HYDRA_LAUNCH_ID";
const CONFIGURATION_HASH_ENV: &str = "HYDRA_CONFIGURATION_HASH";
/// Shells without their own line editor read typed input through the tty's canonical
/// mode, which silently truncates a line at MAX_CANON (1024 bytes on macOS, 4095 on
/// Linux). A truncated launch leaves an unterminated quote at a PS2 prompt that later
/// input would complete, so long launches are refused before input for those shells.
const CANONICAL_LINE_LIMIT: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Right,
    Down,
}

/// Process configuration typed into a classic pane's interactive shell.
pub struct Launch<'a> {
    pub argv: &'a [String],
    pub cwd: Option<&'a str>,
    pub env: &'a BTreeMap<String, String>,
    pub launch_id: &'a str,
    pub configuration_hash: &'a str,
}

/// An unfocused native split of an existing pane, tagged for launch correlation.
pub struct Split<'a> {
    pub target_pane_id: &'a str,
    pub terminal_id: &'a str,
    pub direction: SplitDirection,
    pub cwd: Option<&'a str>,
    pub env: &'a BTreeMap<String, String>,
    pub launch_id: &'a str,
    pub configuration_hash: &'a str,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct StartResult {
    /// Classic accepted input; not a claim that the requested process executed.
    pub accepted: bool,
    /// Authoritative foreground identity observed immediately before input was sent.
    pub prestart: ProcessInfo,
}

#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Correlation {
    Recorded,
    Unavailable,
    Unknown,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct CreateResult {
    pub pane: Pane,
    /// Whether native correlation metadata was acknowledged. Pane creation is confirmed in all cases.
    pub correlation: Correlation,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
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

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct InspectResult {
    pub process_info: ProcessInfo,
}

fn opaque(id: &str, what: &str) -> Result<(), ControlError> {
    if id.is_empty() || id.len() > 256 || id.contains('\0') {
        Err(ControlError::invalid_params(format!("{what} must be 1..256 bytes without NUL")))
    } else {
        Ok(())
    }
}

fn correlation(launch_id: &str, configuration_hash: &str) -> Result<(), ControlError> {
    if launch_id.is_empty()
        || launch_id.len() > 64
        || !launch_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(ControlError::invalid_params("launch_id must be 1..64 ASCII letters, digits, _ or -"));
    }
    if configuration_hash.len() != 64
        || !configuration_hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ControlError::invalid_params("configuration_hash must be a lowercase SHA-256 hex digest"));
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
    env: &BTreeMap<String, String>,
    launch_id: &str,
    configuration_hash: &str,
) -> Result<BTreeMap<String, String>, ControlError> {
    correlation(launch_id, configuration_hash)?;
    if env.len() > 64 {
        return Err(ControlError::invalid_params("at most 64 environment entries"));
    }
    for (key, value) in env {
        if key.is_empty()
            || !key.bytes().enumerate().all(|(i, b)| {
                b.is_ascii_uppercase() || b.is_ascii_lowercase() || b == b'_' || (i > 0 && b.is_ascii_digit())
            })
            || !typed_text(value)
            || value.len() > 16384
        {
            return Err(ControlError::invalid_params(
                "environment keys must be POSIX names and values at most 16 KiB without control characters \
                 (tab, newline, DEL and other C0/C1 controls are rejected because they are typed into a terminal)",
            ));
        }
        if matches!(key.as_str(), LAUNCH_ID_ENV | CONFIGURATION_HASH_ENV) {
            return Err(ControlError::invalid_params("HYDRA launch correlation environment keys are reserved"));
        }
    }
    let mut env = env.clone();
    env.insert(LAUNCH_ID_ENV.into(), launch_id.into());
    env.insert(CONFIGURATION_HASH_ENV.into(), configuration_hash.into());
    Ok(env)
}

fn launch_cwd(cwd: Option<&str>) -> Result<Option<&str>, ControlError> {
    if cwd.is_some_and(|cwd| !cwd.starts_with('/') || cwd.len() > 4096 || !typed_text(cwd)) {
        return Err(ControlError::invalid_params(
            "cwd must be an absolute path of at most 4096 bytes without control characters; \
             the host resolves symlinks, so only a canonical path can match observed process evidence",
        ));
    }
    Ok(cwd)
}

/// Every value is single-quoted. That only keeps *shell* metacharacters literal:
/// the text is typed into a PTY, so control characters are rejected separately by
/// `typed_text` before this point, and the shell is assumed to have default line
/// editing (no user `bind` macros or `stty` special characters on printable keys).
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Serialize a process configuration for the classic pane's interactive shell.
pub fn launch_command(p: &Launch<'_>) -> Result<String, ControlError> {
    if p.argv.is_empty() || p.argv.len() > 128 || p.argv[0].is_empty() {
        return Err(ControlError::invalid_params("argv needs a non-empty executable and at most 128 entries"));
    }
    if p.argv.iter().any(|arg| !typed_text(arg) || arg.len() > 65536) {
        return Err(ControlError::invalid_params(
            "argv entries must be at most 64 KiB without control characters \
             (tab, newline, DEL and other C0/C1 controls are rejected because they are typed into a terminal)",
        ));
    }
    let cwd = launch_cwd(p.cwd)?;
    let env = launch_env(p.env, p.launch_id, p.configuration_hash)?;
    let assignments =
        env.iter().map(|(key, value)| format!("{key}={}", shell_quote(value))).collect::<Vec<_>>().join(" ");
    let program = p.argv.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ");
    let run = if assignments.is_empty() { program } else { format!("{assignments} {program}") };
    let command = cwd.map_or(run.clone(), |cwd| format!("cd -- {} && {run}", shell_quote(cwd)));
    if command.len() > 65536 {
        return Err(ControlError::invalid_params("serialized launch exceeds the classic 64 KiB input limit"));
    }
    Ok(command)
}

pub async fn create<T: ControlTransport + ?Sized>(
    transport: &T,
    session: &str,
    p: &Split<'_>,
) -> Result<CreateResult, ControlError> {
    opaque(p.target_pane_id, "target_pane_id")?;
    opaque(p.terminal_id, "terminal_id")?;
    let cwd = launch_cwd(p.cwd)?;
    let env = launch_env(p.env, p.launch_id, p.configuration_hash)?;
    correlate(&list(transport, session).await?, p.target_pane_id, p.terminal_id)?;
    let params = json!({"target_pane_id":p.target_pane_id,"direction":p.direction,"focus":false,"cwd":cwd,"env":env});
    let v = transport.exchange(session, Request { method: "pane.split", params, mutation: true }).await?;
    if v["type"] != "pane_info" {
        return Err(lost(true));
    }
    let pane: Pane = serde_json::from_value(v["pane"].clone()).map_err(|_| lost(true))?;
    // Correlation is a second native effect. Its failure never erases the confirmed pane identity.
    let params = json!({"pane_id":pane.pane_id,"source":"hydra.launch","tokens":{
        LAUNCH_ID_TOKEN:p.launch_id, CONFIGURATION_HASH_TOKEN:p.configuration_hash
    }});
    let correlation =
        match transport.exchange(session, Request { method: "pane.report_metadata", params, mutation: true }).await {
            Ok(v) if v["type"] == "ok" => Correlation::Recorded,
            Ok(_) => Correlation::Unknown,
            Err(e) if e.kind == "classic_remote" => Correlation::Unavailable,
            Err(_) => Correlation::Unknown,
        };
    Ok(CreateResult { pane, correlation })
}

async fn process_info<T: ControlTransport + ?Sized>(
    transport: &T,
    session: &str,
    pane_id: &str,
    terminal_id: &str,
) -> Result<ProcessInfo, ControlError> {
    correlate(&list(transport, session).await?, pane_id, terminal_id)?;
    let request = Request { method: "pane.process_info", params: json!({"pane_id":pane_id}), mutation: false };
    let v: Value = transport.exchange(session, request).await?;
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

/// Read-only foreground process evidence after checking the pane/runtime association.
pub async fn inspect<T: ControlTransport + ?Sized>(
    transport: &T,
    session: &str,
    pane_id: &str,
    terminal_id: &str,
) -> Result<InspectResult, ControlError> {
    opaque(pane_id, "pane_id")?;
    opaque(terminal_id, "terminal_id")?;
    Ok(InspectResult { process_info: process_info(transport, session, pane_id, terminal_id).await? })
}

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

fn canonical_line_fits(prestart: &ProcessInfo, command: &str) -> Result<(), ControlError> {
    // bash (readline) and zsh (zle) edit lines in raw mode; every other supported
    // shell may be reading through the canonical tty buffer.
    if matches!(shell_name(prestart), Some("bash" | "zsh")) || command.len() < CANONICAL_LINE_LIMIT {
        return Ok(());
    }
    Err(ControlError::new(
        "classic_shell_not_ready",
        format!(
            "serialized launch is {} bytes; a shell without a line editor reads at most {CANONICAL_LINE_LIMIT} \
             bytes per typed line",
            command.len()
        ),
        "no input was sent; shorten the configuration or start from a bash/zsh prompt",
    ))
}

/// Requires an observed idle interactive POSIX shell, then types the serialized launch and Enter.
pub async fn start<T: ControlTransport + ?Sized>(
    transport: &T,
    session: &str,
    pane_id: &str,
    terminal_id: &str,
    launch: &Launch<'_>,
) -> Result<StartResult, ControlError> {
    opaque(pane_id, "pane_id")?;
    opaque(terminal_id, "terminal_id")?;
    let command = launch_command(launch)?;
    let prestart = process_info(transport, session, pane_id, terminal_id).await?;
    if !interactive_posix_shell(&prestart) {
        return Err(ControlError::new(
            "classic_shell_not_ready",
            "native pane is not at one observed interactive POSIX shell prompt",
            "no input was sent; return to a supported sh/bash/dash/ash/zsh/ksh prompt and retry explicitly",
        ));
    }
    canonical_line_fits(&prestart, &command)?;
    // Readiness and input are separate native requests, so a foreground change
    // between them remains an unavoidable race. The reply preserves the observed
    // prestart identity; callers must require new process evidence after the ack.
    send(transport, session, SendInput { pane_id, terminal_id, text: &command, keys: &["Enter".into()] }).await?;
    Ok(StartResult { accepted: true, prestart })
}

#[cfg(test)]
mod tests;
