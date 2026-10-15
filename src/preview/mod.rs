//! Asynchronous, cancellable previews for the selected file.
//!
//! All file content is treated as untrusted input: reads are bounded,
//! decoders get explicit limits, and anything that runs third-party or
//! complex native code (PDF, Shell thumbnails) runs in a helper process.

pub mod helper;
pub mod office;

use std::collections::VecDeque;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

use crate::model::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM};
use crate::util;

/// Files larger than this are not decoded as images.
pub const MAX_IMAGE_FILE: u64 = 128 * 1024 * 1024;
/// Text previews read at most this many bytes.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Longest edge of image previews handed to the UI.
pub const PREVIEW_PX: u32 = 1024;
/// Longest edge used when the user zooms in (sharper, more memory).
pub const DETAIL_PX: u32 = 4096;

/// How to render a preview: resolution for images/PDF pages and which
/// PDF page (0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PreviewOptions {
    pub max_px: u32,
    pub page: u32,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        PreviewOptions { max_px: PREVIEW_PX, page: 0 }
    }
}
const HELPER_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_BUDGET_BYTES: usize = 96 * 1024 * 1024;
const CACHE_MAX_ENTRIES: usize = 64;

#[derive(Clone, Debug)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Content {
    Image { image: ImageData, original: (u32, u32), format: String },
    Text { text: String, truncated: bool, encoding: &'static str },
    Pdf { page: ImageData, page_count: u32, index: u32 },
    /// Metadata only, with the Shell thumbnail/icon when one is available.
    Info { icon: Option<ImageData>, note: Option<String> },
    Folder { entries: Vec<(String, bool)>, total: usize, note: Option<String> },
    Document { blocks: Vec<office::DocBlock>, truncated: bool },
    Sheets { sheets: Vec<office::Sheet> },
}

#[derive(Clone, Debug, Default)]
pub struct FileMeta {
    pub size: u64,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    pub attrs: u32,
    pub is_dir: bool,
    pub readonly: bool,
}

impl FileMeta {
    pub fn attribute_labels(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.readonly {
            v.push("read-only");
        }
        if self.attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
            v.push("hidden");
        }
        if self.attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
            v.push("system");
        }
        if self.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            v.push("link/reparse point");
        }
        if self.attrs & 0x0040_0000 != 0 || self.attrs & 0x0000_1000 != 0 {
            v.push("online-only");
        }
        v
    }
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub id: u64,
    pub path: String,
    pub opts: PreviewOptions,
    pub meta: Option<FileMeta>,
    /// `Err` holds a user-facing explanation (unreadable, corrupt...).
    pub content: Result<Content, String>,
    pub kind_label: String,
    pub elapsed: Duration,
}

impl Preview {
    fn cost(&self) -> usize {
        let c = match &self.content {
            Ok(Content::Image { image, .. }) | Ok(Content::Pdf { page: image, .. }) => image.rgba.len(),
            Ok(Content::Text { text, .. }) => text.len(),
            Ok(Content::Info { icon: Some(i), .. }) => i.rgba.len(),
            Ok(Content::Folder { entries, .. }) => entries.iter().map(|e| e.0.len() + 8).sum(),
            Ok(Content::Document { blocks, .. }) => blocks.len() * 160,
            Ok(Content::Sheets { sheets }) => sheets.iter().map(|s| s.rows.len() * s.rows.first().map_or(1, |r| r.len()) * 24).sum(),
            _ => 0,
        };
        c + self.path.len() + 256
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Text,
    Pdf,
    Folder,
    Word,
    Excel,
    Other,
}

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "webp", "ico", "tif", "tiff"];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "jsonc", "xml", "yaml", "yml", "toml",
    "ini", "cfg", "conf", "config", "properties", "env", "gitignore", "gitattributes", "editorconfig",
    "rs", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "java", "kt", "kts", "go", "py", "pyw",
    "rb", "php", "pl", "pm", "lua", "js", "mjs", "cjs", "jsx", "ts", "tsx", "vue", "svelte", "html",
    "htm", "css", "scss", "sass", "less", "sql", "sh", "bash", "zsh", "fish", "ps1", "psm1", "psd1",
    "bat", "cmd", "vbs", "swift", "m", "mm", "r", "scala", "dart", "fs", "fsx", "vb", "asm", "s",
    "cmake", "make", "mk", "gradle", "sln", "csproj", "vcxproj", "props", "targets", "resx", "xaml",
    "manifest", "reg", "inf", "srt", "vtt", "tex", "bib", "svg", "diff", "patch", "lock", "dockerfile",
    "nfo", "url", "desktop", "graphql", "proto", "hlsl", "glsl", "zig", "nim", "ex", "exs", "erl",
    "hs", "clj", "lisp", "el", "jl", "ipynb",
];
/// Never sniffed as text even if the bytes look textual.
const BINARY_EXTS: &[&str] = &[
    "exe", "dll", "sys", "msi", "zip", "7z", "rar", "gz", "xz", "bz2", "zst", "iso", "img", "vhd",
    "vhdx", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "mp3", "mp4", "mkv", "avi", "mov", "wav",
    "flac", "ogg", "m4a", "pdb", "obj", "lib", "pak", "bin", "dat", "db", "sqlite",
];

