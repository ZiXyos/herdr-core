//! Embedded mobile engine: authenticated SSH plus the shared classic control client.
//! This crate deliberately excludes desktop PTY, subprocess, server and Ghostty code.

mod ffi;
mod keys;
mod ssh;
mod terminal;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use herdr_classic_control::{ControlError, SendInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ssh::{ChannelCommand, HostKey, SshTransport};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

pub type EventSender = mpsc::Sender<Value>;

/// What a verified classic session supports through this engine. Anything absent
/// (layout, workspace/tab creation, modern install, raw classic API access) is unsupported, not implied.
/// `classic.create` is an unfocused split of an existing pane; `terminal.stable` is the decoded
/// stable-endpoint surface (`channel.stable` remains the raw allowlisted byte channel).
pub const CAPABILITIES: &[&str] = &[
    "classic.list",
    "classic.read",
    "classic.send",
    "classic.create",
    "classic.start",
    "classic.inspect",
    "channel.stable",
    "terminal.stable",
];

/// Methods whose in-flight cancellation or shutdown leaves the remote effect unknown.
fn mutating(method: &str) -> bool {
    matches!(method, "send" | "create" | "start" | "terminalInput" | "terminalFocusTab")
}
type EventReceiver = mpsc::Receiver<Value>;

/// Bounded event queue depth. `channelData` waits for space (SSH flow control, never dropped);
/// control events that do not fit are counted and reported as one `eventsDropped` event.
pub const EVENT_CAPACITY: usize = 64;
/// Largest base64 payload accepted by `channelWrite` (64 KiB decoded), checked before decoding.
const MAX_CHANNEL_WRITE_BASE64: usize = 65536_usize.div_ceil(3) * 4;
/// Ids cancelled before their request reached the engine; bounded so a misbehaving caller cannot grow it.
const MAX_PRE_CANCELLED: usize = 256;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub session: String,
    pub private_key: String,
    pub expected_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineError {
    pub kind: String,
    pub message: String,
    pub hint: String,
    /// Present only on host-key challenges; boxed to keep ordinary errors small.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<Box<HostKeyChallenge>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyChallenge {
    pub presented_key: PresentedKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentedKey {
    pub algorithm: String,
    pub fingerprint: String,
}

impl EngineError {
    pub fn new(kind: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), hint: hint.into(), host_key: None }
    }

    fn transport(message: &str) -> Self {
        Self::new("classic_transport", message, "check the connection, then reconnect explicitly")
    }

    pub(crate) fn shutdown() -> Self {
        Self::new("shutdown", "The engine is shut down", "reload the native module")
    }

    fn cancelled() -> Self {
        Self::new("cancelled", "The native request was cancelled", "retry when ready")
    }

    fn host_key(key: HostKey, changed: bool, expected: Option<String>) -> Self {
        Self {
            kind: if changed { "host_key_changed" } else { "host_key_unverified" }.into(),
            message: if changed {
                "The SSH host key does not match the trusted key"
            } else {
                "Verify this SSH host key before connecting"
            }
            .into(),
            hint: if changed {
                "Confirm the new fingerprint independently; replacing trust can enable a man-in-the-middle attack"
            } else {
                "Compare the fingerprint through a trusted channel, then explicitly trust it"
            }
            .into(),
            host_key: Some(Box::new(HostKeyChallenge {
                presented_key: PresentedKey { algorithm: key.algorithm, fingerprint: key.fingerprint },
                expected_fingerprint: expected,
            })),
        }
    }
}

impl From<ControlError> for EngineError {
    fn from(error: ControlError) -> Self {
        Self::new(&error.kind, error.message, error.hint)
    }
}

struct Session {
    transport: SshTransport,
    config: ConnectionConfig,
}

struct ByteChannel {
    session_id: String,
    sender: mpsc::Sender<ChannelCommand>,
}

#[derive(Default)]
struct Requests {
    in_flight: HashMap<String, CancellationToken>,
    pre_cancelled: VecDeque<String>,
}

/// Control-plane event sink shared by the engine and its SSH transports. Control events never
/// block; when the bounded queue is full they are counted and surfaced as `eventsDropped`.
#[derive(Clone)]
pub struct Events {
    sender: EventSender,
    dropped: Arc<AtomicU64>,
}

