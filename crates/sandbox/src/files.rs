//! The file tools: listing folders, reading files, and writing them in the
//! host filesystem for reads and project/scratch folders for writes, from the host process
//! rather than a sandboxed command. Which paths they reach is decided in
//! [`crate::paths`] alone.
//!
//! Like `run_command`'s, their definitions never change, so they name no
//! host paths: the folders' names come from the environment note.

use std::{
    fmt::Write as _,
    fs::File,
    io::{BufRead as _, BufReader, Cursor, Read as _},
    sync::Arc,
};

use agent::{
    model::{Image, ImageSource, ToolResultContent},
    tool::{Tool, ToolCall, ToolError, ToolErrorKind, ToolOutput},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{
    Needs, Sandboxes, ThreadSandbox,
    paths::{FileError, Location, Places, Resolved, Root},
};

/// How much of a file one `read_file` call returns, in bytes. A longer read
/// stops at a line boundary and says where to go on from.
pub const READ_LIMIT: usize = 16 * 1024;

/// How many entries one `list_directory` call shows.
pub const LIST_LIMIT: usize = 500;

const PATHS: &str = "Paths start with the name of a project folder, as the latest <environment> \
note lists them, followed by a path inside it (`myproject/src/main.rs`), or with `scratch://` for \
this conversation's scratch folder (`scratch://notes.md`). Absolute paths anywhere on the host \
can be listed or read, except denied credentials. Writes stay within project and scratch folders.";

impl From<FileError> for ToolError {
    fn from(error: FileError) -> Self {
        let (kind, message) = match error {
            FileError::Invalid(message) => (ToolErrorKind::InvalidArguments, message),
            FileError::NotFound(message) => (ToolErrorKind::NotFound, message),
            FileError::Refused(message) => (ToolErrorKind::PermissionDenied, message),
            FileError::Failed(message) => (ToolErrorKind::Execution, message),
        };
        ToolError::new(kind, message)
    }
}

/// Runs `work` with the thread's places as they are now, off the async
/// runtime, as it does blocking file I/O.
async fn with_places<T: Send + 'static>(
    sandbox: &ThreadSandbox,
    work: impl FnOnce(&Sandboxes, &mut Places) -> Result<T, FileError> + Send + 'static,
) -> Result<T, FileError> {
    let sandboxes = Arc::clone(sandbox.sandboxes());
    let thread = sandbox.thread().to_owned();
    tokio::task::spawn_blocking(move || work(&sandboxes, &mut sandboxes.places(&thread)))
        .await
        .unwrap_or_else(|error| Err(FileError::Failed(error.to_string())))
}

fn location(places: &Places, path: &str, what: &str) -> Result<Location, FileError> {
    match places.resolve(path)? {
        Resolved::Path(location) => Ok(location),
        Resolved::Places => Err(FileError::Invalid(format!(
            "`{path}` stands for the list of project folders, not {what}."
        ))),
    }
}

fn io_error(error: std::io::Error, location: &Location) -> FileError {
    let display = location.display();
    match error.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound(format!("{display} does not exist.")),
        _ => FileError::Failed(format!("{display}: {error}")),
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryArgs {
    /// The folder to list, such as `myproject`, `myproject/src`, or
    /// `scratch://`. `.` lists the project folders themselves.
    pub path: String,
}

/// Lists a host folder, excluding denied credentials.
pub struct ListDirectory;

impl ListDirectory {
    pub const NAME: &'static str = "list_directory";
}

impl Tool for ListDirectory {
    type Args = ListDirectoryArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "List the files and folders in a folder on the user's computer or in scratch, \
             folder, one level deep, as paths you can pass to the other file tools. Use `.` to \
             list the project folders themselves. {PATHS}"
        )
    }

    async fn run(
        &self,
        _call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(with_places(sandbox, move |_, places| list_directory(places, &args.path)).await?)
    }
}