pub fn classify(path: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Folder;
    }
    let name = util::split_path(path).map(|(_, n)| n).unwrap_or(path);
    let ext = util::ext_of(name, false);
    let lname = name.to_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        Kind::Image
    } else if matches!(ext.as_str(), "docx" | "docm" | "dotx" | "dotm") {
        Kind::Word
    } else if matches!(ext.as_str(), "xlsx" | "xlsm" | "xltx" | "xltm") {
        Kind::Excel
    } else if ext == "pdf" {
        Kind::Pdf
    } else if TEXT_EXTS.contains(&ext.as_str())
        || matches!(lname.as_str(), "makefile" | "dockerfile" | "license" | "readme" | "changelog")
    {
        Kind::Text
    } else {
        Kind::Other
    }
}

fn meta_of(path: &str) -> std::io::Result<FileMeta> {
    use std::os::windows::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path)?;
    Ok(FileMeta {
        size: md.file_size(),
        created: util::filetime_to_unix(md.creation_time()),
        modified: util::filetime_to_unix(md.last_write_time()),
        accessed: util::filetime_to_unix(md.last_access_time()),
        attrs: md.file_attributes(),
        is_dir: md.is_dir(),
        readonly: md.permissions().readonly(),
    })
}

fn friendly_io(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "The file no longer exists. The index will catch up shortly.".into(),
        std::io::ErrorKind::PermissionDenied => "Access denied: you do not have permission to read this file.".into(),
        _ => match e.raw_os_error() {
            Some(32) | Some(33) => "The file is locked by another program.".into(),
            Some(21) => "The drive is not ready.".into(),
            _ => format!("Cannot read the file: {e}"),
        },
    }
}

/// Builds a preview synchronously. `cancelled` is polled between stages.
pub fn generate(id: u64, path: &str, is_dir: bool, opts: PreviewOptions, cancelled: &dyn Fn() -> bool) -> Preview {
    let started = Instant::now();
    let kind = classify(path, is_dir);
    let meta = meta_of(path);
    let (meta, content) = match meta {
        Err(e) => (None, Err(friendly_io(&e))),
        Ok(m) => {
            let content = if m.attrs & 0x0040_0000 != 0 {
                // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: reading would download it.
                Ok(Content::Info {
                    icon: None,
                    note: Some("This file is stored online only. It is not downloaded just to preview it.".into()),
                })
            } else {
                match kind {
                    Kind::Folder => folder_preview(path),
                    Kind::Image => image_preview(path, m.size, opts.max_px)
                        .or_else(|err| other_preview(path, cancelled, Some(err))),
                    Kind::Text => text_preview(path, m.size),
                    Kind::Pdf => pdf_preview(path, opts, cancelled),
                    Kind::Word => office::parse_docx(path)
                        .map(|(blocks, truncated)| Content::Document { blocks, truncated })
                        .or_else(|err| other_preview(path, cancelled, Some(err))),
                    Kind::Excel => office::parse_xlsx(path)
                        .map(|sheets| Content::Sheets { sheets })
                        .or_else(|err| other_preview(path, cancelled, Some(err))),
                    Kind::Other => {
                        if !is_known_binary(path)
                            && let Ok(Some(t)) = sniff_text(path, m.size)
                        {
                            Ok(t)
                        } else {
                            other_preview(path, cancelled, None)
                        }
                    }
                }
            };
            (Some(m), content)
        }
    };
    let kind_label = match kind {
        Kind::Image => "Image",
        Kind::Text => "Text",
        Kind::Pdf => "PDF document",
        Kind::Word => "Word document",
        Kind::Excel => "Excel workbook",
        Kind::Folder => "Folder",
        Kind::Other => "File",
    }
    .to_string();
    Preview { id, path: path.to_string(), opts, meta, content, kind_label, elapsed: started.elapsed() }
}

