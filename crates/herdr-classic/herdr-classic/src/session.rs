use crate::ClassicError;
use crate::framing::{FrameReader, FrameWriter, write_frame};
use crate::protocol::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

const COMMAND_QUEUE: usize = 32;
const MAX_INPUT_EVENTS: usize = 64;
const MAX_INPUT_TEXT_BYTES: usize = 64 * 1024;
/// Endpoint requests are small navigation operations. Bounding serialized params before
/// enqueue keeps the 32-command queue at a few MiB in the worst case.
pub const MAX_REQUEST_PARAMS_BYTES: usize = 16 * 1024;
/// Immediately available inbound frames applied before a write turn. Under a sustained
/// inbound flood the next command write still gets a turn after at most this many frames
/// (plus the complete frames already buffered in memory, which are always applied first).
const READ_BURST: usize = 8;
/// Default bound on how long input is held while the snapshot is ahead of its surface.
/// Classic 0.9.5 renders the matching surface in the same pass as the snapshot (and
/// retries a deferred render when the client writer drains), so this only fires when the
/// server or transport stalls. Then unsent input is retired as not sent, and a partially
/// written frame closes the endpoint with `classic_mutation_unknown`.
pub const DEFAULT_INPUT_HOLD_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a command may transmit (further) bytes now.
enum Readiness {
    Ready,
    /// The target fence is unchanged but the state is transiently incoherent: the snapshot
    /// is ahead of its full surface. The command is held, neither sent nor retired.
    Wait,
    Stale(ClassicError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndpointEvent {
    Snapshot(ShellSnapshot),
    Surface(PaneSurface),
    Response(EndpointResponse),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointResponse {
    pub request_id: String,
    pub result: Result<Value, HandshakeError>,
}

/// Generation fences observed when a command is queued. `target` changes on boot change,
/// invalidation, popup appearance/removal and visible-pane (navigation) changes; `boot`
/// only on boot change and invalidation. Content-only surface updates change neither.
#[derive(Default)]
struct Fences {
    boot: AtomicU64,
    target: AtomicU64,
}

enum CommandKind {
    Input {
        pane_id: String,
        events: Vec<PaneInputEvent>,
    },
    /// `await_response` commands complete with the correlated remote result; others
    /// complete with the request id once the frame has been written.
    Request {
        method: String,
        params: Value,
        await_response: bool,
    },
}

enum CommandOutcome {
    Input,
    Requested(String),
    Response(Value),
}

type Completion = oneshot::Sender<Result<CommandOutcome, ClassicError>>;

struct EndpointCommand {
    kind: CommandKind,
    fence: u64,
    completion: Option<Completion>,
    /// Outcome reported when the staged frame is fully written. None for awaited
    /// requests, whose completion moved into the pending response waiter.
    on_written: Option<CommandOutcome>,
}

impl EndpointCommand {
    fn finish(&mut self, result: Result<CommandOutcome, ClassicError>) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(result);
        }
    }
}

struct PendingRequest {
    boot_id: String,
    request_id: String,
    bytes: Vec<u8>,
    waiter: Option<Completion>,
}

impl PendingRequest {
    fn finish(mut self, result: Result<CommandOutcome, ClassicError>) {
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.send(result);
        }
    }
}

fn check_command_bounds(kind: &CommandKind) -> Result<(), ClassicError> {
    match kind {
        CommandKind::Input { pane_id, events } => {
            if pane_id.is_empty() || pane_id.len() > 128 {
                return Err(ClassicError::new("classic_bound", "input pane id must be 1..128 bytes"));
            }
            if events.is_empty() || events.len() > MAX_INPUT_EVENTS {
                return Err(ClassicError::new("classic_bound", "input requires 1..64 events"));
            }
            let text_bytes = events.iter().try_fold(0usize, |total, event| {
                let len = match event {
                    PaneInputEvent::TextCommit(text) | PaneInputEvent::Paste(text) => text.len(),
                    PaneInputEvent::Key { generated_text, .. } => generated_text.as_ref().map_or(0, String::len),
                    PaneInputEvent::Mouse { .. } => 0,
                };
                total.checked_add(len)
            });
            if text_bytes.is_none_or(|bytes| bytes > MAX_INPUT_TEXT_BYTES) {
                return Err(ClassicError::new("classic_bound", "semantic input text exceeds 64 KiB"));
            }
        }
        CommandKind::Request { method, params, .. } => {
            if method.is_empty() || method.len() > 128 {
                return Err(ClassicError::new("classic_bound", "endpoint method must be 1..128 bytes"));
            }
            let size = serde_json::to_vec(params).map_err(|error| ClassicError::protocol(error.to_string()))?.len();
            if size > MAX_REQUEST_PARAMS_BYTES {
                return Err(ClassicError::new("classic_bound", "endpoint request params exceed 16 KiB"));
            }
        }
    }
    Ok(())
}

fn not_sent(reason: &str) -> ClassicError {
    ClassicError::stale(format!("{reason}; command was not sent"))
}

