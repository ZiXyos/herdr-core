use crate::ClassicError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub const GENERATION: u32 = 1;
pub const MAX_FRAME: usize = 2 * 1024 * 1024;
pub const MAX_GRAPHICS_FRAME: usize = 32 * 1024 * 1024;
pub const MAX_COMMAND_BYTES: usize = 1024 * 1024;
pub const MAX_CELLS: usize = 1_000_000;
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_IMAGE_CACHE_BYTES: usize = 32 * 1024 * 1024;

const CLIENT_SHELL_PANE_INPUT_TAG: u32 = 13;
const CLIENT_SHELL_ENDPOINT_REQUEST_TAG: u32 = 15;
const CLIENT_ENDPOINT_CONTROL_TAG: u32 = 20;
pub(crate) const SERVER_PANE_SURFACE_TAG: u32 = 13;
const SERVER_ENDPOINT_RESPONSE_TAG: u32 = 18;
pub(crate) const SERVER_ENDPOINT_CONTROL_TAG: u32 = 20;
const SERVER_PANE_PATCH_TAG: u32 = 19;

pub const HELLO_KIND: &str = "endpoint.hello.v1";
pub const WELCOME_KIND: &str = "endpoint.welcome.v1";
pub const SNAPSHOT_KIND: &str = "shell.snapshot.v1";
pub const SNAPSHOT_CODEC: &str = "shell.snapshot.v1";
pub const SURFACE_CODEC: &str = "shell.surface.v1";
pub const INPUT_CODEC: &str = "shell.input.semantic.v1";
pub const BLOB_CODEC: &str = "shell.blob.v1";
/// Recognized optional surface encodings this client never negotiates. Receiving one
/// means the view cannot be kept current, so it fails explicitly instead of going stale.
pub const UNSUPPORTED_SURFACE_CONTROLS: [&str; 3] =
    ["endpoint.surface-reuse.v1", "endpoint.surface-delta.v1", "endpoint.surface-scroll.v1"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub generation: u32,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub surface_size: SurfaceSize,
    pub pixel_mouse: bool,
    pub direct_graphics: bool,
    pub endpoint_keybindings: bool,
    pub mouse_capture: bool,
    pub surface_active: bool,
    pub surface_reuse: bool,
    pub surface_delta: bool,
    pub surface_scroll: bool,
    pub snapshot_codecs: Vec<String>,
    pub surface_codecs: Vec<String>,
    pub input_codecs: Vec<String>,
    pub blob_codecs: Vec<String>,
}

