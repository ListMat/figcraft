//! The native `.figcraft` format.
//!
//! A zip archive holding `document.json` (the [`Document`] as JSON, with a `format`/`version`
//! header; layer fields at their default values are left out and read back as those defaults),
//! `images/<sha256>` for every image the document (or a saved version) uses, an optional
//! `thumbnail.png` and an optional version history: `versions/index.json` (`{"versions": [{id,
//! name, description, time, images}]}`, oldest first) plus one `versions/<id>.json` snapshot per
//! version (a `document.json` of its own). Branches are kept the same way: `branches/index.json`
//! (`{"branches": [{id, name, time, client, state, baseImages, headImages}]}`) plus
//! `branches/<id>/base.json` and `branches/<id>/head.json`; a branch's own version history is
//! kept like the file's, under `branches/<id>/versions/`. A published file also holds its
//! library (see [`library`]). Readers that don't know about versions, branches or libraries
//! ignore those entries. Plain
//! JSON (the `document.json` alone) also opens. Reading is lenient: structural problems are
//! repaired (see [`Document::repair`]) and reported, damaged versions and libraries are skipped.
//!
//! [`rest`] imports the public REST API file JSON; [`fig`] imports `.fig` files (best effort).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod fig;
pub mod library;
pub mod rest;
pub mod thumbnail;
pub mod video_stream;

pub use library::{is_valid_library_id, read_library, read_library_within};
pub use video_stream::{VideoFrame, VideoStreamDecoder};

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, Write};
use std::sync::Arc;

use figcraft_doc::{Document, ImageData, VideoData};
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
use sha2::Digest;

pub const EXTENSION: &str = "figcraft";
pub const FORMAT: &str = "figcraft";
pub const VERSION: u32 = 1;
/// Largest archive member we read (guards against zip bombs).
const MAX_MEMBER: u64 = 512 * 1024 * 1024;
/// Most versions a file keeps: saving more is refused, extra entries in a file are skipped.
pub const MAX_VERSIONS: usize = 100;
/// Longest version name and description kept (chars).
pub const MAX_VERSION_NAME: usize = 200;
pub const MAX_VERSION_DESCRIPTION: usize = 2000;
/// Largest version index and total size of the version snapshots read from one file.
const MAX_VERSION_INDEX: u64 = 16 * 1024 * 1024;
const MAX_VERSION_BYTES: u64 = 1024 * 1024 * 1024;
/// Most images, and image bytes together, read from one file (members past it are left out).
const IMAGE_BUDGET: ImageBudget = ImageBudget { images: 20_000, bytes: 4 * 1024 * 1024 * 1024 };
const BRANCH_INDEX: &str = "branches/index.json";
/// Most branches a file keeps: making more is refused, extra entries in a file are skipped.
pub const MAX_BRANCHES: usize = 100;
/// Deflate level of JSON members (see [`json_options`]).
const JSON_LEVEL: i64 = 3;

#[derive(Debug)]
pub enum FormatError {
    Io(String),
    Corrupt(String),
    Newer(u32),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Io(e) => write!(f, "{e}"),
            FormatError::Corrupt(e) => write!(f, "not a valid FigCraft file: {e}"),
            FormatError::Newer(v) => write!(f, "this file was saved by a newer FigCraft (format version {v})"),
        }
    }
}

impl std::error::Error for FormatError {}

/// One entry of a file's version history: a named snapshot of the document.
#[derive(Clone, Debug)]
pub struct Version {
    /// Unique within the file; ASCII letters, digits, `-` and `_` (see [`is_valid_version_id`]).
    pub id: String,
    pub name: String,
    pub description: String,
    /// When it was saved (Unix seconds).
    pub time: u64,
    /// Saved automatically (a checkpoint) rather than by the user.
    pub auto: bool,
    snapshot: Snapshot,
}

#[derive(Clone, Debug)]
enum Snapshot {
    /// Saved in this session (or already read).
    Doc(Arc<Document>),
    /// As read from a file: its `document.json`-style JSON, parsed on first use, and the images
    /// it uses.
    Stored { json: Arc<Vec<u8>>, images: BTreeMap<String, ImageData> },
}

impl Version {
    /// A version of `doc`. The name and description are clipped to their maximum lengths.
    pub fn new(id: impl Into<String>, name: &str, description: &str, time: u64, doc: Arc<Document>) -> Self {
        Self {
            id: id.into(),
            name: clip(name, MAX_VERSION_NAME),
            description: clip(description, MAX_VERSION_DESCRIPTION),
            time,
            auto: false,
            snapshot: Snapshot::Doc(doc),
        }
    }

    /// The snapshot (with its images). Versions read from a file are parsed here, so a damaged
    /// one fails now rather than when the file opens.
    pub fn document(&self) -> Result<Arc<Document>, FormatError> {
        match &self.snapshot {
            Snapshot::Doc(d) => Ok(d.clone()),
            Snapshot::Stored { json, images } => {
                let text = std::str::from_utf8(json).map_err(|e| FormatError::Corrupt(e.to_string()))?;
                let (mut doc, _) = from_json(text)?;
                doc.images = images.clone();
                Ok(Arc::new(doc))
            }
        }
    }

    /// The images the snapshot uses, by hash.
    pub fn images(&self) -> &BTreeMap<String, ImageData> {
        match &self.snapshot {
            Snapshot::Doc(d) => &d.images,
            Snapshot::Stored { images, .. } => images,
        }
    }
}

/// Where a branch is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BranchState {
    Active,
    /// Merged into the main file (and archived).
    Merged,
    /// Archived without merging.
    Archived,
}