fn outcome_unknown(reason: &str) -> ClassicError {
    ClassicError::new("classic_mutation_unknown", format!("{reason}; mutation outcome unknown"))
}

/// Visible input target of one connection: the popup (if any) and the selected tab's panes.
fn target_key(surface: &PaneSurface) -> (Option<&str>, Vec<&str>) {
    (
        surface.popup.as_ref().map(|popup| popup.terminal_id.as_str()),
        surface.panes.iter().map(|pane| pane.pane_id.as_str()).collect(),
    )
}

/// Bounded command side of a running endpoint. It can be cloned and used by input/UI
/// tasks while another task drives `ClassicEndpoint::next_event`.
///
/// Commands are bounded before they are queued and fenced by the target generation
/// observed at enqueue time. A command whose target changed (navigation, popup, boot,
/// invalidation) before its first byte is retired with `classic_stale_state` and never
/// sent. Once queued, dropping a command future does not cancel the mutation.
#[derive(Clone)]
pub struct ClassicEndpointHandle {
    commands: mpsc::Sender<EndpointCommand>,
    fences: Arc<Fences>,
}

impl ClassicEndpointHandle {
    async fn execute(&self, kind: CommandKind) -> Result<CommandOutcome, ClassicError> {
        check_command_bounds(&kind)?;
        let fence = match kind {
            CommandKind::Input { .. } => self.fences.target.load(Ordering::SeqCst),
            CommandKind::Request { .. } => self.fences.boot.load(Ordering::SeqCst),
        };
        let (completion, result) = oneshot::channel();
        self.commands
            .send(EndpointCommand { kind, fence, completion: Some(completion), on_written: None })
            .await
            .map_err(|_| ClassicError::new("classic_transport", "endpoint is not running; command was not queued"))?;
        result.await.map_err(|_| outcome_unknown("endpoint stopped after the command was queued"))?
    }

    pub async fn send_input(&self, pane_id: &str, events: Vec<PaneInputEvent>) -> Result<(), ClassicError> {
        match self.execute(CommandKind::Input { pane_id: pane_id.into(), events }).await? {
            CommandOutcome::Input => Ok(()),
            _ => unreachable!("input command returned a request result"),
        }
    }

    pub async fn send_text(&self, pane_id: &str, text: String) -> Result<(), ClassicError> {
        self.send_input(pane_id, vec![PaneInputEvent::TextCommit(text)]).await
    }

    pub async fn send_paste(&self, pane_id: &str, text: String) -> Result<(), ClassicError> {
        self.send_input(pane_id, vec![PaneInputEvent::Paste(text)]).await
    }

    pub async fn send_key(&self, pane_id: &str, code: KeyCode, modifiers: u8) -> Result<(), ClassicError> {
        self.send_input(pane_id, vec![PaneInputEvent::key(code, modifiers)]).await
    }

    /// Performs one boot-fenced endpoint operation and waits for its correlated response.
    /// The response waiter is installed before the first byte is written, so a response
    /// can never arrive unobserved. A remote error is `classic_request_rejected`; a lost
    /// response (boot change, disconnect) is `classic_mutation_unknown`. The driving task
    /// also receives the response as `EndpointEvent::Response`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, ClassicError> {
        match self.execute(CommandKind::Request { method: method.into(), params, await_response: true }).await? {
            CommandOutcome::Response(value) => Ok(value),
            _ => unreachable!("awaited request returned a non-response result"),
        }
    }

    pub async fn focus_workspace(&self, workspace_id: &str) -> Result<Value, ClassicError> {
        self.request("workspace.focus", serde_json::json!({"workspace_id": workspace_id})).await
    }

    pub async fn focus_tab(&self, tab_id: &str) -> Result<Value, ClassicError> {
        self.request("tab.focus", serde_json::json!({"tab_id": tab_id})).await
    }

    pub async fn focus_pane(&self, pane_id: &str) -> Result<Value, ClassicError> {
        self.request("pane.focus", serde_json::json!({"pane_id": pane_id})).await
    }
}

/// One persistent endpoint connection. Read/write halves are injected so native
/// SSH, local fixtures and tests share exactly the same negotiated protocol.
///
/// Every error returned by `next_event` is fatal: the endpoint closes its transport,
/// retires unsent commands (`classic_stale_state`/`classic_transport`) and reports
/// commands with possibly transmitted bytes as `classic_mutation_unknown`.
pub struct ClassicEndpoint<R, W> {
    read: FrameReader<R>,
    write: FrameWriter<W>,
    welcome: ServerWelcome,
    snapshot: Option<ShellSnapshot>,
    surface: Option<PaneSurface>,
    images: HashMap<GraphicsAssetKey, Vec<u8>>,
    image_bytes: usize,
    next_request: u64,
    pending: Option<PendingRequest>,
    fences: Arc<Fences>,
    command_sender: mpsc::Sender<EndpointCommand>,
    commands: mpsc::Receiver<EndpointCommand>,
    active_command: Option<EndpointCommand>,
    /// Inbound frames applied since the last write turn (see `READ_BURST`).
    read_burst: usize,
    input_hold_timeout: Duration,
    /// Deadline of the active command's current `Readiness::Wait` hold.
    hold_deadline: Option<Instant>,
    closed: Option<ClassicError>,
    transport_shut_down: bool,
}