fn is_known_binary(path: &str) -> bool {
    let name = util::split_path(path).map(|(_, n)| n).unwrap_or(path);
    BINARY_EXTS.contains(&util::ext_of(name, false).as_str())
}

fn image_preview(path: &str, size: u64, max_px: u32) -> Result<Content, String> {
    if size > MAX_IMAGE_FILE {
        return Err(format!("Image is too large to preview ({}).", util::format_size(size)));
    }
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| friendly_io(&e))?
        .with_guessed_format()
        .map_err(|e| friendly_io(&e))?;
    let format = reader.format().map(|f| format!("{f:?}")).unwrap_or_else(|| "unknown".into());
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(30_000);
    limits.max_image_height = Some(30_000);
    limits.max_alloc = Some(768 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| format!("The image could not be decoded: {e}"))?;
    let original = (img.width(), img.height());
    // Only the scaled copy leaves this worker.
    let scaled = if img.width() > max_px || img.height() > max_px {
        img.thumbnail(max_px, max_px)
    } else {
        img
    };
    let rgba = scaled.to_rgba8();
    Ok(Content::Image {
        image: ImageData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() },
        original,
        format,
    })
}

fn read_head(path: &str, max: usize) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(max.min(64 * 1024));
    f.take(max as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Decodes text bytes: BOM-marked UTF-8/UTF-16, otherwise UTF-8 with lossy
/// replacement. Control characters other than tab/newline are made visible
/// so untrusted content cannot confuse the renderer.
pub fn decode_text(bytes: &[u8]) -> (String, &'static str) {
    let (mut text, enc) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (String::from_utf8_lossy(rest).into_owned(), "UTF-8 (BOM)")
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        (String::from_utf16_lossy(&u), "UTF-16 LE")
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        (String::from_utf16_lossy(&u), "UTF-16 BE")
    } else {
        match std::str::from_utf8(bytes) {
            Ok(s) => (s.to_string(), "UTF-8"),
            // A multi-byte sequence cut at the read limit is not an error.
            Err(e) if e.error_len().is_none() => {
                (String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned(), "UTF-8")
            }
            Err(_) => (bytes.iter().map(|&b| cp1252(b)).collect(), "Windows-1252"),
        }
    };
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        text = text
            .chars()
            .map(|c| if c.is_control() && !matches!(c, '\n' | '\r' | '\t') { '\u{FFFD}' } else { c })
            .collect();
    }
    (text.replace("\r\n", "\n"), enc)
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

fn text_preview(path: &str, size: u64) -> Result<Content, String> {
    let bytes = read_head(path, MAX_TEXT_BYTES).map_err(|e| friendly_io(&e))?;
    let (text, encoding) = decode_text(&bytes);
    Ok(Content::Text { text, truncated: size > bytes.len() as u64, encoding })
}

/// Heuristic for files without a known extension: no NUL bytes in the
/// first 8 KB (unless UTF-16 BOM) and valid UTF-8.
fn sniff_text(path: &str, size: u64) -> std::io::Result<Option<Content>> {
    if size == 0 {
        return Ok(None);
    }
    let head = read_head(path, 8192)?;
    let utf16 = head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]);
    if !utf16 && (head.contains(&0) || std::str::from_utf8(&head).map_err(|e| e.error_len()).is_err_and(|l| l.is_some())) {
        return Ok(None);
    }
    Ok(text_preview(path, size).ok())
}

fn pdf_preview(path: &str, opts: PreviewOptions, cancelled: &dyn Fn() -> bool) -> Result<Content, String> {
    match helper::render(helper::HelperKind::Pdf, path, opts.max_px, opts.page, HELPER_TIMEOUT, cancelled) {
        Ok(out) => Ok(Content::Pdf { page: out.image, page_count: out.page_count, index: opts.page.min(out.page_count.saturating_sub(1)) }),
        Err(e) if e == "cancelled" => Err(e),
        Err(e) => other_preview(path, cancelled, Some(format!("PDF preview unavailable: {e}"))),
    }
}

fn other_preview(path: &str, cancelled: &dyn Fn() -> bool, note: Option<String>) -> Result<Content, String> {
    let icon = helper::render(helper::HelperKind::Thumbnail, path, 256, 0, Duration::from_secs(8), cancelled).ok();
    Ok(Content::Info { icon: icon.map(|o| o.image), note })
}

