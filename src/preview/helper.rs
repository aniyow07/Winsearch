//! Out-of-process rendering for previews that run code we do not control:
//! Windows PDF rendering and Shell thumbnail providers (which may be
//! third-party DLLs). A crash, hang or memory blow-up in such code only
//! takes down the short-lived helper process, never the search app.
//!
//! Protocol: `<exe> --render-preview <pdf|thumb> <max_px> <path> [page]` writes
//! `WSPV` + width u32 + height u32 + page_count u32 + RGBA bytes to stdout
//! and exits 0, or writes an error message to stderr and exits non-zero.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::ImageData;

pub const HELPER_FLAG: &str = "--render-preview";
const MAGIC: &[u8; 4] = b"WSPV";
/// Largest PDF read into memory by the safe-mode renderer.
const MAX_PDF_BYTES: u64 = 256 * 1024 * 1024;
/// Hard ceiling on pixels the parent accepts from the helper.
const MAX_PIXELS: u64 = 4096 * 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperKind {
    Pdf,
    /// PDF with default render settings, used after a crash.
    PdfSafe,
    Thumbnail,
}

impl HelperKind {
    fn arg(self) -> &'static str {
        match self {
            HelperKind::Pdf => "pdf",
            HelperKind::PdfSafe => "pdf-safe",
            HelperKind::Thumbnail => "thumb",
        }
    }
}

pub struct HelperOutput {
    pub image: ImageData,
    pub page_count: u32,
}

/// Runs the helper and waits up to `timeout`, polling `cancelled`.
pub fn render(
    kind: HelperKind,
    path: &str,
    max_px: u32,
    page: u32,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<HelperOutput, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = helper_exe().ok_or("preview helper executable not found")?;
    let mut child = Command::new(exe)
        .arg(HELPER_FLAG)
        .arg(kind.arg())
        .arg(max_px.to_string())
        .arg(path)
        .arg(page.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start preview helper: {e}"))?;

    // Read stdout on a thread so a full pipe never blocks the helper.
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.by_ref().take(MAX_PIXELS * 4 + 64).read_to_end(&mut buf);
        buf
    });
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.by_ref().take(4096).read_to_string(&mut s);
        s
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("preview timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    // A complete, well-formed image counts even if the helper crashed while
    // shutting down afterwards.
    if !status.success()
        && let Ok(output) = decode_output(&out)
    {
        tracing::warn!(?kind, path, code = ?status.code(), "preview helper crashed after delivering the image");
        return Ok(output);
    }
    if !status.success() {
        // Stage markers ("stage: …") tell how far a crashed helper got.
        let last_stage = err.lines().rev().find_map(|l| l.strip_prefix("stage: ")).map(str::to_string);
        let message = err.lines().filter(|l| !l.starts_with("stage: ")).collect::<Vec<_>>().join(" ");
        let code = status.code().map(|c| c as u32);
        let what = match kind {
            HelperKind::Pdf | HelperKind::PdfSafe => "The Windows PDF renderer",
            HelperKind::Thumbnail => "The thumbnail provider for this file type",
        };
        let text = match code {
            // STATUS_STACK_BUFFER_OVERRUN / fail-fast and access violations:
            // native code crashed inside the isolated helper.
            Some(0xC000_0409) | Some(0xC000_0005) | Some(0xC000_00FD) => format!(
                "{what} crashed on this file (0x{:08X}{}). WinSearch itself is unaffected.",
                code.unwrap_or(0),
                last_stage.map(|s| format!(", while: {s}")).unwrap_or_default()
            ),
            _ if !message.trim().is_empty() => message.trim().to_string(),
            _ => format!("preview helper failed ({status})"),
        };
        tracing::warn!(?kind, path, ?code, stderr = %err.trim(), "preview helper failed");
        return Err(text);
    }
    decode_output(&out)
}

fn decode_output(out: &[u8]) -> Result<HelperOutput, String> {
    if out.len() < 16 || &out[0..4] != MAGIC {
        return Err("preview helper returned no image".into());
    }
    let u = |o: usize| u32::from_le_bytes(out[o..o + 4].try_into().unwrap());
    let (w, h, pages) = (u(4), u(8), u(12));
    let n = w as u64 * h as u64;
    if w == 0 || h == 0 || n > MAX_PIXELS || out.len() as u64 != 16 + n * 4 {
        return Err("preview helper returned a malformed image".into());
    }
    Ok(HelperOutput {
        image: ImageData { width: w, height: h, rgba: out[16..].to_vec() },
        page_count: pages,
    })
}

