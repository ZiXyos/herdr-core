//! Classic stable-endpoint terminals decoded in Rust by `herdr-classic` and delivered to JS as typed,
//! epoch-tagged events matching Hydra's `ClassicTerminalPort` contract. Bytes never reach JS raw.
//!
//! Epoch contract: every terminal id has a connection epoch that increases on each
//! `terminalReconnect`, even to the same classic boot. Every event carries it, and a fatal error
//! ends its epoch. Recovery is always an explicit reconnect; nothing is retried here.

use std::time::Duration;

use base64::Engine as _;
use herdr_classic::{
    ClassicEndpoint, ClassicEndpointHandle, ClassicError, ClientHello, EndpointEvent, GraphicsAsset, GraphicsAssetKey,
    GraphicsFormat, GraphicsSource, GraphicsTarget, KeyCode, KeyKind, PaneInputEvent, PaneSurface, Rect, ShellSnapshot,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{EngineError, Events, ssh::SshTransport};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

impl From<ClassicError> for EngineError {
    fn from(error: ClassicError) -> Self {
        let hint = match error.kind {
            "classic_stale_state" => "nothing was sent; keep the draft and wait for a fresh surface",
            "classic_mutation_unknown" => "input may have been delivered; reconnect and look before retrying",
            "classic_request_rejected" => "the classic session refused the request",
            _ => "reconnect the terminal explicitly",
        };
        EngineError::new(error.kind, error.message, hint)
    }
}

pub struct Terminal {
    pub session_id: String,
    pub epoch: u64,
    pub cols: u16,
    pub rows: u16,
    pub handle: Option<ClassicEndpointHandle>,
    pub stop: CancellationToken,
}

pub struct Opened {
    pub handle: ClassicEndpointHandle,
    pub server_version: String,
}

/// Opens one stable endpoint and starts its single driver task. Events from it carry `epoch`.
pub async fn open(
    transport: &SshTransport,
    session: &str,
    terminal_id: &str,
    epoch: u64,
    size: (u16, u16),
    events: Events,
    stop: CancellationToken,
) -> Result<Opened, EngineError> {
    let stream = transport.open_stable_stream(session).await?;
    let (read, write) = tokio::io::split(stream);
    let connected = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        ClassicEndpoint::connect(read, write, ClientHello::mobile(size.0, size.1)),
    )
    .await
    .map_err(|_| {
        EngineError::new("classic_transport", "Stable endpoint handshake timed out", "reconnect explicitly")
    })??;
    let (endpoint, welcome) = connected;
    let handle = endpoint.command_sender();
    tokio::spawn(drive(endpoint, events, terminal_id.to_owned(), epoch, stop));
    Ok(Opened { handle, server_version: welcome.server_version })
}

async fn drive<R, W>(
    mut endpoint: ClassicEndpoint<R, W>,
    events: Events,
    terminal_id: String,
    epoch: u64,
    stop: CancellationToken,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let next = tokio::select! {
            biased;
            () = stop.cancelled() => {
                // Retires unsent commands as not sent; a partial frame closes as unknown.
                endpoint.invalidate();
                return;
            }
            next = endpoint.next_event() => next,
        };
        let event = match next {
            Ok(EndpointEvent::Snapshot(snapshot)) => {
                json!({"kind":"terminal","terminalId":terminal_id,"epoch":epoch,"type":"snapshot","snapshot":snapshot_json(&snapshot)})
            }
            Ok(EndpointEvent::Surface(surface)) => {
                let assets = endpoint.resolved_assets();
                json!({"kind":"terminal","terminalId":terminal_id,"epoch":epoch,"type":"surface","surface":surface_json(&surface, &assets)})
            }
            // Handle requests resolve their own futures with the correlated result.
            Ok(EndpointEvent::Response(_)) => continue,
            Err(error) => {
                events.control(error_event(&terminal_id, epoch, error.kind, &error.message));
                return;
            }
        };
        // Lossless and ordered: waits for queue space, which applies flow control to the endpoint.
        let delivered = tokio::select! {
            delivered = events.data(event) => delivered,
            () = stop.cancelled() => false,
        };
        if !delivered {
            endpoint.invalidate();
            return;
        }
    }
}

pub fn error_event(terminal_id: &str, epoch: u64, kind: &str, message: &str) -> Value {
    json!({"kind":"terminal","terminalId":terminal_id,"epoch":epoch,"type":"error","errorKind":kind,"message":message,"recoverable":false})
}

