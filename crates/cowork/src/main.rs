use std::{
    borrow::Cow,
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    io::Read as _,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use agent::{Agent as StreamingAgent, AgentEvent};
use anyhow::Context as _;
use base64::Engine as _;
use draft::{
    AttachmentId, AttachmentKind, AttachmentRecord, CommentTarget, Draft, DraftItem, DraftItemKind,
    ItemId, TextEdit,
};
use gpui::{
    Animation, AnimationExt, App, AppContext, AssetSource, AsyncApp, Bounds, ClipboardEntry,
    ClipboardItem, Context, Entity, EntityId, EntityInputHandler as _, ExternalPaths, Focusable,
    FontStyle, FontWeight, FutureExt, HighlightStyle, IntoElement, KeyBinding, KeyDownEvent,
    LineFragment, MouseButton, MouseDownEvent, MouseUpEvent, PathPromptOptions, PlatformInput,
    QuitMode, Render, ScrollHandle, ScrollWheelEvent, SharedString, Subscription, TextRun,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowControlArea, WindowOptions, actions,
    canvas, div, img, point, prelude::*, px, rems, rgb, rgba, size,
};
use gpui_base::{
    GlobalState, SelectableText, TextSelection, TextView, TextViewDefaults, TextViewState,
    TextViewStyle, Textarea,
    input::{
        Backspace, Escape, Input, InputEditorStyle, InputEvent, InputState, MoveDown, MoveUp,
        Paste, TextareaState,
    },
    text::{CodeBlock, SelectionFormat},
};
use gpui_component::{
    Collapsible, Disableable as _, Icon, Root, Sizable as _, ThemeMode, WindowExt as _,
    attachment::{
        Attachment, AttachmentActions, AttachmentContent, AttachmentDescription, AttachmentMedia,
        AttachmentStatus, AttachmentTitle,
    },
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    dialog::{DialogDescription, DialogFooter, DialogHeader, DialogTitle},
    progress::Progress,
    searchable_list::{SearchableGroup, SearchableListItem, SearchableVec},
    shimmer::ShimmerText,
    sidebar::{
        Sidebar, SidebarCollapsible, SidebarItem, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
    },
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName as AssetIconName;
use iroh::{
    Endpoint, EndpointId,
    endpoint::{Accepting, Connection, presets},
};
use itertools::Itertools;
use participant::ParticipantId;
use rig::{
    completion::{
        Message as RigMessage,
        message::{ImageMediaType, UserContent},
    },
    prelude::*,
    providers::ollama::wire::Ollama,
    streaming::{BlockClose, Delta, StreamEvent},
    tool::ToolSet,
};
use serde_json::json;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle as SyntectFontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tokio::{
    runtime::Runtime,
    sync::{broadcast, mpsc},
};
use tools::{RespondToComment, RespondToCommentArgs, TurnComments};
use uuid::Uuid;

mod participant;
mod protocol;

const SIDEBAR_WIDTH: gpui::Pixels = px(275.);
const TOP_BAR_HEIGHT: gpui::Pixels = px(40.);
const BOTTOM_BAR_DIVIDER_THRESHOLD: gpui::Pixels = px(24.);
/// `SidebarToggleButton` is a small icon button (`size_6`, 24px) centered in
/// the top bar; matching its top gap on the left keeps it evenly inset from
/// the window corner.
const SIDEBAR_TOGGLE_INSET: gpui::Pixels = px((40. - 24.) / 2.);
const MACOS_TRAFFIC_LIGHT_X_INSET: gpui::Pixels = px(12.);
const MACOS_TRAFFIC_LIGHT_SIZE: gpui::Pixels = px(14.);
const MACOS_TRAFFIC_LIGHT_SPACING: gpui::Pixels = px(6.);
const MACOS_TRAFFIC_LIGHT_TRAILING_GAP: gpui::Pixels = px(12.);
const OLLAMA_MODEL: &str = "qwen3.8:27b";
const OLLAMA_CONTEXT_TOKENS: u64 = 16 * 8_192;
const OLLAMA_AVATAR_PATH: &str = "providers/ollama.png";
const USER_ACCENT: u32 = 0xe26d5a;
const COWORK_ALPN: &[u8] = b"cowork/0";
const ENDPOINT_ID_TEXT_LENGTH: usize = EndpointId::LENGTH * 2;
/// How long any single step of the collaboration handshake may take.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);
/// How many thread events a collaborator may fall behind before the host
/// re-bases it on a fresh snapshot instead of a delta.
const THREAD_EVENT_CAPACITY: usize = 1024;

static TOKIO_RUNTIME: OnceLock<Runtime> = OnceLock::new();
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SYNTAX_THEME: OnceLock<Option<Theme>> = OnceLock::new();

/// Largest file or clipboard image we will read before inspecting it. Larger
/// than the image limit so uncompressed screenshots (BMP, TIFF) can still be
/// converted to PNG.
const MAX_ATTACHMENT_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest encoded image sent to the model, in line with common provider limits.
const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// Text is inlined into the prompt, so keep it well within `OLLAMA_CONTEXT_TOKENS`.
const MAX_TEXT_ATTACHMENT_BYTES: u64 = 256 * 1024;
/// Largest total of the attachments submitted together.
const MAX_MESSAGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone)]
struct FileAttachment {
    name: String,
    content: FileAttachmentContent,
}

/// Images are held as `gpui::Image` so thumbnails are decoded and cached once
/// rather than rehashed on every frame.
#[derive(Clone)]
enum FileAttachmentContent {
    Text(String),
    Png(Arc<gpui::Image>),
    Jpeg(Arc<gpui::Image>),
}

impl FileAttachment {
    fn len(&self) -> u64 {
        match &self.content {
            FileAttachmentContent::Text(text) => text.len() as u64,
            FileAttachmentContent::Png(image) | FileAttachmentContent::Jpeg(image) => {
                image.bytes().len() as u64
            }
        }
    }

    /// The file's bytes as they travel between participants.
    fn bytes(&self) -> &[u8] {
        match &self.content {
            FileAttachmentContent::Text(text) => text.as_bytes(),
            FileAttachmentContent::Png(image) | FileAttachmentContent::Jpeg(image) => image.bytes(),
        }
    }

    fn kind(&self) -> AttachmentKind {
        match self.content {
            FileAttachmentContent::Text(_) => AttachmentKind::Text,
            FileAttachmentContent::Png(_) => AttachmentKind::Png,
            FileAttachmentContent::Jpeg(_) => AttachmentKind::Jpeg,
        }
    }

    /// Rebuilds a file another participant sent.
    fn from_bytes(name: String, kind: AttachmentKind, bytes: Vec<u8>) -> anyhow::Result<Self> {
        let content =
            match kind {
                AttachmentKind::Text => FileAttachmentContent::Text(
                    String::from_utf8(bytes).context("A text attachment is not UTF-8.")?,
                ),
                AttachmentKind::Png => FileAttachmentContent::Png(Arc::new(
                    gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes),
                )),
                AttachmentKind::Jpeg => FileAttachmentContent::Jpeg(Arc::new(
                    gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes),
                )),
            };
        Ok(Self { name, content })
    }
}

/// The largest file of a kind anyone may attach.
fn max_attachment_size(kind: AttachmentKind) -> u64 {
    match kind {
        AttachmentKind::Text => MAX_TEXT_ATTACHMENT_BYTES,
        AttachmentKind::Png | AttachmentKind::Jpeg => MAX_IMAGE_ATTACHMENT_BYTES,
    }
}

fn kind_to_protocol(kind: AttachmentKind) -> protocol::AttachmentKind {
    match kind {
        AttachmentKind::Text => protocol::AttachmentKind::Text,
        AttachmentKind::Png => protocol::AttachmentKind::Png,
        AttachmentKind::Jpeg => protocol::AttachmentKind::Jpeg,
    }
}

fn kind_from_protocol(kind: protocol::AttachmentKind) -> AttachmentKind {
    match kind {
        protocol::AttachmentKind::Text => AttachmentKind::Text,
        protocol::AttachmentKind::Png => AttachmentKind::Png,
        protocol::AttachmentKind::Jpeg => AttachmentKind::Jpeg,
    }
}

fn record_to_protocol(record: &AttachmentRecord) -> protocol::AttachmentRef {
    protocol::AttachmentRef {
        id: record.id.as_uuid().into_bytes(),
        name: record.name.clone(),
        kind: kind_to_protocol(record.kind),
        size: record.size,
        creator: record.creator.into_bytes(),
    }
}

fn record_from_protocol(record: protocol::AttachmentRef) -> AttachmentRecord {
    AttachmentRecord {
        id: AttachmentId::from_uuid(Uuid::from_bytes(record.id)),
        name: record.name,
        kind: kind_from_protocol(record.kind),
        size: record.size,
        creator: Uuid::from_bytes(record.creator),
    }
}

/// A file whose bytes are still arriving from another participant.
struct IncomingFile {
    name: String,
    kind: AttachmentKind,
    total: u64,
    bytes: Vec<u8>,
    /// Who is sending it; `None` when it comes from the host.
    uploader: Option<ParticipantId>,
}

impl IncomingFile {
    fn progress(&self) -> f32 {
        if self.total == 0 {
            return 100.;
        }
        (self.bytes.len() as f64 / self.total as f64 * 100.) as f32
    }
}

enum AttachmentSource {
    Path(PathBuf),
    ClipboardImage(gpui::Image),
}

impl AttachmentSource {
    fn name(&self) -> String {
        match self {
            Self::Path(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            Self::ClipboardImage(_) => "Pasted image".to_owned(),
        }
    }

    fn looks_like_image(&self) -> bool {
        match self {
            Self::Path(path) => image::ImageFormat::from_path(path).is_ok(),
            Self::ClipboardImage(_) => true,
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn load_attachment(
    source: AttachmentSource,
    on_progress: impl FnMut(f32),
) -> anyhow::Result<FileAttachment> {
    let name = source.name();
    match source {
        AttachmentSource::Path(path) => {
            let bytes = read_attachment_file(&path, &name, on_progress)?;
            attachment_from_bytes(name, bytes)
        }
        AttachmentSource::ClipboardImage(image) => {
            anyhow::ensure!(
                image.bytes.len() as u64 <= MAX_ATTACHMENT_SOURCE_BYTES,
                "The pasted image is larger than {}",
                format_bytes(MAX_ATTACHMENT_SOURCE_BYTES)
            );
            let content = image_content(image.bytes)
                .map_err(|_| anyhow::anyhow!("The pasted image format is not supported"))?;
            let extension = match content {
                FileAttachmentContent::Jpeg(_) => "jpg",
                _ => "png",
            };
            let attachment = FileAttachment {
                name: format!("{name}.{extension}"),
                content,
            };
            ensure_image_size(&attachment)?;
            Ok(attachment)
        }
    }
}

fn read_attachment_file(
    path: &Path,
    name: &str,
    mut on_progress: impl FnMut(f32),
) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("Cannot read {name}"))?;
    let total = file.metadata().ok().map(|metadata| metadata.len());
    let too_large = || {
        anyhow::anyhow!(
            "{name} is larger than {}",
            format_bytes(MAX_ATTACHMENT_SOURCE_BYTES)
        )
    };
    if total.is_some_and(|total| total > MAX_ATTACHMENT_SOURCE_BYTES) {
        return Err(too_large());
    }
    // Read one byte past the limit so files that grew after `metadata` are caught.
    let mut file = file.take(MAX_ATTACHMENT_SOURCE_BYTES + 1);
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buffer = [0; 256 * 1024];
    let mut last_progress = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Cannot read {name}"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > MAX_ATTACHMENT_SOURCE_BYTES {
            return Err(too_large());
        }
        if let Some(total) = total.filter(|total| *total > 0) {
            let progress = ((bytes.len() as f64 / total as f64) * 100.).min(100.) as u32;
            if progress > last_progress {
                last_progress = progress;
                on_progress(progress as f32);
            }
        }
    }
    Ok(bytes)
}

/// Classifies an attachment by its content rather than its extension: images
/// are recognized by their signature, anything else must be UTF-8 text.
fn attachment_from_bytes(name: String, bytes: Vec<u8>) -> anyhow::Result<FileAttachment> {
    match image_content(bytes) {
        Ok(content) => {
            let attachment = FileAttachment { name, content };
            ensure_image_size(&attachment)?;
            Ok(attachment)
        }
        Err(bytes) => {
            let text = String::from_utf8(bytes)
                .ok()
                .filter(|text| !text.contains('\0'))
                .with_context(|| format!("{name} is not a text file or a supported image"))?;
            anyhow::ensure!(
                text.len() as u64 <= MAX_TEXT_ATTACHMENT_BYTES,
                "{name} is {}; text files can be at most {}",
                format_bytes(text.len() as u64),
                format_bytes(MAX_TEXT_ATTACHMENT_BYTES)
            );
            Ok(FileAttachment {
                name,
                content: FileAttachmentContent::Text(text),
            })
        }
    }
}

fn ensure_image_size(attachment: &FileAttachment) -> anyhow::Result<()> {
    anyhow::ensure!(
        attachment.len() <= MAX_IMAGE_ATTACHMENT_BYTES,
        "{} is {}; images can be at most {}",
        attachment.name,
        format_bytes(attachment.len()),
        format_bytes(MAX_IMAGE_ATTACHMENT_BYTES)
    );
    Ok(())
}

/// Keeps PNG and JPEG as they are and converts other common image formats to
/// PNG. Hands the bytes back when they are not a supported image.
fn image_content(bytes: Vec<u8>) -> Result<FileAttachmentContent, Vec<u8>> {
    let format = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => {
            return Ok(FileAttachmentContent::Png(Arc::new(
                gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes),
            )));
        }
        Ok(image::ImageFormat::Jpeg) => {
            return Ok(FileAttachmentContent::Jpeg(Arc::new(
                gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes),
            )));
        }
        // Signatures like BMP's "BM" can also start a text file, so these
        // only count as images if they actually decode.
        Ok(
            format @ (image::ImageFormat::Gif
            | image::ImageFormat::WebP
            | image::ImageFormat::Bmp
            | image::ImageFormat::Tiff),
        ) => format,
        _ => return Err(bytes),
    };
    let Ok(decoded) = image::load_from_memory_with_format(&bytes, format) else {
        return Err(bytes);
    };
    let mut png = Vec::new();
    if decoded
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .is_err()
    {
        return Err(bytes);
    }
    Ok(FileAttachmentContent::Png(Arc::new(
        gpui::Image::from_bytes(gpui::ImageFormat::Png, png),
    )))
}

/// Files copied in a file manager are attached, as are images unless the
/// clipboard also holds text (spreadsheets put a rendered image next to the
/// cells, for example). Everything else is left to the regular text paste.
fn clipboard_attachment_sources(item: &ClipboardItem) -> Vec<AttachmentSource> {
    let paths = item
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
            _ => None,
        })
        .flatten()
        .cloned()
        .map(AttachmentSource::Path)
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        return paths;
    }
    let has_text = item.entries().iter().any(
        |entry| matches!(entry, ClipboardEntry::String(text) if !text.text().trim().is_empty()),
    );
    if has_text {
        return Vec::new();
    }
    item.entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(AttachmentSource::ClipboardImage(image.clone())),
            _ => None,
        })
        .collect()
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The user message sent to the agent for one submission: the comment
/// instructions, then every prompt block under its creator's name, each
/// followed by its own attachments, whose bytes come from `files`.
fn agent_message(
    preface: Option<&str>,
    blocks: &[PromptBlock],
    files: &HashMap<AttachmentId, FileAttachment>,
) -> RigMessage {
    let mut content = preface
        .map(UserContent::text)
        .into_iter()
        .collect::<Vec<_>>();
    for block in blocks {
        content.push(UserContent::text(format!(
            "{}:\n{}",
            block.author.display_name(),
            block.text
        )));
        content.extend(
            block
                .attachments
                .iter()
                .filter_map(|record| files.get(&record.id))
                .map(attachment_content),
        );
    }
    RigMessage::User { content }
}

fn attachment_content(attachment: &FileAttachment) -> UserContent {
    match &attachment.content {
        FileAttachmentContent::Text(body) => UserContent::text(format!(
            "<file name=\"{}\">\n{body}\n</file>",
            escape_xml_attribute(&attachment.name)
        )),
        FileAttachmentContent::Png(image) => UserContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(image.bytes()),
            Some(ImageMediaType::PNG),
            None,
        ),
        FileAttachmentContent::Jpeg(image) => UserContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(image.bytes()),
            Some(ImageMediaType::JPEG),
            None,
        ),
    }
}

/// When the caret is on the first visual line of `editor` (or the last, with
/// `last`), returns its horizontal position, so moving to the neighboring
/// editor can keep it.
fn caret_x_on_edge_line(editor: &TextareaState, last: bool) -> Option<gpui::Pixels> {
    let caret = editor.cursor();
    let edge = if last { editor.value().len() } else { 0 };
    // Without a layout there is nothing to compare, and the edge is as good a
    // guess as any.
    let Some(caret_bounds) = editor.range_to_bounds(&(caret..caret)) else {
        return Some(px(0.));
    };
    let Some(edge_bounds) = editor.range_to_bounds(&(edge..edge)) else {
        return Some(caret_bounds.left());
    };
    same_visual_line(caret_bounds.top(), edge_bounds.top()).then_some(caret_bounds.left())
}

/// The offset on the last visual line of `editor` (or the first, without
/// `last`) horizontally closest to `x`.
fn offset_near_x(editor: &TextareaState, x: gpui::Pixels, last: bool) -> usize {
    let text = editor.value();
    let edge = if last { text.len() } else { 0 };
    let Some(edge_top) = editor
        .range_to_bounds(&(edge..edge))
        .map(|bounds| bounds.top())
    else {
        return edge;
    };
    let boundaries = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain([text.len()])
        .collect::<Vec<_>>();
    let on_line = |offset: &usize| {
        editor
            .range_to_bounds(&(*offset..*offset))
            .map(|bounds| (bounds.left(), same_visual_line(bounds.top(), edge_top)))
    };
    let distance = |left: gpui::Pixels| if left > x { left - x } else { x - left };
    // Walk inwards from the edge and stop at the first offset on another line.
    let candidates: Box<dyn Iterator<Item = &usize>> = if last {
        Box::new(boundaries.iter().rev())
    } else {
        Box::new(boundaries.iter())
    };
    candidates
        .map_while(|offset| {
            on_line(offset)
                .filter(|(_, same_line)| *same_line)
                .map(|(left, _)| (*offset, distance(left)))
        })
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(edge, |(offset, _)| offset)
}

/// The rectangles covering `selection` of `editor`'s text, one per visual
/// line, in window coordinates.
fn selection_rects(
    editor: &TextareaState,
    text: &str,
    selection: Range<usize>,
    text_bounds: Bounds<gpui::Pixels>,
) -> Vec<Bounds<gpui::Pixels>> {
    if selection.is_empty() {
        return Vec::new();
    }
    let mut rects = Vec::new();
    let mut line_start = selection.start;
    loop {
        let line_end = text[line_start..selection.end]
            .find('\n')
            .map_or(selection.end, |newline| line_start + newline);
        if let (Some(start), Some(end)) = (
            editor.range_to_bounds(&(line_start..line_start)),
            editor.range_to_bounds(&(line_end..line_end)),
        ) && start.size.height > px(0.)
        {
            let height = start.size.height;
            // A selected empty line still shows as selected.
            let min_width = if line_end < selection.end {
                px(4.)
            } else {
                px(0.)
            };
            if same_visual_line(start.top(), end.top()) {
                let width = (end.left() - start.left()).max(min_width);
                rects.push(Bounds::new(start.origin, size(width, height)));
            } else {
                // The line wraps: the rest of its first row, whole rows in
                // between, and its last row up to the end.
                rects.push(Bounds::new(
                    start.origin,
                    size(text_bounds.right() - start.left(), height),
                ));
                let mut top = start.top() + height;
                while top + px(1.) < end.top() {
                    rects.push(Bounds::new(
                        point(text_bounds.left(), top),
                        size(text_bounds.size.width, height),
                    ));
                    top += height;
                }
                rects.push(Bounds::new(
                    point(text_bounds.left(), end.top()),
                    size((end.left() - text_bounds.left()).max(min_width), height),
                ));
            }
        }
        if line_end >= selection.end {
            return rects;
        }
        line_start = line_end + 1;
    }
}

/// A participant's name in their color, just above their caret at `origin`.
fn paint_caret_label(
    participant: ParticipantId,
    origin: gpui::Point<gpui::Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    const FONT_SIZE: gpui::Pixels = px(10.);
    const HEIGHT: gpui::Pixels = px(14.);

    let name = SharedString::from(participant.display_name());
    let run = TextRun {
        len: name.len(),
        font: window.text_style().font(),
        color: rgb(0xf4f4f5).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(name, FONT_SIZE, &[run], None);
    let label = Bounds::new(
        point(origin.x, origin.y - HEIGHT),
        size(line.width + px(8.), HEIGHT),
    );
    window.paint_quad(gpui::fill(label, rgb(participant.color())).corner_radii(px(3.)));
    _ = line.paint(
        point(label.left() + px(4.), label.top()),
        HEIGHT,
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

fn same_visual_line(a: gpui::Pixels, b: gpui::Pixels) -> bool {
    a <= b + px(1.) && b <= a + px(1.)
}

fn endpoint_id_input_is_complete(input: &str) -> bool {
    input.trim().len() == ENDPOINT_ID_TEXT_LENGTH
}

fn macos_traffic_light_position() -> gpui::Point<gpui::Pixels> {
    point(
        MACOS_TRAFFIC_LIGHT_X_INSET,
        (TOP_BAR_HEIGHT - MACOS_TRAFFIC_LIGHT_SIZE) / 2.,
    )
}

fn macos_sidebar_toggle_margin() -> gpui::Pixels {
    MACOS_TRAFFIC_LIGHT_X_INSET
        + MACOS_TRAFFIC_LIGHT_SIZE * 3.
        + MACOS_TRAFFIC_LIGHT_SPACING * 2.
        + MACOS_TRAFFIC_LIGHT_TRAILING_GAP
}

fn highlight_code_block(block: &CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> {
    let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme = SYNTAX_THEME.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        themes
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| themes.themes.values().next().cloned())
    });
    let Some(theme) = theme else {
        return Vec::new();
    };

    let syntax = block
        .lang()
        .and_then(|language| {
            let language = language.split_whitespace().next()?;
            syntax_set
                .find_syntax_by_token(language)
                .or_else(|| syntax_set.find_syntax_by_extension(language))
                .or_else(|| {
                    syntax_set
                        .syntaxes()
                        .iter()
                        .find(|syntax| syntax.name.eq_ignore_ascii_case(language))
                })
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let code = block.code();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut offset = 0;
    let mut highlights = Vec::new();

    for line in LinesWithEndings::from(code.as_ref()) {
        let line_highlights = match highlighter.highlight_line(line, syntax_set) {
            Ok(line_highlights) => line_highlights,
            Err(error) => {
                eprintln!(
                    "failed to highlight {syntax_name} code block: {error}",
                    syntax_name = syntax.name
                );
                return Vec::new();
            }
        };

        for (style, text) in line_highlights {
            let end = offset + text.len();
            if offset < end {
                let foreground = style.foreground;
                let color = rgba(
                    (u32::from(foreground.r) << 24)
                        | (u32::from(foreground.g) << 16)
                        | (u32::from(foreground.b) << 8)
                        | u32::from(foreground.a),
                );
                highlights.push((
                    offset..end,
                    HighlightStyle {
                        color: Some(color.into()),
                        font_weight: style
                            .font_style
                            .contains(SyntectFontStyle::BOLD)
                            .then_some(FontWeight::BOLD),
                        font_style: style
                            .font_style
                            .contains(SyntectFontStyle::ITALIC)
                            .then_some(FontStyle::Italic),
                        ..Default::default()
                    },
                ));
            }
            offset = end;
        }
    }

    highlights
}

gpui_kit_assets::icon_assets!(
    AppIconAssets,
    [
        Check,
        ChevronDown,
        ChevronRight,
        ChevronUp,
        Link,
        PanelLeftClose,
        PanelLeftOpen,
        SendHorizontal,
        Square,
        SquarePen,
        UsersRound,
        Paperclip,
        FileText,
        Image,
        X,
    ]
);

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            OLLAMA_AVATAR_PATH => Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../assets/providers/ollama.png"
            )))),
            _ => AppIconAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut assets = AppIconAssets.list(path)?;
        if OLLAMA_AVATAR_PATH.starts_with(path) {
            assets.push(OLLAMA_AVATAR_PATH.into());
        }
        Ok(assets)
    }
}

actions!(cowork, [Quit, SubmitComposer]);

#[derive(Clone, Copy)]
enum MessageAuthor {
    User(ParticipantId),
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ModelProvider {
    Ollama,
}

impl ModelProvider {
    fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
        }
    }

    fn icon_path(self) -> &'static str {
        match self {
            Self::Ollama => OLLAMA_AVATAR_PATH,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ModelSelection {
    catalog_id: &'static str,
    provider: ModelProvider,
    model: &'static str,
}

const RECOMMENDED_QWEN: ModelSelection = ModelSelection {
    catalog_id: "recommended-qwen-3.8-27b",
    provider: ModelProvider::Ollama,
    model: OLLAMA_MODEL,
};
const OLLAMA_QWEN: ModelSelection = ModelSelection {
    catalog_id: "ollama-qwen-3.8-27b",
    provider: ModelProvider::Ollama,
    model: OLLAMA_MODEL,
};
/// Every model the picker offers. Collaborators select models by catalog id,
/// so changing this catalog also requires bumping
/// [`protocol::PROTOCOL_VERSION`].
const MODEL_CATALOG: [ModelSelection; 2] = [RECOMMENDED_QWEN, OLLAMA_QWEN];
const DEFAULT_MODEL: ModelSelection = RECOMMENDED_QWEN;

impl ModelSelection {
    fn from_catalog_id(catalog_id: &str) -> Option<Self> {
        MODEL_CATALOG
            .into_iter()
            .find(|model| model.catalog_id == catalog_id)
    }
}

#[derive(Clone)]
struct LanguageModel {
    name: SharedString,
    selection: ModelSelection,
}

impl LanguageModel {
    fn new(name: impl Into<SharedString>, selection: ModelSelection) -> Self {
        Self {
            name: name.into(),
            selection,
        }
    }
}

impl SearchableListItem for LanguageModel {
    type Value = ModelSelection;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                img(self.selection.provider.icon_path())
                    .size(px(18.))
                    .rounded(px(4.)),
            )
            .child(self.name.clone())
    }

    fn value(&self) -> &Self::Value {
        &self.selection
    }

    fn matches(&self, query: &str) -> bool {
        self.name.to_lowercase().contains(&query.to_lowercase())
            || self
                .selection
                .provider
                .label()
                .to_lowercase()
                .contains(&query.to_lowercase())
            || self
                .selection
                .model
                .to_lowercase()
                .contains(&query.to_lowercase())
    }
}

type ModelPickerItems = SearchableVec<SearchableGroup<LanguageModel>>;
type ModelPickerState = ComboboxState<ModelPickerItems>;

fn language_model_groups() -> ModelPickerItems {
    SearchableVec::new(vec![
        SearchableGroup::new("Recommended")
            .item(LanguageModel::new("Qwen 3.8 27B", RECOMMENDED_QWEN)),
        SearchableGroup::new(ModelProvider::Ollama.label())
            .item(LanguageModel::new("Qwen 3.8 27B", OLLAMA_QWEN)),
    ])
}

#[derive(Clone)]
enum TimelineMessage {
    User(UserMessageGroup),
    Agent(AgentMessage),
}

#[derive(Clone)]
struct AgentMessage {
    id: Uuid,
    comment_group_id: Option<Uuid>,
    comment_responses: Vec<AgentCommentResponse>,
    thinking: String,
    thinking_view: Entity<TextViewState>,
    thinking_complete: bool,
    thinking_expanded: bool,
    text: String,
    text_view: Entity<TextViewState>,
    complete: bool,
    failed: bool,
}

#[derive(Clone)]
struct AgentCommentResponse {
    id: Uuid,
    comment_id: Uuid,
    response: String,
    response_view: Entity<TextViewState>,
}

/// A submitted user message: the comments and prompt blocks of one
/// submission.
#[derive(Clone)]
struct UserMessageGroup {
    id: Uuid,
    comments: Vec<UserComment>,
    blocks: Vec<PromptBlock>,
    /// The comment instructions the agent was given with this message, so
    /// history replays exactly what was sent. Only the host that ran the
    /// agent has them.
    history_preface: Option<String>,
    comments_folded: bool,
}

#[derive(Clone)]
struct PromptBlock {
    id: Uuid,
    author: ParticipantId,
    text: String,
    /// The block's files; their bytes are in the thread's
    /// [`ThreadDraft::files`].
    attachments: Vec<AttachmentRecord>,
}

/// A thread's pending request: the shared draft document, the bytes of the
/// thread's files, and the local editors showing its items.
///
/// Editors are created lazily by [`Cowork::prepare_draft`] because they need
/// a window, and they are kept in sync with the document there too.
struct ThreadDraft {
    /// Routes asynchronous work, such as reading attachments, to this draft
    /// even after the user switched threads.
    id: Uuid,
    /// Who the local user is in this draft.
    author: ParticipantId,
    doc: Draft,
    /// The bytes of every file this participant has of the thread: the
    /// draft's, and those of messages submitted from it. Kept with the draft
    /// because a new thread's draft becomes its thread's.
    files: HashMap<AttachmentId, FileAttachment>,
    /// Files whose bytes are still arriving from another participant.
    incoming: HashMap<AttachmentId, IncomingFile>,
    /// The files the host holds every byte of, which is what submitting
    /// them needs.
    stored: HashSet<AttachmentId>,
    /// How many bytes of each of the local user's files have been sent to
    /// the host so far, while they are being sent.
    uploads: HashMap<AttachmentId, u64>,
    /// Files discarded here, whose pieces still in flight are ignored.
    discarded: HashSet<AttachmentId>,
    /// Whether removing a file from the draft keeps its bytes. Joined
    /// threads do: a removal can race a submission that includes the file,
    /// and the host sends every file only once.
    keeps_removed_files: bool,
    editors: HashMap<ItemId, ItemEditors>,
    /// The text each item editor last agreed on with the document. An editor
    /// catches up with others' edits only when it is next drawn, so a
    /// keystroke can arrive first; the change it makes is then its difference
    /// from this text, not from the document, which would revert those edits.
    synced_text: HashMap<EntityId, String>,
    /// The empty spot below the prompt blocks; typing there creates a block.
    draft_position: Option<Entity<TextareaState>>,

    /// Blocks created for attachments picked together, so they share one.
    attachment_batches: HashMap<Uuid, ItemId>,
    comments_folded: bool,
    /// Every participant's presence in this draft, including the host's echo
    /// of the local user's own, with when it last changed.
    presence: HashMap<ParticipantId, (protocol::Presence, Instant)>,
}

enum ItemEditors {
    Prompt(Entity<TextareaState>),
    /// A comment is edited both next to its excerpt and in the composer.
    Comment {
        inline: Entity<TextareaState>,
        composer: Entity<TextareaState>,
    },
}

impl ItemEditors {
    fn all(&self) -> Vec<&Entity<TextareaState>> {
        match self {
            Self::Prompt(editor) => vec![editor],
            Self::Comment { inline, composer } => vec![inline, composer],
        }
    }
}

/// Which part of a draft an editor edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorSlot {
    DraftPosition,
    Prompt(ItemId),
    CommentInline(ItemId),
    CommentComposer(ItemId),
}

impl EditorSlot {
    fn item(self) -> Option<ItemId> {
        match self {
            Self::DraftPosition => None,
            Self::Prompt(id) | Self::CommentInline(id) | Self::CommentComposer(id) => Some(id),
        }
    }
}

impl ThreadDraft {
    fn new(author: ParticipantId) -> Self {
        Self {
            id: Uuid::new_v4(),
            author,
            doc: Draft::new(),
            files: HashMap::new(),
            incoming: HashMap::new(),
            stored: HashSet::new(),
            uploads: HashMap::new(),
            discarded: HashSet::new(),
            keeps_removed_files: false,
            editors: HashMap::new(),
            synced_text: HashMap::new(),
            draft_position: None,
            attachment_batches: HashMap::new(),
            comments_folded: false,
            presence: HashMap::new(),
        }
    }

    /// Records a participant's presence. The time it last changed only moves
    /// when their caret did, since that is what shows their name for a moment.
    fn set_presence(&mut self, participant: ParticipantId, presence: protocol::Presence) {
        let moved_at = match self.presence.get(&participant) {
            Some((current, moved_at))
                if current.focus == presence.focus && current.selection == presence.selection =>
            {
                *moved_at
            }
            _ => Instant::now(),
        };
        self.presence.insert(participant, (presence, moved_at));
    }

    /// Whether anyone other than `except` is focused in the item.
    fn is_attended(&self, id: ItemId, except: Option<ParticipantId>) -> bool {
        let focus = protocol::PresenceFocus::Item(id.as_uuid().into_bytes());
        self.presence.iter().any(|(participant, (presence, _))| {
            Some(*participant) != except && presence.focus == Some(focus)
        })
    }

    /// Participants focused in the item, in join order.
    fn editors_of(&self, id: ItemId, order: &[ParticipantId]) -> Vec<ParticipantId> {
        let focus = protocol::PresenceFocus::Item(id.as_uuid().into_bytes());
        self.participants_where(order, |presence| presence.focus == Some(focus))
    }

    /// Participants other than the local user at the draft position, in
    /// join order.
    fn others_at_draft_position(&self, order: &[ParticipantId]) -> Vec<ParticipantId> {
        self.participants_where(order, |presence| {
            presence.focus == Some(protocol::PresenceFocus::DraftPosition)
        })
        .into_iter()
        .filter(|participant| *participant != self.author)
        .collect()
    }

    /// Participants whose presence matches, in join order. Anyone missing
    /// from `order` comes last, sorted, so the order never reshuffles.
    fn participants_where(
        &self,
        order: &[ParticipantId],
        matches: impl Fn(&protocol::Presence) -> bool,
    ) -> Vec<ParticipantId> {
        let mut participants = self
            .presence
            .iter()
            .filter(|(_, (presence, _))| matches(presence))
            .map(|(participant, _)| *participant)
            .collect::<Vec<_>>();
        participants.sort_by_key(|participant| {
            (
                order
                    .iter()
                    .position(|joined| joined == participant)
                    .unwrap_or(usize::MAX),
                participant.as_uuid(),
            )
        });
        participants
    }

    /// The carets and selections of everyone else in the item, resolved
    /// against this replica's text of it.
    fn remote_carets(&self, id: ItemId, order: &[ParticipantId]) -> Vec<RemoteCaret> {
        let others = self
            .editors_of(id, order)
            .into_iter()
            .filter(|participant| *participant != self.author)
            .collect::<Vec<_>>();
        if others.is_empty() {
            return Vec::new();
        }
        let Some(body) = self.doc.body(id) else {
            return Vec::new();
        };
        others
            .into_iter()
            .filter_map(|participant| {
                let (presence, changed) = self.presence.get(&participant)?;
                let selection = presence.selection.as_ref()?;
                let resolve = |anchor: &[u8]| {
                    self.doc
                        .resolve_anchor(id, anchor)
                        .map(|offset| body.floor_char_boundary(offset))
                };
                let head = resolve(&selection.head)?;
                let tail = resolve(&selection.anchor).unwrap_or(head);
                Some(RemoteCaret {
                    participant,
                    selection: head.min(tail)..head.max(tail),
                    head,
                    moved_at: *changed,
                })
            })
            .collect()
    }

    /// Everyone else at the draft position, which has no text to place a
    /// caret in, so all of them sit at its start.
    fn draft_position_carets(&self, order: &[ParticipantId]) -> Vec<RemoteCaret> {
        self.others_at_draft_position(order)
            .into_iter()
            .filter_map(|participant| {
                let (_, changed) = self.presence.get(&participant)?;
                Some(RemoteCaret {
                    participant,
                    selection: 0..0,
                    head: 0,
                    moved_at: *changed,
                })
            })
            .collect()
    }

    /// Whether anyone's presence announces files being read into the block.
    fn has_announced_reads_into(&self, id: ItemId) -> bool {
        let block = id.as_uuid().into_bytes();
        self.presence.values().any(|(presence, _)| {
            presence
                .pending_reads
                .iter()
                .any(|read| read.block == Some(block))
        })
    }

    /// Whether a participant other than the local user is reading files
    /// into the draft.
    fn others_are_reading_files(&self) -> bool {
        self.presence.iter().any(|(participant, (presence, _))| {
            *participant != self.author && !presence.pending_reads.is_empty()
        })
    }

    fn slot_of(&self, editor: EntityId) -> Option<EditorSlot> {
        if self
            .draft_position
            .as_ref()
            .is_some_and(|draft_position| draft_position.entity_id() == editor)
        {
            return Some(EditorSlot::DraftPosition);
        }
        self.editors
            .iter()
            .find_map(|(&id, editors)| match editors {
                ItemEditors::Prompt(prompt) => {
                    (prompt.entity_id() == editor).then_some(EditorSlot::Prompt(id))
                }
                ItemEditors::Comment { inline, composer } => {
                    if inline.entity_id() == editor {
                        Some(EditorSlot::CommentInline(id))
                    } else {
                        (composer.entity_id() == editor).then_some(EditorSlot::CommentComposer(id))
                    }
                }
            })
    }

    fn editor(&self, slot: EditorSlot) -> Option<Entity<TextareaState>> {
        match (slot, slot.item().and_then(|id| self.editors.get(&id))) {
            (EditorSlot::DraftPosition, _) => self.draft_position.clone(),
            (EditorSlot::Prompt(_), Some(ItemEditors::Prompt(editor)))
            | (EditorSlot::CommentInline(_), Some(ItemEditors::Comment { inline: editor, .. }))
            | (
                EditorSlot::CommentComposer(_),
                Some(ItemEditors::Comment {
                    composer: editor, ..
                }),
            ) => Some(editor.clone()),
            _ => None,
        }
    }

    /// The editors Up and Down move between, top to bottom: the composer's
    /// comment editors unless folded, the prompt blocks, and the draft
    /// position.
    fn navigation_chain(&self) -> Vec<(EditorSlot, Entity<TextareaState>)> {
        let mut chain = Vec::new();
        let items = self.doc.items();
        if !self.comments_folded {
            chain.extend(
                items
                    .iter()
                    .filter(|item| item.is_comment())
                    .filter_map(|item| {
                        let slot = EditorSlot::CommentComposer(item.id);
                        Some((slot, self.editor(slot)?))
                    }),
            );
        }
        chain.extend(
            items
                .iter()
                .filter(|item| item.is_prompt())
                .filter_map(|item| {
                    let slot = EditorSlot::Prompt(item.id);
                    Some((slot, self.editor(slot)?))
                }),
        );
        if let Some(draft_position) = &self.draft_position {
            chain.push((EditorSlot::DraftPosition, draft_position.clone()));
        }
        chain
    }

    /// Applies what the user typed into an item's editor to the document.
    ///
    /// The editor may not show others' latest edits yet, so its change is
    /// taken relative to the text it last synced and moved past those edits
    /// before it is applied. Returns the item's resulting text.
    fn apply_typing(&mut self, id: ItemId, editor: EntityId, typed: &str) -> Option<String> {
        let body = self.doc.body(id)?;
        let synced = self
            .synced_text
            .get(&editor)
            .cloned()
            .unwrap_or_else(|| body.clone());
        if let Some(mut edit) = TextEdit::diff(&synced, typed) {
            if let Some(remote) = TextEdit::diff(&synced, &body) {
                edit.range = remote.map_offset(edit.range.start)..remote.map_offset(edit.range.end);
            }
            self.doc.edit_body(id, &edit);
        }
        // Everything the editor shows is in the document now.
        self.synced_text.insert(editor, typed.to_owned());
        self.doc.body(id)
    }

    /// Removes items together with their editors and file bytes.
    fn remove_items(&mut self, ids: &[ItemId]) {
        let attachments = self
            .doc
            .items()
            .into_iter()
            .filter(|item| ids.contains(&item.id))
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .map(|record| record.id)
            .collect::<Vec<_>>();
        self.take_items(ids);
        for attachment in attachments {
            self.drop_removed_file(attachment);
        }
    }

    /// Forgets a file whose record was removed, unless this draft keeps
    /// removed files.
    fn drop_removed_file(&mut self, id: AttachmentId) {
        if !self.keeps_removed_files {
            self.drop_file(id);
        }
    }

    /// Removes items and their editors, keeping their files, as a
    /// submission needs them.
    fn take_items(&mut self, ids: &[ItemId]) {
        self.doc.remove_items(ids);
        for id in ids {
            if let Some(editors) = self.editors.remove(id) {
                for editor in editors.all() {
                    self.synced_text.remove(&editor.entity_id());
                }
            }
        }
        self.attachment_batches
            .retain(|_, block| !ids.contains(block));
    }

    /// Forgets everything about a file.
    fn drop_file(&mut self, id: AttachmentId) {
        self.files.remove(&id);
        self.incoming.remove(&id);
        self.stored.remove(&id);
        self.uploads.remove(&id);
        self.discarded.insert(id);
    }

    /// The attachment records of the draft's prompt blocks, in draft order.
    fn attachment_records(&self) -> Vec<AttachmentRecord> {
        self.doc
            .items()
            .into_iter()
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .collect()
    }

    /// Whether a file of a non-empty block is not with the host yet, which
    /// holds submission back.
    fn has_unstored_attachments(&self) -> bool {
        self.doc
            .items()
            .into_iter()
            .filter(|item| !item.is_empty())
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .any(|record| !self.stored.contains(&record.id))
    }

    /// Adds a received piece of a file. Returns the file once every byte
    /// has arrived, or an error when the piece breaks the rules for it.
    ///
    /// Pieces of files already complete, or that continue a file this
    /// participant has discarded or never started, are ignored: they can
    /// still be in flight after a removal.
    fn receive_chunk(
        &mut self,
        chunk: protocol::AttachmentChunk,
        uploader: Option<ParticipantId>,
    ) -> anyhow::Result<Option<AttachmentId>> {
        let id = AttachmentId::from_uuid(Uuid::from_bytes(chunk.id));
        let kind = kind_from_protocol(chunk.kind);
        anyhow::ensure!(
            chunk.total <= max_attachment_size(kind),
            "{} is larger than attachments may be",
            chunk.name
        );
        if self.files.contains_key(&id) || self.discarded.contains(&id) {
            return Ok(None);
        }
        if chunk.offset == 0 {
            if let Some(uploader) = uploader {
                let budget = 2 * MAX_MESSAGE_ATTACHMENT_BYTES;
                let buffered = |incoming: &HashMap<AttachmentId, IncomingFile>| {
                    incoming
                        .iter()
                        .filter(|(other, file)| **other != id && file.uploader == Some(uploader))
                        .map(|(_, file)| file.total)
                        .sum::<u64>()
                };
                if buffered(&self.incoming) + chunk.total > budget {
                    // Complete files still waiting for a record that may
                    // never come, e.g. attached to a block removed at the
                    // same time, make room first.
                    self.incoming.retain(|_, file| {
                        file.uploader != Some(uploader) || file.bytes.len() as u64 != file.total
                    });
                }
                anyhow::ensure!(
                    buffered(&self.incoming) + chunk.total <= budget,
                    "Too many attachment bytes in flight"
                );
            }
            self.incoming.insert(
                id,
                IncomingFile {
                    name: chunk.name,
                    kind,
                    total: chunk.total,
                    bytes: Vec::with_capacity(chunk.total as usize),
                    uploader,
                },
            );
        }
        let Some(file) = self.incoming.get_mut(&id) else {
            return Ok(None);
        };
        if file.uploader != uploader
            || file.kind != kind
            || file.total != chunk.total
            || file.bytes.len() as u64 != chunk.offset
        {
            // Out of step, e.g. restarted after a removal: start over.
            self.incoming.remove(&id);
            return Ok(None);
        }
        anyhow::ensure!(
            chunk.offset + chunk.bytes.len() as u64 <= file.total,
            "{} has more bytes than announced",
            file.name
        );
        file.bytes.extend_from_slice(&chunk.bytes);
        Ok((file.bytes.len() as u64 == file.total).then_some(id))
    }

    /// Turns a completely received file into one this participant has.
    fn complete_file(&mut self, id: AttachmentId) -> anyhow::Result<()> {
        let Some(file) = self.incoming.remove(&id) else {
            return Ok(());
        };
        let attachment = FileAttachment::from_bytes(file.name, file.kind, file.bytes)?;
        self.files.insert(id, attachment);
        Ok(())
    }

    /// The piece of a file this participant has that starts at `offset`,
    /// or `None` once past its end or when the file is gone.
    fn chunk(&self, id: AttachmentId, offset: u64) -> Option<protocol::AttachmentChunk> {
        let file = self.files.get(&id)?;
        let bytes = file.bytes();
        let start = usize::try_from(offset).ok()?;
        if start >= bytes.len() && !(start == 0 && bytes.is_empty()) {
            return None;
        }
        let end = (start + protocol::ATTACHMENT_CHUNK_SIZE).min(bytes.len());
        Some(protocol::AttachmentChunk {
            id: id.as_uuid().into_bytes(),
            name: file.name.clone(),
            kind: kind_to_protocol(file.kind()),
            total: bytes.len() as u64,
            offset,
            bytes: bytes[start..end].to_vec(),
        })
    }

    /// Removes the item if it is empty. Returns whether it was removed.
    fn remove_if_empty(&mut self, id: ItemId) -> bool {
        if !self.doc.item(id).is_some_and(|item| item.is_empty()) {
            return false;
        }
        self.remove_items(&[id]);
        true
    }

    /// Removes an empty item the local user is leaving, unless someone else
    /// is in it. Returns whether it was removed.
    ///
    /// Presence travels separately from the draft, so someone who has just
    /// entered the item can still lose it; that race is accepted.
    fn remove_if_unattended(&mut self, id: ItemId) -> bool {
        !self.is_attended(id, Some(self.author)) && self.remove_if_empty(id)
    }

    /// The block to attach a finished file to, creating it when needed.
    fn attachment_block(&mut self, target: AttachmentTarget) -> ItemId {
        let batch = match target {
            AttachmentTarget::Block(id)
                if self.doc.item(id).is_some_and(|item| item.is_prompt()) =>
            {
                return id;
            }
            AttachmentTarget::Block(_) => None,
            AttachmentTarget::NewBlock(batch) => Some(batch),
        };
        if let Some(id) = batch.and_then(|batch| self.attachment_batches.get(&batch).copied())
            && self.doc.contains(id)
        {
            return id;
        }
        let id = self.doc.create_prompt(self.author.as_uuid(), "");
        if let Some(batch) = batch {
            self.attachment_batches.insert(batch, id);
        }
        id
    }

    /// Where pending files for `target` are shown: the block they will land
    /// in, or `None` for the draft position.
    fn pending_block(&self, target: AttachmentTarget) -> Option<ItemId> {
        let id = match target {
            AttachmentTarget::Block(id) => id,
            AttachmentTarget::NewBlock(batch) => *self.attachment_batches.get(&batch)?,
        };
        self.doc
            .item(id)
            .filter(|item| item.is_prompt())
            .map(|item| item.id)
    }

    /// The draft's comments as the timeline and composer render them, with
    /// who is in them (`order` is the thread's join order).
    fn comment_views(&self, order: &[ParticipantId]) -> Vec<UserComment> {
        self.doc
            .items()
            .into_iter()
            .filter_map(|item| {
                let DraftItemKind::Comment { target } = item.kind else {
                    return None;
                };
                let body = match self.editors.get(&item.id) {
                    Some(ItemEditors::Comment { inline, composer }) => UserCommentBody::Editing {
                        inline: inline.clone(),
                        composer: composer.clone(),
                    },
                    _ => UserCommentBody::Submitted(item.body.into()),
                };
                let creator = ParticipantId::from_uuid(item.creator);
                Some(UserComment {
                    id: item.id.as_uuid(),
                    author: creator,
                    reference: CommentReference {
                        message_id: target.message_id,
                        range: target.range,
                        quote: target.quote,
                    },
                    body,
                    presence: ItemPresence {
                        editors: self
                            .editors_of(item.id, order)
                            .into_iter()
                            .filter(|editor| *editor != creator)
                            .collect(),
                        carets: self.remote_carets(item.id, order),
                    },
                })
            })
            .collect()
    }
}

/// Who is in a draft item besides its creator, and where their carets are.
#[derive(Clone, Default)]
struct ItemPresence {
    /// Participants focused in the item other than its creator, in join
    /// order.
    editors: Vec<ParticipantId>,
    /// Everyone else's carets in the item.
    carets: Vec<RemoteCaret>,
}

/// Another participant's caret and selection in an editor, as byte offsets
/// into the text it shows.
#[derive(Clone, Debug, PartialEq)]
struct RemoteCaret {
    participant: ParticipantId,
    selection: Range<usize>,
    head: usize,
    /// When it last moved, which is when its name is shown for a moment.
    moved_at: Instant,
}

/// How long a remote caret shows its participant's name after moving.
const CARET_LABEL_DURATION: Duration = Duration::from_millis(1_500);

/// Where a file being read will be attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentTarget {
    Block(ItemId),
    /// A block created for the batch of files picked together.
    NewBlock(Uuid),
}

#[derive(Clone)]
struct UserComment {
    id: Uuid,
    author: ParticipantId,
    /// Who is in a draft comment right now; empty once submitted.
    presence: ItemPresence,
    reference: CommentReference,
    body: UserCommentBody,
}

#[derive(Clone)]
struct CommentReference {
    message_id: Uuid,
    range: Range<usize>,
    quote: String,
}

#[derive(Clone)]
enum UserCommentBody {
    Editing {
        inline: Entity<TextareaState>,
        composer: Entity<TextareaState>,
    },
    Submitted(SharedString),
}

struct MarkdownTextLeaf {
    source_range: Range<usize>,
    annotation_range: Range<usize>,
    atomic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ThreadMessageId {
    thread_id: Uuid,
    message_id: Uuid,
}

struct SegmentTextView {
    state: Entity<TextViewState>,
    text: String,
    source_offsets: Option<Vec<usize>>,
    rendered_at: u64,
}

#[derive(Clone)]
struct ThreadSummary {
    id: Uuid,
    title: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SharingStatus {
    NotShared,
    Sharing,
    Shared,
    Connected,
    Failed,
}

/// The host's end of a collaborator connection.
type HostPeer = protocol::Peer<protocol::HostMessage, protocol::CollaboratorMessage>;

/// A collaborator's end of its connection to a thread's host.
type ThreadHost = protocol::Peer<protocol::CollaboratorMessage, protocol::HostMessage>;

enum ThreadSharing {
    NotShared,
    Sharing,
    /// Hosting the thread. `events` fans every local change out to all
    /// collaborators; dropping it tears their connections down.
    Shared {
        endpoint: Endpoint,
        events: broadcast::Sender<protocol::HostMessage>,
    },
    /// Mirroring someone else's thread.
    ///
    /// Requests to the host, such as draft updates, are sent on `host`. It is
    /// unbounded so that no request is ever dropped: a lost draft update
    /// would leave every later one waiting on it forever. Replacing this state
    /// closes `host` and drops `link`, which is what disconnects.
    Connected {
        host: async_channel::Sender<protocol::CollaboratorMessage>,
        /// For attachment bytes; see [`protocol::Peer::bulk`].
        uploads: async_channel::Sender<protocol::CollaboratorMessage>,
        /// `None` when the peer is not reached over the network, as in tests.
        link: Option<PeerLink>,
    },
    Failed,
}

/// Keeps a collaborator's QUIC connection to the host open.
struct PeerLink {
    endpoint: Endpoint,
    _connection: Connection,
}

impl ThreadSharing {
    fn status(&self) -> SharingStatus {
        match self {
            Self::NotShared => SharingStatus::NotShared,
            Self::Sharing => SharingStatus::Sharing,
            Self::Shared { .. } => SharingStatus::Shared,
            Self::Connected { .. } => SharingStatus::Connected,
            Self::Failed => SharingStatus::Failed,
        }
    }

    fn is_collaborating(&self) -> bool {
        matches!(
            self,
            Self::Sharing | Self::Shared { .. } | Self::Connected { .. }
        )
    }
}

enum JoinStatus {
    Idle,
    Joining,
    Failed(String),
}

struct JoinDialog {
    endpoint_token: Entity<InputState>,
    status: JoinStatus,
    _input_subscription: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadOwnership {
    Local,
    Remote,
}

impl ThreadOwnership {
    /// Every participant edits the draft. Read-only viewers will come with
    /// sharing permissions, which the read-only rendering is kept for.
    fn can_write(self) -> bool {
        true
    }

    fn remove_on_disconnect(self) -> bool {
        matches!(self, Self::Remote)
    }
}

struct Thread {
    /// Identifies this local view. Multiple views may mirror the same shared
    /// thread, so this must remain distinct from `summary.id`.
    instance_id: Uuid,
    summary: ThreadSummary,
    /// Who the local user is in this thread: the app's own id for local and
    /// hosted threads, the id the host assigned for mirrored ones.
    participant_id: ParticipantId,
    /// Connected participants in join order, starting with the host. Empty
    /// while the thread is not shared.
    participants: Vec<ParticipantId>,
    model: ModelSelection,
    timeline: Vec<TimelineMessage>,
    draft: ThreadDraft,
    generating: bool,
    sharing: ThreadSharing,
    ownership: ThreadOwnership,
}

impl UserComment {
    fn to_protocol(&self) -> Option<protocol::UserComment> {
        let UserCommentBody::Submitted(body) = &self.body else {
            return None;
        };
        Some(protocol::UserComment {
            id: self.id.into_bytes(),
            author: self.author.into_bytes(),
            reference: protocol::CommentReference {
                message_id: self.reference.message_id.into_bytes(),
                range: self.reference.range.clone(),
                quote: self.reference.quote.clone(),
            },
            body: body.to_string(),
        })
    }
}

impl protocol::UserComment {
    fn into_native(self) -> UserComment {
        UserComment {
            id: Uuid::from_bytes(self.id),
            author: ParticipantId::from_bytes(self.author),
            presence: ItemPresence::default(),
            reference: CommentReference {
                message_id: Uuid::from_bytes(self.reference.message_id),
                range: self.reference.range,
                quote: self.reference.quote,
            },
            body: UserCommentBody::Submitted(self.body.into()),
        }
    }
}

impl UserMessageGroup {
    fn to_protocol(&self) -> protocol::UserMessage {
        protocol::UserMessage {
            id: self.id.into_bytes(),
            comments: self
                .comments
                .iter()
                .filter_map(UserComment::to_protocol)
                .collect(),
            blocks: self.blocks.iter().map(PromptBlock::to_protocol).collect(),
        }
    }

    /// The text the thread is titled after.
    fn title_text(&self) -> &str {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .chain(
                self.comments
                    .iter()
                    .filter_map(|comment| match &comment.body {
                        UserCommentBody::Submitted(body) => Some(body.as_ref()),
                        UserCommentBody::Editing { .. } => None,
                    }),
            )
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
    }
}

impl PromptBlock {
    fn to_protocol(&self) -> protocol::PromptBlock {
        protocol::PromptBlock {
            id: self.id.into_bytes(),
            author: self.author.into_bytes(),
            text: self.text.clone(),
            attachments: self.attachments.iter().map(record_to_protocol).collect(),
        }
    }
}

impl protocol::UserMessage {
    fn into_native(self) -> UserMessageGroup {
        UserMessageGroup {
            id: Uuid::from_bytes(self.id),
            comments: self
                .comments
                .into_iter()
                .map(protocol::UserComment::into_native)
                .collect(),
            blocks: self
                .blocks
                .into_iter()
                .map(|block| PromptBlock {
                    id: Uuid::from_bytes(block.id),
                    author: ParticipantId::from_bytes(block.author),
                    text: block.text,
                    attachments: block
                        .attachments
                        .into_iter()
                        .map(record_from_protocol)
                        .collect(),
                })
                .collect(),
            history_preface: None,
            comments_folded: false,
        }
    }
}

impl AgentMessage {
    /// An empty message for an agent that has just started responding.
    fn new(id: Uuid, comment_group_id: Option<Uuid>, cx: &mut impl AppContext) -> Self {
        Self {
            id,
            comment_group_id,
            comment_responses: Vec::new(),
            thinking: String::new(),
            thinking_view: cx.new(|cx| TextViewState::markdown("", cx)),
            thinking_complete: false,
            thinking_expanded: true,
            text: String::new(),
            text_view: cx.new(|cx| TextViewState::markdown("", cx)),
            complete: false,
            failed: false,
        }
    }

    fn to_protocol(&self) -> protocol::AgentMessage {
        protocol::AgentMessage {
            id: self.id.into_bytes(),
            comment_group_id: self.comment_group_id.map(Uuid::into_bytes),
            comment_responses: self
                .comment_responses
                .iter()
                .map(|response| protocol::AgentCommentResponse {
                    id: response.id.into_bytes(),
                    comment_id: response.comment_id.into_bytes(),
                    response: response.response.clone(),
                })
                .collect(),
            thinking: self.thinking.clone(),
            thinking_complete: self.thinking_complete,
            text: self.text.clone(),
            complete: self.complete,
            failed: self.failed,
        }
    }
}

impl protocol::AgentMessage {
    fn into_native(self, cx: &mut impl AppContext) -> AgentMessage {
        let thinking_view = cx.new(|cx| TextViewState::markdown(&self.thinking, cx));
        let text_view = cx.new(|cx| TextViewState::markdown(&self.text, cx));
        AgentMessage {
            id: Uuid::from_bytes(self.id),
            comment_group_id: self.comment_group_id.map(Uuid::from_bytes),
            comment_responses: self
                .comment_responses
                .into_iter()
                .map(|response| AgentCommentResponse {
                    id: Uuid::from_bytes(response.id),
                    comment_id: Uuid::from_bytes(response.comment_id),
                    response_view: cx.new(|cx| TextViewState::markdown(&response.response, cx)),
                    response: response.response,
                })
                .collect(),
            thinking: self.thinking,
            thinking_view,
            thinking_complete: self.thinking_complete,
            thinking_expanded: !self.thinking_complete,
            text: self.text,
            text_view,
            complete: self.complete,
            failed: self.failed,
        }
    }
}

impl TimelineMessage {
    fn to_protocol(&self) -> protocol::TimelineMessage {
        match self {
            Self::User(message) => protocol::TimelineMessage::User(message.to_protocol()),
            Self::Agent(message) => protocol::TimelineMessage::Agent(message.to_protocol()),
        }
    }
}

impl protocol::TimelineMessage {
    fn into_native(self, cx: &mut impl AppContext) -> TimelineMessage {
        match self {
            Self::User(message) => TimelineMessage::User(message.into_native()),
            Self::Agent(message) => TimelineMessage::Agent(message.into_native(cx)),
        }
    }
}

impl Thread {
    /// Builds the local mirror of a thread hosted by someone else.
    fn from_welcome(
        welcome: protocol::Welcome,
        draft: ThreadDraft,
        sharing: ThreadSharing,
        cx: &mut impl AppContext,
    ) -> Self {
        let mut thread = Self {
            instance_id: Uuid::new_v4(),
            summary: ThreadSummary {
                id: Uuid::from_bytes(welcome.thread.id),
                title: String::new(),
            },
            participant_id: ParticipantId::from_bytes(welcome.participant_id),
            participants: Vec::new(),
            model: DEFAULT_MODEL,
            timeline: Vec::new(),
            draft,
            generating: false,
            sharing,
            ownership: ThreadOwnership::Remote,
        };
        thread.rebase(welcome, cx);
        thread
    }

    /// Replaces everything the host is authoritative for with its snapshot.
    ///
    /// The draft is merged rather than replaced: local edits the host has
    /// not received yet are still on their way to it and must survive.
    fn rebase(&mut self, welcome: protocol::Welcome, cx: &mut impl AppContext) {
        let protocol::Welcome {
            participant_id,
            thread,
            draft,
            presence,
            stored_attachments,
        } = welcome;
        self.draft.stored = stored_attachments
            .into_iter()
            .map(|id| AttachmentId::from_uuid(Uuid::from_bytes(id)))
            .collect();
        let stored = &self.draft.stored;
        self.draft.uploads.retain(|id, _| !stored.contains(id));
        self.draft.keeps_removed_files = true;
        self.participant_id = ParticipantId::from_bytes(participant_id);
        self.draft.author = self.participant_id;
        if let Err(error) = self.draft.doc.apply_update(&draft) {
            eprintln!("failed to merge the host's draft: {error:#}");
        }
        self.draft.presence.clear();
        for (participant, presence) in presence {
            self.draft
                .set_presence(ParticipantId::from_bytes(participant), presence);
        }
        self.participants = thread
            .participants
            .iter()
            .copied()
            .map(ParticipantId::from_bytes)
            .collect();
        // Only a model this app does not know about can fail to resolve, and
        // the protocol version check rules that out between matching apps.
        if let Some(model) = ModelSelection::from_catalog_id(&thread.model) {
            self.model = model;
        }
        let (summary, timeline) = thread.into_native(cx);
        self.summary = summary;
        self.set_timeline(timeline);
    }

    fn to_protocol(&self) -> protocol::ThreadSnapshot {
        protocol::ThreadSnapshot {
            id: self.summary.id.into_bytes(),
            title: self.summary.title.clone(),
            participants: self
                .participants
                .iter()
                .map(|participant| participant.into_bytes())
                .collect(),
            model: self.model.catalog_id.into(),
            messages: self
                .timeline
                .iter()
                .map(TimelineMessage::to_protocol)
                .collect(),
        }
    }

    /// Sends the draft's local changes to whoever else has a copy: every
    /// collaborator when hosting, the host when mirroring.
    fn flush_draft(&mut self) {
        let Some(update) = self.draft.doc.take_local_update() else {
            return;
        };
        match &self.sharing {
            ThreadSharing::Shared { .. } => {
                self.publish(protocol::HostMessage::DraftUpdate(update));
            }
            ThreadSharing::Connected { .. } => {
                self.request(protocol::CollaboratorMessage::DraftUpdate(update));
            }
            // Whoever joins later receives the whole draft with their welcome.
            ThreadSharing::NotShared | ThreadSharing::Sharing | ThreadSharing::Failed => {}
        }
    }

    /// Applies a collaborator's draft update and forwards it to everyone.
    ///
    /// Fails when the update breaks the draft's invariants or changes what
    /// `author` may not change, such as attributing an item to someone else.
    /// It has been applied by then; failing only tells the caller to
    /// disconnect the collaborator.
    fn apply_collaborator_update(
        &mut self,
        author: ParticipantId,
        update: Vec<u8>,
    ) -> anyhow::Result<()> {
        let before = self.draft.doc.items();
        let attachments_before = self.draft.attachment_records();
        self.draft.doc.apply_update(&update)?;
        self.publish(protocol::HostMessage::DraftUpdate(update));
        self.draft.doc.validate()?;
        draft::verify_change(&before, &self.draft.doc.items(), author.as_uuid())?;

        // The host keeps only the bytes of files still attached; submitted
        // ones leave the draft through the host's own submission instead.
        let attachments = self.draft.attachment_records();
        for removed in attachments_before
            .iter()
            .filter(|record| !attachments.iter().any(|kept| kept.id == record.id))
        {
            self.draft.drop_file(removed.id);
        }
        // Bytes can arrive before the record that announces them.
        let complete = self
            .draft
            .incoming
            .iter()
            .filter(|(_, file)| {
                file.uploader == Some(author) && file.bytes.len() as u64 == file.total
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in complete {
            self.store_upload(author, id)?;
        }
        Ok(())
    }

    /// Takes a piece of a file a collaborator is uploading.
    fn receive_upload(
        &mut self,
        uploader: ParticipantId,
        chunk: protocol::AttachmentChunk,
    ) -> anyhow::Result<()> {
        if let Some(id) = self.draft.receive_chunk(chunk, Some(uploader))? {
            self.store_upload(uploader, id)?;
        }
        Ok(())
    }

    /// Stores a completely uploaded file once its record is in the draft,
    /// and announces it. Until the record arrives it waits in `incoming`.
    fn store_upload(&mut self, uploader: ParticipantId, id: AttachmentId) -> anyhow::Result<()> {
        let Some(record) = self
            .draft
            .attachment_records()
            .into_iter()
            .find(|record| record.id == id)
        else {
            return Ok(());
        };
        let Some(file) = self.draft.incoming.get(&id) else {
            return Ok(());
        };
        anyhow::ensure!(
            record.creator == uploader.as_uuid()
                && record.size == file.total
                && record.kind == file.kind,
            "{} does not match its attachment record",
            file.name
        );
        self.draft.complete_file(id)?;
        self.publish_stored(id, uploader);
        Ok(())
    }

    /// The stored files `participant` needs the bytes of: all but the ones
    /// they attached themselves. Timeline files come first, in order.
    fn files_for(&self, participant: ParticipantId) -> Vec<AttachmentId> {
        let timeline = self.timeline.iter().flat_map(|message| match message {
            TimelineMessage::User(group) => group
                .blocks
                .iter()
                .flat_map(|block| block.attachments.clone())
                .collect(),
            TimelineMessage::Agent(_) => Vec::new(),
        });
        timeline
            .chain(self.draft.attachment_records())
            .filter(|record| {
                record.creator != participant.as_uuid()
                    && self.draft.stored.contains(&record.id)
                    && self.draft.files.contains_key(&record.id)
            })
            .map(|record| record.id)
            .collect()
    }

    /// Marks a file as held by the host and tells everyone.
    fn publish_stored(&mut self, id: AttachmentId, uploader: ParticipantId) {
        self.draft.stored.insert(id);
        self.publish(protocol::HostMessage::AttachmentStored {
            id: id.as_uuid().into_bytes(),
            uploader: uploader.into_bytes(),
        });
    }

    /// Tells the other participants where the local user is in the draft.
    fn publish_presence(&mut self, presence: protocol::Presence, cx: &mut impl AppContext) {
        match &self.sharing {
            ThreadSharing::Shared { .. } => {
                self.host_presence(self.participant_id, presence, cx);
            }
            ThreadSharing::Connected { .. } => {
                self.request(protocol::CollaboratorMessage::Presence(presence));
            }
            ThreadSharing::NotShared | ThreadSharing::Sharing | ThreadSharing::Failed => {}
        }
    }

    /// Records and broadcasts a participant's presence in a hosted thread.
    ///
    /// The host also removes an empty item the participant has just left if
    /// nobody is in it anymore. Whoever leaves an item last removes it
    /// themselves, but two people leaving at once each still see the other
    /// there, so the host settles it.
    fn host_presence(
        &mut self,
        participant: ParticipantId,
        presence: protocol::Presence,
        cx: &mut impl AppContext,
    ) {
        let left = self
            .draft
            .presence
            .get(&participant)
            .and_then(|(previous, _)| previous.focus)
            .filter(|previous| Some(*previous) != presence.focus);
        self.emit(
            protocol::HostMessage::Presence {
                participant: participant.into_bytes(),
                presence,
            },
            cx,
        );
        self.remove_if_left_empty(left);
    }

    /// Removes the item behind `focus` if it is empty, nobody is in it, and
    /// no files are on their way into it.
    fn remove_if_left_empty(&mut self, focus: Option<protocol::PresenceFocus>) {
        if let Some(protocol::PresenceFocus::Item(id)) = focus {
            let id = ItemId::from_uuid(Uuid::from_bytes(id));
            if !self.draft.is_attended(id, None)
                && !self.draft.has_announced_reads_into(id)
                && self.draft.remove_if_empty(id)
            {
                self.flush_draft();
            }
        }
    }

    /// Lets everyone know a collaborator has left, first removing the empty
    /// item they were in if nobody else is in it either.
    fn participant_left(&mut self, participant: ParticipantId, cx: &mut impl AppContext) {
        // Files they had not finished uploading can never be submitted.
        let unfinished = self
            .draft
            .attachment_records()
            .into_iter()
            .filter(|record| {
                record.creator == participant.as_uuid() && !self.draft.stored.contains(&record.id)
            })
            .map(|record| record.id)
            .collect::<Vec<_>>();
        for id in &unfinished {
            self.draft.doc.remove_attachment(*id);
        }
        self.draft
            .incoming
            .retain(|_, file| file.uploader != Some(participant));
        for id in unfinished {
            self.draft.drop_file(id);
        }
        self.flush_draft();

        let focus = self
            .draft
            .presence
            .remove(&participant)
            .and_then(|(presence, _)| presence.focus);
        self.remove_if_left_empty(focus);
        self.emit(
            protocol::HostMessage::ParticipantLeft(participant.into_bytes()),
            cx,
        );
    }

    /// How many submissions this thread has accepted.
    fn submission_count(&self) -> u64 {
        self.timeline
            .iter()
            .filter(|message| matches!(message, TimelineMessage::User(_)))
            .count() as u64
    }

    /// Sends a request to the host of a mirrored thread. Returns `false` when
    /// this thread is not mirrored or its connection has closed.
    fn request(&self, request: protocol::CollaboratorMessage) -> bool {
        let ThreadSharing::Connected { host, .. } = &self.sharing else {
            return false;
        };
        match host.try_send(request) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("failed to send request to host: {error}");
                false
            }
        }
    }

    /// Selects the thread's model on behalf of the local user.
    ///
    /// A mirrored thread shows the selection immediately and asks the host to
    /// apply it; the host's broadcast then settles concurrent selections in
    /// the same order on every participant.
    fn select_model(&mut self, model: ModelSelection, cx: &mut impl AppContext) {
        if self.model == model {
            return;
        }
        if matches!(self.sharing, ThreadSharing::Connected { .. }) {
            let sent = self.request(protocol::CollaboratorMessage::SelectModel {
                catalog_id: model.catalog_id.into(),
            });
            // Showing a model the host never heard about would silently run
            // the agent with a different one.
            if sent {
                self.model = model;
            }
        } else {
            self.emit(
                protocol::HostMessage::ModelSelected {
                    catalog_id: model.catalog_id.into(),
                },
                cx,
            );
        }
    }

    /// The agent message currently being generated, if any.
    fn running_agent_message_id(&self) -> Option<Uuid> {
        self.timeline.iter().rev().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if !message.complete => Some(message.id),
            _ => None,
        })
    }

    /// Subscribes to this thread's events, returning `None` when it is not
    /// being hosted.
    ///
    /// Callers that also need a snapshot must take both in the same
    /// `Entity::update`: thread state only changes on the foreground thread, so
    /// pairing them there guarantees the subscription starts exactly where the
    /// snapshot ends, with no event missed or replayed.
    fn subscribe(&self) -> Option<broadcast::Receiver<protocol::HostMessage>> {
        match &self.sharing {
            ThreadSharing::Shared { events, .. } => Some(events.subscribe()),
            _ => None,
        }
    }

    /// Broadcasts an event to every collaborator without applying it locally.
    ///
    /// Needed for the changes whose local representation carries more than the
    /// wire form does, such as a user message that also remembers the prompt
    /// the agent was given and whether its comments are folded.
    fn publish(&self, event: protocol::HostMessage) {
        if let ThreadSharing::Shared { events, .. } = &self.sharing {
            // An error here only means nobody has joined yet.
            _ = events.send(event);
        }
    }

    /// Applies a thread event locally and broadcasts it verbatim.
    ///
    /// Host and collaborators then run the same [`Thread::apply`] over the same
    /// events, so their timelines stay identical by construction. Only use this
    /// for events that fully describe the change they make.
    fn emit(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        // Checked up front so that an unshared thread, which is the common
        // case, never pays to clone a streamed chunk.
        if matches!(self.sharing, ThreadSharing::Shared { .. }) {
            self.publish(event.clone());
        }
        self.apply(event, cx);
    }

    /// Folds a thread event into the timeline.
    fn apply(&mut self, event: protocol::HostMessage, cx: &mut impl AppContext) {
        match event {
            protocol::HostMessage::Welcome(welcome) => self.rebase(welcome, cx),
            // Only ever sent in place of the first `Welcome`, which the join
            // handshake consumes.
            protocol::HostMessage::Rejected(_) => {}
            protocol::HostMessage::ParticipantJoined(participant) => {
                let participant = ParticipantId::from_bytes(participant);
                if !self.participants.contains(&participant) {
                    self.participants.push(participant);
                }
            }
            protocol::HostMessage::ParticipantLeft(participant) => {
                let participant = ParticipantId::from_bytes(participant);
                self.participants
                    .retain(|existing| *existing != participant);
                self.draft.presence.remove(&participant);
            }
            protocol::HostMessage::Presence {
                participant,
                presence,
            } => {
                self.draft
                    .set_presence(ParticipantId::from_bytes(participant), presence);
            }
            protocol::HostMessage::AttachmentStored { id, .. } => {
                let id = AttachmentId::from_uuid(Uuid::from_bytes(id));
                self.draft.stored.insert(id);
                self.draft.uploads.remove(&id);
            }
            // Only ever relayed by the host, which holds every file.
            protocol::HostMessage::AttachmentData(chunk) => {
                match self.draft.receive_chunk(chunk, None) {
                    Ok(Some(id)) => {
                        if let Err(error) = self.draft.complete_file(id) {
                            eprintln!("failed to read a received attachment: {error:#}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("ignored attachment data: {error:#}"),
                }
            }
            protocol::HostMessage::ModelSelected { catalog_id } => {
                if let Some(model) = ModelSelection::from_catalog_id(&catalog_id) {
                    self.model = model;
                }
            }
            protocol::HostMessage::DraftUpdate(update) => {
                if let Err(error) = self.draft.doc.apply_update(&update) {
                    eprintln!("failed to apply a draft update: {error:#}");
                }
            }
            protocol::HostMessage::ThreadTitled(title) => self.summary.title = title,
            protocol::HostMessage::UserMessage(message) => self
                .timeline
                .push(TimelineMessage::User(message.into_native())),
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id,
            } => {
                self.timeline.push(TimelineMessage::Agent(AgentMessage::new(
                    Uuid::from_bytes(id),
                    comment_group_id.map(Uuid::from_bytes),
                    cx,
                )));
                self.generating = true;
            }
            protocol::HostMessage::AgentTextAppended { id, target, text } => {
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                let view = match target {
                    protocol::AgentText::Thinking => {
                        message.thinking.push_str(&text);
                        message.thinking_view.clone()
                    }
                    protocol::AgentText::Response => {
                        // Some models never close the reasoning block, so the
                        // first answer token ends it instead.
                        if !message.thinking.is_empty() && !message.thinking_complete {
                            message.thinking_complete = true;
                            message.thinking_expanded = false;
                        }
                        message.text.push_str(&text);
                        message.text_view.clone()
                    }
                };
                view.update(cx, |view, cx| view.push_str(&text, cx));
            }
            protocol::HostMessage::AgentThinkingEnded { id } => {
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                message.thinking_complete = true;
                message.thinking_expanded = false;
            }
            protocol::HostMessage::AgentCommentResponded {
                id,
                response_id,
                comment_id,
                response,
            } => {
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                message.comment_responses.push(AgentCommentResponse {
                    id: Uuid::from_bytes(response_id),
                    comment_id: Uuid::from_bytes(comment_id),
                    response_view: cx.new(|cx| TextViewState::markdown(&response, cx)),
                    response,
                });
            }
            protocol::HostMessage::AgentEnded { id, failure } => {
                self.generating = false;
                let Some(message) = self.agent_message_mut(id) else {
                    return;
                };
                message.complete = true;
                message.thinking_complete = true;
                message.thinking_expanded = false;
                message.failed = failure.is_some();
                // Only surface the failure when the agent said nothing itself.
                if let Some(failure) = failure
                    && message.text.is_empty()
                {
                    let view = message.text_view.clone();
                    view.update(cx, |view, cx| view.set_text(&failure, cx));
                    message.text = failure;
                }
            }
        }
    }

    fn set_timeline(&mut self, timeline: Vec<TimelineMessage>) {
        self.generating = timeline
            .iter()
            .any(|message| matches!(message, TimelineMessage::Agent(message) if !message.complete));
        self.timeline = timeline;
    }

    fn agent_message_mut(&mut self, id: uuid::Bytes) -> Option<&mut AgentMessage> {
        let id = Uuid::from_bytes(id);
        self.timeline.iter_mut().find_map(|entry| match entry {
            TimelineMessage::Agent(message) if message.id == id => Some(message),
            _ => None,
        })
    }
}

impl protocol::ThreadSnapshot {
    fn into_native(self, cx: &mut impl AppContext) -> (ThreadSummary, Vec<TimelineMessage>) {
        let summary = ThreadSummary {
            id: Uuid::from_bytes(self.id),
            title: self.title,
        };
        let timeline = self
            .messages
            .into_iter()
            .map(|message| message.into_native(cx))
            .collect();
        (summary, timeline)
    }
}

#[derive(Default)]
struct ThreadStore {
    threads: VecDeque<Entity<Thread>>,
}

impl ThreadStore {
    fn thread(&self, thread_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.threads
            .iter()
            .find(|thread| thread.read(cx).instance_id == thread_id)
            .cloned()
    }
}

struct ActiveGeneration {
    message_id: Uuid,
    abort_handle: tokio::task::AbortHandle,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone)]
struct CoworkSidebarSection {
    label: Option<SharedString>,
    menu: SidebarMenu,
    collapsed: bool,
    open: bool,
    on_label_click: Option<Rc<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>>,
}

impl CoworkSidebarSection {
    fn new(label: Option<impl Into<SharedString>>, menu: SidebarMenu) -> Self {
        Self {
            label: label.map(Into::into),
            menu,
            collapsed: false,
            open: true,
            on_label_click: None,
        }
    }

    fn label_toggle(
        mut self,
        open: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.open = open;
        self.on_label_click = Some(Rc::new(on_click));
        self
    }
}

impl Collapsible for CoworkSidebarSection {
    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    fn is_collapsed(&self) -> bool {
        self.collapsed
    }
}

impl SidebarItem for CoworkSidebarSection {
    fn render(
        self,
        id: impl Into<gpui::ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let open = self.open;
        let on_label_click = self.on_label_click;

        div()
            .flex()
            .flex_col()
            .when_some(self.label, |this, label| {
                this.child(
                    div()
                        .id(format!("{id}-label"))
                        .h(px(38.))
                        .flex()
                        .items_end()
                        .justify_between()
                        .px_2()
                        .pb_2()
                        .text_sm()
                        .text_color(rgb(0x71717a))
                        .child(label)
                        .when_some(on_label_click, |this, on_click| {
                            this.cursor_pointer()
                                .hover(|this| this.text_color(rgb(0xa1a1aa)))
                                .on_click(move |event, window, cx| on_click(event, window, cx))
                                .child(
                                    Icon::new(if open {
                                        AssetIconName::ChevronDown
                                    } else {
                                        AssetIconName::ChevronRight
                                    })
                                    .size_4()
                                    .text_color(rgb(0xa1a1aa)),
                                )
                        }),
                )
            })
            .when(open, |this| {
                this.child(
                    <SidebarMenu as SidebarItem>::render(
                        self.menu.collapsed(self.collapsed),
                        format!("{id}-menu"),
                        window,
                        cx,
                    )
                    .into_any_element(),
                )
            })
    }
}

/// What an attachment card shows, read out of a thread for rendering.
struct AttachmentCard {
    id: AttachmentId,
    name: String,
    kind: AttachmentKind,
    size: u64,
    image: Option<Arc<gpui::Image>>,
    transfer: Option<Transfer>,
}

/// A file on its way, with the percent done.
#[derive(Clone, Copy)]
enum Transfer {
    Uploading(f32),
    Downloading(f32),
}

impl AttachmentCard {
    fn new(draft: &ThreadDraft, record: &AttachmentRecord) -> Self {
        let file = draft.files.get(&record.id);
        let percent = |done: u64| {
            if record.size == 0 {
                100.
            } else {
                (done as f64 / record.size as f64 * 100.) as f32
            }
        };
        let transfer = if let Some(sent) = draft.uploads.get(&record.id) {
            Some(Transfer::Uploading(percent(*sent)))
        } else if file.is_none() {
            Some(Transfer::Downloading(
                draft
                    .incoming
                    .get(&record.id)
                    .map_or(0., IncomingFile::progress),
            ))
        } else {
            None
        };
        Self {
            id: record.id,
            name: record.name.clone(),
            kind: record.kind,
            size: record.size,
            image: file.and_then(|file| match &file.content {
                FileAttachmentContent::Png(image) | FileAttachmentContent::Jpeg(image) => {
                    Some(image.clone())
                }
                FileAttachmentContent::Text(_) => None,
            }),
            transfer,
        }
    }
}

/// What the composer shows of a draft, read out of it for rendering.
struct ComposerModel {
    draft_id: Uuid,
    comments: Vec<UserComment>,
    comments_folded: bool,
    blocks: Vec<ComposerBlock>,
    draft_position: Option<Entity<TextareaState>>,
    draft_row_visible: bool,
    /// The avatar the draft position row leads with, and those layered on it.
    draft_position_people: (ParticipantId, Vec<ParticipantId>),
    /// Everyone else's carets at the draft position.
    draft_position_presence: ItemPresence,
    /// Files being read, by the block they will land in; `None` is the draft
    /// position.
    pending: Vec<(Option<ItemId>, Attachment)>,
}

struct ComposerBlock {
    id: ItemId,
    creator: ParticipantId,
    presence: ItemPresence,
    editor: Entity<TextareaState>,
    attachments: Vec<AttachmentCard>,
}

#[derive(Clone)]
struct PendingAttachment {
    id: Uuid,
    draft_id: Uuid,
    target: AttachmentTarget,
    name: String,
    is_image: bool,
    progress: Option<f32>,
}

enum AttachmentReadEvent {
    Progress(Uuid, f32),
    Finished(Uuid, anyhow::Result<FileAttachment>),
}

struct AttachmentError {
    draft_id: Uuid,
    message: String,
}

struct Cowork {
    sidebar_open: bool,
    recents_open: bool,
    new_thread_draft: ThreadDraft,
    attachment_errors: Vec<AttachmentError>,
    pending_attachments: Vec<PendingAttachment>,
    timeline_scroll_handle: ScrollHandle,
    follow_generation: bool,
    thread_store: Entity<ThreadStore>,
    active_thread_id: Option<Uuid>,
    selection_message_id: Option<Uuid>,
    segment_text_views: HashMap<(ThreadMessageId, Range<usize>), SegmentTextView>,
    render_generation: u64,
    titlebar_click_armed: bool,
    copied_endpoint_id: Option<Uuid>,
    join_dialog: Option<Entity<JoinDialog>>,
    tokio_handle: tokio::runtime::Handle,
    active_generations: HashMap<Uuid, ActiveGeneration>,
    /// Who the local user is in the threads this app creates and hosts.
    local_participant_id: ParticipantId,
    /// The draft editor the user is typing in, so that when its item is
    /// removed, by a submission or by someone else, the caret can move to
    /// the draft position instead of vanishing.
    typing_in: Option<(Uuid, EntityId, gpui::FocusHandle)>,
    /// The presence last sent for each shared thread, by instance id.
    published_presence: HashMap<Uuid, protocol::Presence>,
    caret_label_refresh: Option<gpui::Task<()>>,
    /// The model new threads start with: the last one selected locally.
    new_thread_model: ModelSelection,
    /// Always shows the active thread's model; see [`Cowork::sync_model_picker`].
    model_picker: Entity<ModelPickerState>,
    model_picker_hovered: bool,
    _model_picker_subscription: Subscription,
    _window_activation_subscription: Subscription,
}

impl Cowork {
    /// Creates the model picker and subscribes `Cowork` to the user's picks.
    fn new_model_picker(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<ModelPickerState>, Subscription) {
        let picker = cx.new(|cx| {
            let mut picker = ComboboxState::new(language_model_groups(), Vec::new(), window, cx)
                .searchable(true);
            picker.set_selected_values(&[DEFAULT_MODEL], window, cx);
            picker
        });
        let subscription = cx.subscribe(&picker, Self::model_picker_event);
        (picker, subscription)
    }

    fn model_picker_event(
        &mut self,
        _: Entity<ModelPickerState>,
        event: &ComboboxEvent<ModelPickerItems>,
        cx: &mut Context<Self>,
    ) {
        if let ComboboxEvent::Change(selection) = event
            && let Some(model) = selection.first().copied()
        {
            self.select_model(model, cx);
        }
    }

    /// Applies a model picked by the local user to the active thread, and to
    /// every new thread from now on.
    fn select_model(&mut self, model: ModelSelection, cx: &mut Context<Self>) {
        self.new_thread_model = model;
        if let Some(thread) = self.active_thread(cx) {
            thread.update(cx, |thread, cx| thread.select_model(model, cx));
        }
        cx.notify();
    }

    fn active_thread(&self, cx: &App) -> Option<Entity<Thread>> {
        self.active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
    }

    /// The model of the active thread, or of the thread about to be created.
    fn active_model(&self, cx: &App) -> ModelSelection {
        self.active_thread(cx)
            .map(|thread| thread.read(cx).model)
            .unwrap_or(self.new_thread_model)
    }

    /// Points the picker at the active thread's model.
    ///
    /// The picker is shared by every thread, while each thread has its own
    /// model that collaborators can change at any time, so it is re-synced on
    /// every render rather than at each of the places either can change.
    /// Setting the selection does not emit a picker event, so this never feeds
    /// back into [`Cowork::select_model`].
    fn sync_model_picker(&self, window: &mut Window, cx: &mut App) {
        let model = self.active_model(cx);
        if self.model_picker.read(cx).selected_value() != Some(model) {
            self.model_picker.update(cx, |picker, cx| {
                picker.set_selected_values(&[model], window, cx);
            });
        }
    }

    /// An auto-growing editor holding `text`, with the caret at its end.
    fn new_draft_editor(text: &str, window: &mut Window, cx: &mut App) -> Entity<TextareaState> {
        cx.new(|cx| {
            let mut editor = TextareaState::new(window, cx).auto_grow(1, usize::MAX);
            editor.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            if !text.is_empty() {
                editor.set_value(SharedString::from(text.to_owned()), window, cx);
                editor.set_selected_range(text.len()..text.len(), cx);
            }
            editor
        })
    }

    /// A draft editor whose input events are routed to the draft `draft_id`.
    fn new_routed_draft_editor(
        draft_id: Uuid,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let editor = Self::new_draft_editor(text, window, cx);
        // Ends by itself once the editor is dropped along with its item.
        cx.subscribe_in(&editor, window, move |this, editor, event, window, cx| {
            this.draft_editor_event(draft_id, editor, event, window, cx);
        })
        .detach();
        editor
    }

    /// Brings a draft's editors in line with its document: creates editors
    /// for new items and the draft position, drops those of removed items,
    /// and shows text changed by anything other than the editor itself.
    fn prepare_draft(&mut self, draft_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        // Only when the removed editor still has focus: anything else the
        // user has moved to since keeps it.
        let focused = self
            .typing_in
            .as_ref()
            .filter(|(typing_draft, _, focus)| {
                *typing_draft == draft_id
                    && window.focused(cx).is_none_or(|focused| focused == *focus)
            })
            .map(|(_, editor, _)| *editor);
        let Some((missing, needs_draft_position, shown, lost_focus)) =
            self.update_draft(draft_id, cx, |draft| {
                let items = draft.doc.items();
                draft
                    .editors
                    .retain(|id, _| items.iter().any(|item| item.id == *id));
                // The item being typed in is gone: submitted, or removed by
                // someone else.
                let lost_focus = focused.is_some_and(|editor| draft.slot_of(editor).is_none());
                let mut missing = Vec::new();
                let mut shown = Vec::new();
                for item in items {
                    match draft.editors.get(&item.id) {
                        Some(editors) => shown.extend(
                            editors
                                .all()
                                .into_iter()
                                .map(|editor| (editor.clone(), item.body.clone())),
                        ),
                        None => missing.push(item),
                    }
                }
                (missing, draft.draft_position.is_none(), shown, lost_focus)
            })
        else {
            return;
        };
        if lost_focus {
            self.typing_in = None;
        }

        let synced = shown
            .into_iter()
            .filter(|(editor, body)| Self::show_text(editor, body, window, cx))
            .map(|(editor, body)| (editor.entity_id(), body))
            .collect::<Vec<_>>();
        let created = missing
            .into_iter()
            .map(|item| {
                let editors = if item.is_comment() {
                    ItemEditors::Comment {
                        inline: Self::new_routed_draft_editor(draft_id, &item.body, window, cx),
                        composer: Self::new_routed_draft_editor(draft_id, &item.body, window, cx),
                    }
                } else {
                    ItemEditors::Prompt(Self::new_routed_draft_editor(
                        draft_id, &item.body, window, cx,
                    ))
                };
                (item.id, editors)
            })
            .collect::<Vec<_>>();
        let draft_position =
            needs_draft_position.then(|| Self::new_routed_draft_editor(draft_id, "", window, cx));
        self.update_draft(draft_id, cx, |draft| {
            draft.synced_text.extend(synced);
            for (id, editors) in created {
                if let Some(body) = draft.doc.body(id) {
                    for editor in editors.all() {
                        draft.synced_text.insert(editor.entity_id(), body.clone());
                    }
                }
                draft.editors.entry(id).or_insert(editors);
            }
            if let Some(draft_position) = draft_position {
                draft.draft_position.get_or_insert(draft_position);
            }
        });
        if lost_focus {
            self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        }
    }

    /// Replaces an editor's text, keeping its selection on the same text.
    ///
    /// Waits while an IME composition is in progress: its marked text is in
    /// the editor but not yet in the document, and replacing it would cancel
    /// the composition. The text catches up once the composition commits.
    ///
    /// Returns whether the editor shows `text` afterwards.
    fn show_text(
        editor: &Entity<TextareaState>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let (value, selection) = {
            let editor = editor.read(cx);
            (editor.value(), editor.selected_range())
        };
        let Some(edit) = TextEdit::diff(&value, text) else {
            return true;
        };
        if editor
            .update(cx, |editor, cx| editor.marked_text_range(window, cx))
            .is_some()
        {
            return false;
        }
        let selection = edit.map_offset(selection.start)..edit.map_offset(selection.end);
        // Replacing the value also clears the editor's undo history, which
        // keeps undo from reverting anyone else's edits.
        editor.update(cx, |editor, cx| {
            editor.set_value(SharedString::from(text.to_owned()), window, cx);
            editor.set_selected_range(selection, cx);
        });
        true
    }

    fn draft_editor_event(
        &mut self,
        draft_id: Uuid,
        editor: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor_id = editor.entity_id();
        // Switching to another window blurs too, but the user will be back.
        if matches!(event, InputEvent::Blur)
            && window.is_window_active()
            && self
                .typing_in
                .as_ref()
                .is_some_and(|(typing_draft, typing_editor, _)| {
                    (*typing_draft, *typing_editor) == (draft_id, editor_id)
                })
        {
            self.typing_in = None;
        }
        match event {
            InputEvent::Change => {
                let value = editor.read(cx).value().to_string();
                let editor = editor.clone();
                let merged = self
                    .update_draft(draft_id, cx, {
                        let editor = editor.clone();
                        move |draft| match draft.slot_of(editor_id) {
                            // Typing at the draft position creates a block, and
                            // the editor typed into becomes that block's, so
                            // focus, caret and any IME composition carry on
                            // uninterrupted.
                            Some(EditorSlot::DraftPosition) if !value.is_empty() => {
                                let id = draft.doc.create_prompt(draft.author.as_uuid(), &value);
                                draft.editors.insert(id, ItemEditors::Prompt(editor));
                                draft.synced_text.insert(editor_id, value);
                                draft.draft_position = None;
                                None
                            }
                            Some(slot) => slot
                                .item()
                                .and_then(|id| draft.apply_typing(id, editor_id, &value)),
                            None => None,
                        }
                    })
                    .flatten();
                // Show others' edits the keystroke was merged with right away.
                if let Some(body) = merged
                    && Self::show_text(&editor, &body, window, cx)
                {
                    self.update_draft(draft_id, cx, |draft| {
                        draft.synced_text.insert(editor_id, body);
                    });
                }
                cx.notify();
            }
            // Empty items disappear once nobody is in them anymore. Switching
            // to another window also blurs, but the user has not left.
            InputEvent::Blur if window.is_window_active() => {
                let Some(id) = self
                    .read_draft(draft_id, cx, |draft| draft.slot_of(editor_id))
                    .flatten()
                    .and_then(EditorSlot::item)
                else {
                    return;
                };
                // Moving between the two editors of one comment, or leaving
                // a block whose files are still being read, is not leaving.
                let still_in_item = self
                    .read_draft(draft_id, cx, |draft| {
                        draft.editors.get(&id).is_some_and(|editors| {
                            editors
                                .all()
                                .into_iter()
                                .any(|editor| editor.focus_handle(cx).is_focused(window))
                        })
                    })
                    .unwrap_or(false);
                if still_in_item || self.has_pending_reads(draft_id, id, cx) {
                    return;
                }
                self.update_draft(draft_id, cx, |draft| draft.remove_if_unattended(id));
                cx.notify();
            }
            InputEvent::Focus => {
                self.typing_in = Some((draft_id, editor_id, editor.focus_handle(cx)));
            }
            InputEvent::Blur | InputEvent::PressEnter { .. } => {}
        }
    }

    /// Reads the writable draft `draft_id` wherever it lives.
    fn read_draft<R>(
        &self,
        draft_id: Uuid,
        cx: &App,
        read: impl FnOnce(&ThreadDraft) -> R,
    ) -> Option<R> {
        if self.new_thread_draft.id == draft_id {
            return Some(read(&self.new_thread_draft));
        }
        self.thread_store
            .read(cx)
            .threads
            .iter()
            .map(|thread| thread.read(cx))
            .find(|thread| thread.ownership.can_write() && thread.draft.id == draft_id)
            .map(|thread| read(&thread.draft))
    }

    /// The focused editor of the draft the composer shows.
    fn focused_draft_editor(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<(Uuid, EditorSlot, Entity<TextareaState>)> {
        let draft_id = self.writable_draft_id(cx)?;
        self.read_draft(draft_id, cx, |draft| {
            draft
                .draft_position
                .iter()
                .map(|editor| (EditorSlot::DraftPosition, editor.clone()))
                .chain(
                    draft
                        .editors
                        .iter()
                        .flat_map(|(&id, editors)| match editors {
                            ItemEditors::Prompt(editor) => {
                                vec![(EditorSlot::Prompt(id), editor.clone())]
                            }
                            ItemEditors::Comment { inline, composer } => vec![
                                (EditorSlot::CommentInline(id), inline.clone()),
                                (EditorSlot::CommentComposer(id), composer.clone()),
                            ],
                        }),
                )
                .find(|(_, editor)| editor.focus_handle(cx).is_focused(window))
                .map(|(slot, editor)| (draft_id, slot, editor))
        })?
    }

    /// Focuses an editor of a draft, optionally moving its caret.
    fn focus_draft_editor(
        &mut self,
        draft_id: Uuid,
        slot: EditorSlot,
        caret: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.prepare_draft(draft_id, window, cx);
        let Some(editor) = self
            .read_draft(draft_id, cx, |draft| draft.editor(slot))
            .flatten()
        else {
            return false;
        };
        if let Some(caret) = caret {
            let caret = caret.min(editor.read(cx).value().len());
            editor.update(cx, |editor, cx| editor.set_selected_range(caret..caret, cx));
        }
        editor.focus_handle(cx).focus(window, cx);
        // Recorded right away; the focus event only arrives with the next
        // frame, after the editor it replaces may already be gone.
        self.typing_in = Some((draft_id, editor.entity_id(), editor.focus_handle(cx)));
        cx.notify();
        true
    }

    /// Focuses where the user most likely continues typing: the last prompt
    /// block if there is one, otherwise the draft position.
    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        self.prepare_draft(draft_id, window, cx);
        let last_block = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .doc
                    .items()
                    .into_iter()
                    .rfind(|item| item.is_prompt())
                    .map(|item| (item.id, item.body.len()))
            })
            .flatten();
        match last_block {
            Some((id, end)) => {
                self.focus_draft_editor(draft_id, EditorSlot::Prompt(id), Some(end), window, cx)
            }
            None => self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx),
        };
    }

    /// Puts the caret into the composer unless it is already in the draft.
    fn ensure_composer_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focused_draft_editor(window, cx).is_none() {
            self.focus_composer(window, cx);
        }
    }

    /// Where the local user is in `thread`'s draft, as others should see it.
    fn local_presence(&self, thread: &Thread, cx: &App) -> protocol::Presence {
        let draft = &thread.draft;
        let slot = self
            .typing_in
            .as_ref()
            .filter(|(typing_draft, _, _)| {
                *typing_draft == draft.id && self.active_thread_id == Some(thread.instance_id)
            })
            .and_then(|(_, editor, _)| draft.slot_of(*editor));
        let focus = slot.map(|slot| match slot.item() {
            Some(id) => protocol::PresenceFocus::Item(id.as_uuid().into_bytes()),
            None => protocol::PresenceFocus::DraftPosition,
        });
        let selection = slot
            .and_then(|slot| Some((slot.item()?, draft.editor(slot)?)))
            .and_then(|(id, editor)| {
                let editor = editor.read(cx);
                let range = editor.selected_range();
                let head = editor.cursor();
                let tail = if head == range.start {
                    range.end
                } else {
                    range.start
                };
                Some(protocol::PresenceSelection {
                    anchor: draft.doc.anchor(id, tail)?,
                    head: draft.doc.anchor(id, head)?,
                })
            });
        let pending_reads = self
            .pending_attachments
            .iter()
            .filter(|pending| pending.draft_id == draft.id)
            .map(|pending| protocol::PendingRead {
                id: pending.id.into_bytes(),
                name: pending.name.clone(),
                is_image: pending.is_image,
                progress: pending
                    .progress
                    .map(|progress| progress.clamp(0., 100.) as u8),
                block: draft
                    .pending_block(pending.target)
                    .map(|id| id.as_uuid().into_bytes()),
            })
            .collect();
        protocol::Presence {
            focus,
            selection,
            pending_reads,
        }
    }

    /// Sends the local user's presence in every shared thread whose has
    /// changed since it was last sent.
    fn publish_presence(&mut self, cx: &mut Context<Self>) {
        let threads = self.thread_store.read(cx).threads.clone();
        let mut collaborating = HashSet::new();
        for thread in threads {
            let (thread_id, presence) = {
                let thread = thread.read(cx);
                if !matches!(
                    thread.sharing,
                    ThreadSharing::Shared { .. } | ThreadSharing::Connected { .. }
                ) {
                    continue;
                }
                (thread.instance_id, self.local_presence(thread, cx))
            };
            collaborating.insert(thread_id);
            if self.published_presence.get(&thread_id) == Some(&presence) {
                continue;
            }
            self.published_presence.insert(thread_id, presence.clone());
            thread.update(cx, |thread, cx| thread.publish_presence(presence, cx));
        }
        // Sharing again starts over, with everyone joining learning it anew.
        self.published_presence
            .retain(|thread_id, _| collaborating.contains(thread_id));
    }

    /// Redraws once the caret labels shown now have expired.
    fn schedule_caret_label_refresh(&mut self, cx: &mut Context<Self>) {
        if self.caret_label_refresh.is_some() {
            return;
        }
        let Some(thread) = self.active_thread(cx) else {
            return;
        };
        let now = Instant::now();
        let Some(expires_in) = thread
            .read(cx)
            .draft
            .presence
            .values()
            .filter_map(|(_, changed)| {
                (*changed + CARET_LABEL_DURATION).checked_duration_since(now)
            })
            .min()
        else {
            return;
        };
        self.caret_label_refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(expires_in + Duration::from_millis(20))
                .await;
            _ = this.update(cx, |this, cx| {
                this.caret_label_refresh = None;
                cx.notify();
            });
        }));
    }

    /// Whether files are still being read into the block `id`.
    fn has_pending_reads(&self, draft_id: Uuid, id: ItemId, cx: &App) -> bool {
        self.read_draft(draft_id, cx, |draft| {
            self.pending_attachments.iter().any(|pending| {
                pending.draft_id == draft_id && draft.pending_block(pending.target) == Some(id)
            })
        })
        .unwrap_or(false)
    }

    /// Moves between the composer's editors when Up or Down leaves the first
    /// or last visual line. Returns whether it moved.
    fn move_between_draft_editors(
        &mut self,
        up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((draft_id, slot, editor)) = self.focused_draft_editor(window, cx) else {
            return false;
        };
        // A selection collapses first, as it does inside an editor.
        if !editor.read(cx).selected_range().is_empty() {
            return false;
        }
        let Some(caret_x) = caret_x_on_edge_line(editor.read(cx), !up) else {
            return false;
        };
        let Some(chain) = self.read_draft(draft_id, cx, ThreadDraft::navigation_chain) else {
            return false;
        };
        let Some(index) = chain.iter().position(|(chain_slot, _)| *chain_slot == slot) else {
            return false;
        };
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|next| *next < chain.len())
        };
        let Some((target_slot, target)) = target.map(|target| chain[target].clone()) else {
            return false;
        };
        let caret = offset_near_x(target.read(cx), caret_x, up);
        self.focus_draft_editor(draft_id, target_slot, Some(caret), window, cx)
    }

    /// Escape in an empty item removes it and returns to the draft position.
    fn escape_empty_draft_item(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((draft_id, slot, _)) = self.focused_draft_editor(window, cx) else {
            return false;
        };
        let Some(id) = slot.item() else {
            return false;
        };
        if !self.item_is_empty(draft_id, id, cx) {
            return false;
        }
        // Kept while someone else is in it, but the caret moves on either way.
        self.update_draft(draft_id, cx, |draft| draft.remove_if_unattended(id));
        self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
        true
    }

    fn item_is_empty(&self, draft_id: Uuid, id: ItemId, cx: &App) -> bool {
        self.read_draft(draft_id, cx, |draft| {
            draft.doc.item(id).is_some_and(|item| item.is_empty())
        })
        .unwrap_or(false)
    }

    /// Backspace in an empty item removes it, and in the empty draft position
    /// steps back; either way the caret moves to the end of the editor above.
    fn backspace_out_of_empty_draft_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((draft_id, slot, editor)) = self.focused_draft_editor(window, cx) else {
            return false;
        };
        if !editor.read(cx).value().is_empty() {
            return false;
        }
        let Some(chain) = self.read_draft(draft_id, cx, ThreadDraft::navigation_chain) else {
            return false;
        };
        let previous = chain
            .iter()
            .position(|(chain_slot, _)| *chain_slot == slot)
            .and_then(|index| index.checked_sub(1))
            .map(|index| chain[index].clone());
        match slot.item() {
            Some(id) => {
                if !self.item_is_empty(draft_id, id, cx) {
                    return false;
                }
                // Kept while someone else is in it; the caret moves anyway.
                self.update_draft(draft_id, cx, |draft| draft.remove_if_unattended(id));
            }
            None if previous.is_none() => return false,
            None => {}
        }
        match previous {
            Some((slot, editor)) => {
                let end = editor.read(cx).value().len();
                self.focus_draft_editor(draft_id, slot, Some(end), window, cx)
            }
            None => self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx),
        }
    }

    fn new_local_thread(
        title: String,
        timeline: Vec<TimelineMessage>,
        draft: ThreadDraft,
        participant_id: ParticipantId,
        model: ModelSelection,
        cx: &mut App,
    ) -> Entity<Thread> {
        let thread_id = Uuid::new_v4();
        cx.new(|_| Thread {
            instance_id: thread_id,
            summary: ThreadSummary {
                id: thread_id,
                title,
            },
            participant_id,
            participants: Vec::new(),
            model,
            timeline,
            draft,
            generating: false,
            sharing: ThreadSharing::NotShared,
            ownership: ThreadOwnership::Local,
        })
    }

    fn new_empty_local_thread(
        draft: ThreadDraft,
        participant_id: ParticipantId,
        model: ModelSelection,
        cx: &mut App,
    ) -> Entity<Thread> {
        Self::new_local_thread(
            "New thread".into(),
            Vec::new(),
            draft,
            participant_id,
            model,
            cx,
        )
    }

    fn prepare_thread_for_sharing(&mut self, cx: &mut Context<Self>) -> Entity<Thread> {
        if let Some(thread) = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
        {
            return thread;
        }

        let draft = std::mem::replace(
            &mut self.new_thread_draft,
            ThreadDraft::new(self.local_participant_id),
        );
        let thread = Self::new_empty_local_thread(
            draft,
            self.local_participant_id,
            self.new_thread_model,
            cx,
        );
        self.active_thread_id = Some(thread.read(cx).instance_id);
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        thread
    }

    fn end_stale_mouse_drag(window: &mut Window, cx: &mut App) {
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: window.mouse_position(),
                modifiers: window.modifiers(),
                click_count: 1,
            }),
            cx,
        );
    }

    fn render_caption_button(
        id: &'static str,
        icon: &'static str,
        control_area: WindowControlArea,
        is_close: bool,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h_full()
            .w(px(46.))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .text_size(px(10.))
            .text_color(rgb(0xd4d4d8))
            .window_control_area(control_area)
            .when(is_close, |this| this.hover(|this| this.bg(rgb(0xe81123))))
            .when(!is_close, |this| this.hover(|this| this.bg(rgb(0x2d2d30))))
            .child(icon)
    }

    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .ml(if cfg!(target_os = "macos") {
                macos_sidebar_toggle_margin()
            } else {
                SIDEBAR_TOGGLE_INSET
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.titlebar_click_armed = false;
                    cx.stop_propagation();
                }),
            )
            .child(
                SidebarToggleButton::new()
                    .collapsed(!self.sidebar_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    })),
            )
    }

    fn start_sharing(&mut self, thread: Entity<Thread>, cx: &mut Context<Self>) {
        thread.update(cx, |thread, _| {
            thread.sharing = ThreadSharing::Sharing;
        });
        cx.notify();

        let (peers, accepted_peers) = async_channel::bounded(protocol::PEER_CHANNEL_CAPACITY);
        let endpoint_task = Self::bind_shared_endpoint(&self.tokio_handle, peers);

        cx.spawn(async move |this, cx| {
            let endpoint = endpoint_task
                .await
                .context("Endpoint setup task failed.")
                .and_then(|result| result);
            let endpoint = match endpoint {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    eprintln!("failed to share thread: {error:#}");
                    thread.update(cx, |thread, _| thread.sharing = ThreadSharing::Failed);
                    _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
            };
            thread.update(cx, |thread, _| {
                thread.sharing = ThreadSharing::Shared {
                    endpoint,
                    events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
                };
                thread.participants = vec![thread.participant_id];
            });
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }

            // Peers are only served once the thread is hosting, so that every
            // one of them can subscribe to its events. Connections accepted
            // before that wait in the channel.
            let thread = thread.downgrade();
            while let Ok(peer) = accepted_peers.recv().await {
                let this = this.clone();
                let thread = thread.clone();
                cx.spawn(async move |cx| {
                    if let Err(error) = Self::serve_peer(this, thread, peer, cx).await {
                        eprintln!("stopped serving collaborator: {error:#}");
                    }
                })
                .detach();
            }
        })
        .detach();
    }

    /// Binds the endpoint collaborators dial into, forwarding every accepted
    /// connection to `peers` as a ready to use protocol channel.
    fn bind_shared_endpoint(
        tokio_handle: &tokio::runtime::Handle,
        peers: async_channel::Sender<HostPeer>,
    ) -> tokio::task::JoinHandle<anyhow::Result<Endpoint>> {
        tokio_handle.spawn(async move {
            let endpoint = Endpoint::builder(presets::N0)
                .alpns(vec![COWORK_ALPN.to_vec()])
                .bind()
                .await?;

            tokio::spawn({
                let endpoint = endpoint.clone();
                async move {
                    while let Some(incoming) = endpoint.accept().await {
                        let accepting = match incoming.accept() {
                            Ok(accepting) => accepting,
                            Err(error) => {
                                eprintln!("failed to accept connection: {error}");
                                continue;
                            }
                        };
                        let peers = peers.clone();
                        tokio::spawn(async move {
                            if let Err(error) = Self::accept_peer(accepting, peers).await {
                                eprintln!("failed to accept collaborator: {error:#}");
                            }
                        });
                    }
                }
            });

            Ok(endpoint)
        })
    }

    /// Finishes one incoming connection's handshake and hands its protocol
    /// channel over to the foreground.
    async fn accept_peer(
        accepting: Accepting,
        peers: async_channel::Sender<HostPeer>,
    ) -> anyhow::Result<()> {
        let connection = accepting.await?;
        let (send, recv) = tokio::time::timeout(PEER_TIMEOUT, connection.accept_bi())
            .await
            .context("Timed out waiting for a peer protocol stream.")??;
        // The streams keep the connection alive, so `connection` itself can go.
        peers
            .send(protocol::spawn_peer(tokio::io::join(recv, send)))
            .await
            .context("Shared thread is no longer available.")
    }

    /// Serves one collaborator for as long as it stays connected, keeping it
    /// listed as a participant for exactly that long.
    async fn serve_peer(
        cowork: WeakEntity<Self>,
        thread: WeakEntity<Thread>,
        peer: HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let join = peer
            .receive()
            .with_timeout(PEER_TIMEOUT, cx.background_executor())
            .await?
            .context("Peer closed before joining.")?;
        let protocol::CollaboratorMessage::Join { protocol_version } = join else {
            anyhow::bail!("Expected a join message, got {join:?}.");
        };
        if protocol_version != protocol::PROTOCOL_VERSION {
            let reason = format!(
                "The host uses collaboration protocol version {}, but this app uses version \
                 {protocol_version}. Both participants need the same version of Cowork.",
                protocol::PROTOCOL_VERSION,
            );
            _ = peer.send(protocol::HostMessage::Rejected(reason)).await;
            // Dropping the peer would tear down the connection right away,
            // which may discard the rejection before it is delivered. The
            // collaborator hangs up once it has read it.
            while peer
                .receive()
                .with_timeout(PEER_TIMEOUT, cx.background_executor())
                .await
                .is_ok_and(|request| request.is_some())
            {}
            anyhow::bail!("Rejected a peer using protocol version {protocol_version}.");
        }

        let participant_id = ParticipantId::new();
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::ParticipantJoined(participant_id.into_bytes()),
                cx,
            );
        })?;
        _ = cowork.update(cx, |_, cx| cx.notify());

        let result = Self::serve_participant(&cowork, &thread, participant_id, &peer, cx).await;

        _ = thread.update(cx, |thread, cx| thread.participant_left(participant_id, cx));
        _ = cowork.update(cx, |_, cx| cx.notify());
        result
    }

    /// Re-bases a joined collaborator onto a snapshot, then feeds it the
    /// thread's events verbatim while handling its requests, until either
    /// side goes away.
    async fn serve_participant(
        cowork: &WeakEntity<Self>,
        thread: &WeakEntity<Thread>,
        participant_id: ParticipantId,
        peer: &HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let (mut events, files) = Self::send_snapshot(thread, participant_id, peer, cx).await?;
        // Files to send, each with how far it got, one chunk at a time on the
        // bulk queue so they never hold up anything else.
        let mut sends = files.iter().map(|id| (*id, 0)).collect::<VecDeque<_>>();
        let mut queued = files.into_iter().collect::<HashSet<_>>();
        loop {
            let chunk = match sends.front() {
                Some(&(id, offset)) => {
                    thread.update(cx, |thread, _| thread.draft.chunk(id, offset))?
                }
                None => None,
            };
            if sends.front().is_some() && chunk.is_none() {
                // Removed since.
                sends.pop_front();
                continue;
            }
            let bulk = peer.bulk.clone();
            let send_chunk = async move {
                match chunk {
                    Some(chunk) => {
                        let next = chunk.offset + chunk.bytes.len() as u64;
                        let done = next >= chunk.total;
                        bulk.send(protocol::HostMessage::AttachmentData(chunk))
                            .await
                            .map(|()| (next, done))
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(event) => {
                        if let protocol::HostMessage::AttachmentStored { id, uploader } = &event
                            && *uploader != participant_id.into_bytes()
                        {
                            let id = AttachmentId::from_uuid(Uuid::from_bytes(*id));
                            if queued.insert(id) {
                                sends.push_back((id, 0));
                            }
                        }
                        peer.send(event)
                            .await
                            .context("Peer stopped receiving thread events.")?;
                    }
                    // A peer that fell further behind than the event buffer
                    // has missed changes, so re-base it rather than applying
                    // deltas to a timeline that no longer matches the host's.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let files;
                        (events, files) =
                            Self::send_snapshot(thread, participant_id, peer, cx).await?;
                        for id in files {
                            if queued.insert(id) {
                                sends.push_back((id, 0));
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                sent = send_chunk => {
                    let (next, done) = sent.context("Peer stopped receiving attachments.")?;
                    if done {
                        sends.pop_front();
                    } else if let Some(front) = sends.front_mut() {
                        front.1 = next;
                    }
                }
                // Also how a disconnect is noticed while the thread is quiet.
                request = peer.receive() => {
                    let Some(request) = request else {
                        return Ok(());
                    };
                    let Some(thread) = thread.upgrade() else {
                        return Ok(());
                    };
                    cowork.update(cx, |cowork, cx| {
                        cowork.collaborator_request(&thread, participant_id, request, cx)
                    })??;
                }
            }
        }
    }

    /// Sends a peer a full snapshot and returns a subscription that resumes
    /// exactly where the snapshot left off, with the files the peer needs the
    /// bytes of.
    async fn send_snapshot(
        thread: &WeakEntity<Thread>,
        participant_id: ParticipantId,
        peer: &HostPeer,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<(
        broadcast::Receiver<protocol::HostMessage>,
        Vec<AttachmentId>,
    )> {
        let (snapshot, draft, presence, stored_attachments, files, events) = thread
            .update(cx, |thread, _| {
                let presence = thread
                    .draft
                    .presence
                    .iter()
                    .map(|(participant, (presence, _))| {
                        (participant.into_bytes(), presence.clone())
                    })
                    .collect();
                let stored = thread
                    .draft
                    .stored
                    .iter()
                    .map(|id| id.as_uuid().into_bytes())
                    .collect();
                Some((
                    thread.to_protocol(),
                    thread.draft.doc.encode_state(),
                    presence,
                    stored,
                    thread.files_for(participant_id),
                    thread.subscribe()?,
                ))
            })?
            .context("Thread is no longer shared.")?;
        peer.send(protocol::HostMessage::Welcome(protocol::Welcome {
            participant_id: participant_id.into_bytes(),
            thread: snapshot,
            draft,
            presence,
            stored_attachments,
        }))
        .await
        .context("Peer disconnected before receiving the thread snapshot.")?;
        Ok((events, files))
    }

    /// Applies a request a collaborator sent to a thread this app hosts.
    ///
    /// Fails when the collaborator broke the protocol and must be
    /// disconnected.
    fn collaborator_request(
        &mut self,
        thread: &Entity<Thread>,
        participant: ParticipantId,
        request: protocol::CollaboratorMessage,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match request {
            // Only valid as the first message, which `serve_peer` consumes.
            protocol::CollaboratorMessage::Join { .. } => {}
            protocol::CollaboratorMessage::DraftUpdate(update) => {
                thread
                    .update(cx, |thread, _| {
                        thread.apply_collaborator_update(participant, update)
                    })
                    .with_context(|| format!("Invalid draft update from {participant:?}."))?;
                cx.notify();
            }
            protocol::CollaboratorMessage::Submit { sequence } => {
                let (draft_id, stale) = {
                    let thread = thread.read(cx);
                    (
                        thread.draft.id,
                        thread.generating || thread.submission_count() != sequence,
                    )
                };
                // A stale sequence means another submission won the race;
                // everyone sees that one.
                // TODO: tell the submitter why a submission was not accepted.
                if !stale && !self.draft_is_loading_attachments(draft_id, cx) {
                    self.accept_submission(draft_id, Some(thread.clone()), false, cx);
                    let thread_id = thread.read(cx).instance_id;
                    self.thread_updated(thread_id, cx);
                }
            }
            protocol::CollaboratorMessage::AttachmentCancelled(id) => {
                let id = AttachmentId::from_uuid(Uuid::from_bytes(id));
                thread.update(cx, |thread, _| {
                    let draft = &mut thread.draft;
                    let theirs = draft
                        .incoming
                        .get(&id)
                        .is_some_and(|file| file.uploader == Some(participant));
                    if theirs {
                        draft.incoming.remove(&id);
                        draft.discarded.insert(id);
                    }
                });
            }
            protocol::CollaboratorMessage::AttachmentData(chunk) => {
                thread
                    .update(cx, |thread, _| thread.receive_upload(participant, chunk))
                    .with_context(|| format!("Invalid attachment data from {participant:?}."))?;
                cx.notify();
            }
            protocol::CollaboratorMessage::Presence(presence) => {
                thread.update(cx, |thread, cx| {
                    thread.host_presence(participant, presence, cx);
                });
                cx.notify();
            }
            protocol::CollaboratorMessage::SelectModel { catalog_id } => {
                let Some(model) = ModelSelection::from_catalog_id(&catalog_id) else {
                    return Ok(());
                };
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::ModelSelected {
                            catalog_id: model.catalog_id.into(),
                        },
                        cx,
                    );
                });
                cx.notify();
            }
            protocol::CollaboratorMessage::Stop { message_id } => {
                let thread_id = thread.read(cx).instance_id;
                self.cancel_generation(thread_id, Some(Uuid::from_bytes(message_id)), cx);
            }
        }
        Ok(())
    }

    fn copy_endpoint_id(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let endpoint_id = {
            let thread = thread.read(cx);
            let ThreadSharing::Shared { endpoint, .. } = &thread.sharing else {
                return;
            };
            endpoint.id().to_string()
        };

        cx.write_to_clipboard(ClipboardItem::new_string(endpoint_id));
        self.copied_endpoint_id = Some(thread_id);
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_500))
                .await;
            _ = this.update(cx, |this, cx| {
                if this.copied_endpoint_id == Some(thread_id) {
                    this.copied_endpoint_id = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_join_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let endpoint_token = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Paste endpoint token");
            input.set_editor_style(InputEditorStyle {
                caret: rgb(0xffffff).into(),
                ..Default::default()
            });
            input
        });
        let join_dialog = cx.new(|_| JoinDialog {
            endpoint_token: endpoint_token.clone(),
            status: JoinStatus::Idle,
            _input_subscription: None,
        });
        let input_subscription =
            cx.subscribe(&endpoint_token, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(dialog) = &this.join_dialog {
                        dialog.update(cx, |dialog, _| {
                            if matches!(dialog.status, JoinStatus::Failed(_)) {
                                dialog.status = JoinStatus::Idle;
                            }
                        });
                    }
                    cx.notify();
                }
            });
        join_dialog.update(cx, |dialog, _| {
            dialog._input_subscription = Some(input_subscription);
        });
        self.join_dialog = Some(join_dialog.clone());

        let cowork = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let join_state = join_dialog.read(cx);
            let joining = matches!(join_state.status, JoinStatus::Joining);
            let has_endpoint_id_length =
                endpoint_id_input_is_complete(join_state.endpoint_token.read(cx).value().as_ref());
            let error = match &join_state.status {
                JoinStatus::Failed(error) => Some(error.clone()),
                _ => None,
            };

            let cancel_cowork = cowork.clone();
            let cancel_dialog = join_dialog.clone();
            let join_cowork = cowork.clone();
            let dismiss_cowork = cowork.clone();
            let dismiss_dialog = join_dialog.clone();
            let endpoint_token = join_state.endpoint_token.clone();
            dialog
                .w(px(440.))
                .bg(rgb(0x1c1c1f))
                .keyboard(!joining)
                .overlay_closable(!joining)
                .close_button(!joining)
                .on_cancel(move |_, _, cx| {
                    Self::dismiss_join_dialog(&dismiss_cowork, &dismiss_dialog, cx)
                })
                .content(move |content, _, _| {
                    content
                        .child(
                            DialogHeader::new()
                                .child(DialogTitle::new().child("Join shared thread"))
                                .child(
                                    DialogDescription::new()
                                        .child("Paste the endpoint token shared with you."),
                                ),
                        )
                        .child(
                            div()
                                .id("endpoint-token-input")
                                .h(px(38.))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x52525b))
                                .bg(rgb(0x18181b))
                                .child(Input::new(&endpoint_token)),
                        )
                        .children(
                            error
                                .as_ref()
                                .map(|error| div().text_color(rgb(0xf87171)).child(error.clone())),
                        )
                        .child(
                            DialogFooter::new()
                                .child(
                                    Button::new("cancel-join")
                                        .outline()
                                        .label("Cancel")
                                        .disabled(joining)
                                        .on_click({
                                            let cancel_cowork = cancel_cowork.clone();
                                            let cancel_dialog = cancel_dialog.clone();
                                            move |_, window, cx| {
                                                if Self::dismiss_join_dialog(
                                                    &cancel_cowork,
                                                    &cancel_dialog,
                                                    cx,
                                                ) {
                                                    window.close_dialog(cx);
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("confirm-join")
                                        .primary()
                                        .label(if joining { "Joining…" } else { "Join thread" })
                                        .loading(joining)
                                        .disabled(!has_endpoint_id_length)
                                        .on_click({
                                            let join_cowork = join_cowork.clone();
                                            move |_, window, cx| {
                                                _ = join_cowork.update(cx, |cowork, cx| {
                                                    cowork.join_shared_thread(window, cx);
                                                });
                                            }
                                        }),
                                ),
                        )
                })
        });
        endpoint_token.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn dismiss_join_dialog(
        cowork: &WeakEntity<Self>,
        dialog: &Entity<JoinDialog>,
        cx: &mut App,
    ) -> bool {
        if matches!(dialog.read(cx).status, JoinStatus::Joining) {
            return false;
        }
        cowork
            .update(cx, |cowork, cx| {
                cowork.join_dialog = None;
                cx.notify();
            })
            .is_ok()
    }

    fn join_shared_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.join_dialog.clone() else {
            return;
        };
        if matches!(dialog.read(cx).status, JoinStatus::Joining) {
            return;
        }

        let token = dialog
            .read(cx)
            .endpoint_token
            .read(cx)
            .value()
            .trim()
            .to_string();
        let endpoint_id = match token.parse::<EndpointId>() {
            Ok(endpoint_id) => endpoint_id,
            Err(_) => {
                dialog.update(cx, |dialog, _| {
                    dialog.status = JoinStatus::Failed("Enter a valid endpoint token.".into());
                });
                cx.notify();
                return;
            }
        };
        dialog.update(cx, |dialog, _| {
            dialog.status = JoinStatus::Joining;
        });
        let window_handle = window.window_handle();
        cx.notify();

        let join_task = self.tokio_handle.spawn(async move {
            let endpoint = Endpoint::builder(presets::N0).bind().await?;
            let connection =
                tokio::time::timeout(PEER_TIMEOUT, endpoint.connect(endpoint_id, COWORK_ALPN))
                    .await
                    .context("Connection timed out.")??;
            let (send, recv) = tokio::time::timeout(PEER_TIMEOUT, connection.open_bi())
                .await
                .context("Timed out opening the peer protocol stream.")??;
            let host: ThreadHost = protocol::spawn_peer(tokio::io::join(recv, send));
            host.send(protocol::CollaboratorMessage::Join {
                protocol_version: protocol::PROTOCOL_VERSION,
            })
            .await
            .context("Peer connection is no longer available.")?;
            let welcome = tokio::time::timeout(PEER_TIMEOUT, host.receive())
                .await
                .context("Timed out waiting for the thread snapshot.")?
                .context("Host closed the protocol stream before sending the thread snapshot.")?;
            let welcome = match welcome {
                protocol::HostMessage::Welcome(welcome) => welcome,
                protocol::HostMessage::Rejected(reason) => anyhow::bail!(reason),
                _ => anyhow::bail!("Host sent a thread event before the thread snapshot."),
            };
            Ok::<_, anyhow::Error>((endpoint, connection, host, welcome))
        });

        cx.spawn(async move |this, cx| {
            let result = join_task
                .await
                .context("Join task failed.")
                .and_then(|result| result);
            let (endpoint, connection, host, welcome) = match result {
                Ok(joined) => joined,
                Err(error) => {
                    eprintln!("failed to join shared thread: {error:#}");
                    dialog.update(cx, |dialog, _| {
                        dialog.status = JoinStatus::Failed(error.to_string());
                    });
                    _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
            };

            let link = PeerLink {
                endpoint,
                _connection: connection,
            };
            if this
                .update(cx, move |this, cx| {
                    this.mirror_thread(welcome, host, Some(link), cx);
                    this.join_dialog = None;
                })
                .is_err()
            {
                return;
            }
            _ = cx.update_window(window_handle, |_, window, cx| {
                window.close_dialog(cx);
            });
        })
        .detach();
    }

    /// Opens a joined thread: builds its mirror from the host's welcome,
    /// replays the host's events onto it, and forwards its requests to the
    /// host in order. Returns the new thread's id.
    fn mirror_thread(
        &mut self,
        welcome: protocol::Welcome,
        host: ThreadHost,
        link: Option<PeerLink>,
        cx: &mut Context<Self>,
    ) -> Uuid {
        let (host_requests, uploads, events) = host.split();
        let (requests, queued_requests) = async_channel::unbounded();
        cx.background_spawn(async move {
            while let Ok(request) = queued_requests.recv().await {
                if host_requests.send(request).await.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Re-authored with the id the host assigned by `from_welcome`.
        let draft = ThreadDraft::new(self.local_participant_id);
        let thread = cx.new(|cx| {
            Thread::from_welcome(
                welcome,
                draft,
                ThreadSharing::Connected {
                    host: requests,
                    uploads,
                    link,
                },
                cx,
            )
        });
        let thread_id = thread.read(cx).instance_id;
        self.thread_store.update(cx, |store, _| {
            store.threads.push_front(thread.clone());
        });
        self.active_thread_id = Some(thread_id);
        self.selection_message_id = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                // Carets move constantly; they only need a redraw, not the
                // timeline following new output.
                let presence_only = matches!(
                    event,
                    protocol::HostMessage::Presence { .. }
                        | protocol::HostMessage::AttachmentData(_)
                );
                thread.update(cx, |thread, cx| thread.apply(event, cx));
                if this
                    .update(cx, |this, cx| {
                        if presence_only {
                            cx.notify();
                        } else {
                            this.thread_updated(thread_id, cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
            // The host stopped sharing, went away, or dropped us.
            _ = this.update(cx, |this, cx| this.remove_mirrored_thread(thread_id, cx));
        })
        .detach();
        thread_id
    }

    /// Closes a joined thread, which has nothing left to show once it is no
    /// longer connected to its host.
    fn remove_mirrored_thread(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        if !thread.read(cx).ownership.remove_on_disconnect() {
            return;
        }
        self.thread_store.update(cx, |store, cx| {
            store
                .threads
                .retain(|thread| thread.read(cx).instance_id != thread_id);
        });
        if self.active_thread_id == Some(thread_id) {
            self.active_thread_id = None;
            self.selection_message_id = None;
        }
        cx.notify();
    }

    fn toggle_sharing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self.prepare_thread_for_sharing(cx);
        let thread_id = thread.read(cx).instance_id;

        match thread.read(cx).sharing.status() {
            SharingStatus::NotShared | SharingStatus::Failed => self.start_sharing(thread, cx),
            SharingStatus::Sharing => {}
            SharingStatus::Shared => {
                // Replacing the state drops the event channel, which ends every
                // peer's subscription and unwinds the tasks serving them.
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Shared { endpoint, .. } =
                        std::mem::replace(&mut thread.sharing, ThreadSharing::NotShared)
                    else {
                        return None;
                    };
                    thread.participants.clear();
                    thread.draft.presence.clear();
                    Some(endpoint)
                });
                if let Some(endpoint) = endpoint {
                    self.tokio_handle.spawn(async move {
                        endpoint.close().await;
                    });
                }
                cx.notify();
            }
            SharingStatus::Connected => {
                // Replacing the state drops the channel to the host, closing
                // the protocol stream that kept the connection alive.
                let endpoint = thread.update(cx, |thread, _| {
                    let ThreadSharing::Connected { link, .. } =
                        std::mem::replace(&mut thread.sharing, ThreadSharing::NotShared)
                    else {
                        return None;
                    };
                    link.map(|link| link.endpoint)
                });
                if thread.read(cx).ownership.remove_on_disconnect() {
                    self.remove_mirrored_thread(thread_id, cx);
                    self.new_thread_draft = ThreadDraft::new(self.local_participant_id);
                    self.focus_composer(window, cx);
                }
                if let Some(endpoint) = endpoint {
                    self.tokio_handle.spawn(async move {
                        endpoint.close().await;
                    });
                }
                cx.notify();
            }
        }
    }

    fn render_top_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_thread = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx));
        let sharing_status = active_thread
            .as_ref()
            .map(|thread| thread.read(cx).sharing.status())
            .unwrap_or(SharingStatus::NotShared);
        let share_label = match sharing_status {
            SharingStatus::NotShared => "Share",
            SharingStatus::Sharing => "Sharing…",
            SharingStatus::Shared => "Unshare",
            SharingStatus::Connected => "Disconnect",
            SharingStatus::Failed => "Retry share",
        };
        let sharing_enabled = sharing_status != SharingStatus::Sharing;
        let endpoint_copied = self.active_thread_id == self.copied_endpoint_id;
        let copy_endpoint_button = Button::new("copy-endpoint-id")
            .icon(Icon::new(if endpoint_copied {
                AssetIconName::Check
            } else {
                AssetIconName::Link
            }))
            .custom(
                ButtonCustomVariant::new(cx)
                    .hover(rgb(0x2d2d30).into())
                    .active(rgb(0x3f3f46).into()),
            )
            .small()
            .size(px(28.))
            .mr_1()
            .debug_selector(|| "copy-endpoint-id".to_owned())
            .accessibility_label(if endpoint_copied {
                "Endpoint link copied"
            } else {
                "Copy endpoint link"
            })
            .tooltip(if endpoint_copied {
                "Copied"
            } else {
                "Copy endpoint link"
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.titlebar_click_armed = false;
                    cx.stop_propagation();
                }),
            )
            .when(!endpoint_copied, |this| {
                this.on_click(cx.listener(|this, _, _, cx| {
                    this.copy_endpoint_id(cx);
                }))
            });
        let copy_endpoint_button = copy_endpoint_button
            .with_animation(
                if endpoint_copied {
                    "endpoint-copy-copied"
                } else {
                    "endpoint-copy-ready"
                },
                Animation::new(Duration::from_millis(220)).with_easing(gpui::ease_out_quint()),
                |button, delta| button.opacity(0.45 + 0.55 * delta),
            )
            .into_any_element();

        div()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x1c1c1f))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    GlobalState::suppress_text_selection(cx);

                    if cfg!(target_os = "macos") {
                        cx.stop_propagation();
                        let is_titlebar_double_click =
                            event.click_count == 2 && this.titlebar_click_armed;
                        this.titlebar_click_armed = event.click_count == 1;

                        if is_titlebar_double_click {
                            window.titlebar_double_click();
                        } else {
                            window.start_window_move();
                        }
                    }
                }),
            )
            .child(self.render_sidebar_toggle(cx))
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .children(
                        active_thread
                            .as_ref()
                            .and_then(|thread| Self::render_participants(thread.read(cx))),
                    )
                    .when(sharing_status == SharingStatus::Shared, |this| {
                        this.child(copy_endpoint_button)
                    })
                    .child(
                        div()
                            .id("toggle-sharing")
                            .h(px(28.))
                            .px_3()
                            .mr_2()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .occlude()
                            .text_sm()
                            .text_color(rgb(0x71717a))
                            .when(sharing_enabled, |this| {
                                this.cursor_pointer()
                                    .text_color(rgb(0xd4d4d8))
                                    .hover(|this| this.bg(rgb(0x2d2d30)))
                            })
                            .when(sharing_status == SharingStatus::Failed, |this| {
                                this.text_color(rgb(0xf87171))
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.titlebar_click_armed = false;
                                    cx.stop_propagation();
                                }),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_sharing(window, cx);
                            }))
                            .child(share_label),
                    )
                    .when(!cfg!(target_os = "macos"), |this| {
                        this.child(
                            div()
                                .h_full()
                                .flex()
                                .font_family("Segoe Fluent Icons")
                                .child(Self::render_caption_button(
                                    "minimize-window",
                                    "\u{e921}",
                                    WindowControlArea::Min,
                                    false,
                                ))
                                .child(Self::render_caption_button(
                                    "maximize-window",
                                    if window.is_maximized() {
                                        "\u{e923}"
                                    } else {
                                        "\u{e922}"
                                    },
                                    WindowControlArea::Max,
                                    false,
                                ))
                                .child(Self::render_caption_button(
                                    "close-window",
                                    "\u{e8bb}",
                                    WindowControlArea::Close,
                                    true,
                                )),
                        )
                    }),
            )
    }

    /// The connected participants of a shared thread as overlapping avatars,
    /// in join order, each naming its participant on hover.
    fn render_participants(thread: &Thread) -> Option<gpui::AnyElement> {
        const MAX_VISIBLE: usize = 5;
        const AVATAR_SIZE: f32 = 24.;
        const AVATAR_OVERLAP: f32 = 6.;

        if thread.participants.is_empty() {
            return None;
        }
        let visible = thread.participants.len().min(MAX_VISIBLE);
        let hidden = thread.participants.len() - visible;
        let avatars = thread
            .participants
            .iter()
            .take(MAX_VISIBLE)
            .enumerate()
            .map(|(index, &participant)| {
                let name = if participant == thread.participant_id {
                    format!("{} (you)", participant.display_name())
                } else {
                    participant.display_name()
                };
                Self::render_participant_avatar(participant, px(AVATAR_SIZE))
                    // Separates overlapping avatars from each other.
                    .border_2()
                    .border_color(rgb(0x1c1c1f))
                    .id(("participant", index))
                    .when(index > 0, |this| this.ml(px(-AVATAR_OVERLAP)))
                    .tooltip(move |window, cx| Tooltip::new(name.clone()).build(window, cx))
            });
        // Flex layout measures overlapping (negatively margined) children as
        // taking no room at all, so the row is sized explicitly.
        let avatars_width =
            AVATAR_SIZE + (AVATAR_SIZE - AVATAR_OVERLAP) * (visible.saturating_sub(1) as f32);

        Some(
            div()
                .id("participants")
                .debug_selector(|| "participants".to_owned())
                .mr_2()
                .flex_none()
                .flex()
                .items_center()
                .occlude()
                .child(
                    div()
                        .w(px(avatars_width))
                        .flex_none()
                        .flex()
                        .items_center()
                        .children(avatars),
                )
                .when(hidden > 0, |this| {
                    this.child(
                        div()
                            .ml_1()
                            .text_xs()
                            .text_color(rgb(0xa1a1aa))
                            .child(format!("+{hidden}")),
                    )
                })
                .into_any_element(),
        )
    }

    /// A participant's avatar, identical wherever they appear: their color
    /// behind their initials.
    fn render_participant_avatar(participant: ParticipantId, size: gpui::Pixels) -> gpui::Div {
        div()
            .size(size)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(participant.color()))
            .text_size(px(9.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(0xf4f4f5))
            .child(participant.initials())
    }

    fn open_thread(&mut self, thread_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_store.read(cx).thread(thread_id, cx).is_none() {
            return;
        }

        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let can_write = thread.read(cx).ownership.can_write();
        self.active_thread_id = Some(thread_id);
        self.selection_message_id = None;
        self.follow_generation = true;
        self.timeline_scroll_handle.scroll_to_bottom();
        if can_write {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    fn sidebar_thread_item(
        &self,
        thread_id: Uuid,
        thread: &ThreadSummary,
        cx: &mut Context<Self>,
    ) -> SidebarMenuItem {
        SidebarMenuItem::new(thread.title.clone())
            .min_h(px(30.))
            .active(self.active_thread_id == Some(thread_id))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_thread(thread_id, window, cx);
            }))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (collaborating_threads, recent_threads): (Vec<_>, Vec<_>) = self
            .thread_store
            .read(cx)
            .threads
            .iter()
            .map(|thread| {
                let thread = thread.read(cx);
                (
                    thread.instance_id,
                    thread.summary.clone(),
                    thread.sharing.is_collaborating(),
                )
            })
            .partition(|(_, _, collaborating)| *collaborating);

        let actions = CoworkSidebarSection::new(
            None::<SharedString>,
            SidebarMenu::new()
                .child(
                    SidebarMenuItem::new("New chat")
                        .min_h(px(34.))
                        .icon(
                            Icon::new(AssetIconName::SquarePen)
                                .size_4()
                                .text_color(rgb(0xe4e4e7)),
                        )
                        .active(self.active_thread_id.is_none())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_thread_draft = ThreadDraft::new(this.local_participant_id);
                            this.active_thread_id = None;
                            this.selection_message_id = None;
                            this.focus_composer(window, cx);
                            cx.notify();
                        })),
                )
                .child(
                    SidebarMenuItem::new("Join shared thread")
                        .min_h(px(34.))
                        .icon(
                            Icon::new(AssetIconName::UsersRound)
                                .size_4()
                                .text_color(rgb(0xe4e4e7)),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_join_dialog(window, cx);
                        })),
                ),
        );

        let collaborating =
            CoworkSidebarSection::new(
                Some("Collaborating"),
                SidebarMenu::new().children(collaborating_threads.iter().map(
                    |(thread_id, thread, _)| self.sidebar_thread_item(*thread_id, thread, cx),
                )),
            );

        let recents =
            CoworkSidebarSection::new(
                Some("Recents"),
                SidebarMenu::new().children(recent_threads.iter().map(|(thread_id, thread, _)| {
                    self.sidebar_thread_item(*thread_id, thread, cx)
                })),
            )
            .label_toggle(
                self.recents_open,
                cx.listener(|this, _, _, cx| {
                    this.recents_open = !this.recents_open;
                    cx.notify();
                }),
            );

        let sidebar = Sidebar::new("cowork-sidebar")
            .w(SIDEBAR_WIDTH)
            .bg(rgb(0x1c1c1f))
            .border_r_0()
            .collapsible(SidebarCollapsible::Offcanvas)
            .collapsed(!self.sidebar_open)
            .header(
                div()
                    .h(px(42.))
                    .flex()
                    .items_center()
                    .px_2()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Cowork"),
            )
            .footer(self.render_sidebar_bottom_bar())
            .child(actions);
        let sidebar = if collaborating_threads.is_empty() {
            sidebar
        } else {
            sidebar.child(collaborating)
        };

        sidebar.child(recents)
    }

    /// Mirrors the main stage's bottom bar, but with its divider always shown.
    fn render_sidebar_bottom_bar(&self) -> impl IntoElement {
        // `Sidebar` pads its footer slot by `px_3` and `pb_3`; bleeding over
        // that padding lets the bar span the sidebar's full width and line up
        // with the main stage's bottom bar.
        const FOOTER_INSET: gpui::Rems = rems(-0.75);
        let identity = self.local_participant_id;

        div()
            .id("sidebar-bottom-bar")
            .debug_selector(|| "sidebar-bottom-bar".to_owned())
            .relative()
            .flex_1()
            .mx(FOOTER_INSET)
            .mb(FOOTER_INSET)
            .h(TOP_BAR_HEIGHT)
            .flex()
            .items_center()
            .px_1()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(1.))
                    .bg(rgb(0x2d2d30)),
            )
            .child(
                Button::new("identity")
                    .ghost()
                    .debug_selector(|| "identity-button".to_owned())
                    .flex_1()
                    .px_1p5()
                    // `Button` centers its content, so fill it with a single
                    // left-aligned row.
                    .child(
                        div()
                            .debug_selector(|| "identity-button-content".to_owned())
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Self::render_participant_avatar(identity, px(22.)))
                            .child(
                                div()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .child(identity.display_name()),
                            ),
                    ),
            )
    }

    fn markdown_text_leaves(markdown: &str) -> Vec<MarkdownTextLeaf> {
        fn collect(node: &markdown::mdast::Node, source: &str, leaves: &mut Vec<MarkdownTextLeaf>) {
            let atomic = matches!(node, markdown::mdast::Node::InlineCode(_));
            if (matches!(node, markdown::mdast::Node::Text(_)) || atomic)
                && let Some(position) = node.position()
                && position.start.offset < position.end.offset
                && source
                    .get(position.start.offset..position.end.offset)
                    .is_some()
            {
                let mut source_start = position.start.offset;
                if !atomic
                    && let Some(previous) = source_start.checked_sub(1)
                    && source.as_bytes().get(previous) == Some(&b'\\')
                {
                    source_start = previous;
                }
                let source_range = source_start..position.end.offset;
                leaves.push(MarkdownTextLeaf {
                    source_range: source_range.clone(),
                    annotation_range: position.start.offset..position.end.offset,
                    atomic,
                });
                return;
            }
            if let Some(children) = node.children() {
                for child in children {
                    collect(child, source, leaves);
                }
            }
        }

        let Ok(tree) = markdown::to_mdast(markdown, &markdown::ParseOptions::gfm()) else {
            return Vec::new();
        };
        let mut leaves = Vec::new();
        collect(&tree, markdown, &mut leaves);
        leaves
    }

    fn selected_message_source_range(
        &self,
        thread_message_id: ThreadMessageId,
        text_view: &Entity<TextViewState>,
        cx: &App,
    ) -> Option<Range<usize>> {
        let mut selected_ranges = self
            .segment_text_views
            .iter()
            .filter(|((segment_id, _), _)| *segment_id == thread_message_id)
            .filter_map(|((_, source_range), text_view)| {
                text_view
                    .state
                    .read(cx)
                    .selected_source_range()
                    .map(|range| {
                        let range = text_view
                            .source_offsets
                            .as_ref()
                            .and_then(|offsets| {
                                Some(*offsets.get(range.start)?..*offsets.get(range.end)?)
                            })
                            .unwrap_or(range);
                        (range.start + source_range.start)..(range.end + source_range.start)
                    })
            });
        let first = selected_ranges.next();
        let segmented = selected_ranges.fold(first, |combined, range| {
            Some(match combined {
                Some(combined) => combined.start.min(range.start)..combined.end.max(range.end),
                None => range,
            })
        });

        segmented.or_else(|| text_view.read(cx).selected_source_range())
    }

    fn annotation_ranges(markdown: &str, selection: Range<usize>) -> Vec<Range<usize>> {
        let mut ranges = Vec::<Range<usize>>::new();
        for leaf in Self::markdown_text_leaves(markdown) {
            let start = leaf.source_range.start.max(selection.start);
            let end = leaf.source_range.end.min(selection.end);
            if start >= end {
                continue;
            }
            let range = if leaf.atomic {
                leaf.annotation_range
            } else {
                start..end
            };
            if let Some(previous) = ranges.last_mut()
                && previous.end == range.start
            {
                previous.end = range.end;
            } else {
                ranges.push(range);
            }
        }
        ranges
    }

    fn annotate_markdown_with_source_offsets(
        markdown: &str,
        ranges: impl IntoIterator<Item = Range<usize>>,
    ) -> (String, Vec<usize>) {
        let mut ranges = ranges
            .into_iter()
            .filter(|range| {
                range.start < range.end
                    && range.end <= markdown.len()
                    && markdown.is_char_boundary(range.start)
                    && markdown.is_char_boundary(range.end)
            })
            .collect::<Vec<_>>();
        ranges.sort_by_key(|range| range.start);

        let mut merged_ranges = Vec::<Range<usize>>::new();
        for range in ranges {
            if let Some(previous) = merged_ranges.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
            } else {
                merged_ranges.push(range);
            }
        }

        let mut annotated = String::new();
        let mut source_offsets = vec![0];
        let mut cursor = 0;
        for range in merged_ranges {
            annotated.push_str(&markdown[cursor..range.start]);
            source_offsets.extend((cursor + 1)..=range.start);

            annotated.push('[');
            source_offsets.push(range.start);

            annotated.push_str(&markdown[range.clone()]);
            source_offsets.extend((range.start + 1)..=range.end);

            const LINK_SUFFIX: &str = "](#inline-comment)";
            annotated.push_str(LINK_SUFFIX);
            source_offsets.extend(std::iter::repeat_n(range.end, LINK_SUFFIX.len()));
            cursor = range.end;
        }
        annotated.push_str(&markdown[cursor..]);
        source_offsets.extend((cursor + 1)..=markdown.len());

        debug_assert_eq!(source_offsets.len(), annotated.len() + 1);
        (annotated, source_offsets)
    }

    fn begin_inline_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| !thread.read(cx).ownership.can_write())
        {
            return;
        }

        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.function
        {
            return;
        }
        let Some(initial_text) = event.keystroke.key_char.as_deref() else {
            return;
        };
        if initial_text.chars().all(char::is_control) {
            return;
        }
        let quote = TextSelection::selected_text(window, cx).trim().to_string();
        let (Some(thread_id), Some(preferred_message_id)) =
            (self.active_thread_id, self.selection_message_id)
        else {
            return;
        };
        if quote.is_empty() {
            return;
        }
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let (message_id, source_range) = {
            let thread = thread.read(cx);
            let mut targets = thread
                .timeline
                .iter()
                .filter_map(|entry| match entry {
                    TimelineMessage::Agent(message) => Some(
                        std::iter::once((message.id, message.text_view.clone())).chain(
                            message
                                .comment_responses
                                .iter()
                                .map(|response| (response.id, response.response_view.clone())),
                        ),
                    ),
                    TimelineMessage::User(_) => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            targets.sort_by_key(|(message_id, _)| *message_id != preferred_message_id);
            let Some(target) = targets.into_iter().find_map(|(message_id, text_view)| {
                let thread_message_id = ThreadMessageId {
                    thread_id,
                    message_id,
                };
                self.selected_message_source_range(thread_message_id, &text_view, cx)
                    .map(|source_range| (message_id, source_range))
            }) else {
                return;
            };
            target
        };

        let (draft_id, comment_id) = thread.update(cx, |thread, _| {
            let target = CommentTarget {
                message_id,
                range: source_range,
                quote,
            };
            let draft = &mut thread.draft;
            let comment_id = draft
                .doc
                .create_comment(draft.author.as_uuid(), target, initial_text);
            draft.comments_folded = false;
            let draft_id = draft.id;
            thread.flush_draft();
            (draft_id, comment_id)
        });
        TextSelection::clear(window, cx);
        self.focus_draft_editor(
            draft_id,
            EditorSlot::CommentInline(comment_id),
            None,
            window,
            cx,
        );
        window.prevent_default();
        cx.stop_propagation();
    }

    fn toggle_comment_group(&mut self, group_id: Uuid, cx: &mut Context<Self>) {
        if self.new_thread_draft.id == group_id {
            self.new_thread_draft.comments_folded = !self.new_thread_draft.comments_folded;
            cx.notify();
            return;
        }

        let threads = self.thread_store.read(cx).threads.clone();
        for thread in threads {
            let toggled = thread.update(cx, |thread, _| {
                if thread.draft.id == group_id {
                    thread.draft.comments_folded = !thread.draft.comments_folded;
                    return true;
                }
                for entry in &mut thread.timeline {
                    if let TimelineMessage::User(group) = entry
                        && group.id == group_id
                    {
                        group.comments_folded = !group.comments_folded;
                        return true;
                    }
                }
                false
            });
            if toggled {
                break;
            }
        }
        cx.notify();
    }

    fn render_comment_group_toggle(
        group_id: Uuid,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = format!("{} comment{}", count, if count == 1 { "" } else { "s" });
        div()
            .id(format!("toggle-comments-{group_id}"))
            .h(px(24.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_color(rgb(0xa1a1aa))
            .hover(|this| this.text_color(rgb(0xe4e4e7)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_comment_group(group_id, cx);
            }))
            .child(label)
            .child(if collapsed { "›" } else { "⌄" })
    }

    fn render_inline_comment(comment: &UserComment) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing { inline, .. } => div()
                .id(format!("comment-editor-inline-{}", comment.id))
                .relative()
                .flex_1()
                .min_w_0()
                .child(Textarea::new(inline))
                .child(Self::render_remote_carets(
                    inline,
                    comment.presence.carets.clone(),
                ))
                .into_any_element(),
        };

        div()
            .id(format!("inline-comment-{}", comment.id))
            .w_full()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x303036))
            .bg(rgb(0x1d1d20))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_l_2()
                    .border_color(rgb(comment.author.color()))
                    .child(Self::render_layered_avatars(
                        comment.author,
                        &comment.presence.editors,
                    ))
                    .child(body),
            )
            .into_any_element()
    }

    fn render_composer_comment(comment: &UserComment) -> gpui::AnyElement {
        let body = match &comment.body {
            UserCommentBody::Submitted(body) => div()
                .w_full()
                .text_color(rgb(0xe4e4e7))
                .child(body.clone())
                .into_any_element(),
            UserCommentBody::Editing { composer, .. } => div()
                .id(format!("comment-editor-composer-{}", comment.id))
                .relative()
                .flex_1()
                .min_w_0()
                .child(Textarea::new(composer))
                .child(Self::render_remote_carets(
                    composer,
                    comment.presence.carets.clone(),
                ))
                .into_any_element(),
        };

        div()
            .id(format!("composer-comment-{}", comment.id))
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(rgb(0x303036))
            .bg(rgb(0x202023))
            .child(
                div()
                    .w_full()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .text_color(rgb(0xd4d4d8))
                    .line_clamp(2)
                    .child(comment.reference.quote.clone()),
            )
            .child(
                div().w_full().px_3().pb_3().child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .border_1()
                        .border_color(rgb(0x303036))
                        .rounded_md()
                        .bg(rgb(0x1d1d20))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .border_l_2()
                                .border_color(rgb(comment.author.color()))
                                .child(Self::render_layered_avatars(
                                    comment.author,
                                    &comment.presence.editors,
                                ))
                                .child(body),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_avatar(author: MessageAuthor) -> gpui::Div {
        const SIZE: gpui::Pixels = px(22.);

        match author {
            MessageAuthor::User(participant) => Self::render_participant_avatar(participant, SIZE),
            MessageAuthor::Agent => div()
                .size(SIZE)
                .flex_none()
                .overflow_hidden()
                .rounded_full()
                .child(img(OLLAMA_AVATAR_PATH).size_full()),
        }
    }

    fn markdown_style() -> TextViewStyle {
        let code_background = rgb(0x27272a);

        TextViewStyle::default()
            .with_foreground(rgb(0xd4d4d8).into())
            .with_muted_foreground(rgb(0x8b8b95).into())
            .with_link(rgb(0x60a5fa).into())
            .with_selection(rgba(0xe26d5a40).into())
            .with_code_background(code_background.into())
            .with_border(rgb(0x3f3f46).into())
            .with_paragraph_gap(rems(0.75))
            .with_heading_base_font_size(px(14.))
            .with_code_block(
                gpui::StyleRefinement::default()
                    .bg(code_background)
                    .text_color(rgb(0xd4d4d8)),
            )
            .with_inline_code(HighlightStyle {
                color: Some(rgb(0xe4e4e7).into()),
                background_color: Some(code_background.into()),
                ..Default::default()
            })
            .with_table(
                gpui::StyleRefinement::default()
                    .bg(rgb(0x18181b))
                    .text_color(rgb(0xd4d4d8)),
            )
            .with_table_head(
                gpui::StyleRefinement::default()
                    .bg(code_background)
                    .text_color(rgb(0xe4e4e7)),
            )
            .with_table_cell(gpui::StyleRefinement::default().text_color(rgb(0xd4d4d8)))
            .with_dark(true)
    }

    fn annotated_markdown_style() -> TextViewStyle {
        Self::markdown_style().with_link(rgb(USER_ACCENT).into())
    }

    fn render_message_segment(
        &mut self,
        thread_message_id: ThreadMessageId,
        _segment_index: usize,
        source_range: Range<usize>,
        text: &str,
        annotated: bool,
        source_offsets: Option<Vec<usize>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let text_view = self
            .segment_text_views
            .entry((thread_message_id, source_range))
            .or_insert_with(|| SegmentTextView {
                state: cx.new(|cx| TextViewState::markdown(text, cx)),
                text: text.to_owned(),
                source_offsets: source_offsets.clone(),
                rendered_at: self.render_generation,
            });
        text_view.rendered_at = self.render_generation;
        text_view.source_offsets = source_offsets;
        if text_view.text != text {
            text_view.text.clear();
            text_view.text.push_str(text);
            if annotated {
                // Reparse the whole annotated segment. Incrementally replacing
                // Markdown can retain stale link-render caches and drop an
                // existing highlight when a neighboring annotation is added.
                text_view.state = cx.new(|cx| TextViewState::markdown(text, cx));
            } else {
                text_view
                    .state
                    .update(cx, |view, cx| view.set_text(text, cx));
            }
        }
        TextView::new(&text_view.state)
            .selection_format(SelectionFormat::Plain)
            .style(if annotated {
                Self::annotated_markdown_style()
            } else {
                Self::markdown_style()
            })
            .w_full()
            .into_any_element()
    }

    fn hard_line_start(text: &str, offset: usize) -> usize {
        text[..offset].rfind('\n').map_or(0, |offset| offset + 1)
    }

    fn wrapped_line_end(
        text: &str,
        selection_end: usize,
        wrap_width: gpui::Pixels,
        window: &mut Window,
    ) -> usize {
        let hard_line_start = Self::hard_line_start(text, selection_end);
        let hard_line_end = text[selection_end..]
            .find('\n')
            .map_or(text.len(), |offset| selection_end + offset);
        let hard_line = &text[hard_line_start..hard_line_end];
        let selected_end_in_line = selection_end - hard_line_start;
        let font_size = px(14.);
        let mut wrapper = window
            .text_system()
            .line_wrapper(window.text_style().font(), font_size);
        let fragments = [LineFragment::text(hard_line)];
        let end = wrapper
            .wrap_line(&fragments, wrap_width)
            .map(|boundary| boundary.ix)
            .find(|boundary| *boundary >= selected_end_in_line)
            .unwrap_or(hard_line.len());
        let source_end = hard_line_start + end;
        if source_end == hard_line_end && hard_line_end < text.len() {
            hard_line_end + 1
        } else {
            source_end
        }
    }

    /// A submitted user message; `cards` holds each block's attachments.
    fn render_user_message_group(
        &self,
        index: usize,
        group: &UserMessageGroup,
        cards: Vec<Vec<AttachmentCard>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut rows = Vec::new();
        if !group.comments.is_empty() {
            let mut content = vec![
                Self::render_comment_group_toggle(
                    group.id,
                    group.comments.len(),
                    group.comments_folded,
                    cx,
                )
                .into_any_element(),
            ];
            if !group.comments_folded {
                content.extend(group.comments.iter().map(Self::render_composer_comment));
            }
            rows.push(Self::render_comment_group_row(&group.comments, content));
        }
        for (block_index, (block, cards)) in group.blocks.iter().zip(cards).enumerate() {
            let mut content = Vec::new();
            if !cards.is_empty() {
                content.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(
                            cards
                                .iter()
                                .map(|card| self.render_attachment(card, None, cx)),
                        )
                        .into_any_element(),
                );
            }
            if !block.text.trim().is_empty() {
                content.push(
                    SelectableText::new(
                        format!("timeline-user-text-{}", block.id),
                        block.text.clone(),
                    )
                    .document_order((index * 1_000 + block_index) as u64)
                    .into_any_element(),
                );
            }
            rows.push(Self::render_message_row(
                Some(MessageAuthor::User(block.author)),
                content,
            ));
        }

        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.selection_message_id = None;
                }),
            )
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .children(rows)
            .into_any_element()
    }

    /// The row of a group of comments. Its gutter shows who wrote them, so
    /// that a folded group is still recognizably someone's message.
    fn render_comment_group_row(
        comments: &[UserComment],
        content: impl IntoIterator<Item = gpui::AnyElement>,
    ) -> gpui::Div {
        let authors = Self::comment_authors(comments);
        match authors.split_first() {
            Some((first, rest)) => Self::render_presence_row(*first, rest, content),
            None => Self::render_message_row(None, content),
        }
    }

    /// The distinct authors of `comments`, in the order they first commented.
    fn comment_authors(comments: &[UserComment]) -> Vec<ParticipantId> {
        comments.iter().fold(Vec::new(), |mut authors, comment| {
            if !authors.contains(&comment.author) {
                authors.push(comment.author);
            }
            authors
        })
    }

    /// A composer row whose gutter shows `primary` with everyone else in the
    /// row layered below it.
    fn render_presence_row(
        primary: ParticipantId,
        others: &[ParticipantId],
        content: impl IntoIterator<Item = gpui::AnyElement>,
    ) -> gpui::Div {
        Self::render_message_row(None, content)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(40.))
                    .flex()
                    .justify_center()
                    .child(Self::render_layered_avatars(primary, others)),
            )
            .relative()
    }

    /// A participant's avatar with smaller avatars of `others` overlapping
    /// its lower edge. Positioned absolutely so that people coming and going
    /// never move the row's text.
    fn render_layered_avatars(primary: ParticipantId, others: &[ParticipantId]) -> gpui::Div {
        const MAX_OTHERS: usize = 2;
        const SMALL: gpui::Pixels = px(14.);
        const STEP: f32 = 10.;

        let hidden = others.len().saturating_sub(MAX_OTHERS);
        let mut layered = others
            .iter()
            .take(MAX_OTHERS)
            .map(|&participant| {
                Self::render_participant_avatar(participant, SMALL)
                    .text_size(px(6.))
                    .border_1()
                    .border_color(rgb(0x18181b))
            })
            .collect::<Vec<_>>();
        if hidden > 0 {
            layered.push(
                div()
                    .size(SMALL)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(0x18181b))
                    .bg(rgb(0x3f3f46))
                    .text_size(px(7.))
                    .text_color(rgb(0xf4f4f5))
                    .child(format!("+{hidden}")),
            );
        }
        let count = layered.len();
        div()
            .relative()
            .child(Self::render_avatar(MessageAuthor::User(primary)))
            .children(layered.into_iter().enumerate().map(|(index, avatar)| {
                // Centered under the primary avatar, fanned out sideways.
                let offset = (index as f32 - (count as f32 - 1.) / 2.) * STEP;
                avatar.absolute().top(px(14.)).left(px(11. - 7. + offset))
            }))
    }

    /// Paints other participants' carets and selections over `editor`.
    fn render_remote_carets(
        editor: &Entity<TextareaState>,
        carets: Vec<RemoteCaret>,
    ) -> impl IntoElement {
        let editor = editor.clone();
        canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                // Laid out first: the editor cannot stay borrowed while
                // painting.
                let layout = {
                    let editor = editor.read(cx);
                    let text = editor.value();
                    let Some(text_bounds) = editor.text_bounds() else {
                        return;
                    };
                    carets
                        .iter()
                        .map(|caret| {
                            let clamp = |offset: usize| text.floor_char_boundary(offset);
                            let selection =
                                clamp(caret.selection.start)..clamp(caret.selection.end);
                            let head = clamp(caret.head);
                            (
                                caret,
                                selection_rects(editor, &text, selection, text_bounds),
                                editor.range_to_bounds(&(head..head)),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                for (caret, selection, head) in layout {
                    let color = caret.participant.color();
                    for rect in selection {
                        window.paint_quad(gpui::fill(rect, rgba((color << 8) | 0x40)));
                    }
                    let Some(head) = head else {
                        continue;
                    };
                    window.paint_quad(gpui::fill(
                        Bounds::new(head.origin, size(px(2.), head.size.height)),
                        rgb(color),
                    ));
                    if caret.moved_at.elapsed() < CARET_LABEL_DURATION {
                        paint_caret_label(caret.participant, head.origin, window, cx);
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// One row of the timeline or composer: an avatar gutter, the content, and
    /// a matching gap on the right.
    fn render_message_row(
        author: Option<MessageAuthor>,
        content: impl IntoIterator<Item = gpui::AnyElement>,
    ) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .children(author.map(Self::render_avatar)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(content),
            )
            .child(div().w(px(40.)).flex_none())
    }

    fn toggle_thinking(&mut self, thread_id: Uuid, message_id: Uuid, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        thread.update(cx, |thread, _| {
            for entry in &mut thread.timeline {
                if let TimelineMessage::Agent(message) = entry
                    && message.id == message_id
                    && message.thinking_complete
                    && !message.thinking.is_empty()
                {
                    message.thinking_expanded = !message.thinking_expanded;
                    break;
                }
            }
        });
        cx.notify();
    }

    fn render_agent_text(
        &mut self,
        thread_message_id: ThreadMessageId,
        text: &str,
        text_view: &Entity<TextViewState>,
        comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut content = Vec::new();
        let mut cursor = 0;
        let mut anchored_comments = comments
            .iter()
            .filter(|comment| comment.reference.message_id == thread_message_id.message_id)
            .filter(|comment| {
                comment.reference.range.start < comment.reference.range.end
                    && comment.reference.range.end <= text.len()
                    && text.is_char_boundary(comment.reference.range.start)
                    && text.is_char_boundary(comment.reference.range.end)
            })
            .collect::<Vec<_>>();
        anchored_comments.sort_by_key(|comment| comment.reference.range.start);

        if anchored_comments.is_empty() && !text.is_empty() {
            content.push(
                TextView::new(text_view)
                    .selection_format(SelectionFormat::Plain)
                    .style(Self::markdown_style())
                    .w_full()
                    .into_any_element(),
            );
            return content;
        }

        let mut comment_index = 0;
        while comment_index < anchored_comments.len() {
            let first = anchored_comments[comment_index];
            if first.reference.range.start < cursor {
                comment_index += 1;
                continue;
            }
            let annotated_start =
                Self::hard_line_start(text, first.reference.range.start).max(cursor);
            if cursor < annotated_start {
                content.push(self.render_message_segment(
                    thread_message_id,
                    content.len(),
                    cursor..annotated_start,
                    &text[cursor..annotated_start],
                    false,
                    None,
                    cx,
                ));
                cursor = annotated_start;
            }
            let line_end =
                Self::wrapped_line_end(text, first.reference.range.end, wrap_width, window);
            let group_start = comment_index;
            while comment_index < anchored_comments.len()
                && anchored_comments[comment_index].reference.range.start < line_end
            {
                comment_index += 1;
            }
            let group = &anchored_comments[group_start..comment_index];
            let annotation_ranges = group
                .iter()
                .flat_map(|comment| Self::annotation_ranges(text, comment.reference.range.clone()))
                .filter_map(|range| {
                    (range.start >= cursor && range.end <= line_end)
                        .then_some((range.start - cursor)..(range.end - cursor))
                });
            let (annotated, source_offsets) = Self::annotate_markdown_with_source_offsets(
                &text[cursor..line_end],
                annotation_ranges,
            );
            content.push(self.render_message_segment(
                thread_message_id,
                content.len(),
                cursor..line_end,
                &annotated,
                true,
                Some(source_offsets),
                cx,
            ));
            content.extend(
                group
                    .iter()
                    .map(|comment| Self::render_inline_comment(comment)),
            );
            cursor = line_end;
        }
        if cursor < text.len() {
            content.push(self.render_message_segment(
                thread_message_id,
                content.len(),
                cursor..text.len(),
                &text[cursor..],
                false,
                None,
                cx,
            ));
        }
        content
    }

    fn render_agent_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &AgentMessage,
        comments: &[UserComment],
        submitted_comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let waiting = !message.complete && message.thinking.is_empty() && message.text.is_empty();
        let mut submitted_comment_content = Vec::new();
        for comment in submitted_comments {
            submitted_comment_content.push(Self::render_composer_comment(comment));
            if let Some(response) = message
                .comment_responses
                .iter()
                .find(|response| response.comment_id == comment.id)
            {
                let response_id = response.id;
                let response_content = self.render_agent_text(
                    ThreadMessageId {
                        thread_id,
                        message_id: response.id,
                    },
                    &response.response,
                    &response.response_view,
                    comments,
                    wrap_width,
                    window,
                    cx,
                );
                submitted_comment_content.push(
                    div()
                        .id(format!("comment-response-{response_id}"))
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x242428))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, _| {
                                this.selection_message_id = Some(response_id);
                            }),
                        )
                        .children(response_content)
                        .into_any_element(),
                );
            }
        }

        let message_content = self.render_agent_text(
            ThreadMessageId {
                thread_id,
                message_id: message.id,
            },
            &message.text,
            &message.text_view,
            comments,
            wrap_width,
            window,
            cx,
        );

        let message_id = message.id;
        let thinking_expanded = !message.thinking_complete || message.thinking_expanded;
        let thinking_content = (!message.thinking.is_empty() && thinking_expanded).then(|| {
            TextView::new(&message.thinking_view)
                .selection_format(SelectionFormat::Plain)
                .style(Self::markdown_style())
                .w_full()
                .into_any_element()
        });
        let thinking = (!message.thinking.is_empty()).then(|| {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .id(format!("toggle-thinking-{message_id}"))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(rgb(0x71717a))
                        .when(message.thinking_complete, |this| {
                            this.hover(|this| this.text_color(rgb(0xa1a1aa)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_thinking(thread_id, message_id, cx);
                                }))
                        })
                        .child(if message.thinking_complete {
                            "Thinking"
                        } else {
                            "Thinking…"
                        }),
                )
                .children(thinking_content.map(|content| {
                    div()
                        .pl_3()
                        .border_l_1()
                        .border_color(rgb(0x3f3f46))
                        .opacity(0.7)
                        .child(content)
                }))
        });
        div()
            .id(("timeline-message", index))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.selection_message_id = Some(message_id);
                }),
            )
            .w_full()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(Self::render_avatar(MessageAuthor::Agent)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(message.failed, |this| this.text_color(rgb(0xf87171)))
                    .children(submitted_comment_content)
                    .when(waiting, |this| {
                        this.child(
                            ShimmerText::new("Thinking…")
                                .id(("agent-waiting", index))
                                .text_color(rgb(0x8b8b95)),
                        )
                    })
                    .children(thinking)
                    .children(message_content),
            )
            .child(div().w(px(40.)).flex_none())
            .into_any_element()
    }

    fn render_timeline_message(
        &mut self,
        thread_id: Uuid,
        index: usize,
        message: &TimelineMessage,
        comments: &[UserComment],
        submitted_comments: &[UserComment],
        wrap_width: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match message {
            TimelineMessage::User(group) => {
                let cards = self
                    .thread_store
                    .read(cx)
                    .thread(thread_id, cx)
                    .map(|thread| {
                        let draft = &thread.read(cx).draft;
                        group
                            .blocks
                            .iter()
                            .map(|block| {
                                block
                                    .attachments
                                    .iter()
                                    .map(|record| AttachmentCard::new(draft, record))
                                    .collect()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.render_user_message_group(index, group, cards, cx)
            }
            TimelineMessage::Agent(message) => self.render_agent_message(
                thread_id,
                index,
                message,
                comments,
                submitted_comments,
                wrap_width,
                window,
                cx,
            ),
        }
    }

    fn rig_history(
        messages: &[TimelineMessage],
        files: &HashMap<AttachmentId, FileAttachment>,
    ) -> Vec<RigMessage> {
        messages
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(agent_message(
                    group.history_preface.as_deref(),
                    &group.blocks,
                    files,
                )),
                TimelineMessage::Agent(message) if message.complete && !message.failed => {
                    let submitted_comments = message
                        .comment_group_id
                        .and_then(|group_id| {
                            messages.iter().find_map(|entry| match entry {
                                TimelineMessage::User(group) if group.id == group_id => {
                                    Some(group.comments.as_slice())
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    let mut history_text = submitted_comments
                        .iter()
                        .enumerate()
                        .filter_map(|(index, comment)| {
                            message
                                .comment_responses
                                .iter()
                                .find(|response| response.comment_id == comment.id)
                                .map(|response| (index, response))
                        })
                        .map(|(index, response)| {
                            format!("Reply to comment {}: {}", index + 1, response.response)
                        })
                        .join("\n\n");
                    if !message.text.is_empty() {
                        if !history_text.is_empty() {
                            history_text.push_str("\n\n");
                        }
                        history_text.push_str(&message.text);
                    }
                    Some(RigMessage::assistant(history_text))
                }
                TimelineMessage::Agent(_) => None,
            })
            .collect()
    }

    fn start_generation(
        &mut self,
        thread_id: Uuid,
        prompt: RigMessage,
        mut history: Vec<RigMessage>,
        comment_group_id: Option<Uuid>,
        comment_ids: Vec<Uuid>,
        turn_comments: Arc<TurnComments>,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_store.read(cx).thread(thread_id, cx) else {
            return;
        };
        let message_id = Uuid::new_v4();
        let selected_model = thread.read(cx).model;
        thread.update(cx, |thread, cx| {
            thread.emit(
                protocol::HostMessage::AgentStarted {
                    id: message_id.into_bytes(),
                    comment_group_id: comment_group_id.map(Uuid::into_bytes),
                },
                cx,
            );
        });
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let tool_comments = turn_comments.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation_task = self.tokio_handle.spawn(async move {
            let client = Ollama::new().bound()?;
            let model = match selected_model.provider {
                ModelProvider::Ollama => client.completion(selected_model.model),
            };
            let mut tools = ToolSet::default();
            tools.add_tool(RespondToComment::new(tool_comments));
            StreamingAgent::new(model, tools)
                .additional_params(json!({
                    "num_ctx": OLLAMA_CONTEXT_TOKENS,
                    "think": "medium"
                }))
                .run(prompt, &mut history, move |event| {
                    _ = sender.send(event);
                })
                .await?;
            Ok::<_, anyhow::Error>(())
        });
        self.active_generations.insert(
            thread_id,
            ActiveGeneration {
                message_id,
                abort_handle: generation_task.abort_handle(),
                cancelled: cancelled.clone(),
            },
        );

        cx.spawn(async move |this, cx| {
            let mut stream_completed = true;
            let mut published_comment_responses = HashSet::new();
            while let Some(item) = receiver.recv().await {
                if let AgentEvent::ToolCall(call) = &item
                    && call.function.name == "respond_to_comment"
                    && let Ok(response) = serde_json::from_value::<RespondToCommentArgs>(
                        call.function.arguments.clone(),
                    )
                    && !response.response.trim().is_empty()
                    && published_comment_responses.insert(response.comment_id.clone())
                    && let Some(comment_id) = turn_comments
                        .comment_ids()
                        .iter()
                        .position(|comment_id| comment_id.as_str() == response.comment_id)
                        .and_then(|index| comment_ids.get(index))
                {
                    thread.update(cx, |thread, cx| {
                        thread.emit(
                            protocol::HostMessage::AgentCommentResponded {
                                id: message_id.into_bytes(),
                                response_id: Uuid::new_v4().into_bytes(),
                                comment_id: comment_id.into_bytes(),
                                response: response.response,
                            },
                            cx,
                        );
                    });
                    if this
                        .update(cx, |this, cx| this.thread_updated(thread_id, cx))
                        .is_err()
                    {
                        stream_completed = false;
                        break;
                    }
                }

                let Some(event) = Self::agent_stream_event(message_id, item) else {
                    continue;
                };
                thread.update(cx, |thread, cx| thread.emit(event, cx));

                if this
                    .update(cx, |this, cx| this.thread_updated(thread_id, cx))
                    .is_err()
                {
                    stream_completed = false;
                    break;
                }
            }

            if stream_completed {
                let error = match generation_task.await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(error) if error.is_cancelled() && cancelled.load(Ordering::Acquire) => None,
                    Err(error) => Some(error.into()),
                };
                thread.update(cx, |thread, cx| {
                    thread.emit(
                        protocol::HostMessage::AgentEnded {
                            id: message_id.into_bytes(),
                            failure: error
                                .map(|error| format!("Unable to generate a response: {error}")),
                        },
                        cx,
                    );
                });
                _ = this.update(cx, |this, cx| {
                    if let Entry::Occupied(entry) = this.active_generations.entry(thread_id) {
                        if entry.get().message_id == message_id {
                            entry.remove();
                        }
                    }
                    this.thread_updated(thread_id, cx);
                });
            }
        })
        .detach();
    }

    /// Translates one item of the agent's stream into the thread event it
    /// represents, or `None` for items that do not change the timeline.
    fn agent_stream_event(message_id: Uuid, item: AgentEvent) -> Option<protocol::HostMessage> {
        let id = message_id.into_bytes();
        match item {
            AgentEvent::Model(StreamEvent::BlockDelta {
                delta: Delta::Reasoning { text },
                ..
            }) => Some(protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Thinking,
                text,
            }),
            AgentEvent::Model(StreamEvent::BlockEnd {
                end: BlockClose::Reasoning { .. },
                ..
            }) => Some(protocol::HostMessage::AgentThinkingEnded { id }),
            AgentEvent::Model(StreamEvent::BlockDelta {
                delta: Delta::Text { text },
                ..
            }) => Some(protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Response,
                text,
            }),
            AgentEvent::Model(_) | AgentEvent::ToolCall(_) | AgentEvent::ToolResult { .. } => None,
        }
    }

    /// Redraws the timeline after `thread_id` changed, staying pinned to the
    /// newest output unless the user has scrolled away.
    fn thread_updated(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        if self.active_thread_id != Some(thread_id) {
            return;
        }
        if self.follow_generation {
            self.timeline_scroll_handle.scroll_to_bottom();
        }
        cx.notify();
    }

    fn thread_title(prompt: &str) -> String {
        const MAX_CHARACTERS: usize = 32;

        let mut title = Itertools::intersperse(prompt.split_whitespace(), " ")
            .flat_map(str::chars)
            .take(MAX_CHARACTERS + 1)
            .collect::<String>();
        if title.chars().count() > MAX_CHARACTERS {
            title.pop();
            title.push('…');
        }
        title
    }

    fn title_for_first_message(timeline: &[TimelineMessage], prompt: &str) -> Option<String> {
        timeline.is_empty().then(|| Self::thread_title(prompt))
    }

    /// The instructions for answering a submission's comments, sent ahead of
    /// its prompt blocks.
    fn comments_preface(
        comments: &[UserComment],
        comment_ids: &[tools::CommentId],
        timeline: &[TimelineMessage],
    ) -> Option<String> {
        if comments.is_empty() {
            return None;
        }

        let mut result = String::from(
            "Participants attached the following inline comments to immutable excerpts from the conversation. You MUST call `respond_to_comment` exactly once for every comment_id before finishing your response. Put the direct reply to that comment in the tool's `response` argument; do not repeat these replies in your final prose.\n",
        );
        for (index, (comment, comment_id)) in comments.iter().zip(comment_ids.iter()).enumerate() {
            let UserCommentBody::Submitted(body) = &comment.body else {
                continue;
            };
            let message_number = timeline
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        TimelineMessage::Agent(message)
                            if message.id == comment.reference.message_id
                                || message.comment_responses.iter().any(|response| {
                                    response.id == comment.reference.message_id
                                })
                    )
                })
                .map(|index| index + 1)
                .unwrap_or_default();
            result.push_str(&format!(
                "\n{}. {} — {}, on an excerpt from assistant message {}:\n> {}\nComment: {}\n",
                index + 1,
                comment_id,
                comment.author.display_name(),
                message_number,
                comment.reference.quote.replace('\n', "\n> "),
                body.trim(),
            ));
        }
        Some(result)
    }

    /// The draft the composer currently edits, or `None` for read-only threads.
    fn writable_draft_id(&self, cx: &App) -> Option<Uuid> {
        match self
            .active_thread_id
            .and_then(|id| self.thread_store.read(cx).thread(id, cx))
        {
            Some(thread) => {
                let thread = thread.read(cx);
                thread.ownership.can_write().then_some(thread.draft.id)
            }
            None => Some(self.new_thread_draft.id),
        }
    }

    /// Finds a writable draft wherever it lives, so work started on one thread
    /// still lands there after the user switches to another. Local changes
    /// made by `update` are sent to the thread's other participants.
    fn update_draft<R>(
        &mut self,
        draft_id: Uuid,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut ThreadDraft) -> R,
    ) -> Option<R> {
        if self.new_thread_draft.id == draft_id {
            let result = update(&mut self.new_thread_draft);
            // Nobody else has this draft yet; whoever joins once it has a
            // thread receives all of it with their welcome.
            self.new_thread_draft.doc.take_local_update();
            return Some(result);
        }
        let thread = self
            .thread_store
            .read(cx)
            .threads
            .iter()
            .find(|thread| {
                let thread = thread.read(cx);
                thread.ownership.can_write() && thread.draft.id == draft_id
            })
            .cloned()?;
        Some(thread.update(cx, |thread, _| {
            let result = update(&mut thread.draft);
            thread.flush_draft();
            result
        }))
    }

    /// Whether files are still on their way: being read by anyone, or not
    /// yet with the host. Either holds submission back.
    fn draft_is_loading_attachments(&self, draft_id: Uuid, cx: &App) -> bool {
        self.pending_attachments
            .iter()
            .any(|pending| pending.draft_id == draft_id)
            || self
                .read_draft(draft_id, cx, |draft| {
                    draft.others_are_reading_files() || draft.has_unstored_attachments()
                })
                .unwrap_or(false)
    }

    /// Where the attach button attaches to: the focused prompt block, or
    /// otherwise a new block. Buttons do not take focus, so the editor the
    /// user was typing in is still focused when one is clicked.
    fn attachment_target_at_focus(&self, window: &Window, cx: &App) -> AttachmentTarget {
        match self.focused_draft_editor(window, cx) {
            Some((_, EditorSlot::Prompt(id), _)) => AttachmentTarget::Block(id),
            _ => AttachmentTarget::NewBlock(Uuid::new_v4()),
        }
    }

    fn add_attachments(
        &mut self,
        draft_id: Uuid,
        target: AttachmentTarget,
        sources: Vec<AttachmentSource>,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);
        let entries = sources
            .into_iter()
            .map(|source| {
                let id = Uuid::new_v4();
                self.pending_attachments.push(PendingAttachment {
                    id,
                    draft_id,
                    target,
                    name: source.name(),
                    is_image: source.looks_like_image(),
                    progress: None,
                });
                (id, source)
            })
            .collect::<Vec<_>>();
        self.publish_presence(cx);
        cx.notify();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        cx.background_executor()
            .spawn(async move {
                for (id, source) in entries {
                    let result = load_attachment(source, |progress| {
                        _ = sender.send(AttachmentReadEvent::Progress(id, progress));
                    });
                    _ = sender.send(AttachmentReadEvent::Finished(id, result));
                }
            })
            .detach();
        cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.attachment_read_event(draft_id, event, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn attachment_read_event(
        &mut self,
        draft_id: Uuid,
        event: AttachmentReadEvent,
        cx: &mut Context<Self>,
    ) {
        let id = match &event {
            AttachmentReadEvent::Progress(id, _) | AttachmentReadEvent::Finished(id, _) => *id,
        };
        let Some(index) = self
            .pending_attachments
            .iter()
            .position(|pending| pending.id == id)
        else {
            return;
        };
        match event {
            AttachmentReadEvent::Progress(_, progress) => {
                self.pending_attachments[index].progress = Some(progress);
            }
            AttachmentReadEvent::Finished(_, result) => {
                let target = self.pending_attachments.remove(index).target;
                let result = result.and_then(|attachment| {
                    self.update_draft(draft_id, cx, |draft| {
                        // By the records: others' files may not be here yet.
                        let total = draft
                            .attachment_records()
                            .iter()
                            .map(|record| record.size)
                            .sum::<u64>();
                        anyhow::ensure!(
                            total + attachment.len() <= MAX_MESSAGE_ATTACHMENT_BYTES,
                            "Cannot attach {}: attachments on one message can total at most {}",
                            attachment.name,
                            format_bytes(MAX_MESSAGE_ATTACHMENT_BYTES)
                        );
                        let block = draft.attachment_block(target);
                        let record = AttachmentRecord {
                            id: AttachmentId::new(),
                            name: attachment.name.clone(),
                            kind: attachment.kind(),
                            size: attachment.len(),
                            creator: draft.author.as_uuid(),
                        };
                        let id = record.id;
                        draft.files.insert(id, attachment);
                        draft.doc.add_attachment(block, record);
                        Ok(Some(id))
                    })
                    // The draft is gone (sent, or its thread was closed).
                    .unwrap_or(Ok(None))
                });
                let result = result.map(|added| {
                    if let Some(id) = added {
                        self.attachment_added(draft_id, id, cx);
                    }
                });
                if let Err(error) = result {
                    self.attachment_errors.push(AttachmentError {
                        draft_id,
                        message: error.to_string(),
                    });
                }
            }
        }
        // Also sent from render, but a hidden window may not render, and
        // others cannot submit while a read is announced.
        self.publish_presence(cx);
        cx.notify();
    }

    /// Gets a file the local user just attached to whoever needs it: the host
    /// announces it, as it already holds the bytes; a collaborator uploads
    /// it to the host, which announces it once it has every byte.
    fn attachment_added(&mut self, draft_id: Uuid, id: AttachmentId, cx: &mut Context<Self>) {
        let thread = self.draft_thread(draft_id, cx);
        let joined = thread.as_ref().is_some_and(|thread| {
            matches!(thread.read(cx).sharing, ThreadSharing::Connected { .. })
        });
        match thread {
            Some(thread) if joined => self.start_upload(thread, id, cx),
            Some(thread) => thread.update(cx, |thread, cx| {
                let uploader = thread.participant_id.into_bytes();
                thread.emit(
                    protocol::HostMessage::AttachmentStored {
                        id: id.as_uuid().into_bytes(),
                        uploader,
                    },
                    cx,
                );
            }),
            None => {
                self.update_draft(draft_id, cx, |draft| draft.stored.insert(id));
            }
        }
    }

    /// Sends a file of a joined thread's draft to its host, one chunk at a
    /// time. Stops early once the file is removed.
    fn start_upload(&mut self, thread: Entity<Thread>, id: AttachmentId, cx: &mut Context<Self>) {
        let ThreadSharing::Connected { uploads, .. } = &thread.read(cx).sharing else {
            return;
        };
        let uploads = uploads.clone();
        let thread = thread.downgrade();
        cx.spawn(async move |this, cx| {
            let mut offset = 0;
            loop {
                let Ok(chunk) = thread.update(cx, |thread, _| {
                    thread.draft.chunk(id, offset).filter(|_| {
                        thread
                            .draft
                            .attachment_records()
                            .iter()
                            .any(|record| record.id == id)
                    })
                }) else {
                    return;
                };
                let Some(chunk) = chunk else {
                    // Removed: lets the host discard what it has, which the
                    // bulk queue delivers after every chunk sent before.
                    _ = uploads
                        .send(protocol::CollaboratorMessage::AttachmentCancelled(
                            id.as_uuid().into_bytes(),
                        ))
                        .await;
                    return;
                };
                let next = chunk.offset + chunk.bytes.len() as u64;
                let total = chunk.total;
                if uploads
                    .send(protocol::CollaboratorMessage::AttachmentData(chunk))
                    .await
                    .is_err()
                {
                    return;
                }
                let updated = thread.update(cx, |thread, _| {
                    // Until the host confirms it has every byte.
                    if !thread.draft.stored.contains(&id) {
                        thread.draft.uploads.insert(id, next);
                    }
                });
                if updated.is_err() || this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                if next >= total {
                    return;
                }
                offset = next;
            }
        })
        .detach();
    }

    /// The thread whose draft is `draft_id`, if it has one yet.
    fn draft_thread(&self, draft_id: Uuid, cx: &App) -> Option<Entity<Thread>> {
        self.thread_store
            .read(cx)
            .threads
            .iter()
            .find(|thread| thread.read(cx).draft.id == draft_id)
            .cloned()
    }

    /// Removes an attachment, and its block too if that leaves the block
    /// empty while nobody is typing in it.
    fn remove_attachment(
        &mut self,
        draft_id: Uuid,
        block: ItemId,
        attachment: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let block_focused = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .editor(EditorSlot::Prompt(block))
                    .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
            })
            .unwrap_or(false);
        self.update_draft(draft_id, cx, |draft| {
            draft.doc.remove_attachment(attachment);
            draft.drop_removed_file(attachment);
            if !block_focused {
                draft.remove_if_unattended(block);
            }
        });
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);
        cx.notify();
    }

    fn pick_attachments(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach text or images".into()),
        });
        let target = self.attachment_target_at_focus(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = selected.await;
            _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(Some(paths))) => this.add_attachments(
                        draft_id,
                        target,
                        paths.into_iter().map(AttachmentSource::Path).collect(),
                        cx,
                    ),
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(error)) => {
                        this.attachment_errors.push(AttachmentError {
                            draft_id,
                            message: format!("Could not choose files: {error}"),
                        });
                        cx.notify();
                    }
                }
                this.ensure_composer_focus(window, cx);
            });
        })
        .detach();
    }

    fn drop_attachments(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        // Drops onto a block are handled by the block itself.
        let target = AttachmentTarget::NewBlock(Uuid::new_v4());
        self.drop_attachments_on(draft_id, target, paths, window, cx);
    }

    fn drop_attachments_on(
        &mut self,
        draft_id: Uuid,
        target: AttachmentTarget,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_attachments(
            draft_id,
            target,
            paths
                .paths()
                .iter()
                .cloned()
                .map(AttachmentSource::Path)
                .collect(),
            cx,
        );
        self.ensure_composer_focus(window, cx);
    }

    /// Runs before an editor's own paste so images and copied files become
    /// attachments of the block being typed in; plain text, and anything
    /// pasted into a comment, falls through to the text input.
    fn paste_attachments(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some((draft_id, slot, _)) = self.focused_draft_editor(window, cx) else {
            return;
        };
        let target = match slot {
            EditorSlot::Prompt(id) => AttachmentTarget::Block(id),
            EditorSlot::DraftPosition => AttachmentTarget::NewBlock(Uuid::new_v4()),
            EditorSlot::CommentInline(_) | EditorSlot::CommentComposer(_) => return,
        };
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let sources = clipboard_attachment_sources(&item);
        if sources.is_empty() {
            return;
        }
        // Files are not pasted as text even where they cannot be attached.
        cx.stop_propagation();
        self.add_attachments(draft_id, target, sources, cx);
    }

    fn render_pending_attachment(pending: &PendingAttachment) -> Attachment {
        let icon = if pending.is_image {
            AssetIconName::Image
        } else {
            AssetIconName::FileText
        };
        let description = pending.progress.map_or_else(
            || "Preparing".to_owned(),
            |progress| format!("Reading · {progress:.0}%"),
        );
        Attachment::new()
            .xsmall()
            .status(if pending.progress.is_some() {
                AttachmentStatus::Uploading
            } else {
                AttachmentStatus::Pending
            })
            .media(AttachmentMedia::new().child(Icon::new(icon)))
            .content(
                AttachmentContent::new()
                    .title(
                        AttachmentTitle::new(pending.name.clone())
                            .status(AttachmentStatus::Complete),
                    )
                    .description(AttachmentDescription::new(description))
                    .child(
                        Progress::new(format!("attachment-progress-{}", pending.id))
                            .xsmall()
                            .w(px(140.))
                            .loading(pending.progress.is_none())
                            .value(pending.progress.unwrap_or(0.))
                            .accessibility_label(format!("Reading {}", pending.name)),
                    ),
            )
    }

    /// An attachment card, with a remove button when `removal` names the
    /// draft and block it can be removed from.
    fn render_attachment(
        &self,
        attachment: &AttachmentCard,
        removal: Option<(Uuid, ItemId, AttachmentId)>,
        cx: &mut Context<Self>,
    ) -> Attachment {
        let size = format_bytes(attachment.size);
        let label = match attachment.kind {
            AttachmentKind::Text => "Text",
            AttachmentKind::Png => "PNG",
            AttachmentKind::Jpeg => "JPEG",
        };
        let media = match (&attachment.image, attachment.kind) {
            (Some(image), _) => AttachmentMedia::new().src(image.clone()),
            (None, AttachmentKind::Text) => {
                AttachmentMedia::new().child(Icon::new(AssetIconName::FileText))
            }
            (None, _) => AttachmentMedia::new().child(Icon::new(AssetIconName::Image)),
        };
        let (description, progress) = match attachment.transfer {
            None => (format!("{label} · {size}"), None),
            Some(Transfer::Uploading(progress)) => {
                (format!("Uploading · {progress:.0}%"), Some(progress))
            }
            Some(Transfer::Downloading(progress)) => {
                (format!("Downloading · {progress:.0}%"), Some(progress))
            }
        };
        let mut content = AttachmentContent::new()
            .title(AttachmentTitle::new(attachment.name.clone()))
            .description(AttachmentDescription::new(description));
        if let Some(progress) = progress {
            content = content.child(
                Progress::new(format!("attachment-transfer-{}", attachment.id))
                    .xsmall()
                    .w(px(140.))
                    .value(progress)
                    .accessibility_label(format!("Transferring {}", attachment.name)),
            );
        }
        let mut card = Attachment::new()
            .xsmall()
            .when_some(attachment.transfer, |card, transfer| {
                card.status(match transfer {
                    Transfer::Uploading(_) => AttachmentStatus::Uploading,
                    Transfer::Downloading(_) => AttachmentStatus::Pending,
                })
            })
            .media(media)
            .content(content);
        if let Some((draft_id, block, attachment_id)) = removal {
            card = card.actions(
                AttachmentActions::new().child(
                    Button::new(format!("remove-attachment-{attachment_id}"))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(AssetIconName::X))
                        .accessibility_label(format!("Remove {}", attachment.name))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_attachment(draft_id, block, attachment_id, window, cx);
                            this.ensure_composer_focus(window, cx);
                        })),
                ),
            );
        }
        card
    }

    /// Stops the active thread's agent run, asking the host to when the
    /// thread is mirrored.
    fn stop_generation(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread(cx) else {
            return;
        };
        let thread = thread.read(cx);
        if matches!(thread.sharing, ThreadSharing::Connected { .. }) {
            if let Some(message_id) = thread.running_agent_message_id() {
                thread.request(protocol::CollaboratorMessage::Stop {
                    message_id: message_id.into_bytes(),
                });
            }
            return;
        }
        let thread_id = thread.instance_id;
        self.cancel_generation(thread_id, None, cx);
    }

    /// Cancels the agent run of a local or hosted thread. With `message_id`,
    /// only a run still producing that message is cancelled, so a stale stop
    /// request cannot cancel the run that followed it.
    fn cancel_generation(
        &mut self,
        thread_id: Uuid,
        message_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.active_generations.get(&thread_id) else {
            return;
        };
        if message_id.is_some_and(|message_id| message_id != generation.message_id) {
            return;
        }
        generation.cancelled.store(true, Ordering::Release);
        generation.abort_handle.abort();
        cx.notify();
    }

    fn composer_button_clicked(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generating = self
            .active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .is_some_and(|thread| thread.read(cx).generating);
        if generating {
            self.stop_generation(cx);
        } else {
            self.submit_composer(window, cx);
        }
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_thread = self.active_thread(cx);
        if active_thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).generating)
        {
            return;
        }
        let Some(draft_id) = self.writable_draft_id(cx) else {
            return;
        };
        if self.draft_is_loading_attachments(draft_id, cx) {
            return;
        }

        // The host of a mirrored thread accepts submissions, so that exactly
        // one happens however many participants press Ctrl-Enter at once.
        if let Some(thread) = active_thread
            .as_ref()
            .filter(|thread| matches!(thread.read(cx).sharing, ThreadSharing::Connected { .. }))
        {
            let thread = thread.read(cx);
            if !thread.draft.doc.items().iter().all(DraftItem::is_empty) {
                thread.request(protocol::CollaboratorMessage::Submit {
                    sequence: thread.submission_count(),
                });
                self.selection_message_id = None;
                self.follow_generation = true;
            }
            return;
        }

        if self.accept_submission(draft_id, active_thread, true, cx) {
            self.selection_message_id = None;
            self.follow_generation = true;
            self.timeline_scroll_handle.scroll_to_bottom();
            // Whoever was typing in a submitted item continues at the draft
            // position; see `prepare_draft`.
            if self.focused_draft_editor(window, cx).is_none() {
                self.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
            }
        }
    }

    /// Submits a local or hosted thread's draft: publishes its non-empty
    /// items as one user message and starts the agent on it. Without a
    /// thread, the draft starts a new one. Returns whether anything was
    /// submitted. Rejections are shown when the local user submitted.
    fn accept_submission(
        &mut self,
        draft_id: Uuid,
        active_thread: Option<Entity<Thread>>,
        submitted_locally: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // Checked again here, as several participants' files add up.
        let attached = self
            .read_draft(draft_id, cx, |draft| {
                draft
                    .doc
                    .items()
                    .into_iter()
                    .filter(|item| !item.is_empty())
                    .flat_map(|item| match item.kind {
                        DraftItemKind::Prompt { attachments } => attachments,
                        DraftItemKind::Comment { .. } => Vec::new(),
                    })
                    .map(|record| record.size)
                    .sum::<u64>()
            })
            .unwrap_or(0);
        if attached > MAX_MESSAGE_ATTACHMENT_BYTES {
            // TODO: tell a collaborator who submitted, too.
            if !submitted_locally {
                return false;
            }
            self.attachment_errors
                .retain(|error| error.draft_id != draft_id);
            self.attachment_errors.push(AttachmentError {
                draft_id,
                message: format!(
                    "Attachments on one message can total at most {}",
                    format_bytes(MAX_MESSAGE_ATTACHMENT_BYTES)
                ),
            });
            cx.notify();
            return false;
        }
        let Some((comments, blocks, comments_folded)) = self
            .update_draft(draft_id, cx, Self::take_submission)
            .flatten()
        else {
            return false;
        };
        self.attachment_errors
            .retain(|error| error.draft_id != draft_id);

        let (timeline, files) = match &active_thread {
            Some(thread) => {
                let thread = thread.read(cx);
                (thread.timeline.clone(), thread.draft.files.clone())
            }
            None => (Vec::new(), self.new_thread_draft.files.clone()),
        };
        let history = Self::rig_history(&timeline, &files);
        let turn_comments = Arc::new(TurnComments::new(comments.len()));
        let preface = Self::comments_preface(&comments, turn_comments.comment_ids(), &timeline);
        let prompt = agent_message(preface.as_deref(), &blocks, &files);
        let comment_ids = comments
            .iter()
            .map(|comment| comment.id)
            .collect::<Vec<_>>();
        let has_comments = !comments.is_empty();
        let submitted_group = UserMessageGroup {
            id: Uuid::new_v4(),
            comments,
            blocks,
            history_preface: preface,
            comments_folded: has_comments || comments_folded,
        };
        let comment_group_id = has_comments.then_some(submitted_group.id);
        let title = submitted_group.title_text().to_owned();

        let thread_id = if let Some(thread) = active_thread {
            let thread_id = thread.read(cx).instance_id;
            thread.update(cx, |thread, cx| {
                if let Some(title) = Self::title_for_first_message(&thread.timeline, &title) {
                    thread.emit(protocol::HostMessage::ThreadTitled(title), cx);
                }
                thread.publish(protocol::HostMessage::UserMessage(
                    submitted_group.to_protocol(),
                ));
                thread.timeline.push(TimelineMessage::User(submitted_group));
            });
            thread_id
        } else {
            // The draft moves into the new thread, keeping whatever was not
            // submitted and any attachments still being read.
            let draft = std::mem::replace(
                &mut self.new_thread_draft,
                ThreadDraft::new(self.local_participant_id),
            );
            let thread = Self::new_local_thread(
                Self::thread_title(&title),
                vec![TimelineMessage::User(submitted_group)],
                draft,
                self.local_participant_id,
                self.new_thread_model,
                cx,
            );
            let thread_id = thread.read(cx).instance_id;
            self.thread_store.update(cx, |store, _| {
                store.threads.push_front(thread.clone());
            });
            self.active_thread_id = Some(thread_id);
            thread_id
        };

        self.start_generation(
            thread_id,
            prompt,
            history,
            comment_group_id,
            comment_ids,
            turn_comments,
            cx,
        );
        true
    }

    /// Takes every non-empty item out of the draft, in draft order, as the
    /// comments and prompt blocks of one submission. Empty items stay, such
    /// as a comment nobody has written yet. Returns `None` when there is
    /// nothing to submit.
    fn take_submission(
        draft: &mut ThreadDraft,
    ) -> Option<(Vec<UserComment>, Vec<PromptBlock>, bool)> {
        let items = draft
            .doc
            .items()
            .into_iter()
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        if items.is_empty() {
            return None;
        }
        let mut comments = Vec::new();
        let mut blocks = Vec::new();
        for item in &items {
            let author = ParticipantId::from_uuid(item.creator);
            match &item.kind {
                DraftItemKind::Comment { target } => comments.push(UserComment {
                    id: item.id.as_uuid(),
                    author,
                    presence: ItemPresence::default(),
                    reference: CommentReference {
                        message_id: target.message_id,
                        range: target.range.clone(),
                        quote: target.quote.clone(),
                    },
                    body: UserCommentBody::Submitted(item.body.clone().into()),
                }),
                DraftItemKind::Prompt { attachments } => blocks.push(PromptBlock {
                    id: item.id.as_uuid(),
                    author,
                    text: item.body.clone(),
                    attachments: attachments.clone(),
                }),
            }
        }
        let ids = items.iter().map(|item| item.id).collect::<Vec<_>>();
        // Their files stay: the submitted message shows and sends them.
        draft.take_items(&ids);
        Some((comments, blocks, draft.comments_folded))
    }

    fn timeline_scrolled(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        let delta_y = event.delta.pixel_delta(window.line_height()).y;
        let max_offset = self.timeline_scroll_handle.max_offset().y;

        if delta_y > px(0.) && max_offset > px(0.) {
            self.follow_generation = false;
        } else if delta_y < px(0.) {
            let projected_offset = self.timeline_scroll_handle.offset().y + delta_y;
            if projected_offset <= -max_offset + px(1.) {
                self.follow_generation = true;
            }
        }
    }

    fn render_bottom_bar(
        &self,
        composer: Option<Entity<TextareaState>>,
        read_only_line_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_scroll_handle = self.timeline_scroll_handle.clone();
        let can_write = composer.is_some();
        let active_thread = self.active_thread(cx);
        // Picking the model and stopping the agent stay available to
        // participants who cannot write to the draft.
        let can_control = can_write
            || active_thread
                .as_ref()
                .is_some_and(|thread| thread.read(cx).sharing.is_collaborating());

        let loading_attachments = self
            .writable_draft_id(cx)
            .is_some_and(|draft_id| self.draft_is_loading_attachments(draft_id, cx));
        let generating = active_thread.is_some_and(|thread| thread.read(cx).generating);
        let selected_model_title = self
            .model_picker
            .read(cx)
            .selection()
            .first()
            .map(|(_, model)| model.title())
            .unwrap_or_else(|| "Select a model...".into());
        let title_run = TextRun {
            len: selected_model_title.len(),
            font: window.text_style().font(),
            color: rgb(0xd4d4d8).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let title_width = window
            .text_system()
            .shape_line(selected_model_title, px(14.), &[title_run], None)
            .width();
        // Icon + chevron + two gaps + button padding + Combobox's custom-trigger slot gap.
        let model_picker_width = title_width + px(62.);
        let model_picker_hovered = self.model_picker_hovered;
        let model_picker = div()
            .id("model-picker-container")
            .w(model_picker_width)
            .min_w_0()
            .h(px(28.))
            .mt(px(3.))
            .flex_none()
            .flex()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if this.model_picker_hovered != *hovered {
                    this.model_picker_hovered = *hovered;
                    cx.notify();
                }
            }))
            .child(
                Combobox::new(&self.model_picker)
                    .search_placeholder("Search models...")
                    .menu_width(px(360.))
                    .menu_max_h(rems(24.))
                    .appearance(false)
                    .small()
                    .p_0()
                    .render_trigger(move |trigger, _, _| {
                        let selected_model = trigger.selection().first().map(|(_, model)| model);
                        let title = selected_model
                            .map(LanguageModel::title)
                            .unwrap_or_else(|| "Select a model...".into());
                        let provider = selected_model.map(|model| model.selection.provider);

                        div()
                            .h_full()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(
                                div()
                                    .h_full()
                                    .max_w_full()
                                    .min_w_0()
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(model_picker_hovered, |this| this.bg(rgb(0x2d2d30)))
                                    .text_sm()
                                    .text_color(rgb(0xd4d4d8))
                                    .when_some(provider, |this, provider| {
                                        this.child(
                                            img(provider.icon_path())
                                                .size(px(18.))
                                                .flex_none()
                                                .rounded(px(4.)),
                                        )
                                    })
                                    .child(div().child(title))
                                    .child(
                                        Icon::new(if trigger.is_open() {
                                            AssetIconName::ChevronUp
                                        } else {
                                            AssetIconName::ChevronDown
                                        })
                                        .size_4()
                                        .flex_none()
                                        .text_color(rgb(0xa1a1aa)),
                                    ),
                            )
                    }),
            );
        let button = if generating {
            Some(
                Button::new("stop-generation")
                    .icon(Icon::new(AssetIconName::Square))
                    .danger()
                    .small()
                    .accessibility_label("Stop generating")
                    .on_click(cx.listener(Self::composer_button_clicked)),
            )
        } else if can_write {
            Some(
                Button::new("send-message")
                    .icon(Icon::new(AssetIconName::SendHorizontal))
                    .small()
                    .accessibility_label(if loading_attachments {
                        "Send message (waiting for attachments)"
                    } else {
                        "Send message"
                    })
                    .disabled(loading_attachments)
                    .on_click(cx.listener(Self::composer_button_clicked)),
            )
        } else {
            None
        };

        div()
            .id("bottom-bar")
            .debug_selector(|| "bottom-bar".to_owned())
            .relative()
            .h(TOP_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .px_3()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let content_bottom = if let Some(composer) = &composer {
                            let composer = composer.read(cx);
                            let text_end = composer.value().len();
                            composer
                                .range_to_bounds(&(text_end..text_end))
                                .map(|bounds| bounds.bottom())
                        } else {
                            read_only_line_bounds.get().map(|bounds| bounds.bottom())
                        };
                        let Some(content_bottom) = content_bottom else {
                            return;
                        };
                        let scroll_offset = timeline_scroll_handle.offset().y;
                        let max_scroll_offset = timeline_scroll_handle.max_offset().y;
                        let is_scrolled_to_bottom = max_scroll_offset > px(0.)
                            && scroll_offset <= -max_scroll_offset + px(1.);
                        let divider_visible = !is_scrolled_to_bottom
                            && content_bottom >= bounds.top() - BOTTOM_BAR_DIVIDER_THRESHOLD;

                        if divider_visible {
                            window.paint_quad(gpui::fill(bounds, rgb(0x2d2d30)));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(1.)),
            )
            .when(can_write, |this| {
                this.child(
                    Button::new("add-attachment")
                        .icon(Icon::new(AssetIconName::Paperclip))
                        .ghost()
                        .small()
                        .accessibility_label("Attach files")
                        .on_click(cx.listener(Self::pick_attachments)),
                )
            })
            .when(can_control, |this| {
                this.child(div().flex_1())
                    .child(model_picker)
                    .children(button)
            })
    }

    fn submit_composer_action(
        &mut self,
        _: &SubmitComposer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_composer(window, cx);
    }

    fn render_composer_input(composer: &Entity<TextareaState>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(("composer", composer.entity_id()))
            .debug_selector(|| "composer".to_owned())
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(Textarea::new(composer))
    }

    /// A composer editor. The last one is tall, so there is room to click
    /// into, and it stays that tall when typing turns the draft position
    /// into a block.
    fn render_composer_editor(
        editor: &Entity<TextareaState>,
        last: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        Self::render_composer_input(editor)
            .when(last, |this| this.min_h(px(110.)))
            .on_click({
                let editor = editor.clone();
                cx.listener(move |_, _, window, cx| {
                    editor.focus_handle(cx).focus(window, cx);
                })
            })
    }

    /// Whether the draft position is shown below the prompt blocks: always
    /// when there are none, otherwise only while someone is there or files
    /// are being read for a new block.
    fn draft_row_visible(&self, draft: &ThreadDraft, window: &Window, cx: &App) -> bool {
        !draft.doc.items().iter().any(|item| item.is_prompt())
            || draft
                .draft_position
                .as_ref()
                .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
            || !draft.others_at_draft_position(&[]).is_empty()
            || self
                .pending_reads(draft)
                .iter()
                .any(|pending| draft.pending_block(pending.target).is_none())
    }

    /// Every file being read into the draft: the local user's, and those
    /// others announce in their presence.
    fn pending_reads(&self, draft: &ThreadDraft) -> Vec<PendingAttachment> {
        let local = self
            .pending_attachments
            .iter()
            .filter(|pending| pending.draft_id == draft.id)
            .cloned();
        let remote = draft
            .presence
            .iter()
            .filter(|(participant, _)| **participant != draft.author)
            .flat_map(|(_, (presence, _))| &presence.pending_reads)
            .map(|read| PendingAttachment {
                id: Uuid::from_bytes(read.id),
                draft_id: draft.id,
                target: match read.block {
                    Some(block) => {
                        AttachmentTarget::Block(ItemId::from_uuid(Uuid::from_bytes(block)))
                    }
                    // Never matches a batch, so it is shown at the draft
                    // position like the local user's reads for new blocks.
                    None => AttachmentTarget::NewBlock(Uuid::nil()),
                },
                name: read.name.clone(),
                is_image: read.is_image,
                progress: read.progress.map(f32::from),
            });
        local.chain(remote).collect()
    }

    /// Whose avatars the draft position row shows: everyone while the draft
    /// has no blocks, otherwise those at it. The local user comes first when
    /// included, and stands alone in an unshared thread.
    fn draft_position_people(
        draft: &ThreadDraft,
        participants: &[ParticipantId],
        window: &Window,
        cx: &App,
    ) -> (ParticipantId, Vec<ParticipantId>) {
        let local_there = draft
            .draft_position
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
        let others = if draft.doc.items().iter().any(|item| item.is_prompt()) {
            draft.others_at_draft_position(participants)
        } else {
            participants
                .iter()
                .copied()
                .filter(|participant| *participant != draft.author)
                .collect()
        };
        let local_included = local_there
            || participants.is_empty()
            || !draft.doc.items().iter().any(|item| item.is_prompt());
        match (local_included, others.split_first()) {
            (false, Some((first, rest))) => (*first, rest.to_vec()),
            _ => (draft.author, others),
        }
    }

    /// The thread's participants in join order, for the draft `draft_id`.
    fn draft_participants(&self, draft_id: Uuid, cx: &App) -> Vec<ParticipantId> {
        self.thread_store
            .read(cx)
            .threads
            .iter()
            .map(|thread| thread.read(cx))
            .find(|thread| thread.draft.id == draft_id)
            .map(|thread| thread.participants.clone())
            .unwrap_or_default()
    }

    /// The bottom-most editor of the composer.
    fn last_composer_editor(&self, window: &Window, cx: &App) -> Option<Entity<TextareaState>> {
        let draft_id = self.writable_draft_id(cx)?;
        self.read_draft(draft_id, cx, |draft| {
            if self.draft_row_visible(draft, window, cx) {
                return draft.draft_position.clone();
            }
            let last_block = draft
                .doc
                .items()
                .into_iter()
                .rfind(|item| item.is_prompt())?;
            draft.editor(EditorSlot::Prompt(last_block.id))
        })?
    }

    fn composer_model(&self, draft_id: Uuid, window: &Window, cx: &App) -> Option<ComposerModel> {
        let participants = self.draft_participants(draft_id, cx);
        self.read_draft(draft_id, cx, |draft| {
            let blocks = draft
                .doc
                .items()
                .into_iter()
                .filter_map(|item| {
                    let DraftItemKind::Prompt { attachments } = item.kind else {
                        return None;
                    };
                    let creator = ParticipantId::from_uuid(item.creator);
                    Some(ComposerBlock {
                        id: item.id,
                        creator,
                        presence: ItemPresence {
                            editors: draft
                                .editors_of(item.id, &participants)
                                .into_iter()
                                .filter(|editor| *editor != creator)
                                .collect(),
                            carets: draft.remote_carets(item.id, &participants),
                        },
                        editor: draft.editor(EditorSlot::Prompt(item.id))?,
                        attachments: attachments
                            .iter()
                            .map(|record| AttachmentCard::new(draft, record))
                            .collect(),
                    })
                })
                .collect();
            let pending = self
                .pending_reads(draft)
                .iter()
                .map(|pending| {
                    (
                        draft.pending_block(pending.target),
                        Self::render_pending_attachment(pending),
                    )
                })
                .collect();
            ComposerModel {
                draft_id,
                comments: draft.comment_views(&participants),
                comments_folded: draft.comments_folded,
                blocks,
                draft_position: draft.draft_position.clone(),
                draft_row_visible: self.draft_row_visible(draft, window, cx),
                draft_position_people: Self::draft_position_people(
                    draft,
                    &participants,
                    window,
                    cx,
                ),
                draft_position_presence: ItemPresence {
                    editors: Vec::new(),
                    carets: draft.draft_position_carets(&participants),
                },
                pending,
            }
        })
    }

    /// The composer: the draft's comments, one row per prompt block, and the
    /// draft position, followed by empty space that leads to the draft
    /// position when clicked.
    fn render_composer(
        &self,
        composer: ComposerModel,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ComposerModel {
            draft_id,
            comments,
            comments_folded,
            blocks,
            draft_position,
            draft_row_visible,
            draft_position_people,
            draft_position_presence,
            mut pending,
        } = composer;
        let mut rows = Vec::new();
        if !comments.is_empty() {
            let mut content = vec![
                Self::render_comment_group_toggle(draft_id, comments.len(), comments_folded, cx)
                    .into_any_element(),
            ];
            if !comments_folded {
                content.extend(comments.iter().map(Self::render_composer_comment));
            }
            rows.push(Self::render_comment_group_row(&comments, content).into_any_element());
        }

        let block_count = blocks.len();
        for (index, block) in blocks.into_iter().enumerate() {
            let last = !draft_row_visible && index + 1 == block_count;
            let block_pending = pending
                .extract_if(.., |(target, _)| *target == Some(block.id))
                .map(|(_, pending)| pending)
                .collect::<Vec<_>>();
            let mut content = Vec::new();
            if !block.attachments.is_empty() || !block_pending.is_empty() {
                content.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .children(block.attachments.iter().map(|attachment| {
                            self.render_attachment(
                                attachment,
                                Some((draft_id, block.id, attachment.id)),
                                cx,
                            )
                        }))
                        .children(block_pending)
                        .into_any_element(),
                );
            }
            content.push(
                Self::render_composer_editor(&block.editor, last, cx)
                    .child(Self::render_remote_carets(
                        &block.editor,
                        block.presence.carets,
                    ))
                    .into_any_element(),
            );
            let block_id = block.id;
            rows.push(
                Self::render_presence_row(block.creator, &block.presence.editors, content)
                    .can_drop(|value, _, _| {
                        value
                            .downcast_ref::<ExternalPaths>()
                            .is_some_and(|paths| !paths.paths().is_empty())
                    })
                    .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                        this.drop_attachments_on(
                            draft_id,
                            AttachmentTarget::Block(block_id),
                            paths,
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    }))
                    .into_any_element(),
            );
        }

        let errors = self
            .attachment_errors
            .iter()
            .filter(|error| error.draft_id == draft_id)
            .map(|error| {
                div()
                    .text_xs()
                    .text_color(rgb(0xf87171))
                    .child(error.message.clone())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        if draft_row_visible {
            let mut content = Vec::new();
            if !pending.is_empty() {
                content.push(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .children(pending.into_iter().map(|(_, pending)| pending))
                        .into_any_element(),
                );
            }
            content.extend(errors);
            content.extend(draft_position.as_ref().map(|editor| {
                Self::render_composer_editor(editor, true, cx)
                    .child(Self::render_remote_carets(
                        editor,
                        draft_position_presence.carets,
                    ))
                    .into_any_element()
            }));
            let (primary, others) = draft_position_people;
            rows.push(Self::render_presence_row(primary, &others, content).into_any_element());
        } else if !errors.is_empty() {
            rows.push(Self::render_message_row(None, errors).into_any_element());
        }

        div()
            .id("composer-area")
            .w_full()
            .flex_1()
            .flex()
            .flex_col()
            .child(div().w_full().flex().flex_col().gap_3().children(rows))
            .child(
                div()
                    .id("composer-empty-space")
                    .w_full()
                    .flex_1()
                    .min_h(px(24.))
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus_draft_editor(
                            draft_id,
                            EditorSlot::DraftPosition,
                            None,
                            window,
                            cx,
                        );
                    })),
            )
    }

    fn render_main_editor(
        &mut self,
        read_only_line_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_generation = self.render_generation.wrapping_add(1);
        let active_thread_id = self.active_thread_id;
        let (messages, draft_comments) = active_thread_id
            .and_then(|thread_id| self.thread_store.read(cx).thread(thread_id, cx))
            .map(|thread| {
                let thread = thread.read(cx);
                (
                    thread.timeline.clone(),
                    thread.draft.comment_views(&thread.participants),
                )
            })
            .unwrap_or_else(|| (Vec::new(), self.new_thread_draft.comment_views(&[])));
        let composer = self
            .writable_draft_id(cx)
            .and_then(|draft_id| self.composer_model(draft_id, window, cx));
        let comments = messages
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(group.comments.as_slice()),
                TimelineMessage::Agent(_) => None,
            })
            .flatten()
            .cloned()
            .chain(draft_comments)
            .collect::<Vec<_>>();
        let sidebar_width = if self.sidebar_open {
            SIDEBAR_WIDTH
        } else {
            px(0.)
        };
        let available_width = window.viewport_size().width - sidebar_width - px(82.);
        let wrap_width = if available_width > px(120.) {
            available_width
        } else {
            px(120.)
        };
        let mut timeline_messages = Vec::new();
        if let Some(thread_id) = active_thread_id {
            for (index, message) in messages.iter().enumerate() {
                let submitted_comments = match message {
                    TimelineMessage::Agent(message) => message
                        .comment_group_id
                        .and_then(|group_id| {
                            messages.iter().find_map(|entry| match entry {
                                TimelineMessage::User(group) if group.id == group_id => {
                                    Some(group.comments.as_slice())
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default(),
                    TimelineMessage::User(_) => &[],
                };
                timeline_messages.push(self.render_timeline_message(
                    thread_id,
                    index,
                    message,
                    &comments,
                    submitted_comments,
                    wrap_width,
                    window,
                    cx,
                ));
            }
        }

        self.segment_text_views
            .retain(|_, text_view| text_view.rendered_at == self.render_generation);

        let can_write = composer.is_some();
        let composer = composer.map(|composer| self.render_composer(composer, cx));

        div()
            .id("main-editor")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(rgb(0x2d2d30))
            .bg(rgb(0x18181b))
            .child(
                div()
                    .id("timeline-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.timeline_scroll_handle)
                    .on_scroll_wheel(cx.listener(Self::timeline_scrolled))
                    .child(
                        div()
                            .w_full()
                            .pt_6()
                            .min_h_full()
                            .flex()
                            .flex_col()
                            .text_sm()
                            .text_color(rgb(0xd4d4d8))
                            .children(timeline_messages.into_iter().enumerate().map(
                                |(index, message)| {
                                    div().when(index != 0, |this| this.mt_6()).child(message)
                                },
                            ))
                            .children(composer.map(|composer| {
                                composer.when(!messages.is_empty(), |this| this.mt_6())
                            }))
                            .when(!can_write, |this| {
                                this.child(
                                    div()
                                        .id("read-only-thread")
                                        .when(!messages.is_empty(), |this| this.mt_6())
                                        .relative()
                                        .w_full()
                                        .flex()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(rgb(0x71717a))
                                        .child("Read-only thread")
                                        .child(
                                            canvas(
                                                move |bounds, _, _| {
                                                    read_only_line_bounds.set(Some(bounds));
                                                },
                                                |_, _, _, _| {},
                                            )
                                            .absolute()
                                            .size_full(),
                                        ),
                                )
                            }),
                    ),
            )
    }
}

impl Render for Cowork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_model_picker(window, cx);
        if let Some(draft_id) = self.writable_draft_id(cx) {
            self.prepare_draft(draft_id, window, cx);
        }
        self.publish_presence(cx);
        self.schedule_caret_label_refresh(cx);
        let composer = self.last_composer_editor(window, cx);
        let can_write = composer.is_some();
        let read_only_line_bounds = Rc::new(Cell::new(None));

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x1c1c1f))
            .on_action(cx.listener(Self::submit_composer_action))
            .on_key_down(cx.listener(Self::begin_inline_comment))
            // Run before the focused editor's own handling, which they
            // extend to the composer as a whole.
            .capture_action(cx.listener(Self::paste_attachments))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.move_between_draft_editors(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.move_between_draft_editors(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.escape_empty_draft_item(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
                if this.backspace_out_of_empty_draft_item(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_top_bar(window, cx))
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .h_full()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .when(can_write, |this| {
                                this.can_drop(|value, _, _| {
                                    value
                                        .downcast_ref::<ExternalPaths>()
                                        .is_some_and(|paths| !paths.paths().is_empty())
                                })
                                .on_drop(cx.listener(Self::drop_attachments))
                            })
                            .child(self.render_main_editor(
                                read_only_line_bounds.clone(),
                                window,
                                cx,
                            ))
                            .child(self.render_bottom_bar(
                                composer,
                                read_only_line_bounds,
                                window,
                                cx,
                            )),
                    ),
            )
            .children(Root::render_dialog_layer(window, cx))
    }
}

fn main() -> anyhow::Result<()> {
    let worker_threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, 8);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("cowork-agent")
        .enable_all()
        .build()?;
    let tokio_handle = runtime.handle().clone();
    anyhow::ensure!(
        TOKIO_RUNTIME.set(runtime).is_ok(),
        "the agent runtime was already initialized",
    );

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            gpui_component::Theme::change(ThemeMode::Dark, None, cx);
            gpui_component::Theme::update(cx, |theme| {
                theme.popover = rgb(0x1c1c1f).into();
            });
            TextViewDefaults::new()
                .with_code_block_highlighter(highlight_code_block)
                .install(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-enter", SubmitComposer, None),
                KeyBinding::new("cmd-enter", SubmitComposer, None),
            ]);
            #[cfg(target_os = "macos")]
            {
                cx.on_action(|_: &Quit, cx| cx.quit());
                cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            }
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(760.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Cowork".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(macos_traffic_light_position()),
                }),
                app_owns_titlebar_drag: cfg!(target_os = "macos"),
                ..Default::default()
            };

            if let Err(error) = cx.open_window(window_options, move |window, cx| {
                let tokio_handle = tokio_handle.clone();
                let thread_store = cx.new(|_| ThreadStore::default());
                let local_participant_id = ParticipantId::new();
                let new_thread_draft = ThreadDraft::new(local_participant_id);
                let cowork = cx.new(|cx| {
                    let window_activation_subscription =
                        cx.observe_window_activation(window, |_, window, _cx| {
                            if window.is_window_active() {
                                window.on_next_frame(Cowork::end_stale_mouse_drag);
                            }
                        });
                    let (model_picker, model_picker_subscription) =
                        Cowork::new_model_picker(window, cx);
                    Cowork {
                        sidebar_open: true,
                        recents_open: true,
                        new_thread_draft,
                        attachment_errors: Vec::new(),
                        pending_attachments: Vec::new(),
                        timeline_scroll_handle: ScrollHandle::new(),
                        follow_generation: true,
                        thread_store,
                        active_thread_id: None,
                        selection_message_id: None,
                        segment_text_views: HashMap::new(),
                        render_generation: 0,
                        titlebar_click_armed: false,
                        copied_endpoint_id: None,
                        join_dialog: None,
                        tokio_handle,
                        active_generations: HashMap::new(),
                        local_participant_id,
                        typing_in: None,
                        published_presence: HashMap::new(),
                        caret_label_refresh: None,
                        new_thread_model: DEFAULT_MODEL,
                        model_picker,
                        model_picker_hovered: false,
                        _model_picker_subscription: model_picker_subscription,
                        _window_activation_subscription: window_activation_subscription,
                    }
                });
                cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx));
                cx.new(|cx| Root::new(cowork, window, cx))
            }) {
                eprintln!("failed to open Cowork window: {error}");
                cx.quit();
                return;
            }

            cx.set_quit_mode(QuitMode::LastWindowClosed);
            cx.activate(true);
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui_base::TextSelectionLayer;

    fn annotate_markdown(markdown: &str, ranges: impl IntoIterator<Item = Range<usize>>) -> String {
        Cowork::annotate_markdown_with_source_offsets(markdown, ranges).0
    }

    fn encoded_image(width: u32, format: image::ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, 2))
            .write_to(&mut std::io::Cursor::new(&mut bytes), format)
            .expect("encode test image");
        bytes
    }

    fn text_attachment(name: &str, text: &str) -> FileAttachment {
        FileAttachment {
            name: name.into(),
            content: FileAttachmentContent::Text(text.into()),
        }
    }

    /// Records for `files`, and the files by id, as a thread holds them.
    fn attached(
        files: impl IntoIterator<Item = FileAttachment>,
    ) -> (Vec<AttachmentRecord>, HashMap<AttachmentId, FileAttachment>) {
        files
            .into_iter()
            .map(|file| {
                let record = AttachmentRecord {
                    id: AttachmentId::new(),
                    name: file.name.clone(),
                    kind: file.kind(),
                    size: file.len(),
                    creator: Uuid::new_v4(),
                };
                (record.clone(), (record.id, file))
            })
            .unzip()
    }

    #[test]
    fn attachments_become_ollama_text_and_base64_image_parts() {
        let author = ParticipantId::from_bytes([7; 16]);
        let (records, files) = attached([
            text_attachment("say \"hi\".txt", "hello"),
            FileAttachment {
                name: "photo.png".into(),
                content: FileAttachmentContent::Png(Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    vec![1, 2, 3],
                ))),
            },
        ]);
        let block = PromptBlock {
            id: Uuid::new_v4(),
            author,
            text: "Question".into(),
            attachments: records,
        };
        let RigMessage::User { content } = agent_message(None, &[block], &files) else {
            panic!("expected user message");
        };
        assert_eq!(content.len(), 3);
        assert!(
            matches!(&content[0], UserContent::Text(text) if text.text == "Mossy Crane:\nQuestion")
        );
        assert!(
            matches!(&content[1], UserContent::Text(text) if text.text == "<file name=\"say &quot;hi&quot;.txt\">\nhello\n</file>")
        );
        assert!(matches!(&content[2], UserContent::Image(image)
            if image.data == rig::message::DocumentSourceKind::Base64("AQID".into())
                && image.media_type == Some(ImageMediaType::PNG)));
    }

    /// Comments come first, then each block under its creator's name with its
    /// own attachments right after it.
    #[test]
    fn agent_message_keeps_attachments_with_their_blocks() {
        let alice = ParticipantId::from_bytes([7; 16]);
        let bob = ParticipantId::new();
        let (records, files) = attached([text_attachment("crash.log", "boom")]);
        let blocks = [
            PromptBlock {
                id: Uuid::new_v4(),
                author: alice,
                text: "Investigate the crash.".into(),
                attachments: records,
            },
            PromptBlock {
                id: Uuid::new_v4(),
                author: bob,
                text: "Also check the logs.".into(),
                attachments: Vec::new(),
            },
        ];
        let RigMessage::User { content } = agent_message(Some("Comments first."), &blocks, &files)
        else {
            panic!("expected user message");
        };
        let texts = content
            .iter()
            .map(|part| match part {
                UserContent::Text(text) => text.text.clone(),
                _ => panic!("expected only text parts"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "Comments first.".to_owned(),
                "Mossy Crane:\nInvestigate the crash.".to_owned(),
                "<file name=\"crash.log\">\nboom\n</file>".to_owned(),
                format!("{}:\nAlso check the logs.", bob.display_name()),
            ]
        );
    }

    #[test]
    fn attachments_are_classified_by_content() {
        let source = attachment_from_bytes("main.rs".into(), b"fn main() {}".to_vec())
            .expect("any UTF-8 file is text");
        assert!(matches!(source.content, FileAttachmentContent::Text(_)));

        let starts_like_bmp = attachment_from_bytes("notes".into(), b"BMW notes".to_vec())
            .expect("text with an image-like signature is still text");
        assert!(matches!(
            starts_like_bmp.content,
            FileAttachmentContent::Text(_)
        ));

        let binary = attachment_from_bytes("app.exe".into(), vec![0x4d, 0x5a, 0x00, 0xff]);
        assert!(binary.is_err());

        let png = encoded_image(2, image::ImageFormat::Png);
        let kept = attachment_from_bytes("photo".into(), png.clone()).expect("png");
        assert!(matches!(kept.content, FileAttachmentContent::Png(image) if image.bytes() == png));

        let jpeg =
            attachment_from_bytes("photo".into(), encoded_image(2, image::ImageFormat::Jpeg))
                .expect("jpeg");
        assert!(matches!(jpeg.content, FileAttachmentContent::Jpeg(_)));

        let bmp = attachment_from_bytes(
            "screen.bmp".into(),
            encoded_image(2, image::ImageFormat::Bmp),
        )
        .expect("bmp");
        assert!(matches!(bmp.content, FileAttachmentContent::Png(image)
            if image::guess_format(image.bytes()).ok() == Some(image::ImageFormat::Png)));
    }

    #[test]
    fn oversized_text_attachments_are_rejected() {
        let limit = MAX_TEXT_ATTACHMENT_BYTES as usize;
        assert!(attachment_from_bytes("ok.txt".into(), vec![b'a'; limit]).is_ok());
        let error = attachment_from_bytes("big.txt".into(), vec![b'a'; limit + 1])
            .err()
            .expect("text over the limit")
            .to_string();
        assert_eq!(error, "big.txt is 256 KB; text files can be at most 256 KB");
    }

    #[test]
    fn reading_attachment_file_reports_progress() {
        let path = std::env::temp_dir().join(format!("cowork-{}.txt", Uuid::new_v4()));
        let body = "a".repeat(400_000);
        std::fs::write(&path, &body).expect("write test attachment");
        let mut progress = Vec::new();
        let result = read_attachment_file(&path, "test.txt", |value| progress.push(value));
        std::fs::remove_file(&path).expect("remove test attachment");
        assert_eq!(result.expect("read attachment"), body.as_bytes());
        assert!(progress.len() >= 2);
        assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(progress.last(), Some(&100.));
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(400_000), "391 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10 MB");
    }

    #[test]
    fn clipboard_prefers_files_then_text_then_images() {
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![1]);
        let image_only = ClipboardItem::new_image(&image);
        assert!(matches!(
            clipboard_attachment_sources(&image_only).as_slice(),
            [AttachmentSource::ClipboardImage(_)]
        ));

        let text_only = ClipboardItem::new_string("hello".into());
        assert!(clipboard_attachment_sources(&text_only).is_empty());

        let mut text_and_image = ClipboardItem::new_string("A1\tB1".into());
        text_and_image
            .entries
            .push(ClipboardEntry::Image(image.clone()));
        assert!(clipboard_attachment_sources(&text_and_image).is_empty());

        let mut files_and_text = ClipboardItem::new_string("notes.txt".into());
        files_and_text
            .entries
            .push(ClipboardEntry::ExternalPaths(ExternalPaths(
                vec![PathBuf::from("notes.txt")].into(),
            )));
        assert!(matches!(
            clipboard_attachment_sources(&files_and_text).as_slice(),
            [AttachmentSource::Path(path)] if path == Path::new("notes.txt")
        ));
    }

    struct AttachmentTestRoot {
        cowork: Entity<Cowork>,
    }

    impl Render for AttachmentTestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A `Cowork` showing `active_thread_id` out of `thread_store`, for tests
    /// that do not go through the app's window setup.
    fn test_cowork(
        thread_store: Entity<ThreadStore>,
        active_thread_id: Option<Uuid>,
        tokio_handle: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Cowork>,
    ) -> Cowork {
        let (model_picker, model_picker_subscription) = Cowork::new_model_picker(window, cx);
        let local_participant_id = ParticipantId::new();
        Cowork {
            sidebar_open: true,
            recents_open: true,
            new_thread_draft: ThreadDraft::new(local_participant_id),
            attachment_errors: Vec::new(),
            pending_attachments: Vec::new(),
            timeline_scroll_handle: ScrollHandle::new(),
            follow_generation: true,
            thread_store,
            active_thread_id,
            selection_message_id: None,
            segment_text_views: HashMap::new(),
            render_generation: 0,
            titlebar_click_armed: false,
            copied_endpoint_id: None,
            join_dialog: None,
            tokio_handle,
            active_generations: HashMap::new(),
            local_participant_id,
            typing_in: None,
            published_presence: HashMap::new(),
            caret_label_refresh: None,
            new_thread_model: DEFAULT_MODEL,
            model_picker,
            model_picker_hovered: false,
            _model_picker_subscription: model_picker_subscription,
            _window_activation_subscription: cx.observe_window_activation(window, |_, _, _| {}),
        }
    }

    fn test_thread(thread_id: Uuid, timeline: Vec<TimelineMessage>, draft: ThreadDraft) -> Thread {
        Thread {
            instance_id: thread_id,
            summary: ThreadSummary {
                id: thread_id,
                title: "Test".into(),
            },
            participant_id: ParticipantId::new(),
            participants: Vec::new(),
            model: DEFAULT_MODEL,
            timeline,
            draft,
            generating: false,
            sharing: ThreadSharing::NotShared,
            ownership: ThreadOwnership::Local,
        }
    }

    fn attachment_test_cowork(
        cx: &mut gpui::TestAppContext,
        tokio_handle: tokio::runtime::Handle,
    ) -> (Entity<Cowork>, Uuid, &mut gpui::VisualTestContext) {
        cx.update(gpui_component::init);
        let thread_id = Uuid::new_v4();
        let (view, cx) = cx.add_window_view(|window, cx| {
            let draft = ThreadDraft::new(ParticipantId::new());
            let thread = cx.new(|_| test_thread(thread_id, Vec::new(), draft));
            let thread_store = cx.new(|_| ThreadStore {
                threads: VecDeque::from([thread]),
            });
            let cowork =
                cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
            AttachmentTestRoot { cowork }
        });
        let cowork = view.read_with(cx, |root, _| root.cowork.clone());
        (cowork, thread_id, cx)
    }

    /// The attachments of a thread's draft, in draft order.
    fn thread_draft_attachments(
        cowork: &Entity<Cowork>,
        thread_id: Uuid,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<FileAttachment> {
        cowork.read_with(cx, |cowork, cx| {
            let thread = cowork
                .thread_store
                .read(cx)
                .thread(thread_id, cx)
                .expect("thread")
                .read(cx);
            draft_attachments(&thread.draft)
        })
    }

    fn draft_attachments(draft: &ThreadDraft) -> Vec<FileAttachment> {
        draft
            .doc
            .items()
            .into_iter()
            .flat_map(|item| match item.kind {
                DraftItemKind::Prompt { attachments } => attachments,
                DraftItemKind::Comment { .. } => Vec::new(),
            })
            .map(|record| {
                draft
                    .files
                    .get(&record.id)
                    .cloned()
                    .expect("attachment bytes")
            })
            .collect()
    }

    #[gpui::test]
    fn attachments_land_on_their_thread_after_switching_away(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
        let path = std::env::temp_dir().join(format!("cowork-{}.md", Uuid::new_v4()));
        std::fs::write(&path, "# Notes").expect("write test attachment");

        cowork.update(cx, |cowork, cx| {
            let draft_id = cowork.writable_draft_id(cx).expect("writable thread");
            cowork.add_attachments(
                draft_id,
                AttachmentTarget::NewBlock(Uuid::new_v4()),
                vec![AttachmentSource::Path(path.clone())],
                cx,
            );
            assert!(cowork.draft_is_loading_attachments(draft_id, cx));
            cowork.active_thread_id = None;
        });
        cx.run_until_parked();
        std::fs::remove_file(&path).expect("remove test attachment");

        let attachments = thread_draft_attachments(&cowork, thread_id, cx);
        assert!(matches!(
            attachments.as_slice(),
            [FileAttachment { content: FileAttachmentContent::Text(text), .. }] if text == "# Notes"
        ));
        cowork.read_with(cx, |cowork, _| {
            assert!(draft_attachments(&cowork.new_thread_draft).is_empty());
            assert!(cowork.pending_attachments.is_empty());
        });
    }

    #[gpui::test]
    fn pasting_an_image_attaches_it_to_the_draft(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
        let bmp = encoded_image(3, image::ImageFormat::Bmp);
        cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
            gpui::ImageFormat::Bmp,
            bmp,
        )));

        cowork.update_in(cx, |cowork, window, cx| {
            // Pasting attaches to the block, or here the draft position, being
            // typed in.
            cowork.focus_composer(window, cx);
            cowork.paste_attachments(&Paste, window, cx);
        });
        cx.run_until_parked();

        let attachments = thread_draft_attachments(&cowork, thread_id, cx);
        assert!(matches!(
            attachments.as_slice(),
            [FileAttachment { name, content: FileAttachmentContent::Png(_) }] if name == "Pasted image.png"
        ));
    }

    #[test]
    fn copied_endpoint_id_is_accepted_by_join_input() {
        let endpoint_id = iroh::SecretKey::from_bytes(&[42; 32]).public();
        let copied_text = endpoint_id.to_string();

        assert!(endpoint_id_input_is_complete(&copied_text));
        assert_eq!(copied_text.parse::<EndpointId>().unwrap(), endpoint_id);
    }

    #[test]
    fn annotated_markdown_uses_native_markdown_link() {
        let annotated = annotate_markdown("Before selected text after", [7..20]);

        assert_eq!(annotated, "Before [selected text](#inline-comment) after");
    }

    #[test]
    fn annotated_markdown_merges_intersecting_comments() {
        let annotated = annotate_markdown("overlapping", [0..7, 4..11]);

        assert_eq!(annotated, "[overlapping](#inline-comment)");
    }

    #[test]
    fn annotated_markdown_preserves_heading_around_partial_selection() {
        let annotated = annotate_markdown("### A Heading", [6..13]);

        assert_eq!(annotated, "### A [Heading](#inline-comment)");
    }

    #[test]
    fn annotated_markdown_preserves_bold_around_partial_selection() {
        let annotated = annotate_markdown("**Hi**", [3..4]);

        assert_eq!(annotated, "**H[i](#inline-comment)**");
    }

    #[test]
    fn comments_follow_gpui_source_ranges_across_markdown_boundaries() {
        fn through(markdown: &str, start: &str, end: &str) -> Range<usize> {
            let start_offset = markdown.find(start).expect("selection start");
            let end_offset = markdown[start_offset..]
                .find(end)
                .map(|offset| start_offset + offset + end.len())
                .expect("selection end");
            start_offset..end_offset
        }

        let cases = [
            (
                "inside bold",
                "Before **bold text** after",
                through("Before **bold text** after", "old", "old"),
                "Before **b[old](#inline-comment) text** after",
            ),
            (
                "across opening bold edge",
                "Before **bold text** after",
                through("Before **bold text** after", "re ", "bold"),
                "Befo[re ](#inline-comment)**[bold](#inline-comment) text** after",
            ),
            (
                "across closing bold edge",
                "Before **bold text** after",
                through("Before **bold text** after", "text", " af"),
                "Before **bold [text](#inline-comment)**[ af](#inline-comment)ter",
            ),
            (
                "whole bold section",
                "Before **bold text** after",
                through("Before **bold text** after", "bold", "text"),
                "Before **[bold text](#inline-comment)** after",
            ),
            (
                "across several styled sections",
                "A **bold** and *italic* tail",
                through("A **bold** and *italic* tail", "bold", " ta"),
                "A **[bold](#inline-comment)**[ and ](#inline-comment)*[italic](#inline-comment)*[ ta](#inline-comment)il",
            ),
            (
                "nested styles",
                "Start **bold and *italic*** end",
                through("Start **bold and *italic*** end", "and ", "italic"),
                "Start **bold [and ](#inline-comment)*[italic](#inline-comment)*** end",
            ),
            (
                "whole inline code",
                "Use `value` now",
                5..10,
                "Use [`value`](#inline-comment) now",
            ),
            (
                "partial inline code is atomic",
                "Use `value` now",
                6..9,
                "Use [`value`](#inline-comment) now",
            ),
            (
                "heading and emphasis",
                "### A **styled heading** here",
                through("### A **styled heading** here", "A ", "** h"),
                "### [A ](#inline-comment)**[styled heading](#inline-comment)**[ h](#inline-comment)ere",
            ),
        ];

        for (name, markdown, source_range, expected) in cases {
            let ranges = Cowork::annotation_ranges(markdown, source_range);
            let annotated = annotate_markdown(markdown, ranges);

            assert_eq!(annotated, expected, "{name}");
            let html = markdown::to_html_with_options(&annotated, &markdown::Options::gfm())
                .expect("annotated Markdown should compile");
            assert!(
                html.contains("href=\"#inline-comment\""),
                "{name}: annotation should survive Markdown rendering: {html}"
            );
        }
    }

    #[test]
    fn can_comment_on_selection_across_inline_code() {
        let markdown = "In Rust, we use `u128` to handle larger numbers";
        let ranges = Cowork::annotation_ranges(markdown, 0..markdown.len());
        let annotated = annotate_markdown(markdown, ranges);
        let html = markdown::to_html_with_options(&annotated, &markdown::Options::gfm())
            .expect("annotated Markdown should compile");

        assert_eq!(
            annotated,
            "[In Rust, we use `u128` to handle larger numbers](#inline-comment)"
        );
        assert!(html.contains(
            "<a href=\"#inline-comment\">In Rust, we use <code>u128</code> to handle larger numbers</a>"
        ));
    }

    #[test]
    fn comments_use_gpui_range_to_target_identical_styled_text() {
        let markdown = "**same** then **same**";
        let ranges = Cowork::annotation_ranges(markdown, 16..20);
        let annotated = annotate_markdown(markdown, ranges);

        assert_eq!(annotated, "**same** then **[same](#inline-comment)**");
    }

    fn assert_backslash_selection_creates_comment(
        cx: &mut gpui::TestAppContext,
        markdown: &'static str,
        expected_quote: &str,
        expected_range: Range<usize>,
        target_comment_reply: bool,
        existing_comment_range: Option<Range<usize>>,
        selection_start_x: f32,
        selection_end_x: f32,
        expected_highlights_after_comment: Option<usize>,
    ) {
        struct SelectionRoot {
            cowork: Entity<Cowork>,
            text_view: Entity<TextViewState>,
            composer: Entity<TextareaState>,
            thread_id: Uuid,
            message_id: Uuid,
            markdown: &'static str,
            comments: Vec<UserComment>,
        }

        impl Render for SelectionRoot {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let content = self.cowork.update(cx, |cowork, cx| {
                    cowork.render_agent_text(
                        ThreadMessageId {
                            thread_id: self.thread_id,
                            message_id: self.message_id,
                        },
                        self.markdown,
                        &self.text_view,
                        &self.comments,
                        px(160.),
                        window,
                        cx,
                    )
                });
                div()
                    .w(px(160.))
                    .flex()
                    .flex_col()
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        this.cowork.update(cx, |cowork, cx| {
                            cowork.begin_inline_comment(event, window, cx);
                        });
                    }))
                    .child(TextSelectionLayer)
                    .child(
                        div()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.cowork.update(cx, |cowork, _| {
                                        cowork.selection_message_id = Some(this.message_id);
                                    });
                                }),
                            )
                            .children(content),
                    )
                    .child(Textarea::new(&self.composer))
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let tokio_handle = runtime.handle().clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            let message_id = Uuid::new_v4();
            let text_view = cx.new(|cx| TextViewState::markdown(markdown, cx));
            let main_text = if target_comment_reply { "" } else { markdown };
            let main_text_view = if target_comment_reply {
                cx.new(|cx| TextViewState::markdown("", cx))
            } else {
                text_view.clone()
            };
            let thinking_view = cx.new(|cx| TextViewState::markdown("", cx));
            let thread_id = Uuid::new_v4();
            let draft = ThreadDraft::new(ParticipantId::new());
            if let Some(range) = existing_comment_range.clone() {
                draft.doc.create_comment(
                    draft.author.as_uuid(),
                    CommentTarget {
                        message_id,
                        quote: markdown[range.clone()].into(),
                        range,
                    },
                    "Existing comment",
                );
            }
            let comments = draft.comment_views(&[]);
            // Stands in for the composer, which has focus before commenting.
            let composer = Cowork::new_draft_editor("", window, cx);
            let timeline = vec![TimelineMessage::Agent(AgentMessage {
                id: if target_comment_reply {
                    Uuid::new_v4()
                } else {
                    message_id
                },
                comment_group_id: None,
                comment_responses: target_comment_reply
                    .then(|| AgentCommentResponse {
                        id: message_id,
                        comment_id: Uuid::new_v4(),
                        response: markdown.into(),
                        response_view: text_view.clone(),
                    })
                    .into_iter()
                    .collect(),
                thinking: String::new(),
                thinking_view,
                thinking_complete: true,
                thinking_expanded: false,
                text: main_text.into(),
                text_view: main_text_view,
                complete: true,
                failed: false,
            })];
            let thread = cx.new(|_| test_thread(thread_id, timeline, draft));
            let thread_store = cx.new(|_| ThreadStore {
                threads: VecDeque::from([thread]),
            });
            let cowork =
                cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
            composer.focus_handle(cx).focus(window, cx);
            SelectionRoot {
                cowork,
                text_view,
                composer,
                thread_id,
                message_id,
                markdown,
                comments,
            }
        });
        let cx: &mut gpui::VisualTestContext = cx;
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(selection_start_x), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(
            point(px(selection_end_x), px(8.)),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(
            point(px(selection_end_x), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let annotated_state_before = expected_highlights_after_comment.map(|_| {
            view.read_with(cx, |view, cx| {
                view.cowork
                    .read(cx)
                    .segment_text_views
                    .iter()
                    .find(|((segment, _), _)| segment.message_id == view.message_id)
                    .expect("annotated segment")
                    .1
                    .state
                    .entity_id()
            })
        });
        cx.simulate_keystrokes("x");

        view.read_with(cx, |view, cx| {
            let cowork = view.cowork.read(cx);
            let thread = cowork
                .thread_store
                .read(cx)
                .thread(cowork.active_thread_id.expect("active thread"), cx)
                .expect("thread");
            let thread = thread.read(cx);
            // New comments are appended after any existing one.
            let Some(comment) = thread
                .draft
                .comment_views(&[])
                .into_iter()
                .last()
                .filter(|comment| matches!(comment.body, UserCommentBody::Editing { .. }))
            else {
                panic!("typing with the selection should create an editable comment");
            };
            assert_eq!(comment.reference.quote, expected_quote);
            assert_eq!(comment.reference.range, expected_range);
            assert_eq!(comment.author, thread.draft.author);
            let UserCommentBody::Editing { inline, composer } = &comment.body else {
                unreachable!();
            };
            assert_eq!(inline.read(cx).value(), "x");
            assert_eq!(composer.read(cx).value(), "x");
        });
        cx.update(|window, cx| {
            let cowork = view.read(cx).cowork.read(cx);
            let (_, slot, _) = cowork
                .focused_draft_editor(window, cx)
                .expect("a comment editor should have focus");
            assert!(matches!(slot, EditorSlot::CommentInline(_)));
        });

        if let Some(expected_highlights) = expected_highlights_after_comment {
            let comments = view.read_with(cx, |view, cx| {
                let cowork = view.cowork.read(cx);
                cowork
                    .thread_store
                    .read(cx)
                    .thread(cowork.active_thread_id.expect("active thread"), cx)
                    .expect("thread")
                    .read(cx)
                    .draft
                    .comment_views(&[])
            });
            view.update(cx, |view, cx| {
                view.comments = comments;
                cx.notify();
            });
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            view.read_with(cx, |view, cx| {
                let highlight_count = view
                    .cowork
                    .read(cx)
                    .segment_text_views
                    .iter()
                    .filter(|((segment, _), _)| segment.message_id == view.message_id)
                    .map(|(_, segment)| segment.text.match_indices("#inline-comment").count())
                    .sum::<usize>();
                assert_eq!(highlight_count, expected_highlights);
                let annotated_state_after = view
                    .cowork
                    .read(cx)
                    .segment_text_views
                    .iter()
                    .find(|((segment, _), _)| segment.message_id == view.message_id)
                    .expect("annotated segment")
                    .1
                    .state
                    .entity_id();
                assert_ne!(Some(annotated_state_after), annotated_state_before);
            });
        }
    }

    #[gpui::test]
    fn backslash_selections_create_comments_with_gpui_ranges(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            r"a\b",
            r"a\b",
            0..3,
            false,
            None,
            1.,
            155.,
            None,
        );
        assert_backslash_selection_creates_comment(
            cx,
            r"a\\b",
            r"a\b",
            0..4,
            false,
            None,
            1.,
            155.,
            None,
        );
    }

    #[gpui::test]
    fn comments_can_target_agent_comment_replies(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "Agent reply",
            "Agent reply",
            0..11,
            true,
            None,
            1.,
            155.,
            None,
        );
    }

    #[gpui::test]
    fn comments_can_target_text_before_an_existing_comment(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "alpha beta gamma",
            "alpha",
            0..5,
            false,
            Some(11..16),
            1.,
            48.,
            None,
        );
    }

    #[gpui::test]
    fn creating_comment_immediately_before_existing_preserves_both_highlights(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "alpha beta gamma",
            "beta",
            6..10,
            false,
            Some(11..16),
            54.,
            96.,
            Some(2),
        );
    }

    #[gpui::test]
    fn comments_after_an_existing_comment_keep_original_source_offsets(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        assert_backslash_selection_creates_comment(
            cx,
            "alpha beta gamma",
            "gamma",
            11..16,
            false,
            Some(0..5),
            104.,
            155.,
            Some(2),
        );
    }

    #[test]
    fn annotation_segment_excludes_preceding_code_block() {
        let markdown = "Before\n\n```rust\nfn main() {}\n```\n\nParagraph with selected text";
        let selection_start = markdown.find("selected").unwrap();
        let annotation_start = Cowork::hard_line_start(markdown, selection_start);

        assert_eq!(
            &markdown[..annotation_start],
            "Before\n\n```rust\nfn main() {}\n```\n\n"
        );
        assert_eq!(
            &markdown[annotation_start..],
            "Paragraph with selected text"
        );
    }

    #[test]
    fn highlights_fenced_rust_code() {
        let code = "fn main() { println!(\"hello\"); }\n";
        let block = CodeBlock::from_code(code, Some("rust"));
        let highlights = highlight_code_block(&block);

        assert!(!highlights.is_empty());
        assert!(
            highlights
                .iter()
                .all(|(range, _)| range.start < range.end && range.end <= code.len())
        );
    }

    #[test]
    fn collaborator_threads_are_writable_and_removed_on_disconnect() {
        assert!(ThreadOwnership::Remote.can_write());
        assert!(ThreadOwnership::Remote.remove_on_disconnect());
        assert!(ThreadOwnership::Local.can_write());
        assert!(!ThreadOwnership::Local.remove_on_disconnect());
    }

    #[test]
    fn thread_titles_normalize_whitespace_and_truncate_by_character() {
        assert_eq!(
            Cowork::thread_title("  Collaborate\non\tthis prompt  "),
            "Collaborate on this prompt"
        );
        assert_eq!(
            Cowork::thread_title("12345678901234567890123456789012"),
            "12345678901234567890123456789012"
        );
        assert_eq!(
            Cowork::thread_title("12345678901234567890123456789012 more"),
            "12345678901234567890123456789012…"
        );
        assert_eq!(
            Cowork::thread_title("🦀".repeat(33).as_str()),
            format!("{}…", "🦀".repeat(32))
        );
    }

    #[test]
    fn first_message_titles_an_empty_pre_shared_thread() {
        assert_eq!(
            Cowork::title_for_first_message(&[], "  Collaborate on this prompt  "),
            Some("Collaborate on this prompt".into())
        );

        let existing_timeline = vec![TimelineMessage::User(UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            blocks: vec![PromptBlock {
                id: Uuid::new_v4(),
                author: ParticipantId::new(),
                text: "Existing message".into(),
                attachments: Vec::new(),
            }],
            history_preface: None,
            comments_folded: false,
        })];
        assert_eq!(
            Cowork::title_for_first_message(&existing_timeline, "Later message"),
            None
        );
    }

    struct EmptyThreadTestView {
        thread: Entity<Thread>,
        draft_id: Uuid,
    }

    impl Render for EmptyThreadTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn sharing_before_first_message_materializes_an_empty_owned_thread(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_, cx| {
            let draft = ThreadDraft::new(ParticipantId::new());
            let draft_id = draft.id;
            let thread =
                Cowork::new_empty_local_thread(draft, ParticipantId::new(), OLLAMA_QWEN, cx);
            EmptyThreadTestView { thread, draft_id }
        });

        view.read_with(cx, |view, cx| {
            let thread = view.thread.read(cx);
            assert!(thread.timeline.is_empty());
            assert_eq!(thread.draft.id, view.draft_id);
            assert_eq!(thread.summary.title, "New thread");
            assert_eq!(thread.ownership, ThreadOwnership::Local);
            assert_eq!(thread.model, OLLAMA_QWEN);
            assert!(thread.participants.is_empty());
            assert!(matches!(thread.sharing, ThreadSharing::NotShared));
        });
    }

    struct ThreadMirrorTestView {
        host: Entity<Thread>,
        collaborator: Option<Entity<Thread>>,
    }

    impl Render for ThreadMirrorTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The events a host broadcasts while answering one prompt.
    fn agent_stream_events(message_id: Uuid) -> Vec<protocol::HostMessage> {
        let id = message_id.into_bytes();
        let user_message_id = Uuid::new_v4().into_bytes();
        let comment_id = Uuid::new_v4().into_bytes();
        vec![
            protocol::HostMessage::ThreadTitled("Explain this".into()),
            protocol::HostMessage::UserMessage(protocol::UserMessage {
                id: user_message_id,
                blocks: vec![protocol::PromptBlock {
                    id: Uuid::new_v4().into_bytes(),
                    author: ParticipantId::new().into_bytes(),
                    text: "Explain this".into(),
                    attachments: Vec::new(),
                }],
                comments: vec![protocol::UserComment {
                    id: comment_id,
                    author: ParticipantId::new().into_bytes(),
                    reference: protocol::CommentReference {
                        message_id: Uuid::new_v4().into_bytes(),
                        range: 0..10,
                        quote: "an excerpt".into(),
                    },
                    body: "why?".into(),
                }],
            }),
            protocol::HostMessage::AgentStarted {
                id,
                comment_group_id: Some(user_message_id),
            },
            protocol::HostMessage::AgentCommentResponded {
                id,
                response_id: Uuid::new_v4().into_bytes(),
                comment_id,
                response: "Because of this.".into(),
            },
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Thinking,
                text: "Weighing ".into(),
            },
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Thinking,
                text: "options.".into(),
            },
            // No explicit thinking end, so the first response token closes it.
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Response,
                text: "Here is ".into(),
            },
            protocol::HostMessage::AgentTextAppended {
                id,
                target: protocol::AgentText::Response,
                text: "the answer.".into(),
            },
            protocol::HostMessage::AgentEnded { id, failure: None },
            protocol::HostMessage::ParticipantJoined(ParticipantId::new().into_bytes()),
            protocol::HostMessage::ModelSelected {
                catalog_id: OLLAMA_QWEN.catalog_id.into(),
            },
        ]
    }

    /// A collaborator that joins midway through a generation has to end up with
    /// the host's timeline: its snapshot covers what it missed, and the events
    /// it replays afterwards cover the rest.
    #[gpui::test]
    fn collaborators_joining_mid_stream_converge_on_the_host_timeline(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let message_id = Uuid::new_v4();
        let events = agent_stream_events(message_id);
        // The collaborator joins once the agent has started reasoning.
        let joined_after = 4;

        let host_participant = ParticipantId::new();
        let collaborator_participant = ParticipantId::new();
        let (view, cx) = cx.add_window_view(|_, cx| ThreadMirrorTestView {
            host: Cowork::new_empty_local_thread(
                ThreadDraft::new(ParticipantId::new()),
                host_participant,
                DEFAULT_MODEL,
                cx,
            ),
            collaborator: None,
        });

        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.host.update(cx, |thread, cx| {
                    thread.participants = vec![thread.participant_id];
                    thread.apply(
                        protocol::HostMessage::ParticipantJoined(
                            collaborator_participant.into_bytes(),
                        ),
                        cx,
                    );
                });
                for event in events.iter().take(joined_after) {
                    view.host
                        .update(cx, |thread, cx| thread.apply(event.clone(), cx));
                }

                let welcome = protocol::Welcome {
                    participant_id: collaborator_participant.into_bytes(),
                    thread: view.host.read(cx).to_protocol(),
                    draft: view.host.read(cx).draft.doc.encode_state(),
                    presence: Vec::new(),
                    stored_attachments: Vec::new(),
                };
                let draft = ThreadDraft::new(ParticipantId::new());
                let collaborator =
                    cx.new(|cx| Thread::from_welcome(welcome, draft, ThreadSharing::NotShared, cx));

                for event in events.iter().skip(joined_after) {
                    view.host
                        .update(cx, |thread, cx| thread.apply(event.clone(), cx));
                    collaborator.update(cx, |thread, cx| thread.apply(event.clone(), cx));
                }
                view.collaborator = Some(collaborator);
            });
        });

        view.read_with(cx, |view, cx| {
            let host = view.host.read(cx);
            let collaborator = view
                .collaborator
                .as_ref()
                .expect("collaborator should have joined")
                .read(cx);

            assert_eq!(collaborator.to_protocol(), host.to_protocol());
            assert_eq!(collaborator.summary.id, host.summary.id);
            assert_ne!(collaborator.instance_id, host.instance_id);
            assert_eq!(collaborator.participant_id, collaborator_participant);
            assert_eq!(collaborator.draft.author, collaborator_participant);
            assert_eq!(collaborator.participants, host.participants);
            assert_eq!(collaborator.participants.len(), 3);
            assert_eq!(
                collaborator.participants[..2],
                [host_participant, collaborator_participant]
            );
            assert_eq!(collaborator.model, OLLAMA_QWEN);
            assert!(!host.generating);
            assert!(!collaborator.generating);

            let TimelineMessage::Agent(message) = &collaborator.timeline[1] else {
                panic!("expected the agent's reply");
            };
            assert_eq!(message.thinking, "Weighing options.");
            assert_eq!(message.text, "Here is the answer.");
            assert!(message.thinking_complete);
            assert!(message.complete);
            assert!(!message.failed);
        });
    }

    #[gpui::test]
    fn membership_events_are_idempotent_and_unknown_models_are_ignored(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_, cx| {
            let draft = ThreadDraft::new(ParticipantId::new());
            EmptyThreadTestView {
                thread: Cowork::new_empty_local_thread(
                    draft,
                    ParticipantId::new(),
                    DEFAULT_MODEL,
                    cx,
                ),
                draft_id: Uuid::nil(),
            }
        });
        let first = ParticipantId::new();
        let second = ParticipantId::new();

        cx.update(|_, cx| {
            let thread = view.read(cx).thread.clone();
            thread.update(cx, |thread, cx| {
                for event in [
                    protocol::HostMessage::ParticipantJoined(first.into_bytes()),
                    protocol::HostMessage::ParticipantJoined(second.into_bytes()),
                    protocol::HostMessage::ParticipantJoined(first.into_bytes()),
                    protocol::HostMessage::ModelSelected {
                        catalog_id: "no-such-model".into(),
                    },
                ] {
                    thread.apply(event, cx);
                }
                assert_eq!(thread.participants, [first, second]);
                assert_eq!(thread.model, DEFAULT_MODEL);

                thread.apply(
                    protocol::HostMessage::ParticipantLeft(first.into_bytes()),
                    cx,
                );
                thread.apply(
                    protocol::HostMessage::ParticipantLeft(first.into_bytes()),
                    cx,
                );
                assert_eq!(thread.participants, [second]);
            });
        });
    }

    #[gpui::test]
    fn picked_models_apply_to_the_active_thread_and_new_threads(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());

        cowork.update_in(cx, |cowork, window, cx| {
            let thread = cowork.active_thread(cx).expect("active thread");
            assert_eq!(thread.read(cx).model, DEFAULT_MODEL);

            // Picking a model changes the active thread and later new threads.
            cowork.select_model(OLLAMA_QWEN, cx);
            assert_eq!(thread.read(cx).model, OLLAMA_QWEN);
            assert_eq!(cowork.new_thread_model, OLLAMA_QWEN);

            // A change made by someone else only moves the picker along.
            thread.update(cx, |thread, cx| {
                thread.apply(
                    protocol::HostMessage::ModelSelected {
                        catalog_id: RECOMMENDED_QWEN.catalog_id.into(),
                    },
                    cx,
                );
            });
            cowork.sync_model_picker(window, cx);
            assert_eq!(
                cowork.model_picker.read(cx).selected_value(),
                Some(RECOMMENDED_QWEN)
            );
            assert_eq!(cowork.new_thread_model, OLLAMA_QWEN);

            // Without an active thread the picker shows the new thread model.
            cowork.active_thread_id = None;
            cowork.sync_model_picker(window, cx);
            assert_eq!(
                cowork.model_picker.read(cx).selected_value(),
                Some(OLLAMA_QWEN)
            );
            cowork.active_thread_id = Some(thread_id);
        });
    }

    /// `sync_model_picker` runs on every render, so a catalog model the picker
    /// cannot select would make it re-select and redraw forever.
    #[gpui::test]
    fn picker_can_select_every_catalog_model(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());

        cowork.update_in(cx, |cowork, window, cx| {
            for model in MODEL_CATALOG {
                cowork.model_picker.update(cx, |picker, cx| {
                    picker.set_selected_values(&[model], window, cx);
                });
                assert_eq!(cowork.model_picker.read(cx).selected_value(), Some(model));
                assert_eq!(
                    ModelSelection::from_catalog_id(model.catalog_id),
                    Some(model)
                );
            }
        });
    }

    #[gpui::test]
    fn running_agent_message_is_the_incomplete_one(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|_, cx| {
            let draft = ThreadDraft::new(ParticipantId::new());
            EmptyThreadTestView {
                thread: Cowork::new_empty_local_thread(
                    draft,
                    ParticipantId::new(),
                    DEFAULT_MODEL,
                    cx,
                ),
                draft_id: Uuid::nil(),
            }
        });
        let finished = Uuid::new_v4();
        let running = Uuid::new_v4();

        cx.update(|_, cx| {
            let thread = view.read(cx).thread.clone();
            thread.update(cx, |thread, cx| {
                assert_eq!(thread.running_agent_message_id(), None);
                for event in [
                    protocol::HostMessage::AgentStarted {
                        id: finished.into_bytes(),
                        comment_group_id: None,
                    },
                    protocol::HostMessage::AgentEnded {
                        id: finished.into_bytes(),
                        failure: None,
                    },
                    protocol::HostMessage::AgentStarted {
                        id: running.into_bytes(),
                        comment_group_id: None,
                    },
                ] {
                    thread.apply(event, cx);
                }
                assert_eq!(thread.running_agent_message_id(), Some(running));

                thread.apply(
                    protocol::HostMessage::AgentEnded {
                        id: running.into_bytes(),
                        failure: None,
                    },
                    cx,
                );
                assert_eq!(thread.running_agent_message_id(), None);
            });
        });
    }

    #[gpui::test]
    fn collaborator_requests_select_models_and_stop_only_the_running_generation(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let (cowork, thread_id, cx) = attachment_test_cowork(cx, runtime.handle().clone());
        let running_message_id = Uuid::new_v4();
        let task = runtime.spawn(std::future::pending::<()>());
        let cancelled = Arc::new(AtomicBool::new(false));

        cowork.update(cx, |cowork, cx| {
            let thread = cowork.active_thread(cx).expect("active thread");
            cowork.active_generations.insert(
                thread_id,
                ActiveGeneration {
                    message_id: running_message_id,
                    abort_handle: task.abort_handle(),
                    cancelled: cancelled.clone(),
                },
            );

            for request in [
                protocol::CollaboratorMessage::SelectModel {
                    catalog_id: "no-such-model".into(),
                },
                protocol::CollaboratorMessage::Stop {
                    message_id: Uuid::new_v4().into_bytes(),
                },
            ] {
                cowork
                    .collaborator_request(&thread, ParticipantId::new(), request, cx)
                    .expect("valid request");
            }
            assert_eq!(thread.read(cx).model, DEFAULT_MODEL);
            assert!(!cancelled.load(Ordering::Acquire));

            for request in [
                protocol::CollaboratorMessage::SelectModel {
                    catalog_id: OLLAMA_QWEN.catalog_id.into(),
                },
                protocol::CollaboratorMessage::Stop {
                    message_id: running_message_id.into_bytes(),
                },
            ] {
                cowork
                    .collaborator_request(&thread, ParticipantId::new(), request, cx)
                    .expect("valid request");
            }
            assert_eq!(thread.read(cx).model, OLLAMA_QWEN);
            assert!(cancelled.load(Ordering::Acquire));
            // Only the requesting peer picked it; new local threads keep the
            // local user's choice.
            assert_eq!(cowork.new_thread_model, DEFAULT_MODEL);
        });
        let aborted = runtime
            .block_on(task)
            .expect_err("generation task was aborted");
        assert!(aborted.is_cancelled());
    }

    struct ComposerTestView {
        composer: Entity<TextareaState>,
    }

    impl Render for ComposerTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .items_start()
                .child(Cowork::render_composer_input(&self.composer))
        }
    }

    struct MouseDragTestView {
        editor: Entity<TextareaState>,
    }

    impl Render for MouseDragTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(Textarea::new(&self.editor))
        }
    }

    #[gpui::test]
    fn composer_grows_beyond_four_lines(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let composer = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, usize::MAX));
            ComposerTestView { composer }
        });
        let composer = view.read_with(cx, |view, _| view.composer.clone());

        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_value("1\n2\n3\n4\n5\n6\n7\n8\n9\n10", window, cx);
            });
        });
        cx.run_until_parked();

        let composer_bounds = cx
            .debug_bounds("composer")
            .expect("composer should be rendered");
        assert!(
            composer_bounds.size.height >= px(200.),
            "ten text lines should expand the composer, got {composer_bounds:?}"
        );
    }

    /// A full `Cowork` window showing the draft of a new thread.
    fn composer_test_cowork(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<Cowork>,
        tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(gpui_component::init);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let tokio_handle = runtime.handle().clone();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let thread_store = cx.new(|_| ThreadStore::default());
            let cowork = cx.new(|cx| test_cowork(thread_store, None, tokio_handle, window, cx));
            Root::new(cowork, window, cx)
        });
        let cowork = root.read_with(cx, |root, _| {
            root.view()
                .clone()
                .downcast::<Cowork>()
                .expect("root shows cowork")
        });
        // Focus and blur events are only delivered to an active window.
        cx.update(|window, _| window.activate_window());
        cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx)));
        cx.run_until_parked();
        (cowork, runtime, cx)
    }

    #[gpui::test]
    fn sidebar_bottom_bar_lines_up_with_the_main_bottom_bar(cx: &mut gpui::TestAppContext) {
        let (_cowork, _runtime, cx) = composer_test_cowork(cx);

        let sidebar_bar = cx
            .debug_bounds("sidebar-bottom-bar")
            .expect("sidebar bottom bar should be rendered");
        let main_bar = cx
            .debug_bounds("bottom-bar")
            .expect("main bottom bar should be rendered");

        assert_eq!(sidebar_bar.origin.y, main_bar.origin.y);
        assert_eq!(sidebar_bar.size.height, main_bar.size.height);
        assert_eq!(sidebar_bar.origin.x, px(0.));
        assert_eq!(sidebar_bar.size.width, SIDEBAR_WIDTH);

        let button = cx
            .debug_bounds("identity-button")
            .expect("identity button should be rendered");
        let content = cx
            .debug_bounds("identity-button-content")
            .expect("identity button content should be rendered");
        assert_eq!(button.left(), sidebar_bar.left() + px(4.));
        assert_eq!(button.right(), sidebar_bar.right() - px(4.));
        assert_eq!(content.left(), button.left() + px(6.));
    }

    fn new_thread_items(
        cowork: &Entity<Cowork>,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<DraftItem> {
        cowork.read_with(cx, |cowork, _| cowork.new_thread_draft.doc.items())
    }

    fn prompt_bodies(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        new_thread_items(cowork, cx)
            .into_iter()
            .filter(|item| item.is_prompt())
            .map(|item| item.body)
            .collect()
    }

    fn focused_slot(
        cowork: &Entity<Cowork>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<EditorSlot> {
        cx.update(|window, cx| {
            cowork
                .read(cx)
                .focused_draft_editor(window, cx)
                .map(|(_, slot, _)| slot)
        })
    }

    #[gpui::test]
    fn typing_at_the_draft_position_turns_it_into_a_block(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));
        let draft_position = cowork.read_with(cx, |cowork, _| {
            cowork
                .new_thread_draft
                .draft_position
                .clone()
                .expect("draft position editor")
        });

        cx.simulate_input("hi");
        cx.run_until_parked();

        assert_eq!(prompt_bodies(&cowork, cx), ["hi"]);
        let Some(EditorSlot::Prompt(id)) = focused_slot(&cowork, cx) else {
            panic!("the new block should keep focus");
        };
        cowork.read_with(cx, |cowork, cx| {
            let draft = &cowork.new_thread_draft;
            // The same editor carries on, so typing is uninterrupted.
            let block_editor = draft.editor(EditorSlot::Prompt(id)).expect("block editor");
            assert_eq!(block_editor.entity_id(), draft_position.entity_id());
            assert_eq!(block_editor.read(cx).value(), "hi");
            let item = draft.doc.item(id).expect("block");
            assert_eq!(item.creator, draft.author.as_uuid());
            assert_ne!(
                draft.draft_position.as_ref().map(Entity::entity_id),
                Some(draft_position.entity_id())
            );
        });
    }

    #[gpui::test]
    fn up_and_down_move_between_blocks_and_the_draft_position(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        cx.simulate_input("first");
        cx.run_until_parked();

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

        cx.simulate_input("second");
        cx.run_until_parked();
        assert_eq!(prompt_bodies(&cowork, cx), ["first", "second"]);
        let items = new_thread_items(&cowork, cx);
        assert_eq!(
            focused_slot(&cowork, cx),
            Some(EditorSlot::Prompt(items[1].id))
        );

        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(
            focused_slot(&cowork, cx),
            Some(EditorSlot::Prompt(items[0].id))
        );
        // Up from the first line of the first editor stays put.
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(
            focused_slot(&cowork, cx),
            Some(EditorSlot::Prompt(items[0].id))
        );
    }

    #[gpui::test]
    fn up_stays_inside_a_block_until_its_first_line(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        cx.simulate_input("one");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("two");
        cx.run_until_parked();
        let first = new_thread_items(&cowork, cx)[0].id;
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::Prompt(first)));
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        // Moved to the first line of the block rather than out of it.
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::Prompt(first)));
        cowork.read_with(cx, |cowork, cx| {
            let editor = cowork
                .new_thread_draft
                .editor(EditorSlot::Prompt(first))
                .expect("block editor");
            assert!(editor.read(cx).cursor() <= "one".len());
        });
    }

    #[gpui::test]
    fn emptied_blocks_are_removed_by_escape_backspace_and_leaving(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        cx.simulate_input("a");
        cx.simulate_keystrokes("down");
        cx.simulate_input("b");
        cx.run_until_parked();
        assert_eq!(prompt_bodies(&cowork, cx), ["a", "b"]);

        // Emptying a block keeps it while the caret is in it; Escape removes
        // it and returns to the draft position.
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert_eq!(prompt_bodies(&cowork, cx), ["a", ""]);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(prompt_bodies(&cowork, cx), ["a"]);
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

        // Backspace in the empty draft position steps back into the block,
        // and once that is empty, Backspace removes it.
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        let first = new_thread_items(&cowork, cx)[0].id;
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::Prompt(first)));
        cx.simulate_keystrokes("backspace backspace");
        cx.run_until_parked();
        assert!(prompt_bodies(&cowork, cx).is_empty());
        assert_eq!(focused_slot(&cowork, cx), Some(EditorSlot::DraftPosition));

        // Leaving an emptied block removes it too.
        cx.simulate_input("c");
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert_eq!(prompt_bodies(&cowork, cx), [""]);
        cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| {
                let draft_id = cowork.new_thread_draft.id;
                cowork.focus_draft_editor(draft_id, EditorSlot::DraftPosition, None, window, cx);
            });
        });
        // Focus changes are dispatched when the next frame is drawn.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(prompt_bodies(&cowork, cx).is_empty());
    }

    #[gpui::test]
    fn both_editors_of_a_comment_show_the_same_text(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        let comment = cowork.update(cx, |cowork, _| {
            let draft = &cowork.new_thread_draft;
            draft.doc.create_comment(
                draft.author.as_uuid(),
                CommentTarget {
                    message_id: Uuid::new_v4(),
                    range: 0..5,
                    quote: "quote".into(),
                },
                "x",
            )
        });
        cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| {
                let draft_id = cowork.new_thread_draft.id;
                cowork.focus_draft_editor(
                    draft_id,
                    EditorSlot::CommentComposer(comment),
                    Some(1),
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        cx.simulate_input("yz");
        cx.run_until_parked();

        cowork.read_with(cx, |cowork, cx| {
            let draft = &cowork.new_thread_draft;
            assert_eq!(draft.doc.body(comment).as_deref(), Some("xyz"));
            let inline = draft
                .editor(EditorSlot::CommentInline(comment))
                .expect("inline editor");
            assert_eq!(inline.read(cx).value(), "xyz");
        });
    }

    #[gpui::test]
    fn submissions_take_non_empty_items_and_leave_empty_ones(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        let (empty_comment, submission) = cowork.update(cx, |cowork, _| {
            let draft = &mut cowork.new_thread_draft;
            let author = draft.author.as_uuid();
            let target = CommentTarget {
                message_id: Uuid::new_v4(),
                range: 0..5,
                quote: "quote".into(),
            };
            draft.doc.create_prompt(author, "first");
            let empty_comment = draft.doc.create_comment(author, target.clone(), "  ");
            draft.doc.create_comment(author, target, "why?");
            draft.doc.create_prompt(author, "");
            draft.doc.create_prompt(author, "second");
            (empty_comment, Cowork::take_submission(draft))
        });

        let (comments, blocks, _) = submission.expect("something to submit");
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(matches!(
            &comments[..],
            [UserComment { body: UserCommentBody::Submitted(body), .. }] if body.as_ref() == "why?"
        ));
        let remaining = new_thread_items(&cowork, cx);
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].id, empty_comment);
        assert!(remaining.iter().all(DraftItem::is_empty));

        let nothing_left = cowork.update(cx, |cowork, _| {
            Cowork::take_submission(&mut cowork.new_thread_draft)
        });
        assert!(nothing_left.is_none());
    }

    #[gpui::test]
    fn syncing_editors_leaves_an_ime_composition_alone(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        cx.simulate_input("abc");
        cx.run_until_parked();
        let id = new_thread_items(&cowork, cx)[0].id;
        let editor = cowork.read_with(cx, |cowork, _| {
            cowork
                .new_thread_draft
                .editor(EditorSlot::Prompt(id))
                .expect("block editor")
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(None, "ka", None, window, cx);
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();

        cx.update(|window, cx| {
            let marked = editor.update(cx, |editor, cx| editor.marked_text_range(window, cx));
            assert!(marked.is_some(), "the composition should still be active");
            assert_eq!(editor.read(cx).value(), "abcka");
        });
    }

    #[gpui::test]
    fn the_attach_button_targets_the_focused_block_only(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        let target = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| cowork.read(cx).attachment_target_at_focus(window, cx))
        };
        assert!(matches!(target(cx), AttachmentTarget::NewBlock(_)));

        cx.simulate_input("block");
        cx.run_until_parked();
        let block = new_thread_items(&cowork, cx)[0].id;
        assert_eq!(target(cx), AttachmentTarget::Block(block));

        let comment = cowork.update(cx, |cowork, _| {
            let draft = &cowork.new_thread_draft;
            draft.doc.create_comment(
                draft.author.as_uuid(),
                CommentTarget {
                    message_id: Uuid::new_v4(),
                    range: 0..5,
                    quote: "quote".into(),
                },
                "note",
            )
        });
        cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| {
                let draft_id = cowork.new_thread_draft.id;
                cowork.focus_draft_editor(
                    draft_id,
                    EditorSlot::CommentComposer(comment),
                    None,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        assert!(matches!(target(cx), AttachmentTarget::NewBlock(_)));
    }

    #[gpui::test]
    fn files_for_a_removed_block_land_in_a_new_one(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        cx.simulate_input("block");
        cx.run_until_parked();
        let block = new_thread_items(&cowork, cx)[0].id;
        let path = std::env::temp_dir().join(format!("cowork-{}.txt", Uuid::new_v4()));
        std::fs::write(&path, "notes").expect("write test attachment");

        cowork.update(cx, |cowork, cx| {
            let draft_id = cowork.new_thread_draft.id;
            cowork.add_attachments(
                draft_id,
                AttachmentTarget::Block(block),
                vec![AttachmentSource::Path(path.clone())],
                cx,
            );
            cowork.new_thread_draft.remove_items(&[block]);
        });
        cx.run_until_parked();
        std::fs::remove_file(&path).expect("remove test attachment");

        let items = new_thread_items(&cowork, cx);
        assert_eq!(items.len(), 1);
        assert_ne!(items[0].id, block);
        assert!(matches!(
            &items[0].kind,
            DraftItemKind::Prompt { attachments } if attachments.len() == 1
        ));
        cowork.read_with(cx, |cowork, _| {
            assert!(matches!(
                draft_attachments(&cowork.new_thread_draft).as_slice(),
                [FileAttachment { content: FileAttachmentContent::Text(text), .. }] if text == "notes"
            ));
        });
    }

    #[gpui::test]
    fn submitting_keeps_focus_on_an_item_that_stays(cx: &mut gpui::TestAppContext) {
        let (cowork, _runtime, cx) = composer_test_cowork(cx);
        cx.simulate_input("question");
        cx.run_until_parked();
        let comment = cowork.update(cx, |cowork, _| {
            let draft = &cowork.new_thread_draft;
            draft.doc.create_comment(
                draft.author.as_uuid(),
                CommentTarget {
                    message_id: Uuid::new_v4(),
                    range: 0..5,
                    quote: "quote".into(),
                },
                "",
            )
        });
        cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| {
                let draft_id = cowork.new_thread_draft.id;
                cowork.focus_draft_editor(
                    draft_id,
                    EditorSlot::CommentComposer(comment),
                    None,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| cowork.submit_composer(window, cx));
        });
        cx.run_until_parked();

        assert_eq!(
            focused_slot(&cowork, cx),
            Some(EditorSlot::CommentComposer(comment))
        );
        cowork.read_with(cx, |cowork, cx| {
            let thread = cowork
                .active_thread(cx)
                .expect("the submission started a thread");
            let thread = thread.read(cx);
            let remaining = thread.draft.doc.items();
            assert_eq!(remaining.len(), 1);
            assert_eq!(remaining[0].id, comment);
            let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
                panic!("expected the submitted message first");
            };
            assert_eq!(message.blocks.len(), 1);
            assert_eq!(message.blocks[0].text, "question");
            assert_eq!(thread.summary.title, "question");
        });
    }

    #[test]
    fn comment_instructions_name_each_comment_author() {
        let author = ParticipantId::from_bytes([7; 16]);
        let turn_comments = TurnComments::new(1);
        let preface = Cowork::comments_preface(
            &[UserComment {
                id: Uuid::new_v4(),
                author,
                presence: ItemPresence::default(),
                reference: CommentReference {
                    message_id: Uuid::new_v4(),
                    range: 0..5,
                    quote: "quote".into(),
                },
                body: UserCommentBody::Submitted(" why? ".into()),
            }],
            turn_comments.comment_ids(),
            &[],
        )
        .expect("comments need instructions");

        assert!(preface.contains("comment_1 — Mossy Crane, on an excerpt"));
        assert!(preface.contains("> quote\nComment: why?"));
        assert_eq!(Cowork::comments_preface(&[], &[], &[]), None);
    }

    /// A host and a collaborator side by side in one window, connected over
    /// an in-memory stream through the real protocol code.
    struct Collaboration<'a> {
        host: Entity<Cowork>,
        collaborator: Entity<Cowork>,
        host_thread: Entity<Thread>,
        cx: &'a mut gpui::VisualTestContext,
        _runtime: tokio::runtime::Runtime,
    }

    struct PairRoot {
        host: Entity<Cowork>,
        collaborator: Entity<Cowork>,
    }

    /// A connected host and collaborator end, relayed on the test's own
    /// executor, with every message encoded and decoded like on the wire.
    fn in_memory_peers(cx: &mut App) -> (HostPeer, ThreadHost) {
        fn relay<Message: serde::Serialize + serde::de::DeserializeOwned + 'static>(
            from: async_channel::Receiver<Message>,
            to: async_channel::Sender<Message>,
            cx: &mut App,
        ) {
            cx.spawn(async move |_| {
                while let Ok(message) = from.recv().await {
                    let bytes = postcard::to_stdvec(&message).expect("encode message");
                    let message = postcard::from_bytes(&bytes).expect("decode message");
                    if to.send(message).await.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        let (host_out, host_out_relay) = async_channel::unbounded();
        let (host_in_relay, host_in) = async_channel::unbounded();
        let (collaborator_out, collaborator_out_relay) = async_channel::unbounded();
        let (collaborator_in_relay, collaborator_in) = async_channel::unbounded();
        // Bulk messages get their own relay, so they may arrive before
        // control messages sent earlier, like on the wire.
        let (host_bulk, host_bulk_relay) = async_channel::bounded(2);
        let (collaborator_bulk, collaborator_bulk_relay) = async_channel::bounded(2);
        relay::<protocol::HostMessage>(host_out_relay, collaborator_in_relay.clone(), cx);
        relay::<protocol::HostMessage>(host_bulk_relay, collaborator_in_relay, cx);
        relay::<protocol::CollaboratorMessage>(collaborator_out_relay, host_in_relay.clone(), cx);
        relay::<protocol::CollaboratorMessage>(collaborator_bulk_relay, host_in_relay, cx);
        (
            protocol::Peer {
                outgoing: host_out,
                bulk: host_bulk,
                incoming: host_in,
            },
            protocol::Peer {
                outgoing: collaborator_out,
                bulk: collaborator_bulk,
                incoming: collaborator_in,
            },
        )
    }

    impl Render for PairRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .child(div().w_1_2().h_full().child(self.host.clone()))
                .child(div().w_1_2().h_full().child(self.collaborator.clone()))
        }
    }

    impl<'a> Collaboration<'a> {
        /// Starts with the host sharing a thread whose draft has one block,
        /// and the collaborator joined to it.
        fn start(cx: &'a mut gpui::TestAppContext) -> Self {
            cx.update(gpui_component::init);
            // Never driven, so nothing ever runs on it: the test scheduler
            // rejects wake-ups from other threads. The agent never answers.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            let endpoint = runtime
                .block_on(Endpoint::builder(presets::Minimal).bind())
                .expect("bind endpoint");
            let tokio_handle = runtime.handle().clone();
            let thread_id = Uuid::new_v4();
            let (root, cx) = cx.add_window_view(|window, cx| {
                let draft = ThreadDraft::new(ParticipantId::new());
                draft
                    .doc
                    .create_prompt(draft.author.as_uuid(), "from the host");
                let mut thread = test_thread(thread_id, Vec::new(), draft);
                thread.participant_id = thread.draft.author;
                thread.participants = vec![thread.participant_id];
                thread.sharing = ThreadSharing::Shared {
                    endpoint,
                    events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
                };
                let thread = cx.new(|_| thread);
                let host_store = cx.new(|_| ThreadStore {
                    threads: VecDeque::from([thread]),
                });
                let host = cx.new(|cx| {
                    test_cowork(
                        host_store,
                        Some(thread_id),
                        tokio_handle.clone(),
                        window,
                        cx,
                    )
                });
                let collaborator_store = cx.new(|_| ThreadStore::default());
                let collaborator = cx.new(|cx| {
                    test_cowork(collaborator_store, None, tokio_handle.clone(), window, cx)
                });
                let pair = cx.new(|_| PairRoot { host, collaborator });
                Root::new(pair, window, cx)
            });
            cx.update(|window, _| window.activate_window());
            let pair = root.read_with(cx, |root, _| {
                root.view()
                    .clone()
                    .downcast::<PairRoot>()
                    .expect("root shows the pair")
            });
            let (host, collaborator) =
                pair.read_with(cx, |pair, _| (pair.host.clone(), pair.collaborator.clone()));
            let host_thread =
                host.read_with(cx, |host, cx| host.active_thread(cx).expect("thread"));

            let (host_end, collaborator_end) = cx.update(|_, cx| in_memory_peers(cx));
            cx.update(|_, cx| {
                let host_weak = host.downgrade();
                let thread_weak = host_thread.downgrade();
                cx.spawn(async move |cx| {
                    if let Err(error) =
                        Cowork::serve_peer(host_weak, thread_weak, host_end, cx).await
                    {
                        eprintln!("stopped serving collaborator: {error:#}");
                    }
                })
                .detach();
                let collaborator = collaborator.clone();
                cx.spawn(async move |cx| {
                    collaborator_end
                        .send(protocol::CollaboratorMessage::Join {
                            protocol_version: protocol::PROTOCOL_VERSION,
                        })
                        .await
                        .expect("send join");
                    let Some(protocol::HostMessage::Welcome(welcome)) =
                        collaborator_end.receive().await
                    else {
                        panic!("expected a welcome");
                    };
                    collaborator.update(cx, |collaborator, cx| {
                        collaborator.mirror_thread(welcome, collaborator_end, None, cx);
                    });
                })
                .detach();
            });

            let mut collaboration = Self {
                host,
                collaborator,
                host_thread,
                cx,
                _runtime: runtime,
            };
            collaboration.wait_until("the collaborator joins", |this| {
                this.collaborator_thread().is_some()
            });
            collaboration
        }

        /// Pumps both sides, drawing frames so editors catch up, until `done`.
        fn wait_until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
            for _ in 0..20 {
                self.cx.run_until_parked();
                self.cx.update(|window, cx| window.draw(cx).clear(cx));
                self.cx.run_until_parked();
                if done(self) {
                    return;
                }
            }
            panic!("gave up waiting until {what}");
        }

        /// Lets everything in flight settle.
        fn settle(&mut self) {
            for _ in 0..5 {
                self.cx.run_until_parked();
                self.cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            self.cx.run_until_parked();
        }

        fn collaborator_thread(&mut self) -> Option<Entity<Thread>> {
            self.collaborator
                .read_with(self.cx, |collaborator, cx| collaborator.active_thread(cx))
        }

        fn items(&mut self, thread: &Entity<Thread>) -> Vec<DraftItem> {
            thread.read_with(self.cx, |thread, _| thread.draft.doc.items())
        }

        fn bodies(&mut self, thread: &Entity<Thread>) -> Vec<String> {
            self.items(thread)
                .into_iter()
                .map(|item| item.body)
                .collect()
        }

        fn focus(&mut self, cowork: &Entity<Cowork>) {
            self.cx.update(|window, cx| {
                cowork.update(cx, |cowork, cx| cowork.focus_composer(window, cx));
            });
        }
    }

    #[gpui::test]
    fn collaborators_receive_the_draft_and_edit_it_live(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        assert_eq!(session.bodies(&collaborator_thread), ["from the host"]);
        let collaborator_id =
            collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);

        // The collaborator continues the host's block.
        let collaborator = session.collaborator.clone();
        session.focus(&collaborator);
        session.cx.simulate_input("!");
        session.wait_until("the host sees the collaborator's edit", |this| {
            this.bodies(&host_thread) == ["from the host!"]
        });

        // A block the collaborator creates is theirs.
        session.cx.simulate_keystrokes("down");
        session.cx.simulate_input("mine");
        session.wait_until("the host sees the collaborator's block", |this| {
            this.bodies(&host_thread).len() == 2
        });
        let items = session.items(&host_thread);
        assert_eq!(items[1].body, "mine");
        assert_eq!(items[1].creator, collaborator_id.as_uuid());

        // The host's edits appear in the collaborator's editors.
        let host = session.host.clone();
        let block = items[1].id;
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.update_draft(draft_id, cx, |draft| {
                draft.doc.set_body(block, "mine, and the host's");
            });
        });
        session.wait_until("the collaborator's editor shows the host's edit", |this| {
            this.collaborator.read_with(this.cx, |collaborator, cx| {
                let thread = collaborator.active_thread(cx).expect("joined");
                thread
                    .read(cx)
                    .draft
                    .editor(EditorSlot::Prompt(block))
                    .is_some_and(|editor| editor.read(cx).value() == "mine, and the host's")
            })
        });
        assert_eq!(
            session.bodies(&collaborator_thread),
            session.bodies(&host_thread)
        );
    }

    #[gpui::test]
    fn the_host_accepts_one_submission_per_sequence(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();

        let collaborator = session.collaborator.clone();
        session.cx.update(|window, cx| {
            collaborator.update(cx, |collaborator, cx| {
                collaborator.submit_composer(window, cx)
            });
        });
        session.wait_until("the collaborator sees the submission", |this| {
            collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
        });
        assert!(session.items(&host_thread).is_empty());
        assert!(session.items(&collaborator_thread).is_empty());
        host_thread.read_with(session.cx, |thread, _| {
            let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
                panic!("expected the submitted message first");
            };
            assert_eq!(message.blocks[0].text, "from the host");
        });

        // The test's agent never answers, so end its run by hand.
        host_thread.update(session.cx, |thread, cx| {
            let id = thread.running_agent_message_id().expect("a running agent");
            thread.emit(
                protocol::HostMessage::AgentEnded {
                    id: id.into_bytes(),
                    failure: None,
                },
                cx,
            );
        });

        // A submission that raced the one just accepted is ignored, even
        // though the draft has new content by now.
        let host = session.host.clone();
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.update_draft(draft_id, cx, |draft| {
                draft.doc.create_prompt(draft.author.as_uuid(), "later");
            });
        });
        session.wait_until("the collaborator sees the new block", |this| {
            this.bodies(&collaborator_thread) == ["later"]
        });
        collaborator_thread.read_with(session.cx, |thread, _| {
            thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 });
        });
        session.settle();
        assert_eq!(
            host_thread.read_with(session.cx, |thread, _| thread.submission_count()),
            1
        );
        assert_eq!(session.bodies(&host_thread), ["later"]);

        // With the current sequence it goes through.
        collaborator_thread.read_with(session.cx, |thread, _| {
            thread.request(protocol::CollaboratorMessage::Submit { sequence: 1 });
        });
        session.wait_until("the second submission is accepted", |this| {
            collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 2)
        });
        assert!(session.items(&host_thread).is_empty());
    }

    #[gpui::test]
    fn concurrent_blocks_converge_to_one_order(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();

        // Both append before either hears of the other's block.
        let host = session.host.clone();
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.update_draft(draft_id, cx, |draft| {
                draft.doc.create_prompt(draft.author.as_uuid(), "host");
            });
        });
        let collaborator = session.collaborator.clone();
        collaborator.update(session.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.update_draft(draft_id, cx, |draft| {
                draft
                    .doc
                    .create_prompt(draft.author.as_uuid(), "collaborator");
            });
        });

        session.wait_until("both have both blocks", |this| {
            this.items(&host_thread).len() == 3 && this.items(&collaborator_thread).len() == 3
        });
        assert_eq!(
            session.bodies(&host_thread),
            session.bodies(&collaborator_thread)
        );
    }

    #[gpui::test]
    fn a_submitted_block_moves_its_typist_to_the_draft_position(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let collaborator = session.collaborator.clone();
        session.focus(&collaborator);
        let focused = |session: &mut Collaboration| {
            session.cx.update(|window, cx| {
                collaborator
                    .read(cx)
                    .focused_draft_editor(window, cx)
                    .map(|(_, slot, _)| slot)
            })
        };
        assert!(matches!(focused(&mut session), Some(EditorSlot::Prompt(_))));

        collaborator_thread.read_with(session.cx, |thread, _| {
            thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 });
        });
        session.wait_until("the submission arrives", |this| {
            collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
        });
        session.settle();
        assert_eq!(focused(&mut session), Some(EditorSlot::DraftPosition));
    }

    #[test]
    fn caret_labels_only_reappear_when_the_caret_moves() {
        let mut draft = ThreadDraft::new(ParticipantId::new());
        let other = ParticipantId::new();
        let at_draft_position = protocol::Presence {
            focus: Some(protocol::PresenceFocus::DraftPosition),
            ..Default::default()
        };
        draft.set_presence(other, at_draft_position.clone());
        let moved_at = draft.presence[&other].1;

        std::thread::sleep(Duration::from_millis(5));
        let mut reading = at_draft_position.clone();
        reading.pending_reads.push(protocol::PendingRead {
            id: [1; 16],
            name: "big.png".into(),
            is_image: true,
            progress: Some(10),
            block: None,
        });
        draft.set_presence(other, reading);
        assert_eq!(draft.presence[&other].1, moved_at);

        draft.set_presence(other, protocol::Presence::default());
        assert!(draft.presence[&other].1 > moved_at);
    }

    #[test]
    fn empty_items_someone_else_is_in_are_kept() {
        let mut draft = ThreadDraft::new(ParticipantId::new());
        let attended = draft.doc.create_prompt(Uuid::new_v4(), "");
        let unattended = draft.doc.create_prompt(Uuid::new_v4(), "");
        let in_item = |id: ItemId| protocol::Presence {
            focus: Some(protocol::PresenceFocus::Item(id.as_uuid().into_bytes())),
            ..Default::default()
        };
        draft.set_presence(ParticipantId::new(), in_item(attended));
        // The local user's own presence does not count.
        draft.set_presence(draft.author, in_item(unattended));

        assert!(!draft.remove_if_unattended(attended));
        assert!(draft.remove_if_unattended(unattended));
        assert_eq!(
            draft
                .doc
                .items()
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [attended]
        );
    }

    #[gpui::test]
    fn rebasing_keeps_local_edits_the_host_has_not_seen(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let host_draft = Draft::new();
        host_draft.create_prompt(Uuid::new_v4(), "host");
        let welcome = |host_draft: &Draft| protocol::Welcome {
            participant_id: [3; 16],
            thread: protocol::ThreadSnapshot {
                id: [1; 16],
                title: "Shared".into(),
                participants: Vec::new(),
                model: DEFAULT_MODEL.catalog_id.into(),
                messages: Vec::new(),
            },
            draft: host_draft.encode_state(),
            presence: Vec::new(),
            stored_attachments: Vec::new(),
        };

        let thread = cx.new(|cx| {
            Thread::from_welcome(
                welcome(&host_draft),
                ThreadDraft::new(ParticipantId::new()),
                ThreadSharing::NotShared,
                cx,
            )
        });
        thread.update(cx, |thread, cx| {
            let author = thread.draft.author.as_uuid();
            thread.draft.doc.create_prompt(author, "unsent");
            thread.apply(protocol::HostMessage::Welcome(welcome(&host_draft)), cx);
            let bodies = thread
                .draft
                .doc
                .items()
                .into_iter()
                .map(|item| item.body)
                .collect::<Vec<_>>();
            assert_eq!(bodies, ["host", "unsent"]);
        });
    }

    /// Typing into an editor that has not been drawn since someone else's
    /// edit arrived must not revert that edit.
    #[gpui::test]
    fn keystrokes_merge_with_edits_the_editor_does_not_show_yet(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let host = session.host.clone();
        session.focus(&host);
        session.settle();
        let block = session.items(&host_thread)[0].id;

        // The collaborator's edit, as the host receives it.
        let collaborator_id =
            collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
        let remote = Draft::new();
        remote
            .apply_update(
                &host_thread.read_with(session.cx, |thread, _| thread.draft.doc.encode_state()),
            )
            .expect("copy the draft");
        remote.edit_body(
            block,
            &TextEdit {
                range: 0..0,
                insert: "X".into(),
            },
        );
        let update = remote.take_local_update().expect("an update");
        let editor = host.read_with(session.cx, |_, cx| {
            host_thread
                .read(cx)
                .draft
                .editor(EditorSlot::Prompt(block))
                .expect("host editor")
        });

        // Applied and typed over within one update, so no frame is drawn in
        // between that would let the editor catch up. Typing goes through
        // the input handler, as simulated input would draw first.
        session.cx.update(|window, cx| {
            host.update(cx, |host, cx| {
                host.collaborator_request(
                    &host_thread,
                    collaborator_id,
                    protocol::CollaboratorMessage::DraftUpdate(update),
                    cx,
                )
                .expect("a valid update");
            });
            assert_eq!(editor.read(cx).value(), "from the host");
            editor.update(cx, |editor, cx| {
                editor.replace_text_in_range(None, "!", window, cx);
            });
        });
        session.wait_until("both edits reach both sides", |this| {
            this.bodies(&host_thread) == ["Xfrom the host!"]
                && this.bodies(&collaborator_thread) == ["Xfrom the host!"]
        });
        host.read_with(session.cx, |host, cx| {
            let editor = host_thread
                .read(cx)
                .draft
                .editor(EditorSlot::Prompt(block))
                .expect("host editor");
            assert_eq!(editor.read(cx).value(), "Xfrom the host!");
            let _ = host;
        });
    }

    /// Draft updates and the submission travel on one ordered stream, so the
    /// last keystroke before Ctrl-Enter is part of the submission.
    #[gpui::test]
    fn a_submission_includes_the_last_keystroke(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let host_thread = session.host_thread.clone();
        let collaborator = session.collaborator.clone();
        session.focus(&collaborator);
        session.settle();

        session.cx.simulate_input("?");
        session.cx.update(|window, cx| {
            collaborator.update(cx, |collaborator, cx| {
                collaborator.submit_composer(window, cx)
            });
        });
        session.wait_until("the host accepts the submission", |this| {
            host_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
        });
        host_thread.read_with(session.cx, |thread, _| {
            let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
                panic!("expected the submitted message first");
            };
            assert_eq!(message.blocks[0].text, "from the host?");
        });
    }

    #[gpui::test]
    fn a_joined_thread_closes_when_the_host_stops_sharing(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let host_thread = session.host_thread.clone();
        host_thread.update(session.cx, |thread, _| {
            thread.sharing = ThreadSharing::NotShared;
            thread.participants.clear();
        });
        session.wait_until("the collaborator's thread closes", |this| {
            this.collaborator.read_with(this.cx, |collaborator, cx| {
                collaborator.active_thread_id.is_none()
                    && collaborator.thread_store.read(cx).threads.is_empty()
            })
        });
    }

    #[gpui::test]
    fn the_host_sees_where_the_collaborator_is_typing(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let collaborator_id =
            collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
        let block = session.items(&host_thread)[0].id;

        let collaborator = session.collaborator.clone();
        session.focus(&collaborator);
        session.wait_until("the host sees the collaborator in the block", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread.draft.editors_of(block, &thread.participants) == [collaborator_id]
            })
        });
        let caret = host_thread.read_with(session.cx, |thread, _| {
            thread.draft.remote_carets(block, &thread.participants)
        });
        let end = "from the host".len();
        assert!(matches!(
            caret.as_slice(),
            [RemoteCaret { participant, head, selection, .. }]
                if *participant == collaborator_id && *head == end && *selection == (end..end)
        ));

        // The caret follows what the collaborator types, and the host's
        // composer lists them as an editor of the host's block.
        session.cx.simulate_input("!!");
        session.wait_until("the caret moves along", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread
                    .draft
                    .remote_carets(block, &thread.participants)
                    .first()
                    .is_some_and(|caret| caret.head == end + 2)
            })
        });
        let host = session.host.clone();
        let editors = session.cx.update(|window, cx| {
            let host = host.read(cx);
            let draft_id = host_thread.read(cx).draft.id;
            let model = host.composer_model(draft_id, window, cx).expect("composer");
            model.blocks[0].presence.editors.clone()
        });
        assert_eq!(editors, [collaborator_id]);

        // Leaving the draft clears the presence.
        collaborator.update(session.cx, |collaborator, cx| {
            collaborator.active_thread_id = None;
            cx.notify();
        });
        session.wait_until("the host sees the collaborator leave the block", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread
                    .draft
                    .editors_of(block, &thread.participants)
                    .is_empty()
            })
        });
    }

    #[gpui::test]
    fn the_host_sees_the_collaborators_selection(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let host_thread = session.host_thread.clone();
        let block = session.items(&host_thread)[0].id;
        let collaborator = session.collaborator.clone();
        session.focus(&collaborator);
        session.settle();
        session
            .cx
            .simulate_keystrokes("shift-left shift-left shift-left shift-left");
        let end = "from the host".len();
        session.wait_until("the host sees the selection", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread
                    .draft
                    .remote_carets(block, &thread.participants)
                    .first()
                    .is_some_and(|caret| caret.selection == (end - 4..end) && caret.head == end - 4)
            })
        });
    }

    #[test]
    fn comment_groups_show_each_author_once_in_comment_order() {
        let (alice, bob) = (ParticipantId::new(), ParticipantId::new());
        let comment = |author| UserComment {
            id: Uuid::new_v4(),
            author,
            presence: ItemPresence::default(),
            reference: CommentReference {
                message_id: Uuid::new_v4(),
                range: 0..1,
                quote: "q".into(),
            },
            body: UserCommentBody::Submitted("c".into()),
        };
        assert_eq!(
            Cowork::comment_authors(&[comment(bob), comment(alice), comment(bob)]),
            [bob, alice]
        );
        assert!(Cowork::comment_authors(&[]).is_empty());
    }

    #[gpui::test]
    fn an_empty_block_stays_while_someone_else_is_in_it(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let block = session.items(&host_thread)[0].id;
        let host = session.host.clone();
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.update_draft(draft_id, cx, |draft| {
                draft.doc.set_body(block, "");
            });
        });
        let collaborator = session.collaborator.clone();
        session.focus(&collaborator);
        session.wait_until("the host sees the collaborator in the block", |this| {
            host_thread.read_with(this.cx, |thread, _| thread.draft.is_attended(block, None))
        });

        // The host cannot remove it while the collaborator is in it.
        let removed = host_thread.update(session.cx, |thread, _| {
            thread.draft.remove_if_unattended(block)
        });
        assert!(!removed);

        // The collaborator, although not its creator, removes it on leaving.
        session.cx.update(|window, cx| {
            collaborator.update(cx, |collaborator, cx| {
                let draft_id = collaborator_thread.read(cx).draft.id;
                collaborator.focus_draft_editor(
                    draft_id,
                    EditorSlot::DraftPosition,
                    None,
                    window,
                    cx,
                );
            });
        });
        session.wait_until("the block is removed everywhere", |this| {
            this.items(&host_thread).is_empty() && this.items(&collaborator_thread).is_empty()
        });
    }

    #[gpui::test]
    fn the_host_removes_the_empty_block_a_leaving_collaborator_was_in(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let collaborator = session.collaborator.clone();
        let block = collaborator.update(session.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator
                .update_draft(draft_id, cx, |draft| {
                    draft.doc.create_prompt(draft.author.as_uuid(), "")
                })
                .expect("draft")
        });
        session.cx.update(|window, cx| {
            collaborator.update(cx, |collaborator, cx| {
                let draft_id = collaborator_thread.read(cx).draft.id;
                collaborator.focus_draft_editor(
                    draft_id,
                    EditorSlot::Prompt(block),
                    None,
                    window,
                    cx,
                );
            });
        });
        session.wait_until("the host sees the collaborator in the block", |this| {
            host_thread.read_with(this.cx, |thread, _| thread.draft.is_attended(block, None))
        });

        // Closing the collaborator's end disconnects it.
        collaborator_thread.update(session.cx, |thread, _| {
            thread.sharing = ThreadSharing::NotShared;
        });
        session.wait_until("the host drops the collaborator and the block", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread.participants.len() == 1 && !thread.draft.doc.contains(block)
            })
        });
        assert_eq!(session.bodies(&host_thread), ["from the host"]);
    }

    /// Two people leaving an empty block at once each still see the other
    /// in it; the host removes it once both are gone.
    #[gpui::test]
    fn the_host_removes_an_empty_block_everyone_left(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let block = session.items(&host_thread)[0].id;
        let collaborator_id =
            collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
        let host = session.host.clone();
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.update_draft(draft_id, cx, |draft| {
                draft.doc.set_body(block, "");
            });
        });
        let in_block = protocol::Presence {
            focus: Some(protocol::PresenceFocus::Item(block.as_uuid().into_bytes())),
            ..Default::default()
        };
        let host_id = host_thread.read_with(session.cx, |thread, _| thread.participant_id);
        host_thread.update(session.cx, |thread, cx| {
            thread.host_presence(host_id, in_block.clone(), cx);
            thread.host_presence(collaborator_id, in_block, cx);
            // The host leaves first: the collaborator is still there.
            thread.host_presence(host_id, protocol::Presence::default(), cx);
            assert!(thread.draft.doc.contains(block));
            thread.host_presence(collaborator_id, protocol::Presence::default(), cx);
            assert!(!thread.draft.doc.contains(block));
        });
        session.wait_until("the collaborator sees the block go", |this| {
            this.items(&collaborator_thread).is_empty()
        });
    }

    #[gpui::test]
    fn sharing_again_announces_the_hosts_presence_again(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let host_thread = session.host_thread.clone();
        let host = session.host.clone();
        session.focus(&host);
        session.settle();
        let host_id = host_thread.read_with(session.cx, |thread, _| thread.participant_id);
        let announced = |session: &mut Collaboration| {
            host_thread.read_with(session.cx, |thread, _| {
                thread.draft.presence.contains_key(&host_id)
            })
        };
        assert!(announced(&mut session));

        host_thread.update(session.cx, |thread, _| {
            thread.draft.presence.clear();
            thread.sharing = ThreadSharing::NotShared;
        });
        session.settle();
        let endpoint = session
            ._runtime
            .block_on(Endpoint::builder(presets::Minimal).bind())
            .expect("bind endpoint");
        host_thread.update(session.cx, |thread, _| {
            thread.sharing = ThreadSharing::Shared {
                endpoint,
                events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
            };
        });
        session.settle();
        assert!(announced(&mut session));
    }

    #[gpui::test]
    fn files_the_host_is_reading_hold_back_everyones_submission(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let host = session.host.clone();
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.pending_attachments.push(PendingAttachment {
                id: Uuid::new_v4(),
                draft_id,
                target: AttachmentTarget::NewBlock(Uuid::new_v4()),
                name: "big.png".into(),
                is_image: true,
                progress: Some(30.),
            });
            cx.notify();
        });
        let collaborator = session.collaborator.clone();
        session.wait_until("the collaborator sees the pending read", |this| {
            collaborator.read_with(this.cx, |collaborator, cx| {
                let draft_id = collaborator_thread.read(cx).draft.id;
                collaborator.draft_is_loading_attachments(draft_id, cx)
            })
        });
        let pending = collaborator.read_with(session.cx, |collaborator, cx| {
            collaborator.pending_reads(&collaborator_thread.read(cx).draft)
        });
        assert!(matches!(
            pending.as_slice(),
            [PendingAttachment { name, progress: Some(progress), .. }]
                if name == "big.png" && *progress == 30.
        ));

        host.update(session.cx, |host, cx| {
            host.pending_attachments.clear();
            cx.notify();
        });
        session.wait_until("the collaborator sees the read finish", |this| {
            !collaborator.read_with(this.cx, |collaborator, cx| {
                let draft_id = collaborator_thread.read(cx).draft.id;
                collaborator.draft_is_loading_attachments(draft_id, cx)
            })
        });
    }

    /// A text file big enough to take several chunks.
    fn big_text_file() -> (PathBuf, String) {
        let text = "0123456789abcdef\n".repeat(9_000);
        assert!(text.len() > 2 * protocol::ATTACHMENT_CHUNK_SIZE);
        let path = std::env::temp_dir().join(format!("cowork-{}.txt", Uuid::new_v4()));
        std::fs::write(&path, &text).expect("write test attachment");
        (path, text)
    }

    fn file_text(draft: &ThreadDraft, id: AttachmentId) -> Option<String> {
        match &draft.files.get(&id)?.content {
            FileAttachmentContent::Text(text) => Some(text.clone()),
            _ => None,
        }
    }

    #[gpui::test]
    fn a_collaborators_file_is_uploaded_and_stored(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let block = session.items(&host_thread)[0].id;
        let (path, text) = big_text_file();

        let collaborator = session.collaborator.clone();
        collaborator.update(session.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.add_attachments(
                draft_id,
                AttachmentTarget::Block(block),
                vec![AttachmentSource::Path(path.clone())],
                cx,
            );
        });
        session.wait_until("the host stores the upload", |this| {
            collaborator_thread.read_with(this.cx, |thread, _| {
                thread.draft.uploads.is_empty() && thread.draft.stored.len() == 1
            })
        });
        std::fs::remove_file(&path).expect("remove test attachment");

        let record = host_thread.read_with(session.cx, |thread, _| {
            let records = thread.draft.attachment_records();
            assert_eq!(records.len(), 1);
            let record = records[0].clone();
            assert!(thread.draft.stored.contains(&record.id));
            assert_eq!(
                file_text(&thread.draft, record.id).as_deref(),
                Some(text.as_str())
            );
            assert!(thread.draft.incoming.is_empty());
            record
        });
        let collaborator_id =
            collaborator_thread.read_with(session.cx, |thread, _| thread.participant_id);
        assert_eq!(record.creator, collaborator_id.as_uuid());
        // Nothing holds the collaborator's submission back anymore.
        collaborator.read_with(session.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            assert!(!collaborator.draft_is_loading_attachments(draft_id, cx));
        });

        // Submitting sends it along, and the submitted message shows it.
        collaborator_thread.read_with(session.cx, |thread, _| {
            thread.request(protocol::CollaboratorMessage::Submit { sequence: 0 });
        });
        session.wait_until("the collaborator sees the submission", |this| {
            collaborator_thread.read_with(this.cx, |thread, _| thread.submission_count() == 1)
        });
        collaborator_thread.read_with(session.cx, |thread, _| {
            let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
                panic!("expected the submitted message first");
            };
            assert_eq!(message.blocks[0].attachments, std::slice::from_ref(&record));
            assert!(thread.draft.files.contains_key(&record.id));
        });
        host_thread.read_with(session.cx, |thread, _| {
            let files = &thread.draft.files;
            let [TimelineMessage::User(message), ..] = thread.timeline.as_slice() else {
                panic!("expected the submitted message first");
            };
            let RigMessage::User { content } = agent_message(None, &message.blocks, files) else {
                panic!("expected a user message");
            };
            assert!(content.iter().any(
                |part| matches!(part, UserContent::Text(part) if part.text.contains("0123456789abcdef"))
            ));
        });
    }

    #[gpui::test]
    fn the_hosts_files_are_downloaded_by_collaborators(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let block = session.items(&host_thread)[0].id;
        let (path, text) = big_text_file();

        let host = session.host.clone();
        host.update(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            host.add_attachments(
                draft_id,
                AttachmentTarget::Block(block),
                vec![AttachmentSource::Path(path.clone())],
                cx,
            );
        });
        session.wait_until("the collaborator has the file", |this| {
            collaborator_thread.read_with(this.cx, |thread, _| thread.draft.files.len() == 1)
        });
        std::fs::remove_file(&path).expect("remove test attachment");
        collaborator_thread.read_with(session.cx, |thread, _| {
            let record = &thread.draft.attachment_records()[0];
            assert!(thread.draft.stored.contains(&record.id));
            assert_eq!(
                file_text(&thread.draft, record.id).as_deref(),
                Some(text.as_str())
            );
        });
    }

    #[gpui::test]
    fn files_left_unfinished_by_a_leaving_collaborator_are_removed(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        let block = session.items(&host_thread)[0].id;
        // A record whose bytes never come.
        let collaborator = session.collaborator.clone();
        collaborator.update(session.cx, |collaborator, cx| {
            let draft_id = collaborator_thread.read(cx).draft.id;
            collaborator.update_draft(draft_id, cx, |draft| {
                draft.doc.add_attachment(
                    block,
                    AttachmentRecord {
                        id: AttachmentId::new(),
                        name: "never.txt".into(),
                        kind: AttachmentKind::Text,
                        size: 10,
                        creator: draft.author.as_uuid(),
                    },
                );
            });
        });
        session.wait_until("the host sees the record", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread.draft.attachment_records().len() == 1
            })
        });
        // Nobody can submit it while its bytes are missing.
        let host = session.host.clone();
        host.read_with(session.cx, |host, cx| {
            let draft_id = host_thread.read(cx).draft.id;
            assert!(host.draft_is_loading_attachments(draft_id, cx));
        });

        collaborator_thread.update(session.cx, |thread, _| {
            thread.sharing = ThreadSharing::NotShared;
        });
        session.wait_until("the host removes the record", |this| {
            host_thread.read_with(this.cx, |thread, _| {
                thread.participants.len() == 1 && thread.draft.attachment_records().is_empty()
            })
        });
        assert_eq!(session.bodies(&host_thread), ["from the host"]);
    }

    /// Bytes and the record announcing them travel separately, so the bytes
    /// can come first.
    #[gpui::test]
    fn uploads_wait_for_their_record(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let host = cx.new(|_| {
            test_thread(
                Uuid::new_v4(),
                Vec::new(),
                ThreadDraft::new(ParticipantId::new()),
            )
        });
        let uploader = ParticipantId::new();
        let file = text_attachment("notes.txt", "some notes");
        let mut sender = ThreadDraft::new(uploader);
        let block = sender.doc.create_prompt(uploader.as_uuid(), "");
        let record = AttachmentRecord {
            id: AttachmentId::new(),
            name: file.name.clone(),
            kind: file.kind(),
            size: file.len(),
            creator: uploader.as_uuid(),
        };
        sender.files.insert(record.id, file);
        sender.doc.add_attachment(block, record.clone());
        let update = sender.doc.take_local_update().expect("an update");
        let chunk = sender.chunk(record.id, 0).expect("a chunk");

        host.update(cx, |host, _| {
            host.receive_upload(uploader, chunk).expect("valid data");
            assert!(!host.draft.stored.contains(&record.id));
            host.apply_collaborator_update(uploader, update)
                .expect("valid update");
            assert!(host.draft.stored.contains(&record.id));
            assert_eq!(
                file_text(&host.draft, record.id).as_deref(),
                Some("some notes")
            );
            // Removing the record discards the file.
            let removal = Draft::new();
            removal
                .apply_update(&host.draft.doc.encode_state())
                .expect("copy the draft");
            removal.remove_attachment(record.id);
            let update = removal.take_local_update().expect("an update");
            host.apply_collaborator_update(uploader, update)
                .expect("valid update");
            assert!(!host.draft.files.contains_key(&record.id));
            // A piece still in flight is ignored rather than started over.
            let late = sender.chunk(record.id, 0).expect("a chunk");
            host.receive_upload(uploader, late).expect("ignored");
            assert!(host.draft.incoming.is_empty());
        });
    }

    #[gpui::test]
    fn a_cancelled_upload_is_discarded(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let (cowork, _, cx) = attachment_test_cowork(cx, runtime.handle().clone());
        let uploader = ParticipantId::new();
        let mut sender = ThreadDraft::new(uploader);
        let id = AttachmentId::new();
        let text = "x".repeat(protocol::ATTACHMENT_CHUNK_SIZE + 1);
        sender.files.insert(id, text_attachment("big.txt", &text));
        let first = sender.chunk(id, 0).expect("a chunk");
        let second = sender
            .chunk(id, protocol::ATTACHMENT_CHUNK_SIZE as u64)
            .expect("a chunk");

        cowork.update(cx, |cowork, cx| {
            let thread = cowork.active_thread(cx).expect("thread");
            for request in [
                protocol::CollaboratorMessage::AttachmentData(first),
                protocol::CollaboratorMessage::AttachmentCancelled(id.as_uuid().into_bytes()),
                protocol::CollaboratorMessage::AttachmentData(second),
            ] {
                cowork
                    .collaborator_request(&thread, uploader, request, cx)
                    .expect("valid request");
            }
            let draft = &thread.read(cx).draft;
            assert!(draft.incoming.is_empty());
            assert!(!draft.files.contains_key(&id));
        });
    }

    #[gpui::test]
    fn joining_peers_are_sent_the_files_they_do_not_have(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let joiner = ParticipantId::new();
        let (timeline_records, timeline_files) = attached([text_attachment("old.txt", "old")]);
        let timeline = vec![TimelineMessage::User(UserMessageGroup {
            id: Uuid::new_v4(),
            comments: Vec::new(),
            blocks: vec![PromptBlock {
                id: Uuid::new_v4(),
                author: ParticipantId::new(),
                text: "earlier".into(),
                attachments: timeline_records.clone(),
            }],
            history_preface: None,
            comments_folded: false,
        })];
        let thread = cx.new(|_| {
            let mut draft = ThreadDraft::new(ParticipantId::new());
            let block = draft.doc.create_prompt(draft.author.as_uuid(), "");
            let mut own = timeline_files;
            for (creator, name) in [
                (Uuid::new_v4(), "new.txt"),
                (joiner.as_uuid(), "theirs.txt"),
            ] {
                let file = text_attachment(name, name);
                let record = AttachmentRecord {
                    id: AttachmentId::new(),
                    name: name.into(),
                    kind: file.kind(),
                    size: file.len(),
                    creator,
                };
                draft.doc.add_attachment(block, record.clone());
                own.insert(record.id, file);
            }
            draft.stored = own.keys().copied().collect();
            draft.files = own;
            test_thread(Uuid::new_v4(), timeline, draft)
        });

        thread.read_with(cx, |thread, _| {
            let names = thread
                .files_for(joiner)
                .into_iter()
                .map(|id| thread.draft.files[&id].name.clone())
                .collect::<Vec<_>>();
            assert_eq!(names, ["old.txt", "new.txt"]);
        });
    }

    #[test]
    fn files_are_chunked_and_put_back_together() {
        let text = "x".repeat(protocol::ATTACHMENT_CHUNK_SIZE * 2 + 5);
        let mut sender = ThreadDraft::new(ParticipantId::new());
        let id = AttachmentId::new();
        sender.files.insert(id, text_attachment("big.txt", &text));
        let mut receiver = ThreadDraft::new(ParticipantId::new());

        let mut offset = 0;
        let mut chunks = 0;
        let completed = loop {
            let chunk = sender.chunk(id, offset).expect("a chunk");
            assert!(chunk.bytes.len() <= protocol::ATTACHMENT_CHUNK_SIZE);
            offset = chunk.offset + chunk.bytes.len() as u64;
            chunks += 1;
            if let Some(done) = receiver.receive_chunk(chunk, None).expect("valid chunk") {
                break done;
            }
        };
        assert_eq!(chunks, 3);
        assert_eq!(completed, id);
        assert!(sender.chunk(id, offset).is_none());
        receiver.complete_file(id).expect("a text file");
        assert_eq!(file_text(&receiver, id), Some(text));

        // A piece that continues nothing is ignored, and an oversized file
        // is refused.
        let mut stray = sender
            .chunk(id, protocol::ATTACHMENT_CHUNK_SIZE as u64)
            .expect("chunk");
        stray.id = [9; 16];
        assert_eq!(receiver.receive_chunk(stray, None).expect("ignored"), None);
        let mut oversized = sender.chunk(id, 0).expect("chunk");
        oversized.id = [8; 16];
        oversized.total = MAX_TEXT_ATTACHMENT_BYTES + 1;
        assert!(receiver.receive_chunk(oversized, None).is_err());
    }

    #[gpui::test]
    fn misattributed_draft_updates_disconnect_the_collaborator(cx: &mut gpui::TestAppContext) {
        let mut session = Collaboration::start(cx);
        let collaborator_thread = session.collaborator_thread().expect("joined");
        let host_thread = session.host_thread.clone();
        assert_eq!(
            host_thread.read_with(session.cx, |thread, _| thread.participants.len()),
            2
        );

        let forged = Draft::new();
        forged.create_prompt(Uuid::new_v4(), "not mine");
        let update = forged.take_local_update().expect("an update");
        collaborator_thread.read_with(session.cx, |thread, _| {
            thread.request(protocol::CollaboratorMessage::DraftUpdate(update));
        });
        session.wait_until("the host drops the collaborator", |this| {
            host_thread.read_with(this.cx, |thread, _| thread.participants.len() == 1)
        });
    }

    #[gpui::test]
    fn participants_sit_beside_the_copy_link_button(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let endpoint = runtime
            .block_on(Endpoint::builder(presets::Minimal).bind())
            .expect("bind endpoint");
        let tokio_handle = runtime.handle().clone();
        let thread_id = Uuid::new_v4();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let draft = ThreadDraft::new(ParticipantId::new());
            let mut thread = test_thread(thread_id, Vec::new(), draft);
            thread.participants = vec![
                thread.participant_id,
                ParticipantId::new(),
                ParticipantId::new(),
            ];
            thread.sharing = ThreadSharing::Shared {
                endpoint,
                events: broadcast::channel(THREAD_EVENT_CAPACITY).0,
            };
            let thread = cx.new(|_| thread);
            let thread_store = cx.new(|_| ThreadStore {
                threads: VecDeque::from([thread]),
            });
            let cowork =
                cx.new(|cx| test_cowork(thread_store, Some(thread_id), tokio_handle, window, cx));
            Root::new(cowork, window, cx)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let participants = cx
            .debug_bounds("participants")
            .expect("participants should be rendered");
        let copy_button = cx
            .debug_bounds("copy-endpoint-id")
            .expect("copy link button should be rendered");

        assert!(
            participants.size.width >= px(24. * 3. - 6. * 2.),
            "three overlapping avatars need room, got {participants:?}"
        );
        assert!(
            participants.right() <= copy_button.left(),
            "participants {participants:?} overlap the copy link button {copy_button:?}"
        );
    }

    #[gpui::test]
    fn synthetic_mouse_up_ends_a_stale_text_drag(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = TextareaState::new(window, cx);
                editor.set_value("selectable text", window, cx);
                editor
            });
            editor.focus_handle(cx).focus(window, cx);
            MouseDragTestView { editor }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        cx.simulate_mouse_down(
            gpui::point(px(8.), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(Cowork::end_stale_mouse_drag);
        cx.simulate_mouse_move(
            gpui::point(px(120.), px(8.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );

        assert!(view.read_with(cx, |view, cx| {
            view.editor.read(cx).selected_range().is_empty()
        }));
    }
}
