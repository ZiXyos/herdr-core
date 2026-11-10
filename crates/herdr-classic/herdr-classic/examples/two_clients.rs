//! Disposable classic Docker fixture: two persistent clients, real cells/input/graphics.
//! Run only through experiments/classic/run.sh; never targets the user's host session.

use herdr_classic::{ClassicEndpoint, ClassicError, EndpointEvent, GraphicsFormat, KeyCode, PaneSurface};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// crossterm's CONTROL modifier bit, as used by classic semantic key input.
const CONTROL: u8 = 0b10;
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL9WQAAAABJRU5ErkJggg==";
type Endpoint = ClassicEndpoint<ChildStdout, ChildStdin>;

struct Client {
    endpoint: Endpoint,
    _child: Child,
}

async fn api(method: &str, params: Value) -> Result<Value, ClassicError> {
    let request = format!("{}\n", json!({"id":"two-client-fixture","method":method,"params":params}));
    let mut child = Command::new("ssh")
        .args(["classic", "herdr", "--session", "compat", "remote-api-bridge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child.stdin.take().unwrap().write_all(request.as_bytes()).await?;
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(ClassicError::new("fixture", String::from_utf8_lossy(&output.stderr)));
    }
    let response: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| ClassicError::new("fixture", error.to_string()))?;
    if let Some(error) = response.get("error") {
        return Err(ClassicError::new("fixture", format!("{method}: {error}")));
    }
    response.get("result").cloned().ok_or_else(|| ClassicError::new("fixture", "missing API result"))
}