impl Events {
    pub fn control(&self, event: Value) {
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Lossless: waits for queue space, which applies SSH flow control to the byte channel.
    pub async fn data(&self, event: Value) -> bool {
        self.sender.send(event).await.is_ok()
    }
}

pub struct Engine {
    sessions: Mutex<HashMap<String, Session>>,
    channels: Mutex<HashMap<String, ByteChannel>>,
    terminals: Mutex<HashMap<String, terminal::Terminal>>,
    requests: StdMutex<Requests>,
    events: Events,
    event_receiver: Mutex<EventReceiver>,
    next_id: AtomicU64,
    shutdown: CancellationToken,
}

impl Engine {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
            terminals: Mutex::new(HashMap::new()),
            requests: StdMutex::new(Requests::default()),
            events: Events { sender, dropped: Arc::new(AtomicU64::new(0)) },
            event_receiver: Mutex::new(receiver),
            next_id: AtomicU64::new(1),
            shutdown: CancellationToken::new(),
        })
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Registers a request synchronously, so a cancel issued after this returns always reaches it.
    /// A request whose id was cancelled before registration fails here with `cancelled`.
    pub fn begin(&self, id: &str) -> Result<CancellationToken, EngineError> {
        if self.shutdown.is_cancelled() {
            return Err(EngineError::shutdown());
        }
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| EngineError::new("internal", "Request registry unavailable", "reload the native module"))?;
        if let Some(index) = requests.pre_cancelled.iter().position(|item| item == id) {
            requests.pre_cancelled.remove(index);
            return Err(EngineError::cancelled());
        }
        if requests.in_flight.contains_key(id) {
            return Err(EngineError::new(
                "invalid_params",
                "Duplicate request id",
                "use a unique id for each native call",
            ));
        }
        let token = self.shutdown.child_token();
        requests.in_flight.insert(id.to_owned(), token.clone());
        Ok(token)
    }

    /// Runs a request registered by [`Engine::begin`] until it completes or is cancelled.
    pub async fn run(
        &self,
        id: String,
        token: CancellationToken,
        method: String,
        params: Value,
    ) -> Result<Value, EngineError> {
        let result = tokio::select! {
            result = self.execute_inner(method.clone(), params) => result,
            () = token.cancelled() => Err(if self.shutdown.is_cancelled() {
                if mutating(&method) {
                    herdr_classic_control::lost(true).into()
                } else {
                    EngineError::shutdown()
                }
            } else if mutating(&method) {
                ControlError::new(
                    "classic_outcome_unknown",
                    "Input was cancelled while in flight",
                    "input may have been delivered; read the pane before deciding whether to retry",
                ).into()
            } else {
                EngineError::cancelled()
            }),
        };
        if let Ok(mut requests) = self.requests.lock() {
            requests.in_flight.remove(&id);
        }
        result
    }

    pub async fn execute(self: &Arc<Self>, id: String, method: String, params: Value) -> Result<Value, EngineError> {
        let token = self.begin(&id)?;
        self.run(id, token, method, params).await
    }

    async fn execute_inner(&self, method: String, params: Value) -> Result<Value, EngineError> {
        match method.as_str() {
            "generateKey" => {
                let p: GenerateKeyParams = parse(params)?;
                keys::generate(&p.comment)
            }
            "publicKey" => {
                let p: PublicKeyParams = parse(params)?;
                keys::public_line(zeroize::Zeroizing::new(p.private_key), &p.comment)
            }
            "connect" => {
                let config: ConnectionConfig = parse(params)?;
                let id = self.id("ssh-");
                self.connect_with_id(id.clone(), config).await
            }
            "reconnect" => {
                let p: SessionParams = parse(params)?;
                let config = self.remove_session(&p.session_id).await?.config;
                self.connect_with_id(p.session_id, config).await
            }
            "disconnect" => {
                let p: SessionParams = parse(params)?;
                self.remove_session(&p.session_id).await?;
                self.close_terminals(&p.session_id).await;
                self.events.control(json!({"kind":"disconnected","sessionId":p.session_id}));
                Ok(json!({"disconnected":true}))
            }
            "list" => {
                let p: SessionParams = parse(params)?;
                let (transport, session) = self.session(&p.session_id).await?;
                let inventory = herdr_classic_control::list(&transport, &session).await?;
                Ok(serde_json::to_value(inventory)
                    .map_err(|_| EngineError::new("internal", "Could not encode inventory", "retry"))?)
            }
            "read" => {
                let p: ReadParams = parse(params)?;
                let (transport, session) = self.session(&p.session_id).await?;
                let read =
                    herdr_classic_control::read(&transport, &session, &p.pane_id, &p.terminal_id, p.lines).await?;
                Ok(serde_json::to_value(read)
                    .map_err(|_| EngineError::new("internal", "Could not encode read result", "retry"))?)
            }
            "send" => {
                let p: SendParams = parse(params)?;
                let (transport, session) = self.session(&p.session_id).await?;
                let sent = herdr_classic_control::send(
                    &transport,
                    &session,
                    SendInput { pane_id: &p.pane_id, terminal_id: &p.terminal_id, text: &p.text, keys: &p.keys },
                )
                .await?;
                Ok(serde_json::to_value(sent)
                    .map_err(|_| EngineError::new("internal", "Could not encode send result", "retry"))?)
            }
            "create" => {
                let p: CreateParams = parse(params)?;
                let (transport, session) = self.session(&p.session_id).await?;
                let split = herdr_classic_control::Split {
                    target_pane_id: &p.target_pane_id,
                    terminal_id: &p.terminal_id,
                    direction: p.direction,
                    cwd: p.cwd.as_deref(),
                    env: &p.env,
                    launch_id: &p.launch_id,
                    configuration_hash: &p.configuration_hash,
                };
                encode(herdr_classic_control::create(&transport, &session, &split).await?)
            }
            "start" => {
                let p: StartParams = parse(params)?;
                let (transport, session) = self.session(&p.session_id).await?;
                let launch = herdr_classic_control::Launch {
                    argv: &p.argv,
                    cwd: p.cwd.as_deref(),
                    env: &p.env,
                    launch_id: &p.launch_id,
                    configuration_hash: &p.configuration_hash,
                };
                encode(herdr_classic_control::start(&transport, &session, &p.pane_id, &p.terminal_id, &launch).await?)
            }
            "inspect" => {
                let p: PaneParams = parse(params)?;
                let (transport, session) = self.session(&p.session_id).await?;
                encode(herdr_classic_control::inspect(&transport, &session, &p.pane_id, &p.terminal_id).await?)
            }
            "terminalOpen" => {
                let p: TerminalOpenParams = parse(params)?;
                if !(10..=500).contains(&p.cols) || !(4..=300).contains(&p.rows) {
                    return Err(EngineError::new(
                        "invalid_params",
                        "Terminal size out of range",
                        "use 10..500 columns and 4..300 rows",
                    ));
                }
                // Client-chosen id: the caller routes events that may arrive before this call returns.
                let valid = |id: &str| {
                    (1..=64).contains(&id.len())
                        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                };
                if !valid(&p.terminal_id) || self.terminals.lock().await.contains_key(&p.terminal_id) {
                    return Err(EngineError::new(
                        "invalid_params",
                        "Terminal id must be a new 1..64 character [A-Za-z0-9_-] id",
                        "choose a unique terminal id",
                    ));
                }
                self.open_terminal(p.terminal_id, p.session_id, 1, (p.cols, p.rows)).await
            }
            "terminalReconnect" => {
                let p: TerminalParams = parse(params)?;
                let old = self.terminals.lock().await.remove(&p.terminal_id).ok_or_else(terminal_not_found)?;
                old.stop.cancel();
                self.open_terminal(p.terminal_id, old.session_id, old.epoch + 1, (old.cols, old.rows)).await
            }
            "terminalInput" => {
                let p: TerminalInputParams = parse(params)?;
                let events = terminal::input_events(p.events)?;
                let handle = self.terminal_handle(&p.terminal_id).await?;
                handle.send_input(&p.pane_id, events).await?;
                Ok(json!({"sent":true}))
            }
            "terminalFocusTab" => {
                let p: TerminalFocusParams = parse(params)?;
                let handle = self.terminal_handle(&p.terminal_id).await?;
                let result = handle.focus_tab(&p.tab_id).await?;
                Ok(json!({"result":result}))
            }
            "terminalClose" => {
                let p: TerminalParams = parse(params)?;
                let terminal = self.terminals.lock().await.remove(&p.terminal_id).ok_or_else(terminal_not_found)?;
                terminal.stop.cancel();
                Ok(json!({"closed":true}))
            }
            "channelOpen" => {
                let p: ChannelOpenParams = parse(params)?;
                let ChannelKind::Stable = p.kind;
                let (transport, session) = self.session(&p.session_id).await?;
                let channel_id = self.id("channel-");
                let sender =
                    transport.open_stable_channel(&session, channel_id.clone(), self.shutdown.child_token()).await?;
                self.channels.lock().await.insert(channel_id.clone(), ByteChannel { session_id: p.session_id, sender });
                Ok(json!({"channelId":channel_id}))
            }
            "channelWrite" => {
                let p: ChannelWriteParams = parse(params)?;
                let too_large =
                    || EngineError::new("invalid_params", "Channel writes are limited to 64 KiB", "split larger input");
                // Reject by encoded length before allocating, then by exact decoded length.
                if p.data.len() > MAX_CHANNEL_WRITE_BASE64 {
                    return Err(too_large());
                }
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD.decode(p.data).map_err(|_| {
                    EngineError::new(
                        "invalid_params",
                        "Channel data must be base64",
                        "encode binary input before calling channelWrite",
                    )
                })?;
                if bytes.len() > 65536 {
                    return Err(too_large());
                }
                let sender = match self.channels.lock().await.get(&p.channel_id) {
                    Some(channel) => channel.sender.clone(),
                    None => return Err(channel_not_found("open a channel before writing")),
                };
                if sender.send(ChannelCommand::Write(bytes)).await.is_err() {
                    self.channels.lock().await.remove(&p.channel_id);
                    return Err(EngineError::new(
                        "channel_closed",
                        "The byte channel closed before this write was queued",
                        "a channelClosed event carries the reason; open a new channel",
                    ));
                }
                Ok(json!({"accepted":true}))
            }
            "channelClose" => {
                let p: ChannelParams = parse(params)?;
                let channel = self.channels.lock().await.remove(&p.channel_id);
                match channel {
                    Some(channel) => {
                        channel.sender.send(ChannelCommand::Close).await.ok();
                        Ok(json!({"closed":true}))
                    }
                    None => Err(channel_not_found("no action taken")),
                }
            }
            _ => Err(EngineError::new(
                "unknown_method",
                format!("Unknown engine method {method:?}"),
                "use the typed HydraEngine interface",
            )),
        }
    }

    /// Registers the terminal (with its epoch) before dialing, so a failed open still owns the epoch:
    /// the failure is reported as that epoch's fatal error and `terminalReconnect` starts the next one.
    async fn open_terminal(
        &self,
        terminal_id: String,
        session_id: String,
        epoch: u64,
        size: (u16, u16),
    ) -> Result<Value, EngineError> {
        let stop = self.shutdown.child_token();
        self.terminals.lock().await.insert(
            terminal_id.clone(),
            terminal::Terminal {
                session_id: session_id.clone(),
                epoch,
                cols: size.0,
                rows: size.1,
                handle: None,
                stop: stop.clone(),
            },
        );
        let opened = match self.session(&session_id).await {
            Ok((transport, session)) => {
                terminal::open(&transport, &session, &terminal_id, epoch, size, self.events.clone(), stop).await
            }
            Err(error) => Err(error),
        };
        match opened {
            Ok(opened) => {
                if let Some(entry) = self.terminals.lock().await.get_mut(&terminal_id).filter(|t| t.epoch == epoch) {
                    entry.handle = Some(opened.handle);
                }
                Ok(json!({"terminalId":terminal_id,"epoch":epoch,"serverVersion":opened.server_version}))
            }
            Err(error) => {
                self.events.control(terminal::error_event(&terminal_id, epoch, &error.kind, &error.message));
                Err(error)
            }
        }
    }

    async fn terminal_handle(&self, id: &str) -> Result<herdr_classic::ClassicEndpointHandle, EngineError> {
        let terminals = self.terminals.lock().await;
        let terminal = terminals.get(id).ok_or_else(terminal_not_found)?;
        terminal.handle.clone().ok_or_else(|| {
            EngineError::new(
                "classic_stale_state",
                "The terminal is not connected; input was not sent",
                "reconnect the terminal",
            )
        })
    }

    async fn close_terminals(&self, session_id: &str) {
        let mut terminals = self.terminals.lock().await;
        let ids: Vec<String> =
            terminals.iter().filter(|(_, t)| t.session_id == session_id).map(|(id, _)| id.clone()).collect();
        for id in ids {
            if let Some(terminal) = terminals.remove(&id) {
                terminal.stop.cancel();
                self.events.control(terminal::error_event(
                    &id,
                    terminal.epoch,
                    "session_not_found",
                    "The SSH session was disconnected",
                ));
            }
        }
    }

    async fn connect_with_id(&self, id: String, config: ConnectionConfig) -> Result<Value, EngineError> {
        let (transport, key) = ssh::connect(&config, self.events.clone(), id.clone()).await?;
        let version = herdr_classic_control::probe(&transport, &config.session).await?;
        let fingerprint = key.fingerprint.clone();
        self.sessions.lock().await.insert(id.clone(), Session { transport, config });
        self.events.control(json!({"kind":"connected","sessionId":id,"fingerprint":fingerprint}));
        Ok(json!({
            "sessionId": id,
            "fingerprint": key.fingerprint,
            "algorithm": key.algorithm,
            "version": version,
            "capabilities": CAPABILITIES,
        }))
    }

    /// The transport and classic session name; the private key stays in the registry.
    async fn session(&self, id: &str) -> Result<(SshTransport, String), EngineError> {
        let sessions = self.sessions.lock().await;
        let session = sessions.get(id).ok_or_else(|| {
            EngineError::new(
                "session_not_found",
                "Unknown or disconnected session",
                "connect or reconnect before issuing control calls",
            )
        })?;
        Ok((session.transport.clone(), session.config.session.clone()))
    }

    async fn remove_session(&self, id: &str) -> Result<Session, EngineError> {
        let session = self.sessions.lock().await.remove(id).ok_or_else(|| {
            EngineError::new("session_not_found", "Unknown or disconnected session", "connect before disconnecting")
        })?;
        self.close_channels(Some(id)).await;
        session.transport.disconnect().await;
        Ok(session)
    }

    async fn close_channels(&self, session_id: Option<&str>) {
        let mut channels = self.channels.lock().await;
        let ids: Vec<String> = channels
            .iter()
            .filter(|(_, channel)| session_id.is_none_or(|id| channel.session_id == id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(channel) = channels.remove(&id) {
                channel.sender.try_send(ChannelCommand::Close).ok();
            }
        }
    }

    /// Cancels a registered request. An id not yet registered is remembered (bounded), so a request
    /// still queued in the native bridge fails with `cancelled` instead of running later.
    /// Returns true when an in-flight request was signalled, false when the id was only recorded.
    pub fn cancel(&self, id: &str) -> bool {
        let Ok(mut requests) = self.requests.lock() else { return false };
        if let Some(token) = requests.in_flight.get(id) {
            token.cancel();
            return true;
        }
        if id.is_empty() || id.len() > 128 || self.shutdown.is_cancelled() {
            return false;
        }
        if requests.pre_cancelled.len() == MAX_PRE_CANCELLED {
            requests.pre_cancelled.pop_front();
        }
        requests.pre_cancelled.push_back(id.to_owned());
        false
    }

    /// Next event in order, or None once the engine shuts down. Control events lost to a full
    /// queue are reported first as `{"kind":"eventsDropped","count":n}`.
    pub async fn next_event(&self) -> Option<Value> {
        if self.shutdown.is_cancelled() {
            return None;
        }
        let mut receiver = self.event_receiver.lock().await;
        let dropped = self.events.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            return Some(json!({"kind":"eventsDropped","count":dropped}));
        }
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => None,
            event = receiver.recv() => event,
        }
    }

    pub fn is_shut_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Cancels every request and event wait, then disconnects sessions (bounded to 2 s each).
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        self.close_channels(None).await;
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for (_, session) in sessions {
            tokio::time::timeout(std::time::Duration::from_secs(2), session.transport.disconnect()).await.ok();
        }
    }
}