/// The executable that implements the helper mode: the GUI binary, found
/// next to the current executable (the CLI and tests live in the same
/// target directory).
fn helper_exe() -> Option<std::path::PathBuf> {
    let cur = std::env::current_exe().ok()?;
    let dir = cur.parent()?;
    for cand in [dir.join("winsearch.exe"), dir.join("..").join("winsearch.exe")] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Entry point for the helper process. Returns the process exit code.
pub fn helper_main(args: &[String]) -> i32 {
    let (Some(kind), Some(max), Some(path)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("usage: {HELPER_FLAG} <pdf|thumb> <max_px> <path>");
        return 2;
    };
    let max_px: u32 = max.parse().unwrap_or(1024).clamp(16, 4096);
    // Report Rust panics as an error message and exit code 1 instead of
    // an abort, so the parent can tell them apart from native crashes.
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "preview helper internal error: {info}");
    }));
    let page = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(0);
    let result = std::panic::catch_unwind(|| match kind.as_str() {
        "pdf" => render_pdf(path, max_px, page, false),
        "pdf-safe" => render_pdf(path, max_px, page, true),
        "thumb" => render_thumbnail(path, max_px),
        _ => Err("unknown preview kind".to_string()),
    })
    .unwrap_or_else(|_| Err("preview helper internal error".to_string()));
    match result {
        Ok(out) => {
            let mut buf = Vec::with_capacity(16 + out.image.rgba.len());
            buf.extend_from_slice(MAGIC);
            buf.extend_from_slice(&out.image.width.to_le_bytes());
            buf.extend_from_slice(&out.image.height.to_le_bytes());
            buf.extend_from_slice(&out.page_count.to_le_bytes());
            buf.extend_from_slice(&out.image.rgba);
            stage("sending the image");
            let mut so = std::io::stdout().lock();
            if so.write_all(&buf).and_then(|_| so.flush()).is_err() {
                return 3;
            }
            drop(so);
            // The image is delivered; end the process without running DLL
            // shutdown code from components we do not control.
            unsafe {
                use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
                let _ = TerminateProcess(GetCurrentProcess(), 0);
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Renders page 1 with the built-in Windows.Data.Pdf engine (Windows 10+).
fn render_pdf(path: &str, max_px: u32, page_index: u32, safe: bool) -> Result<HelperOutput, String> {
    use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
    use windows::Storage::StorageFile;
    use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::HSTRING;

    let e = |ctx: &str, err: windows::core::Error| format!("{ctx}: {}", err.message());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let doc = if safe {
        // Safe mode: read the bytes ourselves and hand the engine a memory
        // stream. This skips StorageFile, which goes through the Shell and
        // can load third-party PDF handlers (e.g. office suites) that crash.
        stage("reading the file");
        let bytes = std::fs::metadata(path)
            .map_err(|x| format!("cannot open file: {x}"))
            .and_then(|m| {
                if m.len() > MAX_PDF_BYTES {
                    Err(format!("PDF is too large to preview ({} MB)", m.len() / 1_048_576))
                } else {
                    std::fs::read(path).map_err(|x| format!("cannot open file: {x}"))
                }
            })?;
        let mem = InMemoryRandomAccessStream::new().map_err(|x| e("stream", x))?;
        let out = mem.GetOutputStreamAt(0).map_err(|x| e("stream", x))?;
        let writer = DataWriter::CreateDataWriter(&out).map_err(|x| e("stream", x))?;
        writer.WriteBytes(&bytes).map_err(|x| e("stream", x))?;
        writer.StoreAsync().and_then(|op| op.join()).map_err(|x| e("stream", x))?;
        writer.FlushAsync().and_then(|op| op.join()).map_err(|x| e("stream", x))?;
        let _ = writer.DetachStream();
        mem.Seek(0).map_err(|x| e("stream", x))?;
        stage("loading the PDF (safe mode)");
        PdfDocument::LoadFromStreamAsync(&mem)
            .and_then(|op| op.join())
            .map_err(|x| e("cannot read PDF (damaged or password-protected?)", x))?
    } else {
        // WinRT storage APIs only accept absolute paths.
        let abs = std::path::absolute(path).map_err(|x| format!("bad path: {x}"))?;
        stage("opening the file");
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(abs.as_os_str()))
            .and_then(|op| op.join())
            .map_err(|x| e("cannot open file", x))?;
        stage("loading the PDF");
        PdfDocument::LoadFromFileAsync(&file)
            .and_then(|op| op.join())
            .map_err(|x| e("cannot read PDF (damaged or password-protected?)", x))?
    };
    let pages = doc.PageCount().map_err(|x| e("PDF", x))?;
    if pages == 0 {
        return Err("PDF has no pages".into());
    }
    let index = page_index.min(pages - 1);
    stage("reading the page");
    let page = doc.GetPage(index).map_err(|x| e("PDF page", x))?;
    let size = page.Size().map_err(|x| e("PDF page size", x))?;
    let (pw, ph) = (size.Width.max(1.0), size.Height.max(1.0));
    let scale = (max_px as f32 / pw).min(max_px as f32 / ph);
    let (w, h) = (((pw * scale) as u32).max(1), ((ph * scale) as u32).max(1));
    let stream = InMemoryRandomAccessStream::new().map_err(|x| e("stream", x))?;
    if safe {
        // Fallback after a crash: default render settings (page size at
        // 96 DPI, default encoder), resized below.
        stage("rendering the page (safe mode)");
        page.RenderToStreamAsync(&stream)
            .and_then(|op| op.join())
            .map_err(|x| e("cannot render PDF page", x))?;
    } else {
        let opts = PdfPageRenderOptions::new().map_err(|x| e("render options", x))?;
        opts.SetDestinationWidth(w).map_err(|x| e("render options", x))?;
        opts.SetDestinationHeight(h).map_err(|x| e("render options", x))?;
        stage("rendering the page");
        page.RenderWithOptionsToStreamAsync(&stream, &opts)
            .and_then(|op| op.join())
            .map_err(|x| e("cannot render PDF page", x))?;
    }
    let len = stream.Size().map_err(|x| e("stream", x))? as u32;
    let input = stream.GetInputStreamAt(0).map_err(|x| e("stream", x))?;
    stage("reading the rendered page");
    let reader = DataReader::CreateDataReader(&input).map_err(|x| e("stream", x))?;
    reader.LoadAsync(len).and_then(|op| op.join()).map_err(|x| e("stream", x))?;
    let mut png = vec![0u8; len as usize];
    reader.ReadBytes(&mut png).map_err(|x| e("stream", x))?;
    stage(&format!("decoding the rendered page ({} KB PNG)", png.len() / 1024));
    let mut img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|x| format!("rendered page could not be decoded: {x}"))?;
    stage(&format!("scaling the rendered page ({}x{})", img.width(), img.height()));
    // The destination size is interpreted in DIPs, so on scaled displays
    // the page comes back larger than asked for; bring it back in bounds.
    if img.width() > max_px || img.height() > max_px {
        img = img.resize(max_px, max_px, image::imageops::FilterType::Triangle);
    }
    let img = img.to_rgba8();
    stage("releasing the PDF engine");
    // Releasing the PDF objects has crashed inside Windows on some systems
    // (third-party components hooked into it). The process exits right
    // after sending the image, so leak them instead of releasing them.
    std::mem::forget((doc, page, stream, input, reader));
    Ok(HelperOutput {
        image: ImageData { width: img.width(), height: img.height(), rgba: img.into_raw() },
        page_count: pages,
    })
}

/// Asks the Windows Shell for a thumbnail (or the file type icon when no
/// thumbnail provider exists) via IShellItemImageFactory.
fn render_thumbnail(path: &str, max_px: u32) -> Result<HelperOutput, String> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, HGDIOBJ,
    };
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT};
    use windows::core::HSTRING;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        stage("opening the item in the Shell");
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None)
            .map_err(|e| format!("shell cannot open item: {}", e.message()))?;
        let size = SIZE { cx: max_px as i32, cy: max_px as i32 };
        stage("asking the Shell for a thumbnail");
        let hbmp = factory
            .GetImage(size, SIIGBF_RESIZETOFIT)
            .map_err(|e| format!("no thumbnail available: {}", e.message()))?;
        let dc = CreateCompatibleDC(None);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        let ok = GetDIBits(dc, hbmp, 0, 0, None, &mut bmi, DIB_RGB_COLORS);
        let (w, h) = (bmi.bmiHeader.biWidth, bmi.bmiHeader.biHeight.abs());
        let result = if ok == 0 || w <= 0 || h <= 0 || (w as u64 * h as u64) > MAX_PIXELS {
            Err("thumbnail bitmap is unusable".to_string())
        } else {
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            bmi.bmiHeader.biHeight = -h; // top-down rows
            bmi.bmiHeader.biPlanes = 1;
            let mut px = vec![0u8; (w * h * 4) as usize];
            let lines = GetDIBits(dc, hbmp, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
            if lines == 0 {
                Err("could not read thumbnail pixels".to_string())
            } else {
                let opaque = px.chunks_exact(4).all(|p| p[3] == 0);
                for p in px.chunks_exact_mut(4) {
                    p.swap(0, 2); // BGRA -> RGBA
                    if opaque {
                        p[3] = 255;
                    } else if p[3] > 0 && p[3] < 255 {
                        // Un-premultiply.
                        let a = p[3] as u32;
                        for c in &mut p[..3] {
                            *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                        }
                    }
                }
                Ok(HelperOutput {
                    image: ImageData { width: w as u32, height: h as u32, rgba: px },
                    page_count: 0,
                })
            }
        };
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_helper_output() {
        assert!(decode_output(b"").is_err());
        assert!(decode_output(b"XXXX0000000000000000").is_err());
        let mut ok = MAGIC.to_vec();
        ok.extend_from_slice(&1u32.to_le_bytes());
        ok.extend_from_slice(&1u32.to_le_bytes());
        ok.extend_from_slice(&0u32.to_le_bytes());
        ok.extend_from_slice(&[1, 2, 3, 4]);
        let o = decode_output(&ok).unwrap();
        assert_eq!((o.image.width, o.image.height), (1, 1));
        ok.push(0); // wrong length
        assert!(decode_output(&ok).is_err());
    }
}

/// Progress marker on stderr; the parent quotes the last one if the
/// helper crashes, so a crash report says which step failed.
fn stage(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "stage: {s}");
    let _ = e.flush();
}