fn folder_preview(path: &str) -> Result<Content, String> {
    let rd = std::fs::read_dir(path).map_err(|e| friendly_io(&e))?;
    let mut entries = Vec::new();
    let mut total = 0usize;
    for e in rd.flatten() {
        total += 1;
        if entries.len() < 200 {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push((e.file_name().to_string_lossy().into_owned(), is_dir));
        }
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    Ok(Content::Folder { entries, total, note: None })
}

// ---- service --------------------------------------------------------------

type CacheKey = (String, u64, Option<i64>, PreviewOptions);

struct Request {
    id: u64,
    path: String,
    is_dir: bool,
    opts: PreviewOptions,
    key: CacheKey,
}

/// Background preview generation with cancellation of stale requests and
/// an LRU cache bounded by memory use.
pub struct PreviewService {
    tx: Sender<Request>,
    results: Receiver<Arc<Preview>>,
    latest: Arc<AtomicU64>,
    next_id: u64,
    cache: Arc<Mutex<Lru>>,
}

#[derive(Default)]
struct Lru {
    items: VecDeque<(CacheKey, Arc<Preview>)>,
    bytes: usize,
}

impl Lru {
    fn get(&mut self, key: &CacheKey) -> Option<Arc<Preview>> {
        let pos = self.items.iter().position(|(k, _)| k == key)?;
        let item = self.items.remove(pos)?;
        let p = item.1.clone();
        self.items.push_back(item);
        Some(p)
    }

    fn put(&mut self, key: CacheKey, p: Arc<Preview>) {
        let cost = p.cost();
        if cost > CACHE_BUDGET_BYTES / 2 {
            return;
        }
        if let Some(pos) = self.items.iter().position(|(k, _)| *k == key)
            && let Some((_, old)) = self.items.remove(pos)
        {
            self.bytes -= old.cost();
        }
        self.bytes += cost;
        self.items.push_back((key, p));
        while self.bytes > CACHE_BUDGET_BYTES || self.items.len() > CACHE_MAX_ENTRIES {
            match self.items.pop_front() {
                Some((_, old)) => self.bytes -= old.cost(),
                None => break,
            }
        }
    }
}

impl PreviewService {
    /// `notify` is called (from a worker thread) when a result is ready.
    pub fn new(workers: usize, notify: Arc<dyn Fn() + Send + Sync>) -> PreviewService {
        let (tx, rx) = crossbeam_channel::unbounded::<Request>();
        let (rtx, results) = crossbeam_channel::bounded::<Arc<Preview>>(16);
        let latest = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(Lru::default()));
        for i in 0..workers.max(1) {
            let rx = rx.clone();
            let rtx = rtx.clone();
            let latest = latest.clone();
            let cache = cache.clone();
            let notify = notify.clone();
            std::thread::Builder::new()
                .name(format!("preview-{i}"))
                .spawn(move || {
                    for req in rx.iter() {
                        let is_stale = || latest.load(Ordering::Relaxed) != req.id;
                        if is_stale() {
                            continue; // superseded while queued
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            generate(req.id, &req.path, req.is_dir, req.opts, &is_stale)
                        }));
                        let preview = match result {
                            Ok(p) => p,
                            Err(_) => Preview {
                                id: req.id,
                                path: req.path.clone(),
                                opts: req.opts,
                                meta: None,
                                content: Err("The preview failed unexpectedly.".into()),
                                kind_label: "File".into(),
                                elapsed: Duration::ZERO,
                            },
                        };
                        let cancelled = matches!(&preview.content, Err(e) if e == "cancelled");
                        if cancelled {
                            continue;
                        }
                        let preview = Arc::new(preview);
                        if preview.content.is_ok() {
                            cache.lock().put(req.key.clone(), preview.clone());
                        }
                        if !is_stale() {
                            let _ = rtx.send(preview);
                            notify();
                        }
                    }
                })
                .expect("spawn preview worker");
        }
        PreviewService { tx, results, latest, next_id: 0, cache }
    }

    /// Requests a preview; any earlier request becomes stale. Returns the
    /// cached preview immediately when the file is unchanged.
    pub fn request(
        &mut self,
        path: &str,
        is_dir: bool,
        size: u64,
        modified: Option<i64>,
        opts: PreviewOptions,
    ) -> Option<Arc<Preview>> {
        self.next_id += 1;
        let id = self.next_id;
        self.latest.store(id, Ordering::Relaxed);
        let key = (path.to_lowercase(), size, modified, opts);
        if !is_dir && let Some(p) = self.cache.lock().get(&key) {
            return Some(p);
        }
        let _ = self.tx.send(Request { id, path: path.to_string(), is_dir, opts, key });
        None
    }

    /// Marks every pending request stale (selection cleared).
    pub fn cancel(&mut self) {
        self.next_id += 1;
        self.latest.store(self.next_id, Ordering::Relaxed);
    }

    /// Next finished preview for the latest request, if any.
    pub fn poll(&self) -> Option<Arc<Preview>> {
        let latest = self.latest.load(Ordering::Relaxed);
        while let Ok(p) = self.results.try_recv() {
            if p.id == latest {
                return Some(p);
            }
        }
        None
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache.lock().bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension() {
        assert_eq!(classify("C:\\a\\x.PNG", false), Kind::Image);
        assert_eq!(classify("C:\\a\\main.rs", false), Kind::Text);
        assert_eq!(classify("C:\\a\\doc.pdf", false), Kind::Pdf);
        assert_eq!(classify("C:\\a\\Makefile", false), Kind::Text);
        assert_eq!(classify("C:\\a\\setup.exe", false), Kind::Other);
        assert_eq!(classify("C:\\a", true), Kind::Folder);
    }

    #[test]
    fn decodes_text_encodings_and_neutralizes_controls() {
        assert_eq!(decode_text(b"hi\r\nthere").0, "hi\nthere");
        assert_eq!(decode_text(&[0xEF, 0xBB, 0xBF, b'a']).0, "a");
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]).0, "hi");
        assert_eq!(decode_text(&[b'a', 0x1B, b'b']).0, "a\u{FFFD}b");
        let (t, enc) = decode_text(&[b'c', 0xE9, b'!']);
        assert_eq!((t.as_str(), enc), ("cé!", "Windows-1252"));
        // Truncated multi-byte char at the end of the read window.
        assert_eq!(decode_text(&[b'a', 0xC3]).0, "a");
    }

    #[test]
    fn text_preview_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.txt");
        std::fs::write(&p, vec![b'x'; MAX_TEXT_BYTES + 1000]).unwrap();
        let pv = generate(1, p.to_str().unwrap(), false, PreviewOptions::default(), &|| false);
        match pv.content {
            Ok(Content::Text { text, truncated, .. }) => {
                assert!(truncated);
                assert_eq!(text.len(), MAX_TEXT_BYTES);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn image_preview_is_scaled_and_corrupt_images_fail_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        image::RgbaImage::from_pixel(3000, 1500, image::Rgba([10, 20, 30, 255])).save(&p).unwrap();
        let pv = generate(1, p.to_str().unwrap(), false, PreviewOptions::default(), &|| false);
        match pv.content {
            Ok(Content::Image { image, original, .. }) => {
                assert_eq!(original, (3000, 1500));
                assert!(image.width <= PREVIEW_PX && image.height <= PREVIEW_PX);
                assert_eq!(image.rgba.len(), (image.width * image.height * 4) as usize);
            }
            other => panic!("unexpected {other:?}"),
        }
        let bad = dir.path().join("bad.jpg");
        std::fs::write(&bad, b"\xFF\xD8\xFFnot really a jpeg").unwrap();
        let pv = generate(2, bad.to_str().unwrap(), false, PreviewOptions::default(), &|| false);
        // Falls back to an info preview that carries the decode error.
        match pv.content {
            Ok(Content::Info { note: Some(n), .. }) => assert!(n.contains("decoded"), "{n}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_file_reports_error() {
        let pv = generate(1, "C:\\definitely\\not\\here.txt", false, PreviewOptions::default(), &|| false);
        assert!(pv.content.is_err());
        assert!(pv.meta.is_none());
    }

    #[test]
    fn lru_respects_budget() {
        let mut lru = Lru::default();
        for i in 0..100 {
            let p = Arc::new(Preview {
                id: i,
                path: format!("p{i}"),
                opts: PreviewOptions::default(),
                meta: None,
                content: Ok(Content::Text { text: "x".repeat(1000), truncated: false, encoding: "UTF-8" }),
                kind_label: String::new(),
                elapsed: Duration::ZERO,
            });
            lru.put((format!("p{i}"), 0, None, PreviewOptions::default()), p);
        }
        assert!(lru.items.len() <= CACHE_MAX_ENTRIES);
        assert!(lru.get(&("p99".into(), 0, None, PreviewOptions::default())).is_some());
        assert!(lru.get(&("p0".into(), 0, None, PreviewOptions::default())).is_none());
    }
}