fn rect(r: &Rect) -> Value {
    json!({"x":r.x,"y":r.y,"width":r.width,"height":r.height})
}

pub fn snapshot_json(s: &ShellSnapshot) -> Value {
    json!({
        "bootId": s.boot_id,
        "revision": s.revision,
        "focusedWorkspaceId": s.focused_workspace_id,
        "focusedTabId": s.focused_tab_id,
        "focusedPaneId": s.focused_pane_id,
        "workspaces": s.workspaces.iter().map(|w| json!({"workspaceId":w.workspace_id,"activeTabId":w.active_tab_id,"number":w.number,"label":w.label,"focused":w.focused})).collect::<Vec<_>>(),
        "tabs": s.tabs.iter().map(|t| json!({"tabId":t.tab_id,"workspaceId":t.workspace_id,"number":t.number,"label":t.label,"zoomed":t.zoomed,"focused":t.focused})).collect::<Vec<_>>(),
        "panes": s.panes.iter().map(|p| json!({"paneId":p.pane_id,"workspaceId":p.workspace_id,"tabId":p.tab_id,"label":p.label,"cwd":p.cwd,"focused":p.focused})).collect::<Vec<_>>(),
    })
}

fn key_json(k: &GraphicsAssetKey) -> Value {
    let source = match &k.source {
        GraphicsSource::Terminal { target, image_id } => {
            let target = match target {
                GraphicsTarget::Pane { pane_id } => json!({"kind":"pane","paneId":pane_id}),
                GraphicsTarget::Popup { terminal_id } => json!({"kind":"popup","terminalId":terminal_id}),
            };
            json!({"kind":"terminal","target":target,"imageId":image_id})
        }
        GraphicsSource::PaneLayer { pane_id, layer_id } => {
            json!({"kind":"paneLayer","paneId":pane_id,"layerId":layer_id})
        }
    };
    let format = match k.format {
        GraphicsFormat::Rgb => "rgb",
        GraphicsFormat::Rgba => "rgba",
        GraphicsFormat::Png => "png",
    };
    json!({"source":source,"imageWidth":k.image_width,"imageHeight":k.image_height,"format":format,
        "dataLength":k.data_len,"dataFingerprint":k.data_fingerprint.to_string()})
}

pub fn surface_json(s: &PaneSurface, assets: &[GraphicsAsset]) -> Value {
    let f = &s.frame;
    json!({
        "bootId": s.boot_id,
        "projectionRevision": s.projection_revision,
        "surfaceRevision": s.surface_revision,
        "width": f.width,
        "height": f.height,
        "cells": f.cells.iter().map(|c| json!({"symbol":c.symbol,"fg":c.fg,"bg":c.bg,"modifier":c.modifier,"skip":c.skip,"hyperlink":c.hyperlink})).collect::<Vec<_>>(),
        "cursor": f.cursor.as_ref().map(|c| json!({"x":c.x,"y":c.y,"visible":c.visible,"shape":c.shape})),
        "hyperlinks": f.hyperlinks,
        "popup": s.popup.as_ref().map(|p| json!({"terminalId":p.terminal_id,"title":p.title})),
        "panes": s.panes.iter().map(|p| json!({
            "paneId":p.pane_id, "contentRevision":p.content_revision, "rect":rect(&p.rect), "innerRect":rect(&p.inner_rect),
            "scrollbarRect":p.scrollbar_rect.as_ref().map(rect),
            "scroll":p.scroll.as_ref().map(|m| json!({"offsetFromBottom":m.offset_from_bottom,"maxOffsetFromBottom":m.max_offset_from_bottom,"viewportRows":m.viewport_rows})),
            "focused":p.focused, "mouseReporting":p.mouse_reporting, "sgrPixelMouse":p.sgr_pixel_mouse,
            "alternateScreenActive":p.alternate_screen_active, "pixelWidth":p.pixel_width, "pixelHeight":p.pixel_height,
        })).collect::<Vec<_>>(),
        "assets": assets.iter().map(|a| json!({"key":key_json(&a.key),"dataBase64":base64::engine::general_purpose::STANDARD.encode(&a.data)})).collect::<Vec<_>>(),
        "placements": s.graphics.placements.iter().map(|p| json!({
            "asset":key_json(&p.asset), "logicalPlacementId":p.logical_placement_id, "x":p.x, "y":p.y, "cols":p.cols, "rows":p.rows,
            "sourceX":p.source_x, "sourceY":p.source_y, "sourceWidth":p.source_width, "sourceHeight":p.source_height,
            "xOffset":p.x_offset, "yOffset":p.y_offset, "z":p.z, "scrollbackOffset":p.scrollback_offset,
        })).collect::<Vec<_>>(),
    })
}

