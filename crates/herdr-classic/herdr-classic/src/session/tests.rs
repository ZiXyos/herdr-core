use super::*;
use crate::framing::frame_bytes;
use tokio::io::{AsyncWriteExt, duplex};

type TestClient = ClassicEndpoint<tokio::io::DuplexStream, tokio::io::DuplexStream>;
type TestReader = FrameReader<tokio::io::DuplexStream>;

async fn connected() -> (TestClient, TestReader, tokio::io::DuplexStream) {
    connected_with(4096, 4096).await
}

/// `write_capacity` bounds the client->server pipe, so tests can block a command
/// frame either before its first byte or part-way through it.
async fn connected_with(
    read_capacity: usize,
    write_capacity: usize,
) -> (TestClient, TestReader, tokio::io::DuplexStream) {
    let (client_read, mut server_write) = duplex(read_capacity);
    let (client_write, server_read) = duplex(write_capacity);
    let task = tokio::spawn(async move {
        let mut frames = FrameReader::new(server_read);
        let hello = frames.next().await.unwrap();
        let server_read = frames;
        let WireMessage::Control { kind, .. } = decode_message(&hello).unwrap() else { panic!("hello") };
        assert_eq!(kind, HELLO_KIND);
        let welcome = ServerWelcome {
            generation: 1,
            server_version: "0.9.5".into(),
            snapshot_codec: SNAPSHOT_CODEC.into(),
            surface_codec: SURFACE_CODEC.into(),
            input_codec: INPUT_CODEC.into(),
            blob_codec: BLOB_CODEC.into(),
            methods: vec!["tab.focus".into()],
            capabilities: vec![],
            error: None,
        };
        let data = serde_json::to_string(&welcome).unwrap();
        write_frame(&mut server_write, &encode_control(WELCOME_KIND, data).unwrap()).await.unwrap();
        (server_read, server_write)
    });
    let (client, _) = ClassicEndpoint::connect(client_read, client_write, ClientHello::mobile(80, 24)).await.unwrap();
    let (server_read, server_write) = task.await.unwrap();
    (client, server_read, server_write)
}

fn snapshot() -> ShellSnapshot {
    ShellSnapshot {
        boot_id: "boot-a".into(),
        revision: 1,
        focused_workspace_id: None,
        focused_tab_id: None,
        focused_pane_id: None,
        workspaces: vec![],
        tabs: vec![],
        panes: vec![Pane {
            pane_id: "p".into(),
            workspace_id: "w".into(),
            tab_id: "t".into(),
            label: None,
            cwd: None,
            focused: true,
        }],
    }
}

fn ready_surface() -> PaneSurface {
    let mut surface = crate::protocol::tests::surface();
    surface.panes.push(SurfacePane {
        pane_id: "p".into(),
        content_revision: 1,
        rect: Rect { x: 0, y: 0, width: 2, height: 2 },
        inner_rect: Rect { x: 0, y: 0, width: 2, height: 2 },
        scrollbar_rect: None,
        scroll: None,
        focused: true,
        mouse_reporting: false,
        sgr_pixel_mouse: false,
        alternate_screen_active: false,
        pixel_width: 16,
        pixel_height: 32,
    });
    surface
}

async fn send_snapshot(server: &mut tokio::io::DuplexStream, snapshot: &ShellSnapshot) {
    let data = serde_json::to_string(snapshot).unwrap();
    write_frame(server, &encode_control(SNAPSHOT_KIND, data).unwrap()).await.unwrap();
}

async fn make_ready(client: &mut TestClient, server: &mut tokio::io::DuplexStream) {
    send_snapshot(server, &snapshot()).await;
    client.next_event().await.unwrap();
    write_frame(server, &crate::protocol::tests::encode_surface(&ready_surface())).await.unwrap();
    client.next_event().await.unwrap();
    assert!(client.input_ready());
}