pub(crate) fn list_directory(places: &Places, path: &str) -> Result<String, FileError> {
    let folder = match places.resolve(path)? {
        Resolved::Places => {
            return Ok(format!(
                "# Folders:\n{}\n",
                places.names().collect::<Vec<_>>().join("\n")
            ));
        }
        Resolved::Path(location) => location,
    };
    let display = folder.display();
    let entries = match std::fs::read_dir(&folder.real) {
        Ok(entries) => entries,
        // Created by the first command or write that needs it.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && folder.root == Root::Scratch
                && folder.is_root() =>
        {
            return Ok(format!("{display} is empty.\n"));
        }
        Err(_) if folder.real.is_file() => {
            return Err(FileError::Invalid(format!(
                "{display} is a file, not a folder. Use read_file to read it."
            )));
        }
        Err(error) => return Err(io_error(error, &folder)),
    };

    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        // Followed, so a link is listed as what it leads to; one leading
        // outside the place, or nowhere, isn't listed at all.
        let Ok(real) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if !places.reaches(&folder.root, &real) {
            continue;
        }
        let is_folder = real.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let shown = folder.child(name, real).display();
        if is_folder {
            folders.push(shown);
        } else {
            files.push(shown);
        }
    }
    folders.sort();
    files.sort();

    let total = folders.len() + files.len();
    let mut output = String::new();
    let mut shown = 0;
    for (heading, entries) in [("# Folders:", folders), ("# Files:", files)] {
        let entries = entries
            .into_iter()
            .take(LIST_LIMIT - shown)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            continue;
        }
        shown += entries.len();
        if !output.is_empty() {
            output.push('\n');
        }
        _ = writeln!(output, "{heading}\n{}", entries.join("\n"));
    }
    if total == 0 {
        _ = writeln!(output, "{display} is empty.");
    } else if shown < total {
        _ = writeln!(
            output,
            "\n{} more entries are not shown. Use run_command to search the folder.",
            total - shown
        );
    }
    Ok(output)
}

/// The largest image file `view_image` reads, as for image attachments:
/// room for uncompressed screenshots (BMP, TIFF).
const IMAGE_SOURCE_LIMIT: u64 = 64 * 1024 * 1024;

/// The most pixels `view_image` decodes, checked from the header before
/// decoding, so a small file claiming a huge image is refused rather than
/// decoded. 50 megapixels covers 8K frames and 48 MP phone photos, and stays
/// within the decoder's default allocation limit at any pixel format.
const MAX_IMAGE_PIXELS: u64 = 50_000_000;

/// The longest side of an image as the model is sent it; larger ones are
/// scaled down to it. Vision models scale images to far less themselves, so
/// this loses nothing they'd see, and it keeps the result small: the image
/// stays in the conversation, which is saved, sent to every collaborator,
/// and sent with every later request.
const MAX_IMAGE_SIDE: u32 = 2048;

/// The largest encoded image the model is sent. PNG and JPEG files within
/// it and within [`MAX_IMAGE_SIDE`] go as they are; anything else is
/// re-encoded, as JPEG when PNG would be larger than this.
const IMAGE_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

const JPEG_QUALITY: u8 = 85;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewImageArgs {
    /// The image to view, such as `myproject/screenshot.png` or
    /// `scratch://plot.png`.
    pub path: String,
}

/// Views a host image using the same path checks as the other file tools.
pub struct ViewImage;

impl ViewImage {
    pub const NAME: &'static str = "view_image";
}

/// The image goes only in the model's content; the structured result keeps
/// what it was rather than a second copy of its bytes.
#[derive(Debug, Serialize)]
pub struct ViewImageOutput {
    pub path: String,
    /// The image's size upright, before any scaling.
    pub width: u32,
    pub height: u32,
    /// The size the model is sent, when the image was scaled down.
    pub scaled_to: Option<(u32, u32)>,
    #[serde(skip)]
    image: Image,
}

impl ToolOutput for ViewImageOutput {
    /// Says what the image is before showing it, so a model that is sent a
    /// note in place of the image still learns its size, and coordinates in
    /// a scaled image can be mapped back.
    fn model_output(&self) -> Option<Vec<ToolResultContent>> {
        let mut text = format!("{} ({} × {} pixels", self.path, self.width, self.height);
        if let Some((width, height)) = self.scaled_to {
            _ = write!(text, ", shown scaled to {width} × {height}");
        }
        text.push(')');
        Some(vec![
            ToolResultContent::Text { text },
            ToolResultContent::Image(self.image.clone()),
        ])
    }
}

impl Tool for ViewImage {
    type Args = ViewImageArgs;
    type Output = ViewImageOutput;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "View an image on the user's computer or in scratch: the result shows you the \
             image itself, after its size. Use it to look at screenshots, plots, diagrams, and \
             other images, including ones your commands create. Supports PNG, JPEG, GIF, WebP, \
             BMP, and TIFF; animations show their first frame. Images larger than \
             {MAX_IMAGE_SIDE} pixels on a side are scaled down to fit. If the current model \
             can't see images, the result says so in its place. Only view paths you have seen \
             in a listing, a command's output, or the conversation. {PATHS}"
        )
    }

    async fn run(
        &self,
        _call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(with_places(sandbox, move |_, places| view_image(places, &args.path)).await?)
    }
}