impl<R, W> ClassicEndpoint<R, W> {
    fn active_is_request(&self) -> bool {
        self.active_command.as_ref().is_some_and(|command| matches!(command.kind, CommandKind::Request { .. }))
    }

    /// Clears all observed state and advances both fences, so any queued command that was
    /// accepted against the old state is retired before transmission.
    fn clear_state(&mut self) {
        self.snapshot = None;
        self.surface = None;
        self.images.clear();
        self.image_bytes = 0;
        self.fences.boot.fetch_add(1, Ordering::SeqCst);
        self.fences.target.fetch_add(1, Ordering::SeqCst);
    }

    /// Retires the active command, the pending request and every queued command.
    /// Unsent commands get `unsent`; anything that may have reached the server gets
    /// `unknown`. The caller must close the transport if a frame was partially written.
    fn retire_all(&mut self, unsent: &ClassicError, unknown: &ClassicError) {
        let partial = self.write.has_written();
        let active_request_unsent = self.active_is_request() && !partial;
        if let Some(mut command) = self.active_command.take() {
            command.finish(Err(if partial { unknown.clone() } else { unsent.clone() }));
        }
        if !partial && self.write.is_pending() {
            self.write.discard_unsent();
        }
        if let Some(pending) = self.pending.take() {
            pending.finish(Err(if active_request_unsent { unsent.clone() } else { unknown.clone() }));
        }
        while let Ok(mut command) = self.commands.try_recv() {
            command.finish(Err(unsent.clone()));
        }
    }

    fn close(&mut self, error: &ClassicError) {
        if self.closed.is_some() {
            return;
        }
        self.closed = Some(error.clone());
        self.commands.close();
        self.retire_all(
            &ClassicError::new("classic_transport", format!("endpoint closed ({error}); command was not sent")),
            &outcome_unknown(&format!("endpoint closed ({error}) after the command may have been transmitted")),
        );
        self.clear_state();
    }

    /// Explicit reconnect/suspend invalidation. No old surface or image survives, input
    /// is disabled until a fresh snapshot/surface pair arrives, and every unsent queued or
    /// staged command is retired. If a command frame was partially transmitted it can be
    /// neither completed (stale target) nor truncated, so the endpoint closes and that
    /// command is reported `classic_mutation_unknown`.
    pub fn invalidate(&mut self) {
        if self.write.has_written() {
            self.close(&outcome_unknown("invalidated while a command frame was partially transmitted"));
            return;
        }
        let unsent = not_sent("endpoint state was invalidated");
        self.retire_all(&unsent, &outcome_unknown("endpoint state was invalidated while awaiting a response"));
        self.clear_state();
    }
}