#[tokio::test]
async fn snapshot_then_wide_surface_enables_only_visible_semantic_input() {
    let (mut client, mut server_read, mut server) = connected().await;
    make_ready(&mut client, &mut server).await;
    client.send_text("missing", "no".into()).await.unwrap_err();
    client.send_text("p", "yes".into()).await.unwrap();
    let sent = server_read.next().await.unwrap();
    assert_eq!(sent[0], 13);
    assert!(client.surface().unwrap().frame.cells.iter().any(|cell| cell.symbol.is_empty() && !cell.skip));

    let patch = PanePatch {
        boot_id: "boot-a".into(),
        projection_revision: 1,
        base_surface_revision: 1,
        surface_revision: 2,
        rows: vec![PatchRow { x: 0, y: 1, cells: vec![client.surface().unwrap().frame.cells[0].clone()] }],
        panes: vec![],
        cursor: None,
    };
    write_frame(&mut server, &crate::protocol::tests::encode_patch(&patch)).await.unwrap();
    let EndpointEvent::Surface(patched) = client.next_event().await.unwrap() else { panic!("patch") };
    assert_eq!(patched.surface_revision, 2);
    assert_eq!(patched.frame.cells[2].symbol, "界");
    // Replaying the same base is a baseline mismatch: state is dropped, input disabled.
    write_frame(&mut server, &crate::protocol::tests::encode_patch(&patch)).await.unwrap();
    assert_eq!(client.next_event().await.unwrap_err().kind, "classic_stale_state");
    assert!(client.surface().is_none() && !client.input_ready());
}

#[tokio::test]
async fn commands_flush_while_a_fragmented_read_is_pending() {
    let (mut client, mut server_read, mut server) = connected().await;
    make_ready(&mut client, &mut server).await;
    let snapshot_payload = encode_control(SNAPSHOT_KIND, serde_json::to_string(&snapshot()).unwrap()).unwrap();
    let snapshot_frame = frame_bytes(&snapshot_payload).unwrap();
    server.write_all(&snapshot_frame[..3]).await.unwrap();

    let handle = client.command_sender();
    let command = tokio::spawn(async move { handle.send_text("p", "from-command".into()).await });
    let event = tokio::spawn(async move {
        client.next_event().await.unwrap();
        client
    });
    let sent = tokio::time::timeout(std::time::Duration::from_secs(1), server_read.next()).await.unwrap().unwrap();
    assert_eq!(sent[0], 13);
    server.write_all(&snapshot_frame[3..]).await.unwrap();
    client = event.await.unwrap();
    command.await.unwrap().unwrap();
    assert!(client.input_ready());
}

#[tokio::test]
async fn request_is_pending_before_a_blocked_write() {
    let (mut client, _server_read, mut server) = connected_with(1, 1).await;
    let snapshot_task = tokio::spawn(async move {
        send_snapshot(&mut server, &snapshot()).await;
        server
    });
    client.next_event().await.unwrap();
    let _server = snapshot_task.await.unwrap();
    let handle = client.command_sender();
    let request = tokio::spawn(async move { handle.focus_tab("t").await });
    assert!(tokio::time::timeout(std::time::Duration::from_millis(30), client.next_event()).await.is_err());
    // The response waiter exists before any byte is written: a response can never
    // arrive ahead of its waiter.
    assert!(client.pending.as_ref().is_some_and(|pending| pending.waiter.is_some()));
    drop(request);
}

#[tokio::test]
async fn popup_blocks_pane_input_instead_of_silently_dropping_it() {
    let (mut client, _server_read, mut server) = connected().await;
    make_ready(&mut client, &mut server).await;
    let mut popup = ready_surface();
    popup.surface_revision = 2;
    popup.popup = Some(Box::new(PopupSurface {
        terminal_id: "popup".into(),
        title: "Blocking prompt".into(),
        width: None,
        height: None,
        frame: Frame { cells: vec![], width: 0, height: 0, cursor: None, hyperlinks: vec![], graphics: vec![] },
        mouse_reporting: false,
        sgr_pixel_mouse: false,
        pixel_width: 0,
        pixel_height: 0,
    }));
    write_frame(&mut server, &crate::protocol::tests::encode_surface(&popup)).await.unwrap();
    client.next_event().await.unwrap();
    assert!(!client.input_ready());
    let error = client.send_text("p", "must not send".into()).await.unwrap_err();
    assert!(error.message.contains("blocking popup"));
}

#[tokio::test]
async fn unsupported_surface_codec_control_is_fatal() {
    let (mut client, _server_read, mut server) = connected().await;
    write_frame(&mut server, &encode_control(UNSUPPORTED_SURFACE_CONTROLS[0], "{}".into()).unwrap()).await.unwrap();
    let error = client.next_event().await.unwrap_err();
    assert_eq!(error.kind, "unsupported_classic_capability");
    assert!(client.surface().is_none());
}