/// The image at `path` as the model is sent it: a PNG or JPEG file that is
/// upright and within the size limits as it is, so nothing is lost or
/// re-encoded; anything else decoded (the first frame of an animation),
/// turned upright by its EXIF orientation, scaled down to
/// [`MAX_IMAGE_SIDE`], and encoded again.
fn view_image(places: &Places, path: &str) -> Result<ViewImageOutput, FileError> {
    use image::{ImageDecoder as _, ImageFormat, metadata::Orientation};

    let location = location(places, path, "an image file")?;
    let display = location.display();
    if location.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not an image file. Use list_directory to see what it holds."
        )));
    }
    let too_large = |what: String| {
        FileError::Invalid(format!(
            "{display} is {what}, too large to view. Use run_command to make a smaller copy \
             in scratch, then view that."
        ))
    };
    let file = File::open(&location.real).map_err(|error| io_error(error, &location))?;
    let mut bytes = Vec::new();
    file.take(IMAGE_SOURCE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(error, &location))?;
    if bytes.len() as u64 > IMAGE_SOURCE_LIMIT {
        return Err(too_large("larger than 64 MiB".into()));
    }

    let format = match image::guess_format(&bytes) {
        Ok(
            format @ (ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Bmp
            | ImageFormat::Tiff),
        ) => format,
        _ => {
            return Err(FileError::Invalid(format!(
                "{display} is not a PNG, JPEG, GIF, WebP, BMP, or TIFF image."
            )));
        }
    };
    let invalid = |error: image::ImageError| match error {
        image::ImageError::Limits(_) => too_large("an image".into()),
        error => FileError::Invalid(format!("{display} can't be decoded as an image: {error}")),
    };
    let mut decoder = image::ImageReader::with_format(Cursor::new(bytes.as_slice()), format)
        .into_decoder()
        .map_err(invalid)?;
    let (mut width, mut height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(too_large(format!("{width} × {height} pixels")));
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let as_is = matches!(format, ImageFormat::Png | ImageFormat::Jpeg)
        && orientation == Orientation::NoTransforms
        && width.max(height) <= MAX_IMAGE_SIDE
        && bytes.len() <= IMAGE_OUTPUT_LIMIT;
    let (media_type, data, scaled_to) = if as_is {
        drop(decoder);
        (format.to_mime_type(), bytes, None)
    } else {
        let mut image = image::DynamicImage::from_decoder(decoder).map_err(invalid)?;
        image.apply_orientation(orientation);
        (width, height) = (image.width(), image.height());
        let mut scaled_to = None;
        if width.max(height) > MAX_IMAGE_SIDE {
            image = image.resize(
                MAX_IMAGE_SIDE,
                MAX_IMAGE_SIDE,
                image::imageops::FilterType::CatmullRom,
            );
            scaled_to = Some((image.width(), image.height()));
        }
        // Photos stay JPEG; PNG would make them several times larger.
        let (media_type, data) = encode_image(&image, format == ImageFormat::Jpeg)?;
        (media_type, data, scaled_to)
    };
    if data.len() > IMAGE_OUTPUT_LIMIT {
        return Err(too_large("still larger than 4 MiB once scaled down".into()));
    }
    Ok(ViewImageOutput {
        path: display,
        width,
        height,
        scaled_to,
        image: Image {
            media_type: media_type.into(),
            source: ImageSource::Base64 {
                data: base64::engine::general_purpose::STANDARD.encode(data),
            },
        },
    })
}

/// `image` as PNG, which keeps it exact, unless it is a photo or the PNG
/// would be over [`IMAGE_OUTPUT_LIMIT`]; then as JPEG, without transparency.
fn encode_image(
    image: &image::DynamicImage,
    photo: bool,
) -> Result<(&'static str, Vec<u8>), FileError> {
    let failed =
        |error: image::ImageError| FileError::Failed(format!("Can't encode the image: {error}"));
    if !photo {
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(failed)?;
        if png.len() <= IMAGE_OUTPUT_LIMIT {
            return Ok(("image/png", png));
        }
    }
    let mut jpeg = Vec::new();
    image
        .to_rgb8()
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut jpeg,
            JPEG_QUALITY,
        ))
        .map_err(failed)?;
    Ok(("image/jpeg", jpeg))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    /// The file to read, such as `myproject/src/main.rs` or
    /// `scratch://notes.md`.
    pub path: String,
    /// The first line to read, counting from 1.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// The last line to read, inclusive.
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// Reads a host text file, excluding denied credentials.
pub struct ReadFile;

