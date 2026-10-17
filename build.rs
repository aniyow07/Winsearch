//! Embeds Windows resources into winsearch.exe: version information
//! (company, product, description), the application manifest and the icon
//! (assets/icon.png, scaled to the usual icon sizes).
//! Nothing here changes what the program does; it only describes it to
//! Windows, Explorer and antivirus software.

use std::path::{Path, PathBuf};

const COMPANY: &str = "Ani Van Astrea";
const PRODUCT: &str = "WinSearch";
const DESCRIPTION: &str = "WinSearch - fast local file name search with previews";
const COPYRIGHT: &str = "Copyright (c) 2026 Ani Van Astrea. MIT License.";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=assets/winsearch.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let dotted = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(".");
    let commas = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");

    let ico = out.join("winsearch.ico");
    std::fs::write(&ico, ico_file("assets/icon.png", &[16, 20, 24, 32, 40, 48, 64, 128, 256])).unwrap();
    let manifest = out.join("winsearch.exe.manifest");
    let template = std::fs::read_to_string("assets/winsearch.exe.manifest").unwrap();
    std::fs::write(&manifest, template.replace("{VERSION}", &dotted)).unwrap();

    let rc = format!(
        r#"1 ICON "{ico}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "{COMPANY}"
      VALUE "FileDescription", "{DESCRIPTION}"
      VALUE "FileVersion", "{dotted}"
      VALUE "InternalName", "winsearch"
      VALUE "LegalCopyright", "{COPYRIGHT}"
      VALUE "OriginalFilename", "winsearch.exe"
      VALUE "ProductName", "{PRODUCT}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = rc_path(&ico),
        manifest = rc_path(&manifest),
    );
    let rc_file = out.join("winsearch.rc");
    std::fs::write(&rc_file, rc).unwrap();
    embed_resource::compile_for(&rc_file, ["winsearch"], embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

fn rc_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

/// An .ico with one PNG-compressed image per size, scaled from `source`.
fn ico_file(source: &str, sizes: &[u32]) -> Vec<u8> {
    use image::codecs::ico::{IcoEncoder, IcoFrame};
    use image::imageops::FilterType;
    let src = image::open(source).unwrap().to_rgba8();
    let frames: Vec<IcoFrame> = sizes
        .iter()
        .map(|&n| {
            let img = image::imageops::resize(&src, n, n, FilterType::Lanczos3);
            IcoFrame::as_png(img.as_raw(), n, n, image::ExtendedColorType::Rgba8).unwrap()
        })
        .collect();
    let mut out = Vec::new();
    IcoEncoder::new(&mut out).encode_images(&frames).unwrap();
    out
}