#[tokio::test]
async fn new_boot_invalidates_surface_images_and_pending_input() {
    let (mut client, _server_read, mut server) = connected().await;
    send_snapshot(&mut server, &snapshot()).await;
    client.next_event().await.unwrap();
    let mut reboot = snapshot();
    reboot.boot_id = "boot-b".into();
    send_snapshot(&mut server, &reboot).await;
    client.next_event().await.unwrap();
    assert!(!client.input_ready());
    assert_eq!(client.cached_image_bytes(), 0);
    assert!(client.surface().is_none());
}

fn image(id: &str) -> GraphicsAsset {
    let pixels = 2_000 * 2_000;
    let data = vec![7; pixels * 3];
    GraphicsAsset {
        key: GraphicsAssetKey {
            source: GraphicsSource::PaneLayer { pane_id: "p".into(), layer_id: id.into() },
            image_width: 2_000,
            image_height: 2_000,
            format: GraphicsFormat::Rgb,
            data_len: data.len() as u64,
            data_fingerprint: 1,
        },
        data,
    }
}

fn image_surface(revision: u64, assets: Vec<GraphicsAsset>, retained: Vec<GraphicsAssetKey>) -> PaneSurface {
    let mut surface = crate::protocol::tests::surface();
    surface.surface_revision = revision;
    surface.graphics = GraphicsScene { assets, placements: vec![], retained_assets: retained };
    surface
}

#[tokio::test]
async fn cache_prunes_before_cap_and_resolves_retained_assets() {
    let (mut client, _server_read, mut server) = connected().await;
    send_snapshot(&mut server, &snapshot()).await;
    client.next_event().await.unwrap();
    let a = image("a");
    let b = image("b");
    let first = image_surface(1, vec![a.clone(), b.clone()], vec![a.key.clone(), b.key.clone()]);
    client.apply_surface(first, 100).unwrap();
    assert_eq!(client.cached_image_bytes(), a.data.len() + b.data.len());

    let c = image("c");
    let second = image_surface(2, vec![c.clone()], vec![a.key.clone(), c.key.clone()]);
    client.apply_surface(second, 100).unwrap();
    assert_eq!(client.cached_image_bytes(), a.data.len() + c.data.len());
    assert!(client.cached_image(&b.key).is_none());
    let resolved = client.resolved_assets();
    assert_eq!(resolved.len(), 2);
    assert!(resolved.iter().any(|asset| asset.key == a.key && asset.data == a.data));
    assert!(resolved.iter().any(|asset| asset.key == c.key && asset.data == c.data));

    client.invalidate();
    assert!(client.resolved_assets().is_empty());
    assert_eq!(client.cached_image_bytes(), 0);
}

#[tokio::test]
async fn chunked_responses_are_correlated_and_bounded() {
    let (mut client, _server_read, mut server) = connected().await;
    send_snapshot(&mut server, &snapshot()).await;
    client.next_event().await.unwrap();
    let id = client.focus_tab("t").await.unwrap();
    let body = serde_json::to_vec(&serde_json::json!({"id":id,"result":{"type":"tab_focused"}})).unwrap();
    let split = body.len() / 2;
    for (final_chunk, data) in [(false, body[..split].to_vec()), (true, body[split..].to_vec())] {
        let payload = crate::protocol::tests::encode_response("boot-a", &id, final_chunk, data);
        write_frame(&mut server, &payload).await.unwrap();
    }
    let EndpointEvent::Response(response) = client.next_event().await.unwrap() else { panic!("response") };
    assert_eq!(response.request_id, id);
    assert!(response.result.is_ok());
}

const QUIET: std::time::Duration = std::time::Duration::from_millis(30);

fn text_frame_len(pane_id: &str, text: &str) -> usize {
    frame_bytes(&encode_input(pane_id, vec![PaneInputEvent::TextCommit(text.into())]).unwrap()).unwrap().len()
}