/// A branch of the file: a copy of the document to work on separately, and the main document as
/// it was when the branch was made or last updated from it (its base).
#[derive(Clone, Debug)]
pub struct Branch {
    /// Unique among the file's branches; a valid version id ([`is_valid_version_id`]).
    pub id: String,
    pub name: String,
    /// When it was made (Unix seconds).
    pub time: u64,
    /// The client id space the branch makes ids in ([`Document::set_client_id`]), so they never
    /// clash with ids made on the main file or on other branches.
    pub client: u32,
    pub state: BranchState,
    pub base: Arc<Document>,
    pub head: Arc<Document>,
    /// The branch's own version history, oldest first.
    pub versions: Vec<Version>,
}

/// Whether `id` can name a version (and its archive entry).
pub fn is_valid_version_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `s` cut to at most `max` chars.
pub fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// A file as read: the document, the repairs made (and versions skipped), and its version
/// history, oldest first.
#[derive(Debug)]
pub struct Loaded {
    pub doc: Document,
    pub issues: Vec<String>,
    pub versions: Vec<Version>,
    pub branches: Vec<Branch>,
}

impl From<Document> for Loaded {
    fn from(doc: Document) -> Self {
        Self { doc, issues: Vec::new(), versions: Vec::new(), branches: Vec::new() }
    }
}

/// Content hash used as an image's key.
pub fn image_hash(bytes: &[u8]) -> String {
    let d = sha2::Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content hash used as a video's key.
pub fn video_hash(bytes: &[u8]) -> String {
    let d = sha2::Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert YUV420 planar buffer to RGBA8 buffer.
pub fn yuv420_to_rgba(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    width: usize,
    height: usize,
    out_rgba: &mut [u8],
) {
    if width == 0 || height == 0 {
        return;
    }
    let expected_len = width.saturating_mul(height).saturating_mul(4);
    if out_rgba.len() < expected_len {
        return;
    }

    let y_stride = width;
    let u_stride = width / 2;
    let v_stride = width / 2;

    for row in 0..height {
        let y_row_start = row * y_stride;
        let uv_row_start = (row / 2) * u_stride;
        let out_row_start = row * width * 4;

        let Some(y_row) = y.get(y_row_start..y_row_start + width) else { continue };
        let Some(u_row) = u.get(uv_row_start..uv_row_start + u_stride) else { continue };
        let Some(v_row) = v.get(uv_row_start..uv_row_start + v_stride) else { continue };
        let Some(out_row) = out_rgba.get_mut(out_row_start..out_row_start + width * 4) else { continue };

        let pairs = width / 2;
        for col_pair in 0..pairs {
            let u_val = u_row[col_pair] as i32 - 128;
            let v_val = v_row[col_pair] as i32 - 128;

            let r_off = (359 * v_val + 128) >> 8;
            let g_off = (88 * u_val + 183 * v_val + 128) >> 8;
            let b_off = (454 * u_val + 128) >> 8;

            let col0 = col_pair * 2;
            let y0 = y_row[col0] as i32;
            let out0 = col0 * 4;
            out_row[out0] = (y0 + r_off).clamp(0, 255) as u8;
            out_row[out0 + 1] = (y0 - g_off).clamp(0, 255) as u8;
            out_row[out0 + 2] = (y0 + b_off).clamp(0, 255) as u8;
            out_row[out0 + 3] = 255;

            let col1 = col0 + 1;
            let y1 = y_row[col1] as i32;
            let out1 = col1 * 4;
            out_row[out1] = (y1 + r_off).clamp(0, 255) as u8;
            out_row[out1 + 1] = (y1 - g_off).clamp(0, 255) as u8;
            out_row[out1 + 2] = (y1 + b_off).clamp(0, 255) as u8;
            out_row[out1 + 3] = 255;
        }

        if width % 2 != 0 {
            let col = width - 1;
            let u_val = u_row.get(col / 2).copied().unwrap_or(128) as i32 - 128;
            let v_val = v_row.get(col / 2).copied().unwrap_or(128) as i32 - 128;
            let y0 = y_row[col] as i32;
            let out0 = col * 4;
            out_row[out0] = (y0 + ((359 * v_val + 128) >> 8)).clamp(0, 255) as u8;
            out_row[out0 + 1] = (y0 - ((88 * u_val + 183 * v_val + 128) >> 8)).clamp(0, 255) as u8;
            out_row[out0 + 2] = (y0 + ((454 * u_val + 128) >> 8)).clamp(0, 255) as u8;
            out_row[out0 + 3] = 255;
        }
    }
}

/// Reads video dimensions and duration from container headers without decoding frames.
pub(crate) fn read_video_metadata(bytes: &[u8]) -> Result<(u32, u32, f64), FormatError> {
    let size = bytes.len() as u64;
    let cursor = std::io::Cursor::new(bytes);
    let mp4 = mp4::Mp4Reader::read_header(cursor, size)
        .map_err(|e| FormatError::Corrupt(format!("invalid mp4 container: {e}")))?;

    for track in mp4.tracks().values() {
        if track.track_type().map_err(|e| FormatError::Corrupt(e.to_string()))? == mp4::TrackType::Video {
            let width = track.width() as u32;
            let height = track.height() as u32;
            let duration = mp4.duration().as_secs_f64();
            return Ok((width, height, duration));
        }
    }
    Err(FormatError::Corrupt("no video track found in MP4".into()))
}

/// Extracts video dimensions, duration, and first decoded poster frame as PNG.
pub fn extract_video_metadata_and_poster(bytes: &[u8]) -> Result<(u32, u32, f64, Vec<u8>), FormatError> {
    let size = bytes.len() as u64;
    let cursor = std::io::Cursor::new(bytes);
    let mut mp4 = mp4::Mp4Reader::read_header(cursor, size)
        .map_err(|e| FormatError::Corrupt(format!("invalid mp4 container: {e}")))?;

    let mut video_track_id = None;
    let mut track_w = 0u32;
    let mut track_h = 0u32;

    for track in mp4.tracks().values() {
        if track.track_type().map_err(|e| FormatError::Corrupt(e.to_string()))? == mp4::TrackType::Video {
            video_track_id = Some(track.track_id());
            track_w = track.width() as u32;
            track_h = track.height() as u32;
            break;
        }
    }

    let track_id = video_track_id.ok_or_else(|| FormatError::Corrupt("no video track found in MP4".into()))?;
    let sample_count = mp4.sample_count(track_id).map_err(|e| FormatError::Corrupt(e.to_string()))?;
    if sample_count == 0 {
        return Err(FormatError::Corrupt("video contains no samples".into()));
    }
    let duration = mp4.duration().as_secs_f64();

    let (sps, pps) = {
        let track = mp4.tracks().get(&track_id).ok_or_else(|| FormatError::Corrupt("missing video track".into()))?;
        let stsd = track
            .trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_ref()
            .ok_or_else(|| FormatError::Corrupt("unsupported video codec: only H.264/AVC is currently supported".into()))?;

        if stsd.avcc.sequence_parameter_sets.is_empty() || stsd.avcc.picture_parameter_sets.is_empty() {
            return Err(FormatError::Corrupt("missing SPS or PPS in AVC configuration".into()));
        }

        let s = stsd.avcc.sequence_parameter_sets.first().map(|s| s.bytes.clone()).ok_or_else(|| FormatError::Corrupt("missing SPS".into()))?;
        let p = stsd.avcc.picture_parameter_sets.first().map(|p| p.bytes.clone()).ok_or_else(|| FormatError::Corrupt("missing PPS".into()))?;
        (s, p)
    };

    let sample = mp4.read_sample(track_id, 1)
        .map_err(|e| FormatError::Corrupt(e.to_string()))?
        .ok_or_else(|| FormatError::Corrupt("could not read first video sample".into()))?;

    let mut annex_b = Vec::with_capacity(sample.bytes.len() + 64);
    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    annex_b.extend_from_slice(&sps);
    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    annex_b.extend_from_slice(&pps);

    let mut offset = 0;
    while offset + 4 <= sample.bytes.len() {
        let b = &sample.bytes;
        let len = u32::from_be_bytes([
            b.get(offset).copied().unwrap_or(0),
            b.get(offset + 1).copied().unwrap_or(0),
            b.get(offset + 2).copied().unwrap_or(0),
            b.get(offset + 3).copied().unwrap_or(0),
        ]) as usize;
        offset += 4;
        if offset + len <= sample.bytes.len() {
            annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            annex_b.extend_from_slice(&b[offset..offset + len]);
            offset += len;
        } else {
            break;
        }
    }

    let mut decoder = rusty_h264::Decoder::new();
    let frames = decoder
        .decode_stream(&annex_b)
        .map_err(|e| FormatError::Corrupt(format!("failed to decode video poster frame: {e:?}")))?;

    let f0 = frames.first().ok_or_else(|| FormatError::Corrupt("decoder produced no poster frame".into()))?;
    let fw = f0.width;
    let fh = f0.height;
    if fw == 0 || fh == 0 {
        return Err(FormatError::Corrupt("invalid video frame dimensions".into()));
    }

    let mut rgba = vec![0u8; fw * fh * 4];
    yuv420_to_rgba(&f0.y, &f0.u, &f0.v, fw, fh, &mut rgba);

    let img = image::RgbaImage::from_raw(fw as u32, fh as u32, rgba)
        .ok_or_else(|| FormatError::Corrupt("failed to construct RGBA poster image".into()))?;
    let mut png_bytes = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .map_err(|e| FormatError::Corrupt(format!("failed to encode poster PNG: {e}")))?;

    let w = if track_w > 0 { track_w } else { fw as u32 };
    let h = if track_h > 0 { track_h } else { fh as u32 };

    Ok((w, h, duration, png_bytes))
}

/// Register encoded image bytes with the document; returns the hash and pixel size.
pub fn add_image(doc: &mut Document, bytes: Vec<u8>) -> Result<(String, u32, u32), FormatError> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| FormatError::Corrupt(e.to_string()))?
        .into_dimensions()
        .map_err(|e| FormatError::Corrupt(format!("unsupported image: {e}")))?;
    let hash = image_hash(&bytes);
    doc.images.insert(hash.clone(), ImageData { bytes: Arc::new(bytes), width: w, height: h });
    Ok((hash, w, h))
}