impl ReadFile {
    pub const NAME: &'static str = "read_file";
}

impl Tool for ReadFile {
    type Args = ReadFileArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "Read a text file on the user's computer or in scratch. Each line comes back \
             as its line number, right-aligned in six characters, then a tab, then the line; the \
             number and tab are not part of the file. A file longer than about 16 KB comes back \
             in parts: the result says where it stopped, and start_line and end_line read any \
             range. Only read paths you have seen in a listing, a command's output, or the \
             conversation. {PATHS}"
        )
    }

    async fn run(
        &self,
        _call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(with_places(sandbox, move |_, places| {
            read_file(places, &args.path, args.start_line, args.end_line)
        })
        .await?)
    }
}

pub(crate) fn read_file(
    places: &Places,
    path: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<String, FileError> {
    let file = location(places, path, "a file")?;
    let display = file.display();
    if file.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not a file. Use list_directory to see what it holds."
        )));
    }
    let mut reader =
        BufReader::new(File::open(&file.real).map_err(|error| io_error(error, &file))?);

    // Models sometimes count from 0, or end before they start; either way
    // they get at least the line they started at.
    let first = start_line.unwrap_or(1).max(1);
    let last = end_line.unwrap_or(u32::MAX).max(first);

    let mut output = String::new();
    let mut line = Vec::new();
    let mut number = 0u32;
    let mut last_shown = None;
    let mut stopped = false;
    let mut cut = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| io_error(error, &file))?;
        if read == 0 {
            break;
        }
        number = number.saturating_add(1);
        if number < first {
            continue;
        }
        if number > last {
            break;
        }
        if line.contains(&0) {
            return Err(FileError::Invalid(format!(
                "{display} is a binary file, which read_file can't show."
            )));
        }
        let text = std::str::from_utf8(&line).map_err(|_| {
            FileError::Invalid(format!(
                "{display} is not UTF-8 text, which read_file can't show."
            ))
        })?;
        let room = READ_LIMIT.saturating_sub(output.len());
        let text = if text.len() <= room {
            text
        } else if last_shown.is_none() {
            // A single line longer than the limit, as in minified files.
            cut = true;
            let mut end = room;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            stopped = true;
            break;
        };
        _ = write!(output, "{number:>6}\t{text}");
        last_shown = Some(number);
        if cut {
            stopped = true;
            break;
        }
    }

    let Some(last_shown) = last_shown else {
        return if number == 0 {
            Ok(format!("{display} is empty."))
        } else {
            Err(FileError::Invalid(format!(
                "{display} has {number} lines, so start_line {first} is past its end."
            )))
        };
    };
    if stopped {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        if cut {
            _ = write!(
                output,
                "\n[Line {last_shown} is longer than the read limit and was cut."
            );
        } else {
            _ = write!(
                output,
                "\n[Stopped after line {last_shown} to keep the result short."
            );
        }
        _ = write!(
            output,
            " Read on with start_line {}.]",
            last_shown.saturating_add(1)
        );
    }
    Ok(output)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArgs {
    /// The file to create or replace, such as `myproject/src/main.rs` or
    /// `scratch://notes.md`.
    pub path: String,
    /// The file's entire new content.
    pub content: String,
}

/// Creates or replaces a file in the thread's project folders or scratch
/// folder.
pub struct WriteFile;

impl WriteFile {
    pub const NAME: &'static str = "write_file";
}

impl Tool for WriteFile {
    type Args = WriteFileArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "Create a file, or replace a file's entire content, in the user's project or the \
             scratch folder. Missing parent folders are created. Project writes are automatically \
             approved when the latest <environment> note says the project's auto-approval level \
             is write or network; in read mode, approving this call permits this specific write. Changes \
             apply to the user's computer at once; the scratch folder can always be written, \
             and suits notes and intermediate results that are not part of the project. In write \
             or network mode, calls run at once. In read mode, every call waits for the user \
             to allow it, except in a scheduled run, where the first such call is refused with \
             an explanation. {PATHS}"
        )
    }

    fn access(&self, args: &WriteFileArgs, _: &ThreadSandbox) -> Needs {
        Needs::Write {
            path: args.path.clone(),
        }
    }

    /// Writes the target the call was allowed to write: in a read-only
    /// project, that is the one write the host approved. The target remains
    /// subject to every path check.
    async fn run(
        &self,
        call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let approved = call.access
            == Needs::Write {
                path: args.path.clone(),
            };
        Ok(with_places(sandbox, move |_, places| {
            if approved {
                places.allow_project_write();
            }
            write_file(places, &args.path, &args.content)
        })
        .await?)
    }
}