fn popup_surface(revision: u64) -> PaneSurface {
    let mut popup = ready_surface();
    popup.surface_revision = revision;
    popup.popup = Some(Box::new(PopupSurface {
        terminal_id: "popup".into(),
        title: "Blocking prompt".into(),
        width: None,
        height: None,
        frame: Frame { cells: vec![], width: 0, height: 0, cursor: None, hyperlinks: vec![], graphics: vec![] },
        mouse_reporting: false,
        sgr_pixel_mouse: false,
        pixel_width: 0,
        pixel_height: 0,
    }));
    popup
}

/// Drives the endpoint until it is idle (no event within QUIET).
async fn settle(client: &mut TestClient) {
    assert!(tokio::time::timeout(QUIET, client.next_event()).await.is_err(), "unexpected endpoint event");
}

fn spawn_text(handle: &ClassicEndpointHandle, text: &str) -> tokio::task::JoinHandle<Result<(), ClassicError>> {
    let handle = handle.clone();
    let text = text.to_owned();
    tokio::spawn(async move { handle.send_text("p", text).await })
}

/// With the pipe sized to exactly one "fill" frame, the next command blocks before
/// its first byte is transmitted.
async fn fill_pipe(client: &mut TestClient, handle: &ClassicEndpointHandle) {
    let fill = spawn_text(handle, "fill");
    settle(client).await;
    fill.await.unwrap().unwrap();
}

async fn assert_only_fill_was_sent(server_read: &mut TestReader) {
    let fill = server_read.next().await.unwrap();
    assert_eq!(fill, encode_input("p", vec![PaneInputEvent::TextCommit("fill".into())]).unwrap());
    assert!(tokio::time::timeout(QUIET, server_read.next()).await.is_err(), "stale command reached the server");
}

#[tokio::test]
async fn blocked_unsent_input_is_retired_when_a_popup_arrives() {
    let (mut client, mut server_read, mut server) = connected_with(4096, text_frame_len("p", "fill")).await;
    make_ready(&mut client, &mut server).await;
    let handle = client.command_sender();
    fill_pipe(&mut client, &handle).await;
    let stale = spawn_text(&handle, "stale");
    settle(&mut client).await;
    assert!(client.write.is_pending() && !client.write.has_written(), "command must block before byte one");

    write_frame(&mut server, &crate::protocol::tests::encode_surface(&popup_surface(2))).await.unwrap();
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Surface(_)));
    settle(&mut client).await;
    let error = stale.await.unwrap().unwrap_err();
    assert_eq!(error.kind, "classic_stale_state");
    assert!(error.message.contains("not sent"), "{error}");
    assert!(client.closed.is_none() && !client.write.is_pending());
    assert_only_fill_was_sent(&mut server_read).await;
}

#[tokio::test]
async fn queued_input_is_fenced_by_its_enqueue_time_target() {
    let (mut client, mut server_read, mut server) = connected().await;
    make_ready(&mut client, &mut server).await;
    let handle = client.command_sender();
    // Typed while the target was ready; a popup opens and closes before it is written.
    let stale = spawn_text(&handle, "typed-before-popup");
    tokio::time::sleep(QUIET).await;
    write_frame(&mut server, &crate::protocol::tests::encode_surface(&popup_surface(2))).await.unwrap();
    let mut closed = ready_surface();
    closed.surface_revision = 3;
    write_frame(&mut server, &crate::protocol::tests::encode_surface(&closed)).await.unwrap();
    client.next_event().await.unwrap();
    client.next_event().await.unwrap();
    assert!(client.input_ready());
    // Both frames are applied before the queued command gets its write turn.
    settle(&mut client).await;
    assert_eq!(stale.await.unwrap().unwrap_err().kind, "classic_stale_state");
    assert!(tokio::time::timeout(QUIET, server_read.next()).await.is_err(), "stale input reached the server");

    // Input queued against the current target is still delivered.
    let fresh = spawn_text(&handle, "fresh");
    settle(&mut client).await;
    fresh.await.unwrap().unwrap();
    assert_eq!(
        server_read.next().await.unwrap(),
        encode_input("p", vec![PaneInputEvent::TextCommit("fresh".into())]).unwrap()
    );
}