/// Register encoded video bytes with the document; extracts a poster frame, inserts it into
/// `doc.images`, inserts the video into `doc.videos`, and returns `(video_hash, poster_hash, width, height, duration)`.
pub fn add_video(doc: &mut Document, bytes: Vec<u8>) -> Result<(String, String, u32, u32, f64), FormatError> {
    let (w, h, duration, poster_png) = extract_video_metadata_and_poster(&bytes)?;
    let v_hash = video_hash(&bytes);
    let p_hash = image_hash(&poster_png);

    doc.images.insert(p_hash.clone(), ImageData {
        bytes: Arc::new(poster_png),
        width: w,
        height: h,
    });

    doc.videos.insert(v_hash.clone(), VideoData {
        bytes: Arc::new(bytes),
        width: w,
        height: h,
        duration_seconds: duration,
    });

    Ok((v_hash, p_hash, w, h, duration))
}

/// Image bytes with their pixel size (0×0 when they can't be decoded).
pub(crate) fn image_data(data: Vec<u8>) -> ImageData {
    let (w, h) =
        image::ImageReader::new(std::io::Cursor::new(&data)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok()).unwrap_or((0, 0));
    ImageData { bytes: Arc::new(data), width: w, height: h }
}

/// Video bytes with their pixel size and duration (0×0, 0.0s when they can't be decoded).
pub(crate) fn video_data(data: Vec<u8>) -> VideoData {
    let (w, h, duration) = read_video_metadata(&data).unwrap_or((0, 0, 0.0));
    VideoData { bytes: Arc::new(data), width: w, height: h, duration_seconds: duration }
}