impl<R, W> Drop for ClassicEndpoint<R, W> {
    fn drop(&mut self) {
        // Dropping the receiver alone would make queued-but-unsent commands look unknown.
        self.close(&ClassicError::new("classic_transport", "endpoint dropped"));
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> ClassicEndpoint<R, W> {
    pub async fn connect(read: R, mut write: W, hello: ClientHello) -> Result<(Self, ServerWelcome), ClassicError> {
        if hello.generation != GENERATION
            || hello.surface_reuse
            || hello.surface_delta
            || hello.surface_scroll
            || hello.direct_graphics
            || hello.surface_size.cols == 0
            || hello.surface_size.rows == 0
        {
            return Err(ClassicError::new(
                "unsupported_classic_capability",
                "hello requests an unimplemented endpoint mode",
            ));
        }
        let data = serde_json::to_string(&hello).map_err(|error| ClassicError::protocol(error.to_string()))?;
        write_frame(&mut write, &encode_control(HELLO_KIND, data)?).await?;
        let mut read = FrameReader::new(read);
        let welcome = loop {
            let frame = read.next().await?;
            match decode_message(&frame)? {
                WireMessage::Control { kind, data } if kind == WELCOME_KIND => {
                    break serde_json::from_str::<ServerWelcome>(&data)
                        .map_err(|error| ClassicError::protocol(format!("invalid endpoint welcome: {error}")))?;
                }
                WireMessage::Other(_) => continue,
                _ => return Err(ClassicError::protocol("expected endpoint welcome")),
            }
        };
        if let Some(error) = &welcome.error {
            return Err(ClassicError::new("classic_handshake_rejected", format!("{}: {}", error.code, error.message)));
        }
        if welcome.generation != GENERATION
            || welcome.snapshot_codec != SNAPSHOT_CODEC
            || welcome.surface_codec != SURFACE_CODEC
            || welcome.input_codec != INPUT_CODEC
            || welcome.blob_codec != BLOB_CODEC
        {
            return Err(ClassicError::new("classic_handshake_rejected", "no compatible generation-1 codecs"));
        }
        let (command_sender, commands) = mpsc::channel(COMMAND_QUEUE);
        let result = Self {
            read,
            write: FrameWriter::new(write),
            welcome: welcome.clone(),
            snapshot: None,
            surface: None,
            images: HashMap::new(),
            image_bytes: 0,
            next_request: 0,
            pending: None,
            fences: Arc::default(),
            command_sender,
            commands,
            active_command: None,
            read_burst: 0,
            input_hold_timeout: DEFAULT_INPUT_HOLD_TIMEOUT,
            hold_deadline: None,
            closed: None,
            transport_shut_down: false,
        };
        Ok((result, welcome))
    }

    pub fn welcome(&self) -> &ServerWelcome {
        &self.welcome
    }

    pub fn snapshot(&self) -> Option<&ShellSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn surface(&self) -> Option<&PaneSurface> {
        self.surface.as_ref()
    }

    /// Overrides `DEFAULT_INPUT_HOLD_TIMEOUT` for holds that start after this call.
    pub fn set_input_hold_timeout(&mut self, timeout: Duration) {
        self.input_hold_timeout = timeout;
    }

    pub fn command_sender(&self) -> ClassicEndpointHandle {
        ClassicEndpointHandle { commands: self.command_sender.clone(), fences: self.fences.clone() }
    }

    pub fn cached_image(&self, key: &GraphicsAssetKey) -> Option<&[u8]> {
        self.images.get(key).map(Vec::as_slice)
    }

    pub fn cached_image_bytes(&self) -> usize {
        self.image_bytes
    }

    /// Resolves every asset referenced by the current surface, including assets retained
    /// from earlier frames and therefore absent from `surface.graphics.assets`.
    pub fn resolved_assets(&self) -> Vec<GraphicsAsset> {
        let mut seen = HashSet::new();
        self.surface
            .iter()
            .flat_map(|surface| {
                surface
                    .graphics
                    .placements
                    .iter()
                    .map(|placement| &placement.asset)
                    .chain(surface.graphics.retained_assets.iter())
            })
            .filter(|key| seen.insert((*key).clone()))
            .filter_map(|key| self.images.get(key).map(|data| GraphicsAsset { key: key.clone(), data: data.clone() }))
            .collect()
    }

    pub fn input_ready(&self) -> bool {
        self.pending.is_none()
            && self.snapshot.as_ref().is_some_and(|snapshot| {
                self.surface.as_ref().is_some_and(|surface| {
                    surface.boot_id == snapshot.boot_id
                        && surface.projection_revision == snapshot.revision
                        && surface.popup.is_none()
                })
            })
    }

    /// Drives reads and queued writes concurrently. Both framing and write progress are
    /// persistent, so cancelling this future does not corrupt the next call. Every
    /// returned error is fatal and closes the endpoint.
    pub async fn next_event(&mut self) -> Result<EndpointEvent, ClassicError> {
        if self.closed.is_none() {
            match self.drive().await {
                Ok(event) => return Ok(event),
                Err(error) => self.close(&error),
            }
        }
        let error = self.closed.clone().expect("endpoint is closed");
        if !self.transport_shut_down {
            self.transport_shut_down = true;
            self.write.shutdown().await;
        }
        Err(error)
    }

    /// Fair read/write scheduling:
    /// 1. Complete frames already buffered in memory are always applied first.
    /// 2. Up to `READ_BURST` frames that the transport can deliver without waiting are
    ///    applied next (a non-blocking poll).
    /// 3. Then queued input gets a write turn: it is dequeued and validated against all
    ///    applied state, and its write is polled before the next read. If the write blocks,
    ///    reads continue, and every applied frame revalidates the command before any
    ///    further byte. Bytes still inside the transport are indistinguishable from bytes
    ///    in flight, so the write turn bounds how long sustained output can delay Ctrl-C.
    async fn drive(&mut self) -> Result<EndpointEvent, ClassicError> {
        loop {
            if let Some(bytes) = self.read.take()? {
                self.read_burst += 1;
                match self.process_frame(bytes)? {
                    Some(event) => return Ok(event),
                    None => continue,
                }
            }
            if self.read_burst < READ_BURST {
                let ready = tokio::select! {
                    biased;
                    bytes = self.read.next() => Some(bytes?),
                    () = std::future::ready(()) => None,
                };
                if let Some(bytes) = ready {
                    self.read_burst += 1;
                    match self.process_frame(bytes)? {
                        Some(event) => return Ok(event),
                        None => continue,
                    }
                }
            }
            // Write turn.
            self.read_burst = 0;
            if self.active_command.is_none()
                && let Ok(command) = self.commands.try_recv()
            {
                self.activate(command);
            }
            // Re-check the active command against every frame applied so far, before any
            // (further) byte is written.
            let writable = self.revalidate_active()?;
            if writable || self.active_command.is_none() {
                self.hold_deadline = None;
            }

            let bytes = if self.active_command.is_none() {
                tokio::select! {
                    biased;
                    bytes = self.read.next() => bytes?,
                    command = self.commands.recv() => {
                        // `command_sender` is retained by self and the queue is only closed
                        // together with the endpoint, so this cannot be None here.
                        self.activate(command.expect("open endpoint retains its command sender"));
                        continue;
                    }
                }
            } else if writable {
                tokio::select! {
                    biased;
                    result = self.write.flush() => {
                        if let Err(error) = result {
                            self.fail_active(&error);
                            return Err(error);
                        }
                        self.complete_active();
                        continue;
                    }
                    bytes = self.read.next() => bytes?,
                }
            } else {
                // Held until the full surface for the current snapshot arrives, but never
                // indefinitely. The deadline is polled first so a frame flood cannot defer it.
                let timeout = self.input_hold_timeout;
                let deadline = *self.hold_deadline.get_or_insert_with(|| Instant::now() + timeout);
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(deadline) => {
                        self.expire_hold()?;
                        continue;
                    }
                    bytes = self.read.next() => bytes?,
                }
            };
            self.read_burst += 1;
            if let Some(event) = self.process_frame(bytes)? {
                return Ok(event);
            }
        }
    }

    fn process_frame(&mut self, bytes: Vec<u8>) -> Result<Option<EndpointEvent>, ClassicError> {
        match decode_message(&bytes)? {
            WireMessage::Control { kind, data } if kind == SNAPSHOT_KIND => {
                let snapshot: ShellSnapshot = serde_json::from_str(&data)
                    .map_err(|error| ClassicError::protocol(format!("invalid shell snapshot: {error}")))?;
                self.apply_snapshot(snapshot.clone())?;
                Ok(Some(EndpointEvent::Snapshot(snapshot)))
            }
            WireMessage::Surface(surface) => {
                self.apply_surface(surface.clone(), bytes.len())?;
                Ok(Some(EndpointEvent::Surface(surface)))
            }
            WireMessage::Response { boot_id, request_id, final_chunk, data } => {
                Ok(self.apply_response(&boot_id, &request_id, final_chunk, data)?.map(EndpointEvent::Response))
            }
            WireMessage::Patch(patch) => {
                let current = self
                    .surface
                    .as_ref()
                    .ok_or_else(|| ClassicError::stale("surface patch without a committed full surface"))?;
                // Never continue on a guessed baseline: the error closes the endpoint and
                // drops state; the caller reconnects for a full surface.
                let next = apply_patch(current, &patch)?;
                self.commit_surface(next.clone());
                Ok(Some(EndpointEvent::Surface(next)))
            }
            WireMessage::Control { kind, .. } if UNSUPPORTED_SURFACE_CONTROLS.contains(&kind.as_str()) => {
                Err(ClassicError::new(
                    "unsupported_classic_capability",
                    format!("server selected unsupported surface codec {kind}; reconnect for full surfaces"),
                ))
            }
            WireMessage::Control { .. } | WireMessage::Other(_) => Ok(None),
        }
    }

    fn apply_snapshot(&mut self, snapshot: ShellSnapshot) -> Result<(), ClassicError> {
        if snapshot.boot_id.is_empty() || snapshot.boot_id.len() > 128 {
            return Err(ClassicError::protocol("snapshot has an invalid boot id"));
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| current.boot_id == snapshot.boot_id && snapshot.revision < current.revision)
        {
            return Err(ClassicError::stale("snapshot revision moved backwards"));
        }
        if self.snapshot.as_ref().is_some_and(|current| current.boot_id != snapshot.boot_id) {
            self.clear_state();
            // A transmitted request's response cannot cross a boot. An active request is
            // retired by `revalidate_active` (unsent, or fatal if partially written).
            if !self.active_is_request()
                && let Some(pending) = self.pending.take()
            {
                pending.finish(Err(outcome_unknown("server boot changed before the response arrived")));
            }
        }
        self.snapshot = Some(snapshot);
        Ok(())
    }

    fn apply_surface(&mut self, surface: PaneSurface, wire_bytes: usize) -> Result<(), ClassicError> {
        validate_surface(&surface, wire_bytes)?;
        let snapshot =
            self.snapshot.as_ref().ok_or_else(|| ClassicError::stale("surface arrived before a snapshot"))?;
        if surface.boot_id != snapshot.boot_id || surface.projection_revision > snapshot.revision {
            return Err(ClassicError::stale("surface does not belong to the current snapshot boot/projection"));
        }
        if self.surface.as_ref().is_some_and(|current| {
            current.boot_id == surface.boot_id && surface.surface_revision <= current.surface_revision
        }) {
            return Err(ClassicError::stale("full surface revision did not advance"));
        }

        let live: HashSet<_> = surface
            .graphics
            .placements
            .iter()
            .map(|placement| placement.asset.clone())
            .chain(surface.graphics.retained_assets.iter().cloned())
            .collect();
        for asset in &surface.graphics.assets {
            if self.images.get(&asset.key).is_some_and(|previous| previous != &asset.data) {
                return Err(ClassicError::protocol("same image key changed its bytes"));
            }
        }
        for key in &live {
            let data_len = self
                .images
                .get(key)
                .map(Vec::len)
                .or_else(|| {
                    surface.graphics.assets.iter().find(|asset| &asset.key == key).map(|asset| asset.data.len())
                })
                .ok_or_else(|| ClassicError::stale("surface references an unavailable image asset"))?;
            if data_len as u64 != key.data_len {
                return Err(ClassicError::protocol("cached image byte count does not match its key"));
            }
        }
        for placement in &surface.graphics.placements {
            let key = &placement.asset;
            let right = placement
                .source_x
                .checked_add(placement.source_width)
                .ok_or_else(|| ClassicError::protocol("graphics crop overflow"))?;
            let bottom = placement
                .source_y
                .checked_add(placement.source_height)
                .ok_or_else(|| ClassicError::protocol("graphics crop overflow"))?;
            if right > key.image_width || bottom > key.image_height {
                return Err(ClassicError::protocol("graphics crop exceeds cached source image"));
            }
        }
        let next_bytes = live.iter().try_fold(0usize, |total, key| {
            let len = self
                .images
                .get(key)
                .map(Vec::len)
                .or_else(|| {
                    surface.graphics.assets.iter().find(|asset| &asset.key == key).map(|asset| asset.data.len())
                })
                .expect("live assets checked above");
            total.checked_add(len).ok_or_else(|| ClassicError::new("classic_bound", "image cache size overflow"))
        })?;
        if next_bytes > MAX_IMAGE_CACHE_BYTES {
            return Err(ClassicError::new("classic_bound", "live image cache exceeds 32 MiB"));
        }

        // Prune obsolete data before charging new assets, and clone only live incoming images.
        self.images.retain(|key, _| live.contains(key));
        for asset in &surface.graphics.assets {
            if live.contains(&asset.key) && !self.images.contains_key(&asset.key) {
                self.images.insert(asset.key.clone(), asset.data.clone());
            }
        }
        self.image_bytes = next_bytes;
        self.commit_surface(surface);
        Ok(())
    }

    fn commit_surface(&mut self, surface: PaneSurface) {
        if self.surface.as_ref().is_none_or(|current| target_key(current) != target_key(&surface)) {
            self.fences.target.fetch_add(1, Ordering::SeqCst);
        }
        self.surface = Some(surface);
    }

    pub async fn send_text(&mut self, pane_id: &str, text: String) -> Result<(), ClassicError> {
        self.send_input(pane_id, vec![PaneInputEvent::TextCommit(text)]).await
    }

    pub async fn send_paste(&mut self, pane_id: &str, text: String) -> Result<(), ClassicError> {
        self.send_input(pane_id, vec![PaneInputEvent::Paste(text)]).await
    }

    pub async fn send_key(&mut self, pane_id: &str, code: KeyCode, modifiers: u8) -> Result<(), ClassicError> {
        self.send_input(pane_id, vec![PaneInputEvent::key(code, modifiers)]).await
    }

    pub async fn send_input(&mut self, pane_id: &str, events: Vec<PaneInputEvent>) -> Result<(), ClassicError> {
        match self.execute_local(CommandKind::Input { pane_id: pane_id.into(), events }).await? {
            CommandOutcome::Input => Ok(()),
            _ => unreachable!("input command returned a request result"),
        }
    }

    pub async fn focus_workspace(&mut self, workspace_id: &str) -> Result<String, ClassicError> {
        self.request("workspace.focus", serde_json::json!({"workspace_id": workspace_id})).await
    }

    pub async fn focus_tab(&mut self, tab_id: &str) -> Result<String, ClassicError> {
        self.request("tab.focus", serde_json::json!({"tab_id": tab_id})).await
    }

    pub async fn focus_pane(&mut self, pane_id: &str) -> Result<String, ClassicError> {
        self.request("pane.focus", serde_json::json!({"pane_id": pane_id})).await
    }

    /// Single-task variant: writes one boot-fenced request and returns its id. The
    /// response arrives through `next_event`; a lost response does not mean the operation
    /// did not run. Concurrent callers should use `ClassicEndpointHandle::request`, which
    /// awaits the correlated result.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<String, ClassicError> {
        let kind = CommandKind::Request { method: method.into(), params, await_response: false };
        match self.execute_local(kind).await? {
            CommandOutcome::Requested(id) => Ok(id),
            _ => unreachable!("request command returned an input result"),
        }
    }

