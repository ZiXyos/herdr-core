//! Desktop subprocess transport for the transport-neutral classic control client.

use crate::machines::{Backend, Machine, Reach};
use crate::rpc::RpcError;
use herdr_classic_control::{ControlError, ControlTransport, MAX_BYTES, Request};
pub use herdr_classic_control::{Inventory, Pane, ReadResult, SendResult, Text};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

impl From<ControlError> for RpcError {
    fn from(error: ControlError) -> Self {
        if error.kind == "invalid_params" {
            RpcError::invalid_params(error.message)
        } else {
            RpcError::new(&error.kind, error.message, error.hint)
        }
    }
}

pub fn unsupported(operation: &str) -> RpcError {
    herdr_classic_control::unsupported(operation).into()
}

async fn read_reply(read: impl AsyncRead + Unpin) -> Result<Value, ControlError> {
    let mut bytes = Vec::new();
    BufReader::new(read.take((MAX_BYTES + 1) as u64))
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| herdr_classic_control::lost(false))?;
    if bytes.len() > MAX_BYTES || bytes.last() != Some(&b'\n') {
        return Err(ControlError::new(
            "classic_protocol",
            "classic response exceeds 1 MiB or lacks newline",
            "supported endpoint: classic Herdr speaking JSON API protocol 22",
        ));
    }
    herdr_classic_control::decode(&bytes)
}

struct Subprocess<'a> {
    paths: &'a crate::paths::Paths,
    to: Reach<'a>,
}

impl ControlTransport for Subprocess<'_> {
    async fn exchange(&self, session: &str, request: Request) -> Result<Value, ControlError> {
        crate::machines::check_target(self.to).map_err(|e| ControlError::new(&e.kind, e.message, e.hint))?;
        let mutation = request.mutation;
        let request = request.encode()?;
        // Session syntax was checked by the control client before this interpolation.
        let command = format!("herdr --session {session} remote-api-bridge");
        let mut cmd = tokio::process::Command::from(self.to.command(
            self.paths,
            &["BatchMode=yes", "ConnectTimeout=10"],
            &command,
        ));
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| ControlError::new("classic_transport", e.to_string(), "check SSH"))?;
        let mut stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            stdin.write_all(&request).await.map_err(|_| herdr_classic_control::lost(mutation))?;
            // Do not close stdin before the reply: the native bridge owns its lifetime.
            read_reply(stdout).await
        })
        .await
        .unwrap_or_else(|_| Err(herdr_classic_control::lost(mutation)));
        let _ = child.kill().await;
        let _ = child.wait().await;
        result.map_err(|e| if mutation && e.kind != "classic_remote" { herdr_classic_control::lost(true) } else { e })
    }
}

pub async fn probe(paths: &crate::paths::Paths, to: Reach<'_>, session: &str) -> Result<String, RpcError> {
    Ok(herdr_classic_control::probe(&Subprocess { paths, to }, session).await?)
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

pub async fn list(paths: &crate::paths::Paths, m: &Machine) -> Result<Inventory, RpcError> {
    Ok(herdr_classic_control::list(&Subprocess { paths, to: m.reach() }, session(m)?).await?)
}

pub async fn read(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicReadParams,
) -> Result<ReadResult, RpcError> {
    Ok(herdr_classic_control::read(
        &Subprocess { paths, to: m.reach() },
        session(m)?,
        &p.pane_id,
        &p.terminal_id,
        p.lines,
    )
    .await?)
}

pub async fn send(
    paths: &crate::paths::Paths,
    m: &Machine,
    p: &crate::api::ClassicSendParams,
) -> Result<SendResult, RpcError> {
    Ok(herdr_classic_control::send(
        &Subprocess { paths, to: m.reach() },
        session(m)?,
        herdr_classic_control::SendInput {
            pane_id: &p.pane_id,
            terminal_id: &p.terminal_id,
            text: &p.text,
            keys: &p.keys,
        },
    )
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_is_bounded_and_requires_a_complete_line() {
        assert!(read_reply(&b"{\"id\":\"hc-classic\",\"result\":{}}\n"[..]).await.is_ok());
        assert!(read_reply(&b"{\"id\":\"hc-classic\",\"result\":{}}"[..]).await.is_err());
        assert!(read_reply(&vec![b'x'; MAX_BYTES + 1][..]).await.is_err());
    }
}