/// `document.json`: the format header, the image index, the video index, and the document's own fields, written
/// straight from the document (no intermediate JSON tree).
#[derive(Serialize)]
struct DocumentJson<'a> {
    format: &'static str,
    version: u32,
    #[serde(rename = "imageIndex", serialize_with = "image_index")]
    images: &'a BTreeMap<String, ImageData>,
    #[serde(rename = "videoIndex", serialize_with = "video_index", skip_serializing_if = "BTreeMap::is_empty")]
    videos: &'a BTreeMap<String, VideoData>,
    #[serde(flatten)]
    doc: &'a Document,
}

impl<'a> DocumentJson<'a> {
    fn new(doc: &'a Document) -> Self {
        Self { format: FORMAT, version: VERSION, images: &doc.images, videos: &doc.videos, doc }
    }
}

/// The `imageIndex`: `[{hash, width, height}]` for every image the document holds.
fn image_index<S: Serializer>(images: &&BTreeMap<String, ImageData>, s: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Entry<'a> {
        hash: &'a str,
        width: u32,
        height: u32,
    }
    s.collect_seq(images.iter().map(|(hash, i)| Entry { hash, width: i.width, height: i.height }))
}

/// The `videoIndex`: `[{hash, width, height, durationSeconds}]` for every video the document holds.
fn video_index<S: Serializer>(videos: &&BTreeMap<String, VideoData>, s: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Entry<'a> {
        hash: &'a str,
        width: u32,
        height: u32,
        #[serde(rename = "durationSeconds")]
        duration_seconds: f64,
    }
    s.collect_seq(videos.iter().map(|(hash, v)| Entry {
        hash,
        width: v.width,
        height: v.height,
        duration_seconds: v.duration_seconds,
    }))
}

/// Serialize the document (without images) to the JSON value stored in `document.json`.
pub fn to_json(doc: &Document) -> Result<Value, FormatError> {
    serde_json::to_value(DocumentJson::new(doc)).map_err(io_err)
}

/// Write the document (without images) as `document.json` content to `w`.
pub fn write_json(doc: &Document, w: impl Write) -> Result<(), FormatError> {
    serde_json::to_writer(w, &DocumentJson::new(doc)).map_err(io_err)
}

/// Parse `document.json` content.
pub fn from_json(text: &str) -> Result<(Document, Vec<String>), FormatError> {
    parse_json(text).map(|(doc, issues, _, _)| (doc, issues))
}

/// The image hashes a `document.json` lists in its `imageIndex` (`None` without one).
type ImageIndex = Option<BTreeSet<String>>;
/// The video hashes a `document.json` lists in its `videoIndex` (`None` without one).
type VideoIndex = Option<BTreeSet<String>>;

/// The header of a `document.json` (its format version, image index and video index), read in a pass that
/// skips over everything else without building it.
#[derive(Default)]
struct Header {
    version: Option<u64>,
    images: ImageIndex,
    videos: VideoIndex,
}

impl<'de> Deserialize<'de> for Header {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct HeaderVisitor;
        impl<'de> Visitor<'de> for HeaderVisitor {
            type Value = Header;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a FigCraft document object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Header, A::Error> {
                let mut h = Header::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" => h.version = map.next_value::<Value>()?.as_u64(),
                        "imageIndex" => {
                            h.images = map
                                .next_value::<Value>()?
                                .as_array()
                                .map(|a| a.iter().filter_map(|i| i.get("hash").and_then(Value::as_str).map(str::to_string)).collect());
                        }
                        "videoIndex" => {
                            h.videos = map
                                .next_value::<Value>()?
                                .as_array()
                                .map(|a| a.iter().filter_map(|i| i.get("hash").and_then(Value::as_str).map(str::to_string)).collect());
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(h)
            }
        }
        d.deserialize_map(HeaderVisitor)
    }
}

/// Parse `document.json` content; also returns its [`ImageIndex`] and [`VideoIndex`]. The header is checked first
/// (a newer file is refused before its document is read), then the document is deserialized
/// straight from the text.
fn parse_json(text: &str) -> Result<(Document, Vec<String>, ImageIndex, VideoIndex), FormatError> {
    let corrupt = |e: serde_json::Error| FormatError::Corrupt(e.to_string());
    let header: Header = serde_json::from_str(text).map_err(corrupt)?;
    if let Some(ver) = header.version
        && ver > u64::from(VERSION)
    {
        return Err(FormatError::Newer(u32::try_from(ver).unwrap_or(u32::MAX)));
    }
    let mut doc: Document = serde_json::from_str(text).map_err(corrupt)?;
    let issues = doc.validate();
    if !issues.is_empty() {
        doc.repair();
    }
    Ok((doc, issues, header.images, header.videos))
}

pub(crate) fn io_err(e: impl std::fmt::Display) -> FormatError {
    FormatError::Io(e.to_string())
}

/// Add one member to the archive.
pub(crate) fn put<W: Write + Seek>(
    z: &mut zip::ZipWriter<W>,
    name: &str,
    opts: zip::write::SimpleFileOptions,
    bytes: &[u8],
) -> Result<(), FormatError> {
    z.start_file(name, opts).map_err(io_err)?;
    z.write_all(bytes).map_err(io_err)
}

/// Add `doc` as a `document.json`-style member, serialized straight into the compressor.
fn put_json<W: Write + Seek>(z: &mut zip::ZipWriter<W>, name: &str, doc: &Document) -> Result<(), FormatError> {
    z.start_file(name, json_options()).map_err(io_err)?;
    // The serializer writes many small pieces: batch them before they reach the compressor.
    let mut w = std::io::BufWriter::with_capacity(1 << 16, z);
    write_json(doc, &mut w)?;
    w.flush().map_err(io_err)
}

/// How JSON members are compressed. Document JSON is very repetitive: a fast level compresses it
/// nearly as well as the default one (about 15% larger) in half the time.
pub(crate) fn json_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(JSON_LEVEL))
}