#[tokio::test]
async fn partially_written_stale_frame_closes_with_unknown_outcome() {
    let (mut client, mut server_read, mut server) = connected_with(4096, 16).await;
    make_ready(&mut client, &mut server).await;
    let handle = client.command_sender();
    let command = spawn_text(&handle, &"x".repeat(200));
    settle(&mut client).await;
    assert!(client.write.has_written());

    write_frame(&mut server, &crate::protocol::tests::encode_surface(&popup_surface(2))).await.unwrap();
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Surface(_)));
    assert_eq!(client.next_event().await.unwrap_err().kind, "classic_mutation_unknown");
    assert_eq!(command.await.unwrap().unwrap_err().kind, "classic_mutation_unknown");
    // Closed for good: no truncated frame followed by more traffic, no new commands.
    assert_eq!(client.next_event().await.unwrap_err().kind, "classic_mutation_unknown");
    assert_eq!(handle.send_text("p", "after".into()).await.unwrap_err().kind, "classic_transport");
    // The server sees the truncated prefix and then EOF, never a completed stale frame.
    assert_eq!(server_read.next().await.unwrap_err().kind, "classic_transport");
}

#[tokio::test]
async fn explicit_invalidation_retires_active_and_queued_unsent_commands() {
    let (mut client, mut server_read, mut server) = connected_with(4096, text_frame_len("p", "fill")).await;
    make_ready(&mut client, &mut server).await;
    let handle = client.command_sender();
    fill_pipe(&mut client, &handle).await;
    let active = spawn_text(&handle, "active");
    settle(&mut client).await;
    let queued = spawn_text(&handle, "queued");
    tokio::time::sleep(QUIET).await;

    // Suspend/reconnect invalidation on a connection with no partially sent frame.
    client.invalidate();
    for command in [active, queued] {
        let error = command.await.unwrap().unwrap_err();
        assert_eq!(error.kind, "classic_stale_state");
        assert!(error.message.contains("not sent"), "{error}");
    }
    assert!(client.closed.is_none() && !client.input_ready());
    assert_only_fill_was_sent(&mut server_read).await;
}

#[tokio::test]
async fn invalidation_during_a_partial_frame_closes_with_unknown_outcome() {
    let (mut client, _server_read, mut server) = connected_with(4096, 16).await;
    make_ready(&mut client, &mut server).await;
    let handle = client.command_sender();
    let command = spawn_text(&handle, &"x".repeat(200));
    settle(&mut client).await;
    assert!(client.write.has_written());
    client.invalidate();
    assert_eq!(command.await.unwrap().unwrap_err().kind, "classic_mutation_unknown");
    assert_eq!(client.next_event().await.unwrap_err().kind, "classic_mutation_unknown");
}

#[tokio::test]
async fn boot_change_retires_an_unsent_request_and_loses_a_sent_one() {
    let (mut client, mut server_read, mut server) = connected_with(4096, text_frame_len("p", "fill")).await;
    make_ready(&mut client, &mut server).await;
    let handle = client.command_sender();
    fill_pipe(&mut client, &handle).await;
    let blocked = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.focus_tab("t").await })
    };
    settle(&mut client).await;
    assert!(client.pending.is_some() && !client.write.has_written());
    let mut reboot = snapshot();
    reboot.boot_id = "boot-b".into();
    send_snapshot(&mut server, &reboot).await;
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Snapshot(_)));
    settle(&mut client).await;
    let error = blocked.await.unwrap().unwrap_err();
    assert_eq!(error.kind, "classic_stale_state");
    assert!(error.message.contains("not sent"), "{error}");
    assert!(client.pending.is_none());
    assert_only_fill_was_sent(&mut server_read).await;

    // A fully transmitted request whose response is lost to a reboot is unknown.
    let sent = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.focus_tab("t").await })
    };
    // The request frame is larger than the tiny pipe; the server reads concurrently.
    let received = tokio::spawn(async move { server_read.next().await.unwrap()[0] });
    settle(&mut client).await;
    assert_eq!(received.await.unwrap(), 15);
    let mut reboot = snapshot();
    reboot.boot_id = "boot-c".into();
    send_snapshot(&mut server, &reboot).await;
    client.next_event().await.unwrap();
    assert_eq!(sent.await.unwrap().unwrap_err().kind, "classic_mutation_unknown");
}