fn terminal_not_found() -> EngineError {
    EngineError::new("terminal_not_found", "Unknown or closed terminal", "open a terminal first")
}

fn encode<T: Serialize>(value: T) -> Result<Value, EngineError> {
    serde_json::to_value(value).map_err(|_| EngineError::new("internal", "Could not encode result", "retry"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateParams {
    session_id: String,
    target_pane_id: String,
    terminal_id: String,
    direction: herdr_classic_control::SplitDirection,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    launch_id: String,
    configuration_hash: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartParams {
    session_id: String,
    pane_id: String,
    terminal_id: String,
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    launch_id: String,
    configuration_hash: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaneParams {
    session_id: String,
    pane_id: String,
    terminal_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalOpenParams {
    session_id: String,
    terminal_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalParams {
    terminal_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalInputParams {
    terminal_id: String,
    pane_id: String,
    events: Vec<terminal::InputEvent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalFocusParams {
    terminal_id: String,
    tab_id: String,
}

fn channel_not_found(hint: &str) -> EngineError {
    EngineError::new("channel_not_found", "Unknown or closed byte channel", hint)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateKeyParams {
    #[serde(default)]
    comment: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicKeyParams {
    private_key: String,
    #[serde(default)]
    comment: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadParams {
    session_id: String,
    pane_id: String,
    terminal_id: String,
    lines: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    session_id: String,
    pane_id: String,
    terminal_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelOpenParams {
    session_id: String,
    kind: ChannelKind,
}

/// Only the classic stable surface bridge. Raw classic API access (`remote-api-bridge`) is deliberately
/// not exposed: it would bypass pane identity checks and the no-retry mutation rules of the typed calls.
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ChannelKind {
    Stable,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelWriteParams {
    channel_id: String,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelParams {
    channel_id: String,
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, EngineError> {
    serde_json::from_value(value).map_err(|error| {
        EngineError::new("invalid_params", error.to_string(), "use the typed HydraEngine request parameters")
    })
}

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("hydra-engine")
            .enable_all()
            .build()
            .expect("create Hydra Tokio runtime")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_key_is_never_serialized_in_errors() {
        let error = EngineError::new("ssh_auth_failed", "The host rejected this key", "check authorization");
        assert!(!serde_json::to_string(&error).unwrap().contains("private"));
    }

    #[tokio::test]
    async fn cancellation_is_scoped_and_shutdown_is_explicit() {
        let engine = Engine::new();
        assert!(!engine.cancel("absent"));
        let result = engine.execute("1".into(), "unknown".into(), json!({})).await.unwrap_err();
        assert_eq!(result.kind, "unknown_method");
        engine.shutdown().await;
        let result = engine.execute("2".into(), "unknown".into(), json!({})).await.unwrap_err();
        assert_eq!(result.kind, "shutdown");
        assert!(engine.next_event().await.is_none(), "event waits end at shutdown");
    }

    #[tokio::test]
    async fn cancel_before_registration_reaches_the_queued_request() {
        let engine = Engine::new();
        assert!(!engine.cancel("queued"), "not in flight yet, only recorded");
        let error = engine.execute("queued".into(), "list".into(), json!({"sessionId":"x"})).await.unwrap_err();
        assert_eq!(error.kind, "cancelled");
        // The record is consumed: the id can be reused afterwards.
        let error = engine.execute("queued".into(), "list".into(), json!({"sessionId":"x"})).await.unwrap_err();
        assert_eq!(error.kind, "session_not_found");
        for index in 0..(MAX_PRE_CANCELLED + 10) {
            engine.cancel(&format!("never-{index}"));
        }
        assert_eq!(engine.requests.lock().unwrap().pre_cancelled.len(), MAX_PRE_CANCELLED);
    }

    #[tokio::test]
    async fn full_event_queue_reports_dropped_control_events() {
        let engine = Engine::new();
        for index in 0..(EVENT_CAPACITY + 3) {
            engine.events.control(json!({"kind":"connected","sessionId":index.to_string()}));
        }
        let first = engine.next_event().await.unwrap();
        assert_eq!(first, json!({"kind":"eventsDropped","count":3}));
        let next = engine.next_event().await.unwrap();
        assert_eq!(next["sessionId"], "0", "queued events stay in order after the overflow marker");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn channel_data_waits_for_space_and_is_never_dropped() {
        let engine = Engine::new();
        let events = engine.events.clone();
        let producer = tokio::spawn(async move {
            for index in 0..(EVENT_CAPACITY * 4) {
                assert!(events.data(json!({"kind":"channelData","seq":index})).await);
            }
        });
        for expected in 0..(EVENT_CAPACITY * 4) {
            let event = engine.next_event().await.unwrap();
            assert_eq!(event["seq"], expected, "ordered and lossless under backpressure");
        }
        producer.await.unwrap();
        assert_eq!(engine.events.dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn oversized_channel_write_is_rejected_before_decoding() {
        let engine = Engine::new();
        let data = "!".repeat(MAX_CHANNEL_WRITE_BASE64 + 1);
        let error =
            engine.execute("w".into(), "channelWrite".into(), json!({"channelId":"c","data":data})).await.unwrap_err();
        assert_eq!(error.message, "Channel writes are limited to 64 KiB");
        assert_eq!(MAX_CHANNEL_WRITE_BASE64, 87_384);
    }

    #[test]
    fn only_the_stable_channel_is_allowlisted() {
        assert!(serde_json::from_value::<ChannelOpenParams>(json!({"sessionId":"ssh-1","kind":"stable"})).is_ok());
        for kind in ["control", "sh -c anything", "remote-api-bridge"] {
            assert!(serde_json::from_value::<ChannelOpenParams>(json!({"sessionId":"ssh-1","kind":kind})).is_err());
        }
        assert!(!CAPABILITIES.contains(&"channel.control"));
    }
}