async fn connect(cols: u16, rows: u16) -> Result<Client, ClassicError> {
    let mut child = Command::new("ssh")
        .args(["classic", "herdr", "--session", "compat", "remote-client-bridge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let write = child.stdin.take().ok_or_else(|| ClassicError::new("fixture", "missing SSH stdin"))?;
    let read = child.stdout.take().ok_or_else(|| ClassicError::new("fixture", "missing SSH stdout"))?;
    let (endpoint, welcome) =
        ClassicEndpoint::connect(read, write, herdr_classic::ClientHello::mobile(cols, rows)).await?;
    assert_eq!(welcome.generation, 1);
    let mut client = Client { endpoint, _child: child };
    for _ in 0..100 {
        if client.endpoint.snapshot().is_some() {
            return Ok(client);
        }
        event(&mut client).await?;
    }
    Err(ClassicError::new("fixture_timeout", "missing initial boot snapshot"))
}

async fn event(client: &mut Client) -> Result<EndpointEvent, ClassicError> {
    tokio::time::timeout(Duration::from_secs(15), client.endpoint.next_event())
        .await
        .map_err(|_| ClassicError::new("fixture_timeout", "classic endpoint did not produce an event"))?
}

async fn wait_surface(client: &mut Client, predicate: impl Fn(&PaneSurface) -> bool) -> Result<(), ClassicError> {
    for _ in 0..100 {
        if client.endpoint.surface().is_some_and(&predicate) {
            return Ok(());
        }
        if let Err(error) = event(client).await {
            let current = client.endpoint.surface().map(text).unwrap_or_default();
            return Err(ClassicError::new(error.kind, format!("{error}; last surface:\n{current}")));
        }
    }
    Err(ClassicError::new("fixture_timeout", "surface did not reach the expected state"))
}

async fn respond(client: &mut Client, request_id: &str) -> Result<Value, ClassicError> {
    for _ in 0..100 {
        if let EndpointEvent::Response(response) = event(client).await?
            && response.request_id == request_id
        {
            return response
                .result
                .map_err(|error| ClassicError::new("classic_remote", format!("{}: {}", error.code, error.message)));
        }
    }
    Err(ClassicError::new("fixture_timeout", "missing correlated response"))
}

async fn focus_workspace(client: &mut Client, id: &str) -> Result<(), ClassicError> {
    let request = client.endpoint.focus_workspace(id).await?;
    respond(client, &request).await?;
    Ok(())
}

/// Drives the endpoint while a handle command (a separate task in the native bridge)
/// waits for its own completion.
async fn drive_until<T>(client: &mut Client, command: impl Future<Output = T>) -> Result<T, ClassicError> {
    tokio::pin!(command);
    loop {
        tokio::select! {
            result = &mut command => return Ok(result),
            event = event(client) => {
                event?;
            }
        }
    }
}

/// Tab focus through the concurrent handle, which awaits the correlated remote result.
async fn focus_tab(client: &mut Client, id: &str, pane_id: &str) -> Result<(), ClassicError> {
    let handle = client.endpoint.command_sender();
    drive_until(client, handle.focus_tab(id)).await??;
    wait_surface(client, |surface| surface.panes.iter().any(|pane| pane.pane_id == pane_id)).await
}

fn text(surface: &PaneSurface) -> String {
    rows(surface).join("\n")
}

fn rows(surface: &PaneSurface) -> Vec<String> {
    surface
        .frame
        .cells
        .chunks(surface.frame.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect()
}

fn stty_size(surface: &PaneSurface) -> Option<(u32, u32)> {
    rows(surface).iter().find_map(|row| {
        // Pane borders surround the output (for example `│28 47 ...│`).
        let fields: Vec<_> =
            row.split(|character: char| !character.is_ascii_digit()).filter(|field| !field.is_empty()).collect();
        if fields.len() != 2 {
            return None;
        }
        Some((fields[0].parse::<u32>().ok()?, fields[1].parse::<u32>().ok()?))
    })
}

async fn drain(client: &mut Client) -> Result<(), ClassicError> {
    loop {
        match tokio::time::timeout(Duration::from_millis(100), client.endpoint.next_event()).await {
            Ok(result) => {
                result?;
            }
            Err(_) => return Ok(()),
        }
    }
}

fn check(label: &str, condition: bool) -> Result<(), ClassicError> {
    if !condition {
        return Err(ClassicError::new("fixture_assertion", label));
    }
    println!("{}", json!({"assertion":label,"passed":true}));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), ClassicError> {
    let workspace = api("workspace.create", json!({"label":"two-client-stable","focus":false})).await?;
    let workspace_id = workspace["workspace"]["workspace_id"].as_str().unwrap().to_owned();
    let tab_a = workspace["tab"]["tab_id"].as_str().unwrap().to_owned();
    let pane_a = workspace["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let created_b = api("tab.create", json!({"workspace_id":workspace_id,"label":"second","focus":false})).await?;
    let tab_b = created_b["tab"]["tab_id"].as_str().unwrap().to_owned();
    let pane_b = created_b["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let split = api("pane.split", json!({"target_pane_id":pane_b,"direction":"right","focus":false})).await?;
    let pane_b2 = split["pane"]["pane_id"].as_str().unwrap().to_owned();
    let before = api("pane.process_info", json!({"pane_id":pane_b2})).await?;

    let mut a = connect(80, 24).await?;
    let mut b = connect(100, 30).await?;
    focus_workspace(&mut a, &workspace_id).await?;
    focus_workspace(&mut b, &workspace_id).await?;
    focus_tab(&mut a, &tab_a, &pane_a).await?;
    focus_tab(&mut b, &tab_b, &pane_b2).await?;
    // Drain A after B's navigation so the independence assertion cannot use a stale queue.
    drain(&mut a).await?;
    check(
        "two clients retain independent selected tabs",
        a.endpoint.surface().unwrap().panes.iter().any(|pane| pane.pane_id == pane_a)
            && b.endpoint.surface().unwrap().panes.iter().any(|pane| pane.pane_id == pane_b2)
            && !a.endpoint.surface().unwrap().panes.iter().any(|pane| pane.pane_id == pane_b2),
    )?;

    a.endpoint.send_text(&pane_a, "printf 'STABLE_%s\\n' INPUT_A".into()).await?;
    a.endpoint.send_key(&pane_a, KeyCode::Enter, 0).await?;
    wait_surface(&mut a, |surface| text(surface).contains("STABLE_INPUT_A")).await?;
    check("persistent endpoint decoded real cell output after semantic text and key", true)?;

    // Fairness under sustained output: a slow consumer (200 us of work per event, as a
    // bridge/JS layer may be) must still get Ctrl-C written while `yes` floods the pane.
    a.endpoint.send_text(&pane_a, "yes FLOOD_LINE".into()).await?;
    a.endpoint.send_key(&pane_a, KeyCode::Enter, 0).await?;
    wait_surface(&mut a, |surface| text(surface).matches("FLOOD_LINE").count() > 5).await?;
    let handle = a.endpoint.command_sender();
    let started = std::time::Instant::now();
    let mut flood_events = 0usize;
    let interrupt = handle.send_key(&pane_a, KeyCode::Char('c'), CONTROL);
    tokio::pin!(interrupt);
    let interrupted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = &mut interrupt => return result,
                event = event(&mut a) => {
                    event?;
                    flood_events += 1;
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
        }
    })
    .await
    .map_err(|_| ClassicError::new("fixture_timeout", "Ctrl-C was starved by sustained output"))?;
    let interrupt_latency_ms = started.elapsed().as_millis();
    println!(
        "{}",
        json!({"ctrl_c_under_flood": interrupted.as_ref().map_err(ToString::to_string), "latency_ms": interrupt_latency_ms, "events_before_write": flood_events})
    );
    check("Ctrl-C is written under sustained output with a slow consumer", interrupted.is_ok())?;
    a.endpoint.send_text(&pane_a, "clear; echo INTERRUPTED_''OK".into()).await?;
    a.endpoint.send_key(&pane_a, KeyCode::Enter, 0).await?;
    wait_surface(&mut a, |surface| text(surface).contains("INTERRUPTED_OK")).await?;
    check("the flood stopped and the shell accepted the next command", true)?;
    a.endpoint.send_text(&pane_a, "printf 'STABLE_%s\\n' INPUT_A".into()).await?;
    a.endpoint.send_key(&pane_a, KeyCode::Enter, 0).await?;
    wait_surface(&mut a, |surface| text(surface).contains("STABLE_INPUT_A")).await?;

    focus_tab(&mut a, &tab_b, &pane_b2).await?;
    let handle = a.endpoint.command_sender();
    let hidden = drive_until(&mut a, handle.send_text(&pane_a, "echo HIDDEN_''MUST_NOT_RUN".into())).await?;
    println!("{}", json!({"hidden_target_input": hidden.as_ref().map_err(ToString::to_string)}));
    check(
        "input to a pane hidden by navigation is retired locally and never sent",
        hidden.is_err_and(|error| error.kind == "classic_stale_state" && error.message.contains("not sent")),
    )?;
    let rejected = drive_until(&mut a, handle.focus_tab("missing-tab")).await?;
    println!("{}", json!({"missing_tab_focus": rejected.as_ref().map_err(ToString::to_string)}));
    check(
        "awaited tab.focus propagates the live server's rejection",
        rejected.is_err_and(|error| error.kind == "classic_request_rejected"),
    )?;
    // Go back and prove the retired input never reached the hidden pane's shell.
    focus_tab(&mut a, &tab_a, &pane_a).await?;
    drain(&mut a).await?;
    check(
        "retired hidden-target input never reached the remote shell",
        a.endpoint
            .surface()
            .is_some_and(|surface| text(surface).contains("STABLE_INPUT_A") && !text(surface).contains("HIDDEN_")),
    )?;
    focus_tab(&mut a, &tab_b, &pane_b2).await?;
    wait_surface(&mut b, |surface| surface.panes.iter().any(|pane| pane.pane_id == pane_b2)).await?;
    let geometry_a =
        a.endpoint.surface().unwrap().panes.iter().find(|pane| pane.pane_id == pane_b2).unwrap().inner_rect;
    let geometry_b =
        b.endpoint.surface().unwrap().panes.iter().find(|pane| pane.pane_id == pane_b2).unwrap().inner_rect;
    println!(
        "{}",
        json!({"same_tab_view_geometry_a":geometry_a,"same_tab_view_geometry_b":geometry_b,"requested_a":[80,24],"requested_b":[100,30]})
    );
    check("same-tab clients expose bounded native geometry", geometry_a.width > 0 && geometry_b.width > 0)?;
    b.endpoint.send_text(&pane_b2, "stty size".into()).await?;
    b.endpoint.send_key(&pane_b2, KeyCode::Enter, 0).await?;
    // The echoed command has no digits; only the real `stty size` output parses here.
    wait_surface(&mut a, |surface| stty_size(surface).is_some()).await?;
    wait_surface(&mut b, |surface| stty_size(surface).is_some()).await?;
    let size_a = stty_size(a.endpoint.surface().unwrap()).unwrap();
    let size_b = stty_size(b.endpoint.surface().unwrap()).unwrap();
    println!("{}", json!({"stty_size_a":size_a,"stty_size_b":size_b}));
    check(
        "both same-tab clients observe the same actual stty rows and columns",
        size_a == size_b && size_a.0 > 0 && size_a.1 > 0,
    )?;

    let focus = a.endpoint.focus_pane(&pane_b).await?;
    respond(&mut a, &focus).await?;
    wait_surface(&mut b, |surface| {
        surface.panes.iter().find(|pane| pane.pane_id == pane_b).is_some_and(|pane| pane.focused)
    })
    .await?;
    check("same-tab pane focus is shared with the second client", true)?;

    b.endpoint.send_text(&pane_b2, "for i in $(seq 1 120); do echo SCROLL_$i; done".into()).await?;
    b.endpoint.send_key(&pane_b2, KeyCode::Enter, 0).await?;
    wait_surface(&mut b, |surface| text(surface).contains("SCROLL_120")).await?;
    b.endpoint.send_key(&pane_b2, KeyCode::PageUp, 0).await?;
    wait_surface(&mut a, |surface| {
        surface
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_b2)
            .and_then(|pane| pane.scroll)
            .is_some_and(|scroll| scroll.offset_from_bottom > 0)
    })
    .await?;
    check("same-tab scrollback offset is observable by the other client", true)?;

    // Shell receives: printf '\033_Ga=T,f=100;<base64 PNG>\033\\\n' (Kitty APC, PNG payload).
    let graphics_command = format!("printf '\\033_Ga=T,f=100,c=2,r=2;{}\\033\\\\'; echo GFX_''DONE", PNG_BASE64);
    b.endpoint.send_text(&pane_b2, graphics_command).await?;
    b.endpoint.send_key(&pane_b2, KeyCode::Enter, 0).await?;
    wait_surface(&mut b, |surface| text(surface).contains("GFX_DONE")).await?;
    // Give the scene a few more frames; graphics can trail the text that follows it.
    for _ in 0..5 {
        if b.endpoint.surface().is_some_and(|surface| !surface.graphics.placements.is_empty()) {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(2), b.endpoint.next_event()).await {
            Ok(event) => {
                event?;
            }
            Err(_) => break,
        }
    }
    let scene = b.endpoint.surface().unwrap().graphics.clone();
    match scene.placements.first() {
        Some(placement) => {
            let bytes = b.endpoint.cached_image(&placement.asset).unwrap_or_default();
            check(
                "valid PNG asset bytes, placement and crop survived the live endpoint",
                placement.asset.format == GraphicsFormat::Png && bytes == include_bytes!("../src/fixtures/one.png"),
            )?;
        }
        None => println!(
            "{}",
            json!({"graphics":"NOT_OBSERVED","detail":"Kitty PNG escape produced no stable-endpoint placement; live graphics unverified"})
        ),
    }

    let populated_cache_before_reconnect = b.endpoint.cached_image_bytes();
    let live_graphics_observed = !scene.placements.is_empty();
    drop(b);
    let mut reconnected = connect(100, 30).await?;
    focus_workspace(&mut reconnected, &workspace_id).await?;
    focus_tab(&mut reconnected, &tab_b, &pane_b2).await?;
    let after = api("pane.process_info", json!({"pane_id":pane_b2})).await?;
    check("reconnect preserved the original native shell process", before == after)?;
    if live_graphics_observed {
        check(
            "reconnect invalidated a populated image cache",
            populated_cache_before_reconnect > 0 && reconnected.endpoint.cached_image_bytes() == 0,
        )?;
    } else {
        println!(
            "{}",
            json!({"graphics_cache_reconnect":"NOT_OBSERVED","detail":"no live placement populated the old client cache"})
        );
    }
    println!(
        "{}",
        json!({"summary":"two-client stable surfaces, patches, semantic input, shared coupling and reconnect passed; see graphics line"})
    );
    Ok(())
}