#[tokio::test]
async fn awaited_requests_return_remote_results_and_propagate_rejection() {
    let (mut client, mut server_read, mut server) = connected().await;
    send_snapshot(&mut server, &snapshot()).await;
    client.next_event().await.unwrap();
    let handle = client.command_sender();
    let replies = [
        serde_json::json!({"id":"hydra-1","result":{"type":"tab_focused"}}),
        serde_json::json!({"id":"hydra-2","error":{"code":"tab_not_found","message":"no such tab"}}),
    ];
    let mut results = vec![];
    for (index, reply) in replies.iter().enumerate() {
        let request = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.focus_tab("t").await })
        };
        settle(&mut client).await;
        assert_eq!(server_read.next().await.unwrap()[0], 15);
        let id = format!("hydra-{}", index + 1);
        let payload = crate::protocol::tests::encode_response("boot-a", &id, true, serde_json::to_vec(reply).unwrap());
        write_frame(&mut server, &payload).await.unwrap();
        let EndpointEvent::Response(response) = client.next_event().await.unwrap() else { panic!("response") };
        assert_eq!(response.request_id, id);
        results.push(request.await.unwrap());
    }
    assert_eq!(results[0].as_ref().unwrap()["type"], "tab_focused");
    let rejected = results[1].as_ref().unwrap_err();
    assert_eq!(rejected.kind, "classic_request_rejected");
    assert!(rejected.message.contains("tab_not_found"));
}

#[tokio::test]
async fn command_bounds_are_enforced_before_enqueue() {
    let (client, _server_read, _server) = connected().await;
    // Nothing drives the endpoint: an accepted command would wait forever.
    let handle = client.command_sender();
    let oversized = handle.send_text("p", "x".repeat(MAX_INPUT_TEXT_BYTES + 1));
    let error = tokio::time::timeout(QUIET, oversized).await.expect("rejected without queueing").unwrap_err();
    assert_eq!(error.kind, "classic_bound");
    let keys = vec![PaneInputEvent::key(KeyCode::Enter, 0); MAX_INPUT_EVENTS + 1];
    let error = tokio::time::timeout(QUIET, handle.send_input("p", keys)).await.unwrap().unwrap_err();
    assert_eq!(error.kind, "classic_bound");
    let params = serde_json::json!({"tab_id": "x".repeat(MAX_REQUEST_PARAMS_BYTES)});
    let error = tokio::time::timeout(QUIET, handle.request("tab.focus", params)).await.unwrap().unwrap_err();
    assert_eq!(error.kind, "classic_bound");
}

#[tokio::test]
async fn dropping_the_endpoint_reports_queued_commands_as_not_sent() {
    let (client, _server_read, _server) = connected().await;
    let handle = client.command_sender();
    let queued = spawn_text(&handle, "never-sent");
    tokio::time::sleep(QUIET).await;
    drop(client);
    let error = queued.await.unwrap().unwrap_err();
    assert_eq!(error.kind, "classic_transport");
    assert!(error.message.contains("not sent"), "{error}");
}

fn surface_at(projection: u64, revision: u64) -> PaneSurface {
    let mut surface = ready_surface();
    surface.projection_revision = projection;
    surface.surface_revision = revision;
    surface
}

fn snapshot_at(revision: u64) -> ShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.revision = revision;
    snapshot
}

/// Regression for read starvation: the server floods full surfaces faster than a slow
/// consumer (200 us of work per event) drains them. Queued Ctrl-C must still be written
/// after a bounded number of applied frames, and only against the latest applied state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flooding_reads_do_not_starve_queued_input() {
    let (mut client, mut server_read, mut server) = connected_with(1 << 20, 4096).await;
    make_ready(&mut client, &mut server).await;
    let flood = tokio::spawn(async move {
        for revision in 2..2_000_000u64 {
            if write_frame(&mut server, &crate::protocol::tests::encode_surface(&surface_at(1, revision)))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    // Let the inbound pipe saturate before input is queued.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let handle = client.command_sender();
    let ctrl_c = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.send_key("p", KeyCode::Char('c'), 0b10).await })
    };
    let (stop, mut stopped) = oneshot::channel::<()>();
    let driver = tokio::spawn(async move {
        let mut events = 0usize;
        while stopped.try_recv().is_err() {
            client.next_event().await.unwrap();
            events += 1;
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        events
    });
    let started = std::time::Instant::now();
    let sent = tokio::time::timeout(std::time::Duration::from_secs(2), server_read.next())
        .await
        .expect("queued input starved by inbound frames")
        .unwrap();
    let latency = started.elapsed();
    assert_eq!(sent, encode_input("p", vec![PaneInputEvent::key(KeyCode::Char('c'), 0b10)]).unwrap());
    tokio::time::timeout(std::time::Duration::from_secs(2), ctrl_c).await.unwrap().unwrap().unwrap();
    let _ = stop.send(());
    let events = driver.await.unwrap();
    flood.abort();
    assert!(events > 0);
    assert!(latency < std::time::Duration::from_millis(500), "input took {latency:?}");
}