/// Write a `.figcraft` archive to bytes.
pub fn save_bytes(doc: &Document, thumbnail_png: Option<&[u8]>) -> Result<Vec<u8>, FormatError> {
    save_bytes_with_versions(doc, thumbnail_png, &[])
}

/// Write a `.figcraft` archive with a version history (oldest first) to bytes.
pub fn save_bytes_with_versions(doc: &Document, thumbnail_png: Option<&[u8]>, versions: &[Version]) -> Result<Vec<u8>, FormatError> {
    save_bytes_with_history(doc, thumbnail_png, versions, &[])
}

/// Write a `.figcraft` archive with a version history (oldest first) and branches to bytes.
pub fn save_bytes_with_history(
    doc: &Document,
    thumbnail_png: Option<&[u8]>,
    versions: &[Version],
    branches: &[Branch],
) -> Result<Vec<u8>, FormatError> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        put_json(&mut z, "document.json", doc)?;
        // Every image once, whether the document, a version or a branch uses it.
        let mut written = BTreeSet::new();
        let branch_images = branches.iter().flat_map(|b| [&b.base.images, &b.head.images].into_iter().chain(b.versions.iter().map(Version::images)));
        for (hash, img) in std::iter::once(&doc.images).chain(versions.iter().map(Version::images)).chain(branch_images).flatten() {
            if written.insert(hash) {
                put(&mut z, &format!("images/{hash}"), stored, &img.bytes)?;
            }
        }
        // Every video once.
        let mut written_videos = BTreeSet::new();
        for (hash, vid) in &doc.videos {
            if written_videos.insert(hash) {
                put(&mut z, &format!("videos/{hash}"), stored, &vid.bytes)?;
            }
        }
        if let Some(t) = thumbnail_png {
            put(&mut z, "thumbnail.png", stored, t)?;
        }
        put_versions(&mut z, "versions", versions)?;
        if !branches.is_empty() {
            let mut ids = BTreeSet::new();
            let mut index = Vec::with_capacity(branches.len());
            for b in branches {
                if !is_valid_version_id(&b.id) || !ids.insert(b.id.as_str()) {
                    return Err(FormatError::Io(format!("invalid or duplicate branch id `{}`", b.id)));
                }
                put_json(&mut z, &format!("branches/{}/base.json", b.id), &b.base)?;
                put_json(&mut z, &format!("branches/{}/head.json", b.id), &b.head)?;
                put_versions(&mut z, &format!("branches/{}/versions", b.id), &b.versions)?;
                let images = |d: &Document| d.images.keys().cloned().collect::<Vec<String>>();
                index.push(json!({"id": b.id, "name": b.name, "time": b.time, "client": b.client, "state": b.state,
                    "baseImages": images(&b.base), "headImages": images(&b.head)}));
            }
            put(&mut z, BRANCH_INDEX, json_options(), &serde_json::to_vec(&json!({"branches": index})).map_err(io_err)?)?;
        }
        if let Some(lib) = &doc.published {
            library::write(&mut z, lib, &written)?;
        }
        z.finish().map_err(io_err)?;
    }
    Ok(buf.into_inner())
}

/// Write version history `versions` (oldest first) under `dir`: `<dir>/index.json` and one
/// `<dir>/<id>.json` snapshot per version. Nothing when there are none.
fn put_versions<W: Write + Seek>(z: &mut zip::ZipWriter<W>, dir: &str, versions: &[Version]) -> Result<(), FormatError> {
    if versions.is_empty() {
        return Ok(());
    }
    let mut ids = BTreeSet::new();
    let mut index = Vec::with_capacity(versions.len());
    for v in versions {
        if !is_valid_version_id(&v.id) || !ids.insert(v.id.as_str()) {
            return Err(FormatError::Io(format!("invalid or duplicate version id `{}`", v.id)));
        }
        let name = format!("{dir}/{}.json", v.id);
        match &v.snapshot {
            Snapshot::Doc(d) => put_json(z, &name, d)?,
            Snapshot::Stored { json, .. } => put(z, &name, json_options(), json)?,
        }
        let images: Vec<&String> = v.images().keys().collect();
        let mut entry = json!({"id": v.id, "name": v.name, "description": v.description, "time": v.time, "images": images});
        if v.auto {
            entry["auto"] = json!(true);
        }
        index.push(entry);
    }
    put(z, &format!("{dir}/index.json"), json_options(), &serde_json::to_vec(&json!({"versions": index})).map_err(io_err)?)?;
    Ok(())
}

/// Read a `.figcraft` archive (or bare JSON) from bytes. Returns the document and any repairs made.
pub fn load_bytes(bytes: &[u8]) -> Result<(Document, Vec<String>), FormatError> {
    load_bytes_with_versions(bytes).map(|l| (l.doc, l.issues))
}

/// Read archive member `name`, refusing members larger than `limit` bytes.
pub(crate) fn read_member<R: Read + Seek>(z: &mut zip::ZipArchive<R>, name: &str, limit: u64) -> Result<Vec<u8>, FormatError> {
    let f = z.by_name(name).map_err(|e| FormatError::Corrupt(format!("{name}: {e}")))?;
    let too_large = || FormatError::Corrupt(format!("{name} is too large"));
    if f.size() > limit {
        return Err(too_large());
    }
    let mut data = Vec::new();
    f.take(limit.saturating_add(1)).read_to_end(&mut data).map_err(|e| FormatError::Corrupt(format!("{name}: {e}")))?;
    if data.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(data)
}

/// How many images, and how many bytes of them together, one file may give.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImageBudget {
    pub images: usize,
    pub bytes: u64,
}

/// Read a `.figcraft` archive (or bare JSON) with its version history.
pub fn load_bytes_with_versions(bytes: &[u8]) -> Result<Loaded, FormatError> {
    load_within(bytes, IMAGE_BUDGET)
}

