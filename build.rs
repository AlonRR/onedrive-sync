//! Generate a multi-resolution .ico from the same rasteriser the app uses for its
//! tray/window icon, and embed it as the Windows .exe icon. Best-effort: a missing
//! resource compiler degrades to the default icon rather than failing the build.

include!("src/icon.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out.join("ods.ico");

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let img = ico::IconImage::from_rgba_data(size, size, rgba(size, BRAND));
        match ico::IconDirEntry::encode(&img) {
            Ok(e) => dir.add_entry(e),
            Err(e) => println!("cargo:warning=icon encode {size}px failed: {e}"),
        }
    }
    let Ok(file) = std::fs::File::create(&ico_path) else { return };
    if dir.write(file).is_err() {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap());
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon resource compile failed (using default icon): {e}");
    }
}