pub(crate) fn write_file(places: &Places, path: &str, content: &str) -> Result<String, FileError> {
    let file = location(places, path, "a file")?;
    let display = file.display();
    if !places.writable(&file) {
        return Err(FileError::Refused(format!(
            "{display} can't be written: file writes require a writable project or scratch target."
        )));
    }
    if file.is_root() || file.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not a file."
        )));
    }
    let previous = match std::fs::read(&file.real) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error, &file)),
    };
    if previous.as_deref() == Some(content.as_bytes()) {
        return Ok(format!(
            "{display} already has this content; nothing changed."
        ));
    }
    if let Some(parent) = file.real.parent() {
        std::fs::create_dir_all(parent).map_err(|error| io_error(error, &file))?;
    }
    std::fs::write(&file.real, content).map_err(|error| io_error(error, &file))?;
    let now = lines(content);
    Ok(match previous {
        None => format!("Created {display} ({now})."),
        Some(previous) => format!(
            "Replaced {display} ({} before, {now} now).",
            lines(&String::from_utf8_lossy(&previous))
        ),
    })
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditFileArgs {
    /// The file to change, such as `myproject/src/main.rs` or
    /// `scratch://notes.md`.
    pub path: String,
    /// The exact text to replace, including whitespace and indentation. It
    /// must appear exactly once unless `replace_all` is set; include
    /// surrounding lines to make it unique.
    pub old_string: String,
    /// The text to put in its place.
    pub new_string: String,
    /// Replace every occurrence of `old_string` instead of exactly one.
    #[serde(default)]
    pub replace_all: bool,
}

/// Replaces exact text in an existing file in the thread's project folders
/// or scratch folder. Its arguments are the change itself, so every copy of
/// a thread can show it without the file.
pub struct EditFile;

impl EditFile {
    pub const NAME: &'static str = "edit_file";
}

impl Tool for EditFile {
    type Args = EditFileArgs;
    type Output = String;
    type Context = ThreadSandbox;
    type Access = Needs;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> String {
        format!(
            "Change part of an existing file in the user's project or the scratch folder by \
             replacing exact text: `old_string` must match the file exactly, including \
             whitespace and indentation, and appear exactly once unless `replace_all` is set. \
             Read the file first. Prefer this to write_file for changing existing files; use \
             write_file to create a file or replace all of it. Approval works as for \
             write_file: in write or network mode, calls run at once; in read mode, every call \
             waits for the user to allow it, except in a scheduled run, where the first such \
             call is refused with an explanation. {PATHS}"
        )
    }

    fn access(&self, args: &EditFileArgs, _: &ThreadSandbox) -> Needs {
        Needs::Write {
            path: args.path.clone(),
        }
    }

    /// Edits the target the call was allowed to write, as `write_file` does.
    async fn run(
        &self,
        call: ToolCall<Needs>,
        sandbox: &Self::Context,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let approved = call.access
            == Needs::Write {
                path: args.path.clone(),
            };
        Ok(with_places(sandbox, move |_, places| {
            if approved {
                places.allow_project_write();
            }
            edit_file(places, &args)
        })
        .await?)
    }
}

pub(crate) fn edit_file(places: &Places, args: &EditFileArgs) -> Result<String, FileError> {
    let file = location(places, &args.path, "a file")?;
    let display = file.display();
    if !places.writable(&file) {
        return Err(FileError::Refused(format!(
            "{display} can't be written: file writes require a writable project or scratch target."
        )));
    }
    if file.is_root() || file.real.is_dir() {
        return Err(FileError::Invalid(format!(
            "{display} is a folder, not a file."
        )));
    }
    if args.old_string.is_empty() {
        return Err(FileError::Invalid(
            "old_string is empty; use write_file to create a file or replace all of it.".into(),
        ));
    }
    if args.old_string == args.new_string {
        return Err(FileError::Invalid(
            "old_string and new_string are the same; nothing would change.".into(),
        ));
    }
    let bytes = std::fs::read(&file.real).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound(format!(
            "{display} does not exist; use write_file to create it."
        )),
        _ => io_error(error, &file),
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        FileError::Invalid(format!(
            "{display} is not UTF-8 text, so it can't be edited."
        ))
    })?;
    let count = text.matches(&args.old_string).count();
    let edited = match count {
        0 => {
            return Err(FileError::Invalid(format!(
                "old_string was not found in {display}. It must match the file exactly, \
                 including whitespace and indentation; read the file again if it may have \
                 changed."
            )));
        }
        1 => text.replacen(&args.old_string, &args.new_string, 1),
        _ if args.replace_all => text.replace(&args.old_string, &args.new_string),
        _ => {
            return Err(FileError::Invalid(format!(
                "old_string appears {count} times in {display}. Include more surrounding \
                 lines to make it unique, or set replace_all to replace every occurrence."
            )));
        }
    };
    std::fs::write(&file.real, &edited).map_err(|error| io_error(error, &file))?;
    Ok(match count {
        1 => format!("Edited {display} ({} now).", lines(&edited)),
        count => format!(
            "Edited {display}: replaced {count} occurrences ({} now).",
            lines(&edited)
        ),
    })
}