/// [`load_bytes_with_versions`] reading at most `budget` of images.
pub(crate) fn load_within(bytes: &[u8], budget: ImageBudget) -> Result<Loaded, FormatError> {
    let trimmed = bytes.iter().position(|b| !b.is_ascii_whitespace()).and_then(|i| bytes.get(i));
    if trimmed == Some(&b'{') {
        let text = std::str::from_utf8(bytes).map_err(|e| FormatError::Corrupt(e.to_string()))?;
        let (doc, issues) = from_json(text)?;
        return Ok(Loaded { issues, ..Loaded::from(doc) });
    }
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| FormatError::Corrupt(e.to_string()))?;
    let text = String::from_utf8(read_member(&mut z, "document.json", MAX_MEMBER)?).map_err(|e| FormatError::Corrupt(e.to_string()))?;
    let (mut doc, mut issues, own_images, own_videos) = parse_json(&text)?;
    let (images, unread) = read_images(&mut z, own_images.as_ref(), budget);
    report_lost_images(&mut issues, &unread, own_images.as_ref(), &images);
    let (videos, unread_vids) = read_videos(&mut z, own_videos.as_ref(), VIDEO_BUDGET);
    report_lost_videos(&mut issues, &unread_vids, own_videos.as_ref(), &videos);
    match library::read(&mut z, &images, u64::MAX) {
        Ok(lib) => doc.published = lib.map(Arc::new),
        Err(e) => issues.push(format!("the published library was skipped: {e}")),
    }
    let mut budget = MAX_VERSION_BYTES;
    let versions = read_versions(&mut z, "versions", &images, &mut issues, &mut budget);
    let branches = read_branches(&mut z, &images, &mut issues);
    // Images only the version history or branches use stay with them.
    let branch_images =
        branches.iter().flat_map(|b| b.base.images.keys().chain(b.head.images.keys()).chain(b.versions.iter().flat_map(|v| v.images().keys())));
    let history_only: BTreeSet<&String> = versions
        .iter()
        .flat_map(|v| v.images().keys())
        .chain(branch_images)
        .filter(|h| own_images.as_ref().is_some_and(|o| !o.contains(*h)))
        .collect();
    doc.images = images.into_iter().filter(|(h, _)| !history_only.contains(h)).collect();
    doc.videos = videos;
    Ok(Loaded { doc, issues, versions, branches })
}

/// The archive's `images/` members by hash, within `budget`, and the ones that couldn't be read
/// with why. The document's own images (`own`) are read first, so a file past the budget loses
/// the images only its history uses before its own.
fn read_images<R: Read + Seek>(
    z: &mut zip::ZipArchive<R>,
    own: Option<&BTreeSet<String>>,
    budget: ImageBudget,
) -> (BTreeMap<String, ImageData>, Vec<(String, FormatError)>) {
    let mut hashes: Vec<String> = z.file_names().filter_map(|n| n.strip_prefix("images/")).map(str::to_string).collect();
    hashes.sort_by_key(|h| !own.is_some_and(|o| o.contains(h)));
    let mut images = BTreeMap::new();
    let mut unread = Vec::new();
    let mut left = budget.bytes;
    for (i, hash) in hashes.into_iter().enumerate() {
        let name = format!("images/{hash}");
        if i >= budget.images {
            unread.push((hash, FormatError::Corrupt(format!("the file has more than {} images", budget.images))));
            continue;
        }
        if z.by_name(&name).is_ok_and(|f| f.size() > left) {
            let mb = budget.bytes / (1024 * 1024);
            unread.push((hash, FormatError::Corrupt(format!("the file's images together pass {mb} MB"))));
            continue;
        }
        match read_member(z, &name, left.min(MAX_MEMBER)) {
            Ok(data) => {
                left = left.saturating_sub(data.len() as u64);
                images.insert(hash, image_data(data));
            }
            Err(e) => unread.push((hash, e)),
        }
    }
    (images, unread)
}

/// Most lost images reported one by one (the rest as a count).
const MAX_IMAGE_ISSUES: usize = 20;

/// Report the images a file lost on loading: members that couldn't be read (damaged, too large)
/// and images its document lists but the archive doesn't hold. They are left out, so saving the
/// file again would drop them for good.
fn report_lost_images(
    issues: &mut Vec<String>,
    unread: &[(String, FormatError)],
    listed: Option<&BTreeSet<String>>,
    images: &BTreeMap<String, ImageData>,
) {
    let mut lost: Vec<String> = unread
        .iter()
        .map(|(hash, e)| {
            let why = match e {
                FormatError::Corrupt(m) | FormatError::Io(m) => m.clone(),
                FormatError::Newer(v) => format!("format version {v}"),
            };
            format!("image {hash} couldn't be read ({why}): it is left out, and saving this file again drops it")
        })
        .collect();
    let unread_hashes: BTreeSet<&str> = unread.iter().map(|(h, _)| h.as_str()).collect();
    for hash in listed.into_iter().flatten() {
        if !images.contains_key(hash) && !unread_hashes.contains(hash.as_str()) {
            lost.push(format!("image {hash} is used by the file but its data is missing: it is left out, and saving this file again drops it"));
        }
    }
    let more = lost.len().saturating_sub(MAX_IMAGE_ISSUES);
    lost.truncate(MAX_IMAGE_ISSUES);
    issues.extend(lost);
    if more > 0 {
        issues.push(format!("{more} more images couldn't be loaded"));
    }
}

/// Most lost videos reported one by one (the rest as a count).
const MAX_VIDEO_ISSUES: usize = 20;