/// A same-boot snapshot that arrives before its surface is a transient readiness gap, not
/// a target change: unsent input is held, then delivered once the surface catches up.
#[tokio::test]
async fn snapshot_ahead_of_surface_holds_input_until_coherent() {
    let (mut client, mut server_read, mut server) = connected().await;
    make_ready(&mut client, &mut server).await;
    send_snapshot(&mut server, &snapshot_at(2)).await;
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Snapshot(_)));
    assert!(!client.input_ready());
    let handle = client.command_sender();
    let held = spawn_text(&handle, "held");
    settle(&mut client).await;
    assert!(!held.is_finished(), "held input must be neither retired nor completed");
    assert!(tokio::time::timeout(QUIET, server_read.next()).await.is_err(), "input sent before the surface");

    write_frame(&mut server, &crate::protocol::tests::encode_surface(&surface_at(2, 2))).await.unwrap();
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Surface(_)));
    settle(&mut client).await;
    held.await.unwrap().unwrap();
    assert_eq!(
        server_read.next().await.unwrap(),
        encode_input("p", vec![PaneInputEvent::TextCommit("held".into())]).unwrap()
    );
}

/// A held command is still retired (never sent) when the late surface changes the target.
#[tokio::test]
async fn held_input_is_retired_when_the_late_surface_changes_target() {
    let (mut client, mut server_read, mut server) = connected().await;
    make_ready(&mut client, &mut server).await;
    send_snapshot(&mut server, &snapshot_at(2)).await;
    client.next_event().await.unwrap();
    let held = spawn_text(&client.command_sender(), "held");
    settle(&mut client).await;
    let mut popup = popup_surface(2);
    popup.projection_revision = 2;
    write_frame(&mut server, &crate::protocol::tests::encode_surface(&popup)).await.unwrap();
    client.next_event().await.unwrap();
    settle(&mut client).await;
    let error = held.await.unwrap().unwrap_err();
    assert_eq!(error.kind, "classic_stale_state");
    assert!(error.message.contains("not sent"), "{error}");
    assert!(tokio::time::timeout(QUIET, server_read.next()).await.is_err(), "stale input reached the server");
}

/// A partially written large paste survives a harmless snapshot bump (e.g. a cwd change)
/// whose surface arrives later; only an actual target change would close the connection.
#[tokio::test]
async fn partial_paste_survives_a_same_target_snapshot_bump() {
    let (mut client, mut server_read, mut server) = connected_with(4096, 16).await;
    make_ready(&mut client, &mut server).await;
    let paste = "x".repeat(8 * 1024);
    let command = {
        let handle = client.command_sender();
        let paste = paste.clone();
        tokio::spawn(async move { handle.send_paste("p", paste).await })
    };
    settle(&mut client).await;
    assert!(client.write.has_written());

    send_snapshot(&mut server, &snapshot_at(2)).await;
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Snapshot(_)));
    settle(&mut client).await;
    assert!(client.closed.is_none() && client.write.has_written() && !command.is_finished());

    write_frame(&mut server, &crate::protocol::tests::encode_surface(&surface_at(2, 2))).await.unwrap();
    let received = tokio::spawn(async move { server_read.next().await.unwrap() });
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Surface(_)));
    settle(&mut client).await;
    command.await.unwrap().unwrap();
    assert_eq!(received.await.unwrap(), encode_input("p", vec![PaneInputEvent::Paste(paste)]).unwrap());
    assert!(client.closed.is_none() && client.input_ready());
}

const SHORT_HOLD: std::time::Duration = std::time::Duration::from_millis(100);