    async fn execute_local(&mut self, kind: CommandKind) -> Result<CommandOutcome, ClassicError> {
        if let Some(error) = &self.closed {
            return Err(ClassicError::new("classic_transport", format!("endpoint closed ({error}); not sent")));
        }
        if self.active_command.is_some() {
            return Err(ClassicError::new("classic_endpoint_busy", "another endpoint command is being written"));
        }
        check_command_bounds(&kind)?;
        let fence = self.fence_for(&kind);
        let (completion, result) = oneshot::channel();
        self.activate(EndpointCommand { kind, fence, completion: Some(completion), on_written: None });
        if self.active_command.is_some() && !self.revalidate_active()? {
            // No frame is read during a single-task send, so a held command cannot become ready.
            self.retire_active(not_sent("input target is awaiting the full surface for its snapshot"));
        }
        if self.active_command.is_some() {
            // No frame is read while this flush runs, so state cannot change underneath it.
            if let Err(error) = self.write.flush().await {
                self.fail_active(&error);
                self.close(&error);
                return Err(error);
            }
            self.complete_active();
        }
        result.await.map_err(|_| ClassicError::new("classic_transport", "local command completion was dropped"))?
    }

    fn fence_for(&self, kind: &CommandKind) -> u64 {
        match kind {
            CommandKind::Input { .. } => self.fences.target.load(Ordering::SeqCst),
            CommandKind::Request { .. } => self.fences.boot.load(Ordering::SeqCst),
        }
    }