/// Report the videos a file lost on loading: members that couldn't be read (damaged, too large, hash mismatch)
/// and videos its document lists but the archive doesn't hold.
fn report_lost_videos(
    issues: &mut Vec<String>,
    unread: &[(String, FormatError)],
    listed: Option<&BTreeSet<String>>,
    videos: &BTreeMap<String, VideoData>,
) {
    let mut lost: Vec<String> = unread
        .iter()
        .map(|(hash, e)| {
            let why = match e {
                FormatError::Corrupt(m) | FormatError::Io(m) => m.clone(),
                FormatError::Newer(v) => format!("format version {v}"),
            };
            format!("video {hash} couldn't be read ({why}): it is left out, and saving this file again drops it")
        })
        .collect();
    let unread_hashes: BTreeSet<&str> = unread.iter().map(|(h, _)| h.as_str()).collect();
    for hash in listed.into_iter().flatten() {
        if !videos.contains_key(hash) && !unread_hashes.contains(hash.as_str()) {
            lost.push(format!("video {hash} is used by the file but its data is missing: it is left out, and saving this file again drops it"));
        }
    }
    let more = lost.len().saturating_sub(MAX_VIDEO_ISSUES);
    lost.truncate(MAX_VIDEO_ISSUES);
    issues.extend(lost);
    if more > 0 {
        issues.push(format!("{more} more videos couldn't be loaded"));
    }
}

/// How many videos, and how many bytes of them together, one file may give.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoBudget {
    pub videos: usize,
    pub bytes: u64,
}

pub(crate) const VIDEO_BUDGET: VideoBudget = VideoBudget {
    videos: 50,
    bytes: 512 * 1024 * 1024,
};

/// The archive's `videos/` members by hash, within `budget`, and the ones that couldn't be read
/// with why.
fn read_videos<R: Read + Seek>(
    z: &mut zip::ZipArchive<R>,
    own: Option<&BTreeSet<String>>,
    budget: VideoBudget,
) -> (BTreeMap<String, VideoData>, Vec<(String, FormatError)>) {
    let mut hashes: Vec<String> = z.file_names().filter_map(|n| n.strip_prefix("videos/")).map(str::to_string).collect();
    hashes.sort_by_key(|h| !own.is_some_and(|o| o.contains(h)));
    let mut videos = BTreeMap::new();
    let mut unread = Vec::new();
    let mut left = budget.bytes;
    for (i, hash) in hashes.into_iter().enumerate() {
        let name = format!("videos/{hash}");
        if i >= budget.videos {
            unread.push((hash, FormatError::Corrupt(format!("the file has more than {} videos", budget.videos))));
            continue;
        }
        if z.by_name(&name).is_ok_and(|f| f.size() > left) {
            let mb = budget.bytes / (1024 * 1024);
            unread.push((hash, FormatError::Corrupt(format!("the file's videos together pass {mb} MB"))));
            continue;
        }
        match read_member(z, &name, left.min(MAX_MEMBER)) {
            Ok(data) => {
                let actual = video_hash(&data);
                if actual != hash {
                    unread.push((hash, FormatError::Corrupt("video content hash mismatch".into())));
                    continue;
                }
                left = left.saturating_sub(data.len() as u64);
                videos.insert(hash, video_data(data));
            }
            Err(e) => unread.push((hash, e)),
        }
    }
    (videos, unread)
}

/// The version history kept under `dir` in an archive (none when it has no index), reading at
/// most `budget` bytes of snapshots. Damaged entries are skipped and reported in `issues`; nothing
/// here fails the load.
fn read_versions<R: Read + Seek>(
    z: &mut zip::ZipArchive<R>,
    dir: &str,
    images: &BTreeMap<String, ImageData>,
    issues: &mut Vec<String>,
    budget: &mut u64,
) -> Vec<Version> {
    let index_name = format!("{dir}/index.json");
    if z.index_for_name(&index_name).is_none() {
        return Vec::new();
    }
    let index = match read_member(z, &index_name, MAX_VERSION_INDEX)
        .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| FormatError::Corrupt(e.to_string())))
    {
        Ok(v) => v,
        Err(e) => {
            issues.push(format!("the version history was skipped: {e}"));
            return Vec::new();
        }
    };
    let entries = index.get("versions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let mut out: Vec<Version> = Vec::new();
    for e in entries {
        if out.len() >= MAX_VERSIONS {
            issues.push(format!("only the first {MAX_VERSIONS} versions were kept"));
            break;
        }
        let id = e.get("id").and_then(Value::as_str).unwrap_or_default();
        if !is_valid_version_id(id) || out.iter().any(|v| v.id == id) {
            issues.push("a version with an invalid id was skipped".into());
            continue;
        }
        let json = match read_member(z, &format!("{dir}/{id}.json"), (*budget).min(MAX_MEMBER)) {
            Ok(j) => j,
            Err(err) => {
                issues.push(format!("version `{id}` was skipped: {err}"));
                continue;
            }
        };
        *budget = budget.saturating_sub(json.len() as u64);
        let text = |k: &str, max: usize| e.get(k).and_then(Value::as_str).map(|s| clip(s, max)).unwrap_or_default();
        let images = e
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|h| images.get(h).map(|i| (h.to_string(), i.clone())))
            .collect();
        out.push(Version {
            id: id.to_string(),
            name: text("name", MAX_VERSION_NAME),
            description: text("description", MAX_VERSION_DESCRIPTION),
            time: e.get("time").and_then(Value::as_u64).unwrap_or(0),
            auto: e.get("auto").and_then(Value::as_bool).unwrap_or(false),
            snapshot: Snapshot::Stored { json: Arc::new(json), images },
        });
    }
    out
}