/// Liveness bound: if the surface for a newer snapshot never arrives, held unsent input is
/// retired as not sent instead of blocking every later command; the connection stays usable.
#[tokio::test]
async fn held_unsent_input_times_out_as_not_sent() {
    let (mut client, mut server_read, mut server) = connected().await;
    client.set_input_hold_timeout(SHORT_HOLD);
    make_ready(&mut client, &mut server).await;
    send_snapshot(&mut server, &snapshot_at(2)).await;
    client.next_event().await.unwrap();
    let handle = client.command_sender();
    let held = spawn_text(&handle, "held");
    let queued = spawn_text(&handle, "queued");
    // No surface ever follows; drive past the deadline of both held commands.
    assert!(tokio::time::timeout(SHORT_HOLD * 4, client.next_event()).await.is_err());
    for command in [held, queued] {
        assert!(command.is_finished(), "held input was not retired by the hold deadline");
        let error = command.await.unwrap().unwrap_err();
        assert_eq!(error.kind, "classic_stale_state");
        assert!(error.message.contains("did not arrive") && error.message.contains("not sent"), "{error}");
    }
    assert!(client.closed.is_none() && !client.write.is_pending());
    assert!(tokio::time::timeout(QUIET, server_read.next()).await.is_err(), "timed-out input reached the server");

    write_frame(&mut server, &crate::protocol::tests::encode_surface(&surface_at(2, 2))).await.unwrap();
    client.next_event().await.unwrap();
    let fresh = spawn_text(&handle, "fresh");
    settle(&mut client).await;
    fresh.await.unwrap().unwrap();
    assert_eq!(
        server_read.next().await.unwrap(),
        encode_input("p", vec![PaneInputEvent::TextCommit("fresh".into())]).unwrap()
    );
}

/// A partially written paste held across a snapshot whose surface never arrives cannot be
/// completed or truncated: after the deadline the endpoint closes with an unknown outcome.
#[tokio::test]
async fn held_partial_frame_times_out_with_unknown_outcome() {
    let (mut client, mut server_read, mut server) = connected_with(4096, 16).await;
    client.set_input_hold_timeout(SHORT_HOLD);
    make_ready(&mut client, &mut server).await;
    let command = {
        let handle = client.command_sender();
        tokio::spawn(async move { handle.send_paste("p", "x".repeat(8 * 1024)).await })
    };
    settle(&mut client).await;
    assert!(client.write.has_written());
    send_snapshot(&mut server, &snapshot_at(2)).await;
    assert!(matches!(client.next_event().await.unwrap(), EndpointEvent::Snapshot(_)));

    let started = std::time::Instant::now();
    let error = tokio::time::timeout(SHORT_HOLD * 10, client.next_event()).await.unwrap().unwrap_err();
    assert!(started.elapsed() >= SHORT_HOLD / 2, "closed before the hold deadline");
    assert_eq!(error.kind, "classic_mutation_unknown");
    assert!(error.message.contains("partially transmitted"), "{error}");
    let outcome = tokio::time::timeout(SHORT_HOLD * 10, command).await.expect("partial command never completed");
    assert_eq!(outcome.unwrap().unwrap_err().kind, "classic_mutation_unknown");
    assert_eq!(client.command_sender().send_text("p", "after".into()).await.unwrap_err().kind, "classic_transport");
    // The server sees a truncated prefix and then EOF, never a completed held frame.
    assert_eq!(server_read.next().await.unwrap_err().kind, "classic_transport");
}

/// The hold deadline is armed once per held command and is not re-armed by inbound frames,
/// so a stream of snapshots that never brings the matching surface cannot defer it.
#[tokio::test]
async fn hold_deadline_fires_while_frames_keep_arriving() {
    let (mut client, mut server_read, mut server) = connected_with(1 << 16, 4096).await;
    client.set_input_hold_timeout(SHORT_HOLD);
    make_ready(&mut client, &mut server).await;
    let churn = tokio::spawn(async move {
        for revision in 2..1_000_000u64 {
            send_snapshot(&mut server, &snapshot_at(revision)).await;
            tokio::task::yield_now().await;
        }
    });
    client.next_event().await.unwrap();
    let held = spawn_text(&client.command_sender(), "held");
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            client.next_event().await.unwrap();
            if held.is_finished() {
                break held.await.unwrap();
            }
        }
    })
    .await
    .expect("hold deadline was deferred by inbound frames");
    churn.abort();
    assert!(result.unwrap_err().message.contains("did not arrive"));
    assert!(tokio::time::timeout(QUIET, server_read.next()).await.is_err(), "timed-out input reached the server");
}