    /// Validates a command against the current state: its enqueue-time fence and, for
    /// input, snapshot/surface coherence and target visibility.
    fn validate(&self, command: &EndpointCommand) -> Readiness {
        if command.fence != self.fence_for(&command.kind) {
            return Readiness::Stale(not_sent(
                "input target or boot changed (navigation, popup, reboot or invalidation) after queueing",
            ));
        }
        match &command.kind {
            CommandKind::Input { pane_id, .. } => self.check_input(pane_id),
            CommandKind::Request { .. } if self.snapshot.is_none() => {
                Readiness::Stale(not_sent("endpoint request requires a fresh boot snapshot"))
            }
            CommandKind::Request { .. } => Readiness::Ready,
        }
    }

    fn check_input(&self, pane_id: &str) -> Readiness {
        if self.input_ready() {
            let visible =
                self.surface.as_ref().is_some_and(|surface| surface.panes.iter().any(|pane| pane.pane_id == pane_id));
            if !visible {
                return Readiness::Stale(not_sent(
                    "semantic input target is not visible in this connection's selected tab",
                ));
            }
            // This is a local readiness check, not an atomic server-side target-incarnation fence:
            // classic semantic input carries only pane_id and can still race target replacement.
            return Readiness::Ready;
        }
        let (Some(snapshot), Some(surface)) = (&self.snapshot, &self.surface) else {
            return Readiness::Stale(not_sent("input requires a fresh snapshot and full surface"));
        };
        if surface.popup.is_some() {
            return Readiness::Stale(not_sent("a blocking popup is present; pane input is unsupported"));
        }
        if self.pending.is_some() {
            return Readiness::Stale(not_sent("input is not sent while an endpoint request is in flight"));
        }
        // Only remaining gap: the same boot's snapshot is ahead of its surface. The target
        // fence is unchanged (it advances only when a surface changes the target), so the
        // command waits; a target-changing surface then retires it through the fence.
        let snapshot_ahead = surface.boot_id == snapshot.boot_id && surface.projection_revision < snapshot.revision;
        if !snapshot_ahead {
            Readiness::Stale(not_sent("input requires a fresh snapshot and full surface"))
        } else if surface.panes.iter().any(|pane| pane.pane_id == pane_id) {
            Readiness::Wait
        } else {
            Readiness::Stale(not_sent("semantic input target is not visible in this connection's selected tab"))
        }
    }