/// The branches of an archive (none when it has no index), parsed. Damaged entries are skipped and
/// reported in `issues`; nothing here fails the load.
fn read_branches<R: Read + Seek>(z: &mut zip::ZipArchive<R>, images: &BTreeMap<String, ImageData>, issues: &mut Vec<String>) -> Vec<Branch> {
    if z.index_for_name(BRANCH_INDEX).is_none() {
        return Vec::new();
    }
    let index = match read_member(z, BRANCH_INDEX, MAX_VERSION_INDEX)
        .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| FormatError::Corrupt(e.to_string())))
    {
        Ok(v) => v,
        Err(e) => {
            issues.push(format!("the branches were skipped: {e}"));
            return Vec::new();
        }
    };
    let entries = index.get("branches").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let mut out: Vec<Branch> = Vec::new();
    let mut budget = MAX_VERSION_BYTES;
    for e in entries {
        if out.len() >= MAX_BRANCHES {
            issues.push(format!("only the first {MAX_BRANCHES} branches were kept"));
            break;
        }
        let id = e.get("id").and_then(Value::as_str).unwrap_or_default();
        if !is_valid_version_id(id) || out.iter().any(|b| b.id == id) {
            issues.push("a branch with an invalid id was skipped".into());
            continue;
        }
        let mut snapshot = |part: &str, list: &str| -> Result<Arc<Document>, FormatError> {
            let json = read_member(z, &format!("branches/{id}/{part}.json"), budget.min(MAX_MEMBER))?;
            budget = budget.saturating_sub(json.len() as u64);
            let (mut doc, _) = from_json(std::str::from_utf8(&json).map_err(|e| FormatError::Corrupt(e.to_string()))?)?;
            let hashes = e.get(list).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
            doc.images = hashes.filter_map(|h| images.get(h).map(|i| (h.to_string(), i.clone()))).collect();
            Ok(Arc::new(doc))
        };
        let (base, head) = match (snapshot("base", "baseImages"), snapshot("head", "headImages")) {
            (Ok(b), Ok(h)) => (b, h),
            (Err(err), _) | (_, Err(err)) => {
                issues.push(format!("branch `{id}` was skipped: {err}"));
                continue;
            }
        };
        out.push(Branch {
            id: id.to_string(),
            name: e.get("name").and_then(Value::as_str).map(|s| clip(s, MAX_VERSION_NAME)).unwrap_or_default(),
            time: e.get("time").and_then(Value::as_u64).unwrap_or(0),
            client: e.get("client").and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok()).unwrap_or(0),
            state: e.get("state").and_then(|s| BranchState::deserialize(s).ok()).unwrap_or(BranchState::Active),
            versions: read_versions(z, &format!("branches/{id}/versions"), images, issues, &mut budget),
            base,
            head,
        });
    }
    out
}

/// Save atomically: write a staging file next to `path`, then rename over it.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_file(doc: &Document, path: &std::path::Path, thumbnail_png: Option<&[u8]>) -> Result<(), FormatError> {
    let bytes = save_bytes(doc, thumbnail_png)?;
    write_file_atomic(path, &bytes).map_err(|e| FormatError::Io(format!("{}: {e}", path.display())))
}

/// Write `bytes` to `path` through a staging file and one atomic rename, so a reader of `path`
/// sees either the previous file or the complete new one, never a partial write. The staging file
/// is a uniquely named sibling (`<path>.tmp-figcraft-<process id>-<counter>`) created exclusively
/// (`create_new`) and written through its own handle: a file or symlink that already exists at a
/// staging name is never truncated or followed, and separate writers (also of separate processes)
/// never share a staging file. On failure only the staging file this call created is removed, and
/// `path` is replaced only on success. The rename replaces an existing destination on every
/// platform (`std::fs::rename` uses `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` on Windows).
/// (No `sync_all` yet: nothing around here promises durability across a power cut.)
#[cfg(not(target_arch = "wasm32"))]
pub fn write_file_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static STAGING: AtomicU64 = AtomicU64::new(0);
    // Three tries at a fresh name: the counter is unique within this process, so only a leftover
    // of a dead process that now shares our process id can collide, and the next counter won't.
    for _ in 0..3 {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".tmp-figcraft-{}-{}", std::process::id(), STAGING.fetch_add(1, Relaxed)));
        let tmp = std::path::PathBuf::from(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                return match std::fs::rename(&tmp, path) {
                    Ok(()) => Ok(()),
                    // Leave nothing behind when the file can't take its place.
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        Err(e)
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free staging name next to the file"))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_file(path: &std::path::Path) -> Result<(Document, Vec<String>), FormatError> {
    let bytes = std::fs::read(path).map_err(|e| FormatError::Io(format!("{}: {e}", path.display())))?;
    load_bytes(&bytes)
}

#[cfg(test)]
mod branch_tests;
#[cfg(test)]
mod image_budget_tests;
#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod large_tests;
#[cfg(test)]
mod staging_tests;
#[cfg(test)]
mod thumbnail_tests;
#[cfg(test)]
mod version_tests;
#[cfg(test)]
mod video_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_doc::NodeKind;

    fn png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn roundtrip_with_images() {
        let mut d = Document::new("My file");
        let p = d.first_page().unwrap();
        d.create(p, NodeKind::Rectangle, "R", 1.0, 2.0, 3.0, 4.0).unwrap();
        let (hash, w, h) = add_image(&mut d, png()).unwrap();
        assert_eq!((w, h), (3, 2));
        let bytes = save_bytes(&d, None).unwrap();
        let (back, issues) = load_bytes(&bytes).unwrap();
        assert!(issues.is_empty());
        assert_eq!(back.name, "My file");
        assert_eq!(back.node_count(), d.node_count());
        assert!(back.images.contains_key(&hash));
        // Bare JSON opens too.
        let json = serde_json::to_vec(&to_json(&d).unwrap()).unwrap();
        assert!(load_bytes(&json).is_ok());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(load_bytes(b"not a zip").is_err());
        assert!(load_bytes(b"{\"nope\":1}").is_err());
        assert!(matches!(load_bytes(b"{\"version\":999}"), Err(FormatError::Newer(999))));
        assert!(add_image(&mut Document::new("x"), b"zzz".to_vec()).is_err());
    }
}