impl ClientHello {
    /// Full surfaces only. Optional baseline codecs stay off until their exact
    /// projection/surface fences are implemented and tested.
    pub fn mobile(cols: u16, rows: u16) -> Self {
        Self {
            generation: GENERATION,
            cell_width_px: 8,
            cell_height_px: 16,
            surface_size: SurfaceSize { cols, rows },
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            snapshot_codecs: vec![SNAPSHOT_CODEC.into()],
            surface_codecs: vec![SURFACE_CODEC.into()],
            input_codecs: vec![INPUT_CODEC.into()],
            blob_codecs: vec![BLOB_CODEC.into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerWelcome {
    pub generation: u32,
    pub server_version: String,
    pub snapshot_codec: String,
    pub surface_codec: String,
    pub input_codec: String,
    pub blob_codec: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub error: Option<HandshakeError>,
}

/// Typed projection of the stable JSON snapshot. Vendor agent state and update
/// notices are deliberately not projected into the terminal client contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub boot_id: String,
    pub revision: u64,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    pub active_tab_id: String,
    pub number: usize,
    pub label: String,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    #[serde(default)]
    pub zoomed: bool,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub label: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub symbol: String,
    pub fg: u32,
    pub bg: u32,
    pub modifier: u16,
    pub skip: bool,
    pub hyperlink: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    #[serde(default)]
    pub shape: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub cells: Vec<Cell>,
    pub width: u16,
    pub height: u16,
    pub cursor: Option<Cursor>,
    pub hyperlinks: Vec<String>,
    pub graphics: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfacePane {
    pub pane_id: String,
    pub content_revision: u64,
    pub rect: Rect,
    pub inner_rect: Rect,
    pub scrollbar_rect: Option<Rect>,
    pub scroll: Option<ScrollMetrics>,
    pub focused: bool,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub alternate_screen_active: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollMetrics {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Split {
    pub direction: SplitDirection,
    pub pos: u16,
    pub area: Rect,
    pub hit_rect: Rect,
    pub path: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsTarget {
    Pane { pane_id: String },
    Popup { terminal_id: String },
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsSource {
    Terminal { target: GraphicsTarget, image_id: u32 },
    PaneLayer { pane_id: String, layer_id: String },
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsFormat {
    Rgb,
    Rgba,
    Png,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsAssetKey {
    pub source: GraphicsSource,
    pub image_width: u32,
    pub image_height: u32,
    pub format: GraphicsFormat,
    pub data_len: u64,
    pub data_fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsAsset {
    pub key: GraphicsAssetKey,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsPlacement {
    pub asset: GraphicsAssetKey,
    pub logical_placement_id: u32,
    pub x: u16,
    pub y: u16,
    pub cols: u32,
    pub rows: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub z: i32,
    pub scrollback_offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsScene {
    pub assets: Vec<GraphicsAsset>,
    pub placements: Vec<GraphicsPlacement>,
    pub retained_assets: Vec<GraphicsAssetKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopupSurface {
    pub terminal_id: String,
    pub title: String,
    pub width: Option<PopupSize>,
    pub height: Option<PopupSize>,
    pub frame: Frame,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PopupSize {
    Cells(u16),
    Percent(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurface {
    pub boot_id: String,
    pub projection_revision: u64,
    pub surface_revision: u64,
    pub frame: Frame,
    pub panes: Vec<SurfacePane>,
    pub splits: Vec<Split>,
    pub popup: Option<Box<PopupSurface>>,
    pub graphics: GraphicsScene,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchRow {
    pub x: u16,
    pub y: u16,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePatch {
    pub boot_id: String,
    pub projection_revision: u64,
    pub base_surface_revision: u64,
    pub surface_revision: u64,
    pub rows: Vec<PatchRow>,
    pub panes: Vec<SurfacePane>,
    pub cursor: Option<Cursor>,
}

/// Apply only to the exact committed baseline; on any mismatch the caller resyncs.
pub fn apply_patch(surface: &PaneSurface, patch: &PanePatch) -> Result<PaneSurface, ClassicError> {
    if patch.boot_id != surface.boot_id
        || patch.projection_revision != surface.projection_revision
        || patch.base_surface_revision != surface.surface_revision
        || patch.surface_revision != surface.surface_revision.saturating_add(1)
    {
        return Err(ClassicError::stale("surface patch does not match the committed baseline"));
    }
    let mut next = surface.clone();
    let width = usize::from(next.frame.width);
    for row in &patch.rows {
        let end = usize::from(row.x).checked_add(row.cells.len());
        if row.y >= next.frame.height || row.cells.is_empty() || end.is_none_or(|end| end > width) {
            return Err(ClassicError::protocol("surface patch row exceeds the baseline"));
        }
        let start = usize::from(row.y) * width + usize::from(row.x);
        next.frame.cells[start..start + row.cells.len()].clone_from_slice(&row.cells);
    }
    for updated in &patch.panes {
        let existing = next
            .panes
            .iter_mut()
            .find(|pane| pane.pane_id == updated.pane_id)
            .ok_or_else(|| ClassicError::stale("surface patch names an uncommitted pane"))?;
        *existing = updated.clone();
    }
    next.frame.cursor = patch.cursor.clone();
    next.surface_revision = patch.surface_revision;
    validate_frame(&next.frame)?;
    for pane in &next.panes {
        validate_rect(pane.inner_rect, next.frame.width, next.frame.height)?;
    }
    Ok(next)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    Char(char),
    F(u8),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsKeyRecord {
    pub key_down: bool,
    pub repeat_count: u16,
    pub virtual_key_code: u16,
    pub virtual_scan_code: u16,
    pub unicode: u16,
    pub control_key_state: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MousePosition {
    Cell { column: u16, row: u16 },
    Pixels { x: u32, y: u32, column: u16, row: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseGeometry {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneInputEvent {
    Key {
        code: KeyCode,
        modifiers: u8,
        kind: KeyKind,
        repeat_count: u16,
        shifted_codepoint: Option<u32>,
        generated_text: Option<String>,
        tracks_release: bool,
        physical_key_id: Option<u32>,
        windows_record: Option<WindowsKeyRecord>,
    },
    TextCommit(String),
    Mouse {
        kind: MouseKind,
        position: MousePosition,
        geometry: Option<MouseGeometry>,
        modifiers: u8,
        lines: u16,
    },
    Paste(String),
}

impl PaneInputEvent {
    pub fn key(code: KeyCode, modifiers: u8) -> Self {
        Self::Key {
            code,
            modifiers,
            kind: KeyKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: false,
            physical_key_id: None,
            windows_record: None,
        }
    }
}

#[derive(Serialize)]
struct ControlPayload {
    kind: String,
    data: String,
}

#[derive(Deserialize)]
struct ControlPayloadOwned {
    kind: String,
    data: String,
}

#[derive(Serialize)]
struct InputPayload {
    pane_id: String,
    events: Vec<PaneInputEvent>,
}

#[derive(Serialize)]
struct RequestPayload {
    boot_id: String,
    request: String,
}

#[derive(Serialize, Deserialize)]
struct ResponsePayload {
    boot_id: String,
    request_id: String,
    final_chunk: bool,
    data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WireMessage {
    Control { kind: String, data: String },
    Surface(PaneSurface),
    Response { boot_id: String, request_id: String, final_chunk: bool, data: Vec<u8> },
    Patch(PanePatch),
    Other(u32),
}

fn config() -> impl bincode::config::Config {
    bincode::config::standard().with_limit::<MAX_GRAPHICS_FRAME>()
}

fn encode_payload<T: Serialize>(tag: u32, payload: &T) -> Result<Vec<u8>, ClassicError> {
    let mut bytes = bincode::encode_to_vec(tag, bincode::config::standard())
        .map_err(|error| ClassicError::protocol(error.to_string()))?;
    let mut payload = bincode::serde::encode_to_vec(payload, bincode::config::standard())
        .map_err(|error| ClassicError::protocol(error.to_string()))?;
    bytes.append(&mut payload);
    Ok(bytes)
}

fn decode_payload<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, ClassicError> {
    let (value, consumed) = bincode::serde::decode_from_slice(bytes, config())
        .map_err(|error| ClassicError::protocol(format!("invalid bincode payload: {error}")))?;
    if consumed != bytes.len() {
        return Err(ClassicError::protocol("trailing bytes in bincode payload"));
    }
    Ok(value)
}

pub fn encode_control(kind: &str, data: String) -> Result<Vec<u8>, ClassicError> {
    encode_payload(CLIENT_ENDPOINT_CONTROL_TAG, &ControlPayload { kind: kind.into(), data })
}

pub fn encode_input(pane_id: &str, events: Vec<PaneInputEvent>) -> Result<Vec<u8>, ClassicError> {
    encode_payload(CLIENT_SHELL_PANE_INPUT_TAG, &InputPayload { pane_id: pane_id.into(), events })
}

pub fn encode_request(boot_id: &str, request: String) -> Result<Vec<u8>, ClassicError> {
    encode_payload(CLIENT_SHELL_ENDPOINT_REQUEST_TAG, &RequestPayload { boot_id: boot_id.into(), request })
}

pub fn decode_message(bytes: &[u8]) -> Result<WireMessage, ClassicError> {
    let (tag, consumed) = bincode::decode_from_slice::<u32, _>(bytes, config())
        .map_err(|error| ClassicError::protocol(format!("invalid message tag: {error}")))?;
    let payload = &bytes[consumed..];
    match tag {
        SERVER_ENDPOINT_CONTROL_TAG => {
            let value: ControlPayloadOwned = decode_payload(payload)?;
            Ok(WireMessage::Control { kind: value.kind, data: value.data })
        }
        SERVER_PANE_SURFACE_TAG => Ok(WireMessage::Surface(decode_payload(payload)?)),
        SERVER_ENDPOINT_RESPONSE_TAG => {
            let value: ResponsePayload = decode_payload(payload)?;
            Ok(WireMessage::Response {
                boot_id: value.boot_id,
                request_id: value.request_id,
                final_chunk: value.final_chunk,
                data: value.data,
            })
        }
        // Core v1 patch against one committed full surface (optional reuse/delta/scroll stay off).
        SERVER_PANE_PATCH_TAG => Ok(WireMessage::Patch(decode_payload(payload)?)),
        other => Ok(WireMessage::Other(other)),
    }
}

pub fn validate_surface(surface: &PaneSurface, wire_bytes: usize) -> Result<(), ClassicError> {
    let graphics_frame = !surface.graphics.assets.is_empty()
        || !surface.graphics.placements.is_empty()
        || !surface.graphics.retained_assets.is_empty();
    if wire_bytes > if graphics_frame { MAX_GRAPHICS_FRAME } else { MAX_FRAME } {
        return Err(ClassicError::new("classic_bound", "non-graphics surface exceeds 2 MiB"));
    }
    validate_frame(&surface.frame)?;
    if let Some(popup) = &surface.popup {
        validate_frame(&popup.frame)?;
    }
    if !surface.frame.graphics.is_empty()
        || surface.popup.as_ref().is_some_and(|popup| !popup.frame.graphics.is_empty())
    {
        return Err(ClassicError::new(
            "unsupported_classic_capability",
            "direct terminal graphics were not negotiated",
        ));
    }
    for pane in &surface.panes {
        validate_rect(pane.rect, surface.frame.width, surface.frame.height)?;
        validate_rect(pane.inner_rect, surface.frame.width, surface.frame.height)?;
    }
    for asset in &surface.graphics.assets {
        validate_asset(asset)?;
    }
    let dimensions: HashMap<GraphicsAssetKey, (u32, u32)> = surface
        .graphics
        .assets
        .iter()
        .map(|asset| (asset.key.clone(), (asset.key.image_width, asset.key.image_height)))
        .collect();
    for placement in &surface.graphics.placements {
        if placement.cols == 0 || placement.rows == 0 {
            return Err(ClassicError::protocol("zero-sized graphics placement"));
        }
        // Renderers scale by the source crop; an empty crop is malformed, never "whole image".
        if placement.source_width == 0 || placement.source_height == 0 {
            return Err(ClassicError::protocol("zero-sized graphics source crop"));
        }
        let Some(&(width, height)) = dimensions.get(&placement.asset) else {
            // Previously cached dimensions are checked by the session cache.
            continue;
        };
        let right = placement
            .source_x
            .checked_add(placement.source_width)
            .ok_or_else(|| ClassicError::protocol("graphics crop overflow"))?;
        let bottom = placement
            .source_y
            .checked_add(placement.source_height)
            .ok_or_else(|| ClassicError::protocol("graphics crop overflow"))?;
        if right > width || bottom > height {
            return Err(ClassicError::protocol("graphics crop exceeds source image"));
        }
    }
    Ok(())
}

fn validate_frame(frame: &Frame) -> Result<(), ClassicError> {
    let expected = usize::from(frame.width)
        .checked_mul(usize::from(frame.height))
        .ok_or_else(|| ClassicError::new("classic_bound", "surface dimensions overflow"))?;
    if expected > MAX_CELLS || frame.cells.len() != expected {
        return Err(ClassicError::protocol("surface cell count does not match bounded dimensions"));
    }
    if let Some(cursor) = &frame.cursor
        && cursor.visible
        && (cursor.x >= frame.width || cursor.y >= frame.height)
    {
        return Err(ClassicError::protocol("cursor lies outside the surface"));
    }
    for cell in &frame.cells {
        // Empty symbols are real continuation columns: classic emits a wide glyph's
        // SpacerTail as `symbol: ""` with `skip: false`.
        if cell.symbol.len() > 1024 {
            return Err(ClassicError::protocol("invalid cell symbol"));
        }
        if cell.hyperlink.is_some_and(|index| index as usize >= frame.hyperlinks.len()) {
            return Err(ClassicError::protocol("cell hyperlink index is invalid"));
        }
    }
    Ok(())
}

fn validate_rect(rect: Rect, width: u16, height: u16) -> Result<(), ClassicError> {
    if rect.x.checked_add(rect.width).is_none_or(|end| end > width)
        || rect.y.checked_add(rect.height).is_none_or(|end| end > height)
    {
        return Err(ClassicError::protocol("pane rectangle lies outside the surface"));
    }
    Ok(())
}

fn validate_asset(asset: &GraphicsAsset) -> Result<(), ClassicError> {
    let key = &asset.key;
    if asset.data.len() > MAX_IMAGE_BYTES || asset.data.len() as u64 != key.data_len {
        return Err(ClassicError::new("classic_bound", "image byte length is invalid or exceeds 16 MiB"));
    }
    let pixels = u64::from(key.image_width) * u64::from(key.image_height);
    if key.image_width == 0 || key.image_height == 0 || pixels > MAX_IMAGE_PIXELS {
        return Err(ClassicError::new("classic_bound", "image dimensions exceed the 16-megapixel bound"));
    }
    match key.format {
        GraphicsFormat::Rgb | GraphicsFormat::Rgba => {
            let channels = if key.format == GraphicsFormat::Rgb { 3 } else { 4 };
            let expected = usize::try_from(pixels * channels)
                .map_err(|_| ClassicError::new("classic_bound", "raw image size overflow"))?;
            if asset.data.len() != expected {
                return Err(ClassicError::protocol("raw image byte count does not match its dimensions"));
            }
        }
        GraphicsFormat::Png => {
            if asset.data.len() < 24 || &asset.data[..8] != b"\x89PNG\r\n\x1a\n" || &asset.data[12..16] != b"IHDR" {
                return Err(ClassicError::protocol("graphics asset is not a PNG with an IHDR header"));
            }
            let width = u32::from_be_bytes(asset.data[16..20].try_into().expect("four bytes"));
            let height = u32::from_be_bytes(asset.data[20..24].try_into().expect("four bytes"));
            if (width, height) != (key.image_width, key.image_height) {
                return Err(ClassicError::protocol("PNG dimensions do not match its asset key"));
            }
        }
    }
    Ok(())
}

pub fn request_json(id: &str, method: &str, params: Value) -> Result<String, ClassicError> {
    if id.len() > 128 || method.len() > 128 {
        return Err(ClassicError::new("classic_bound", "endpoint request identity is too long"));
    }
    let request = serde_json::to_string(&serde_json::json!({"id": id, "method": method, "params": params}))
        .map_err(|error| ClassicError::protocol(error.to_string()))?;
    if request.len() > MAX_COMMAND_BYTES {
        return Err(ClassicError::new("classic_bound", "endpoint request exceeds 1 MiB"));
    }
    Ok(request)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn cell(symbol: &str) -> Cell {
        Cell { symbol: symbol.into(), fg: 0x02ff_ffff, bg: 0, modifier: 0, skip: false, hyperlink: None }
    }

    pub(crate) fn surface() -> PaneSurface {
        // Rows: [界][tail] / [🙂][tail], exactly as classic encodes wide glyphs.
        PaneSurface {
            boot_id: "boot-a".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: Frame {
                cells: vec![cell("界"), cell(""), cell("🙂"), cell("")],
                width: 2,
                height: 2,
                cursor: Some(Cursor { x: 0, y: 1, visible: true, shape: 6 }),
                hyperlinks: vec![],
                graphics: vec![],
            },
            panes: vec![],
            splits: vec![],
            popup: None,
            graphics: GraphicsScene::default(),
        }
    }

    #[test]
    fn stable_hello_uses_frozen_control_tag_and_standard_bincode() {
        let data = serde_json::to_string(&ClientHello::mobile(80, 24)).unwrap();
        let encoded = encode_control(HELLO_KIND, data).unwrap();
        assert_eq!(encoded[0], 20);
        assert_eq!(encoded[1], HELLO_KIND.len() as u8);
        assert_eq!(&encoded[2..2 + HELLO_KIND.len()], HELLO_KIND.as_bytes());
        let WireMessage::Control { kind, .. } = decode_message(&encoded).unwrap() else { panic!("control") };
        assert_eq!(kind, HELLO_KIND);
    }

    #[test]
    fn semantic_text_paste_and_key_have_distinct_frozen_variants() {
        let text = encode_input("w:p", vec![PaneInputEvent::TextCommit("é".into())]).unwrap();
        assert_eq!(&text[..4], &[13, 3, b'w', b':']);
        assert_eq!(text[6], 1);
        let paste = encode_input("w:p", vec![PaneInputEvent::Paste("é".into())]).unwrap();
        assert_eq!(paste[6], 3);
        let key = encode_input("w:p", vec![PaneInputEvent::key(KeyCode::Enter, 0)]).unwrap();
        assert_eq!(&key[6..8], &[0, 1]);
        assert!(key.ends_with(&[0, 0, 1, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn full_surface_round_trip_rejects_trailing_bytes() {
        let bytes = encode_payload(SERVER_PANE_SURFACE_TAG, &surface()).unwrap();
        let WireMessage::Surface(decoded) = decode_message(&bytes).unwrap() else { panic!("surface") };
        assert_eq!(decoded, surface());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_message(&trailing).is_err());
    }

    #[test]
    fn wide_spacer_tail_is_independently_encoded_and_accepted() {
        // Hand-encoded bincode standard layout of classic's SpacerTail cell:
        // symbol "" (len 0), fg 0, bg 0, modifier 0, skip false, hyperlink None.
        let tail = Cell { symbol: String::new(), fg: 0, bg: 0, modifier: 0, skip: false, hyperlink: None };
        assert_eq!(bincode::serde::encode_to_vec(&tail, bincode::config::standard()).unwrap(), [0, 0, 0, 0, 0, 0]);
        // "界" (3 UTF-8 bytes), fg 0x02_74_d3_de (u32 varint tag 252), bg 0, mod 0.
        let wide = Cell { symbol: "界".into(), fg: 0x0274_d3de, bg: 0, modifier: 0, skip: false, hyperlink: None };
        let mut expected = vec![3, 0xe7, 0x95, 0x8c, 252];
        expected.extend_from_slice(&0x0274_d3de_u32.to_le_bytes());
        expected.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(bincode::serde::encode_to_vec(&wide, bincode::config::standard()).unwrap(), expected);
        let mut grid = surface();
        grid.frame.cells = vec![wide, tail.clone(), cell("🙂"), tail];
        validate_surface(&grid, 100).unwrap();
    }

    #[test]
    fn valid_png_dimensions_and_raw_lengths_are_enforced() {
        // A real, decodable 1x1 RGBA PNG, not merely a signature fixture.
        let png = include_bytes!("fixtures/one.png").as_slice();
        let key = GraphicsAssetKey {
            source: GraphicsSource::PaneLayer { pane_id: "p".into(), layer_id: "l".into() },
            image_width: 1,
            image_height: 1,
            format: GraphicsFormat::Png,
            data_len: png.len() as u64,
            data_fingerprint: 1,
        };
        validate_asset(&GraphicsAsset { key: key.clone(), data: png.to_vec() }).unwrap();
        let mut bad = GraphicsAsset { key, data: png.to_vec() };
        bad.key.image_width = 2;
        assert!(validate_asset(&bad).is_err());
    }

    pub(crate) fn encode_surface(surface: &PaneSurface) -> Vec<u8> {
        encode_payload(SERVER_PANE_SURFACE_TAG, surface).unwrap()
    }

    pub(crate) fn encode_patch(patch: &PanePatch) -> Vec<u8> {
        encode_payload(SERVER_PANE_PATCH_TAG, patch).unwrap()
    }

    pub(crate) fn encode_response(boot_id: &str, request_id: &str, final_chunk: bool, data: Vec<u8>) -> Vec<u8> {
        encode_payload(
            SERVER_ENDPOINT_RESPONSE_TAG,
            &ResponsePayload { boot_id: boot_id.into(), request_id: request_id.into(), final_chunk, data },
        )
        .unwrap()
    }

    #[test]
    fn zero_sized_source_crops_are_rejected_for_new_and_cached_assets() {
        let png = include_bytes!("fixtures/one.png").as_slice();
        let key = GraphicsAssetKey {
            source: GraphicsSource::PaneLayer { pane_id: "p".into(), layer_id: "l".into() },
            image_width: 1,
            image_height: 1,
            format: GraphicsFormat::Png,
            data_len: png.len() as u64,
            data_fingerprint: 1,
        };
        let placement = GraphicsPlacement {
            asset: key.clone(),
            logical_placement_id: 1,
            x: 0,
            y: 0,
            cols: 1,
            rows: 1,
            source_x: 0,
            source_y: 0,
            source_width: 1,
            source_height: 1,
            x_offset: 0,
            y_offset: 0,
            z: 0,
            scrollback_offset: 0,
        };
        let mut valid = surface();
        valid.graphics.assets = vec![GraphicsAsset { key, data: png.to_vec() }];
        valid.graphics.placements = vec![placement];
        validate_surface(&valid, 100).unwrap();
        for (width, height) in [(0, 1), (1, 0), (0, 0)] {
            let mut fresh = valid.clone();
            fresh.graphics.placements[0].source_width = width;
            fresh.graphics.placements[0].source_height = height;
            assert!(validate_surface(&fresh, 100).unwrap_err().message.contains("source crop"));
            // A placement against a previously cached asset (bytes not retransmitted).
            let mut cached = fresh.clone();
            cached.graphics.assets.clear();
            assert!(validate_surface(&cached, 100).unwrap_err().message.contains("source crop"));
        }
    }

    #[test]
    fn malformed_grids_and_cursor_are_rejected() {
        let mut invalid = surface();
        invalid.frame.cells.pop();
        assert!(validate_surface(&invalid, 100).is_err());
        let mut invalid = surface();
        invalid.frame.cursor.as_mut().unwrap().x = 2;
        assert!(validate_surface(&invalid, 100).is_err());
    }
}