    /// Stages a dequeued command, or completes it with an error without sending.
    fn activate(&mut self, mut command: EndpointCommand) {
        if let Readiness::Stale(error) = self.validate(&command) {
            command.finish(Err(error));
            return;
        }
        let staged = match &command.kind {
            CommandKind::Input { pane_id, events } => {
                encode_input(pane_id, events.clone()).map(|payload| (payload, Some(CommandOutcome::Input), None))
            }
            CommandKind::Request { method, params, await_response } => {
                self.stage_request(method, params).map(|(id, payload)| {
                    if *await_response {
                        (payload, None, Some(id))
                    } else {
                        (payload, Some(CommandOutcome::Requested(id)), None)
                    }
                })
            }
        };
        let (payload, on_written, awaited_id) = match staged {
            Ok(staged) => staged,
            Err(error) => {
                command.finish(Err(error));
                return;
            }
        };
        if let Err(error) = self.write.stage(&payload) {
            if matches!(command.kind, CommandKind::Request { .. }) {
                self.pending = None;
            }
            command.finish(Err(error));
            return;
        }
        if awaited_id.is_some() {
            // Install the response waiter before the first byte can be written.
            self.pending.as_mut().expect("request staged its pending entry").waiter = command.completion.take();
        }
        command.on_written = on_written;
        self.active_command = Some(command);
        self.hold_deadline = None;
    }

    /// Re-checks the active command before any further byte is written and returns whether
    /// it may write now. A held command (`Readiness::Wait`) keeps its staged or partially
    /// written frame. An unsent stale command is retired; a partially written stale frame
    /// is fatal with unknown outcome.
    fn revalidate_active(&mut self) -> Result<bool, ClassicError> {
        let Some(command) = &self.active_command else { return Ok(false) };
        let error = match self.validate(command) {
            Readiness::Ready => return Ok(true),
            Readiness::Wait => return Ok(false),
            Readiness::Stale(error) => error,
        };
        if self.write.has_written() {
            return Err(outcome_unknown(&format!(
                "target changed while a command frame was partially transmitted ({}); connection closed",
                error.message
            )));
        }
        self.retire_active(error);
        Ok(false)
    }