fn lines(text: &str) -> String {
    match text.lines().count() {
        1 => "1 line".to_owned(),
        count => format!("{count} lines"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, Project, ProjectFolder, TestRoot};

    fn places(root: &TestRoot, writable: bool) -> Places {
        let zed = root.0.join("zed");
        std::fs::create_dir_all(zed.join("src")).unwrap();
        let project = Project {
            folders: vec![ProjectFolder {
                name: "zed".into(),
                path: zed,
            }],
            access: if writable {
                Access::Write
            } else {
                Access::Read
            },
        };
        Places::new(&project, &root.0.join("scratch"), None)
    }

    #[test]
    fn listings_show_folders_then_files_as_paths_to_pass_back() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/Cargo.toml"), "").unwrap();
        std::fs::write(root.0.join("zed/README.md"), "").unwrap();

        assert_eq!(
            list_directory(&places, "zed").unwrap(),
            "# Folders:\nzed/src\n\n# Files:\nzed/Cargo.toml\nzed/README.md\n"
        );
        assert_eq!(
            list_directory(&places, "zed/src").unwrap(),
            "zed/src is empty.\n"
        );
        assert_eq!(
            list_directory(&places, ".").unwrap(),
            "# Folders:\nzed\nscratch://\n"
        );
        assert_eq!(
            list_directory(&places, "scratch://").unwrap(),
            "scratch:// is empty.\n",
            "a scratch folder not made yet is empty"
        );
        assert!(matches!(
            list_directory(&places, "zed/Cargo.toml"),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            list_directory(&places, "zed/missing"),
            Err(FileError::NotFound(_))
        ));
    }

    /// The text, media type, and bytes `view_image` sends the model.
    fn viewed(places: &Places, path: &str) -> (String, String, Vec<u8>) {
        let output = view_image(places, path).unwrap();
        let typed = agent::tool::TypedToolOutput::new(ViewImage::NAME, output).unwrap();
        let [
            ToolResultContent::Text { text },
            ToolResultContent::Image(image),
        ] = typed.model_output.as_slice()
        else {
            panic!("expected text and image content");
        };
        let ImageSource::Base64 { data } = &image.source else {
            panic!("expected the image's bytes");
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        (text.clone(), image.media_type.clone(), bytes)
    }

    fn encoded(image: &image::RgbImage, format: image::ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    #[test]
    fn images_go_as_they_are_when_they_can_and_are_converted_or_scaled_otherwise() {
        let root = TestRoot::new();
        let places = places(&root, false);
        let small = image::RgbImage::from_pixel(3, 2, image::Rgb([200, 100, 50]));

        // Formats are detected by content, not extension.
        for (format, media_type) in [
            (image::ImageFormat::Png, "image/png"),
            (image::ImageFormat::Jpeg, "image/jpeg"),
        ] {
            let bytes = encoded(&small, format);
            std::fs::write(root.0.join("zed/image.data"), &bytes).unwrap();
            assert_eq!(
                viewed(&places, "zed/image.data"),
                (
                    "zed/image.data (3 × 2 pixels)".into(),
                    media_type.into(),
                    bytes
                ),
            );
        }

        std::fs::write(
            root.0.join("zed/image.bmp"),
            encoded(&small, image::ImageFormat::Bmp),
        )
        .unwrap();
        let (_, media_type, png) = viewed(&places, "zed/image.bmp");
        assert_eq!(media_type, "image/png");
        assert_eq!(image::load_from_memory(&png).unwrap().to_rgb8(), small);

        let wide = image::RgbImage::from_pixel(3 * MAX_IMAGE_SIDE / 2, 4, image::Rgb([0, 0, 255]));
        std::fs::create_dir_all(root.0.join("scratch")).unwrap();
        std::fs::write(
            root.0.join("scratch/wide.png"),
            encoded(&wide, image::ImageFormat::Png),
        )
        .unwrap();
        let (text, media_type, png) = viewed(&places, "scratch://wide.png");
        let shown = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (media_type.as_str(), shown.width()),
            ("image/png", MAX_IMAGE_SIDE)
        );
        assert_eq!(
            text,
            format!(
                "scratch://wide.png ({} × 4 pixels, shown scaled to {} × {})",
                wide.width(),
                shown.width(),
                shown.height()
            )
        );
        view_image(&places, root.0.join("scratch/wide.png").to_str().unwrap()).unwrap();

        // Photos stay JPEG when they have to be scaled.
        std::fs::write(
            root.0.join("scratch/wide.jpg"),
            encoded(&wide, image::ImageFormat::Jpeg),
        )
        .unwrap();
        let (_, media_type, jpeg) = viewed(&places, "scratch://wide.jpg");
        assert_eq!(
            (
                media_type.as_str(),
                image::load_from_memory(&jpeg).unwrap().width()
            ),
            ("image/jpeg", MAX_IMAGE_SIDE)
        );
    }

    #[test]
    fn invalid_or_oversized_images_and_unreadable_paths_are_errors() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/not.png"), b"not an image").unwrap();
        for path in ["zed/not.png", "zed/src", ".", "zed/../outside.png"] {
            assert!(
                matches!(view_image(&places, path), Err(FileError::Invalid(_))),
                "{path}"
            );
        }
        assert!(matches!(
            view_image(&places, "zed/missing.png"),
            Err(FileError::NotFound(_))
        ));

        let file = File::create(root.0.join("zed/huge.png")).unwrap();
        file.set_len(IMAGE_SOURCE_LIMIT + 1).unwrap();
        assert!(matches!(
            view_image(&places, "zed/huge.png"),
            Err(FileError::Invalid(message)) if message.contains("64 MiB")
        ));

        // A tiny file whose header claims 10000 × 10000 pixels is refused
        // before anything is decoded.
        let mut bomb = encoded(&image::RgbImage::new(1, 1), image::ImageFormat::Bmp);
        bomb[18..22].copy_from_slice(&10_000i32.to_le_bytes());
        bomb[22..26].copy_from_slice(&10_000i32.to_le_bytes());
        std::fs::write(root.0.join("zed/bomb.bmp"), bomb).unwrap();
        assert!(matches!(
            view_image(&places, "zed/bomb.bmp"),
            Err(FileError::Invalid(message)) if message.contains("10000 × 10000 pixels")
        ));
    }

    #[test]
    fn files_come_back_numbered_and_in_ranges() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/a.txt"), "one\ntwo\r\nthree").unwrap();

        assert_eq!(
            read_file(&places, "zed/a.txt", None, None).unwrap(),
            "     1\tone\n     2\ttwo\r\n     3\tthree"
        );
        assert_eq!(
            read_file(&places, "zed/a.txt", Some(2), Some(2)).unwrap(),
            "     2\ttwo\r\n"
        );
        assert_eq!(
            read_file(&places, "zed/a.txt", Some(0), Some(1)).unwrap(),
            "     1\tone\n",
            "line 0 is read as line 1"
        );
        assert_eq!(
            read_file(&places, "zed/a.txt", Some(3), Some(1)).unwrap(),
            "     3\tthree",
            "an end before the start reads the start"
        );
        assert!(matches!(
            read_file(&places, "zed/a.txt", Some(9), None),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            read_file(&places, "zed/src", None, None),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            read_file(&places, ".", None, None),
            Err(FileError::Invalid(_))
        ));
    }

    #[test]
    fn long_files_stop_at_a_line_and_say_where_to_go_on() {
        let root = TestRoot::new();
        let places = places(&root, false);
        let line = "x".repeat(99) + "\n";
        std::fs::write(root.0.join("zed/long.txt"), line.repeat(1000)).unwrap();

        let first = read_file(&places, "zed/long.txt", None, None).unwrap();
        let shown = first.lines().filter(|line| line.contains('\t')).count();
        assert!(shown > 100 && first.len() < READ_LIMIT + 200, "{shown}");
        assert!(
            first.ends_with(&format!(
                "[Stopped after line {shown} to keep the result short. Read on with start_line {}.]",
                shown + 1
            )),
            "{first}"
        );
        let next = read_file(&places, "zed/long.txt", Some(shown as u32 + 1), None).unwrap();
        assert!(next.starts_with(&format!("{:>6}\t", shown + 1)), "{next}");

        std::fs::write(root.0.join("zed/minified.js"), "é".repeat(READ_LIMIT)).unwrap();
        let cut = read_file(&places, "zed/minified.js", None, None).unwrap();
        assert!(cut.contains(
            "[Line 1 is longer than the read limit and was cut. Read on with start_line 2.]"
        ));
    }

    #[test]
    fn binary_and_non_utf8_files_are_refused() {
        let root = TestRoot::new();
        let places = places(&root, false);
        std::fs::write(root.0.join("zed/a.bin"), b"\x00\x01").unwrap();
        std::fs::write(root.0.join("zed/latin1.txt"), b"caf\xe9\n").unwrap();
        for path in ["zed/a.bin", "zed/latin1.txt"] {
            assert!(matches!(
                read_file(&places, path, None, None),
                Err(FileError::Invalid(_))
            ));
        }
        std::fs::write(root.0.join("zed/empty.txt"), "").unwrap();
        assert_eq!(
            read_file(&places, "zed/empty.txt", None, None).unwrap(),
            "zed/empty.txt is empty."
        );
    }

    #[test]
    fn writes_need_a_writable_project_but_scratch_always_takes_them() {
        let root = TestRoot::new();
        let read_only = places(&root, false);
        let outside = root.0.join("outside.txt");
        std::fs::write(&outside, "readable\n").unwrap();
        let path = outside.to_str().unwrap();
        assert!(
            read_file(&read_only, path, None, None)
                .unwrap()
                .contains("readable")
        );
        assert!(
            list_directory(&read_only, root.0.to_str().unwrap())
                .unwrap()
                .contains("outside.txt")
        );
        for places in [&read_only, &places(&root, true)] {
            assert!(matches!(
                write_file(places, path, "no"),
                Err(FileError::Refused(_))
            ));
        }
        assert!(matches!(
            write_file(&read_only, "zed/new.txt", "hello\n"),
            Err(FileError::Refused(_))
        ));
        assert!(!root.0.join("zed/new.txt").exists());

        assert_eq!(
            write_file(&read_only, "scratch://notes/a.md", "a\nb\n").unwrap(),
            "Created scratch://notes/a.md (2 lines)."
        );
        assert_eq!(
            std::fs::read_to_string(root.0.join("scratch/notes/a.md")).unwrap(),
            "a\nb\n"
        );

        let writable = places(&root, true);
        assert_eq!(
            write_file(&writable, "zed/src/lib.rs", "pub fn a() {}\n").unwrap(),
            "Created zed/src/lib.rs (1 line)."
        );
        assert_eq!(
            write_file(&writable, "zed/src/lib.rs", "pub fn a() {}\n").unwrap(),
            "zed/src/lib.rs already has this content; nothing changed."
        );
        assert_eq!(
            write_file(&writable, "zed/src/lib.rs", "\n\n").unwrap(),
            "Replaced zed/src/lib.rs (1 line before, 2 lines now)."
        );
        for folder in ["zed", "zed/src"] {
            assert!(matches!(
                write_file(&writable, folder, ""),
                Err(FileError::Invalid(_))
            ));
        }
    }

    /// Edits replace exactly one match, or every one when asked, and are
    /// refused, leaving the file as it was, when the text is missing or
    /// ambiguous, the file doesn't exist, or the project is read-only.
    #[test]
    fn edits_replace_exact_text() {
        let root = TestRoot::new();
        let writable = places(&root, true);
        let file = root.0.join("zed/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "let a = 1;\nlet b = 1;\n").unwrap();
        let edit = |places: &Places, old: &str, new: &str, replace_all| {
            edit_file(
                places,
                &EditFileArgs {
                    path: "zed/lib.rs".into(),
                    old_string: old.into(),
                    new_string: new.into(),
                    replace_all,
                },
            )
        };
        for (places, old, new) in [
            (&writable, "= 1", "= 2"),
            (&writable, "= 3", "= 2"),
            (&writable, "", "x"),
            (&places(&root, false), "let a", "let c"),
        ] {
            assert!(edit(places, old, new, false).is_err(), "{old:?}");
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "let a = 1;\nlet b = 1;\n"
        );

        assert_eq!(
            edit(&writable, "let a = 1", "let a = 3", false).unwrap(),
            "Edited zed/lib.rs (2 lines now)."
        );
        assert_eq!(
            edit(&writable, "= ", "== ", true).unwrap(),
            "Edited zed/lib.rs: replaced 2 occurrences (2 lines now)."
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "let a == 3;\nlet b == 1;\n"
        );
        let missing = edit_file(
            &writable,
            &EditFileArgs {
                path: "zed/new.rs".into(),
                old_string: "a".into(),
                new_string: "b".into(),
                replace_all: false,
            },
        );
        assert!(matches!(missing, Err(FileError::NotFound(_))));
    }
}
