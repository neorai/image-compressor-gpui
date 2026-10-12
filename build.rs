//! Embeds the application icon and version details into the Windows
//! executable. GPUI loads icon resource 1 for the window and taskbar.

use std::env;
use std::fs;
use std::path::PathBuf;

const ICON: &str = "assets/icon.ico";

fn main() {
    println!("cargo:rerun-if-changed={ICON}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by Cargo"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("set by Cargo"));
    let icon = manifest_dir
        .join(ICON)
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let version = env::var("CARGO_PKG_VERSION").expect("set by Cargo");
    let numeric = ["MAJOR", "MINOR", "PATCH"]
        .map(|part| env::var(format!("CARGO_PKG_VERSION_{part}")).expect("set by Cargo"))
        .join(",");

    let script = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric},0
PRODUCTVERSION {numeric},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Robert (neorai)"
      VALUE "FileDescription", "Image Compressor"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "image-compressor"
      VALUE "LegalCopyright", "Copyright (c) 2026 Robert (neorai). MIT License."
      VALUE "OriginalFilename", "image-compressor.exe"
      VALUE "ProductName", "Image Compressor"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let resource = out_dir.join("app.rc");
    fs::write(&resource, script).expect("the resource script could not be written");
    embed_resource::compile(&resource, embed_resource::NONE)
        .manifest_required()
        .expect("the Windows resources could not be compiled");
}