    /// The surface for the held command's snapshot did not arrive in time. Unsent input is
    /// retired and the connection stays open; a partially written frame can be neither
    /// completed nor truncated, so it is fatal with an unknown outcome.
    fn expire_hold(&mut self) -> Result<(), ClassicError> {
        self.hold_deadline = None;
        let reason =
            format!("the full surface for the current snapshot did not arrive within {:?}", self.input_hold_timeout);
        if self.write.has_written() {
            return Err(outcome_unknown(&format!(
                "{reason} while a command frame was partially transmitted; connection closed"
            )));
        }
        self.retire_active(not_sent(&reason));
        Ok(())
    }

    /// Retires the active command, of which no byte was transmitted.
    fn retire_active(&mut self, error: ClassicError) {
        self.write.discard_unsent();
        let mut command = self.active_command.take().expect("retired active command");
        if matches!(command.kind, CommandKind::Request { .. })
            && let Some(pending) = self.pending.take()
        {
            pending.finish(Err(error.clone()));
        }
        command.finish(Err(error));
    }

    fn complete_active(&mut self) {
        let mut command = self.active_command.take().expect("completed active command");
        if let Some(outcome) = command.on_written.take() {
            command.finish(Ok(outcome));
        }
    }

    fn fail_active(&mut self, error: &ClassicError) {
        if let Some(mut command) = self.active_command.take() {
            if matches!(command.kind, CommandKind::Request { .. })
                && let Some(pending) = self.pending.take()
            {
                pending.finish(Err(error.clone()));
            }
            command.finish(Err(error.clone()));
        }
    }

    fn stage_request(&mut self, method: &str, params: &Value) -> Result<(String, Vec<u8>), ClassicError> {
        if self.pending.is_some() {
            return Err(ClassicError::new("classic_endpoint_busy", "another endpoint operation is in flight"));
        }
        if !self.welcome.methods.iter().any(|allowed| allowed == method) {
            return Err(ClassicError::new(
                "unsupported_classic_capability",
                format!("endpoint did not negotiate {method}"),
            ));
        }
        let boot_id = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.boot_id.clone())
            .ok_or_else(|| not_sent("endpoint request requires a fresh boot snapshot"))?;
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or_else(|| ClassicError::new("classic_bound", "request id space exhausted"))?;
        let id = format!("hydra-{}", self.next_request);
        let request = request_json(&id, method, params.clone())?;
        let payload = encode_request(&boot_id, request)?;
        // Establish correlation before the first await/write. A response can never find
        // an unstaged request, and a partial write has an explicitly unknown outcome.
        self.pending = Some(PendingRequest { boot_id, request_id: id.clone(), bytes: Vec::new(), waiter: None });
        Ok((id, payload))
    }

    fn apply_response(
        &mut self,
        boot_id: &str,
        request_id: &str,
        final_chunk: bool,
        data: Vec<u8>,
    ) -> Result<Option<EndpointResponse>, ClassicError> {
        if request_id.len() > 128 || boot_id.len() > 128 {
            return Err(ClassicError::new("classic_bound", "endpoint response identity is too long"));
        }
        let Some(pending) = self.pending.as_mut().filter(|pending| pending.request_id == request_id) else {
            // A late response from an explicitly retired request cannot mutate new state.
            return Ok(None);
        };
        if pending.boot_id != boot_id {
            return Err(ClassicError::protocol("uncorrelated endpoint response"));
        }
        if pending.bytes.len().saturating_add(data.len()) > MAX_COMMAND_BYTES {
            return Err(ClassicError::new("classic_bound", "endpoint response exceeds 1 MiB"));
        }
        pending.bytes.extend(data);
        if !final_chunk {
            return Ok(None);
        }
        let mut pending = self.pending.take().expect("final pending response");
        let value: Value = serde_json::from_slice(&pending.bytes)
            .map_err(|error| ClassicError::protocol(format!("invalid endpoint JSON response: {error}")))?;
        if value.get("id").and_then(Value::as_str) != Some(request_id) {
            return Err(ClassicError::protocol("endpoint JSON response id does not match"));
        }
        let result = if let Some(error) = value.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| ClassicError::protocol("endpoint error lacks a code"))?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| ClassicError::protocol("endpoint error lacks a message"))?;
            Err(HandshakeError { code: code.into(), message: message.into() })
        } else {
            Ok(value
                .get("result")
                .cloned()
                .ok_or_else(|| ClassicError::protocol("endpoint response lacks a result"))?)
        };
        if let Some(waiter) = pending.waiter.take() {
            let _ = waiter.send(match &result {
                Ok(value) => Ok(CommandOutcome::Response(value.clone())),
                Err(error) => {
                    Err(ClassicError::new("classic_request_rejected", format!("{}: {}", error.code, error.message)))
                }
            });
        }
        Ok(Some(EndpointResponse { request_id: request_id.into(), result }))
    }
}

#[cfg(test)]
mod tests;