/// JS `TerminalInputEvent`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum InputEvent {
    Text {
        text: String,
    },
    Paste {
        text: String,
    },
    Key {
        code: Value,
        #[serde(default)]
        modifiers: u8,
        #[serde(default)]
        kind: Option<String>,
    },
}

fn bad(message: &str) -> EngineError {
    EngineError::new("invalid_params", message, "use the TerminalInputEvent shape")
}

fn key_code(code: &Value) -> Result<KeyCode, EngineError> {
    if let Some(name) = code.as_str() {
        return Ok(match name {
            "backspace" => KeyCode::Backspace,
            "enter" => KeyCode::Enter,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageUp" => KeyCode::PageUp,
            "pageDown" => KeyCode::PageDown,
            "tab" => KeyCode::Tab,
            "backTab" => KeyCode::BackTab,
            "delete" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "esc" => KeyCode::Esc,
            "null" => KeyCode::Null,
            _ => return Err(bad("unknown key code")),
        });
    }
    if let Some(c) = code.get("char").and_then(Value::as_str) {
        let mut chars = c.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(KeyCode::Char(c)),
            _ => Err(bad("char key needs exactly one character")),
        };
    }
    match code.get("f").and_then(Value::as_u64) {
        Some(n @ 1..=24) => Ok(KeyCode::F(n as u8)),
        _ => Err(bad("unknown key code")),
    }
}

pub fn input_events(events: Vec<InputEvent>) -> Result<Vec<PaneInputEvent>, EngineError> {
    events
        .into_iter()
        .map(|event| {
            Ok(match event {
                InputEvent::Text { text } => PaneInputEvent::TextCommit(text),
                InputEvent::Paste { text } => PaneInputEvent::Paste(text),
                InputEvent::Key { code, modifiers, kind } => {
                    let mut key = PaneInputEvent::key(key_code(&code)?, modifiers);
                    if let PaneInputEvent::Key { kind: k, .. } = &mut key {
                        *k = match kind.as_deref() {
                            None | Some("press") => KeyKind::Press,
                            Some("repeat") => KeyKind::Repeat,
                            Some("release") => KeyKind::Release,
                            Some(_) => return Err(bad("unknown key kind")),
                        };
                    }
                    key
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_input_maps_one_to_one_onto_semantic_events() {
        let events: Vec<InputEvent> = serde_json::from_value(json!([
            {"type":"text","text":"echo héllo 界"},
            {"type":"key","code":"enter"},
            {"type":"key","code":{"char":"c"},"modifiers":2},
            {"type":"key","code":{"f":5},"kind":"release"},
            {"type":"paste","text":"x"},
        ]))
        .unwrap();
        let mapped = input_events(events).unwrap();
        assert_eq!(mapped[0], PaneInputEvent::TextCommit("echo héllo 界".into()));
        assert_eq!(mapped[1], PaneInputEvent::key(KeyCode::Enter, 0));
        assert_eq!(mapped[2], PaneInputEvent::key(KeyCode::Char('c'), 2));
        assert!(matches!(mapped[3], PaneInputEvent::Key { code: KeyCode::F(5), kind: KeyKind::Release, .. }));
        assert_eq!(mapped[4], PaneInputEvent::Paste("x".into()));
        for bad in [
            json!({"type":"key","code":"meta"}),
            json!({"type":"key","code":{"char":"ab"}}),
            json!({"type":"raw","bytes":"x"}),
        ] {
            let parsed = serde_json::from_value::<InputEvent>(bad);
            assert!(parsed.is_err() || input_events(vec![parsed.unwrap()]).is_err());
        }
    }

    #[test]
    fn classic_errors_keep_their_kind_across_the_boundary() {
        let e: EngineError = ClassicError::stale("not sent").into();
        assert_eq!(e.kind, "classic_stale_state");
        let event = error_event("terminal-1", 3, "classic_mutation_unknown", "cut");
        assert_eq!(event["epoch"], 3);
        assert_eq!(event["recoverable"], false);
    }
}
