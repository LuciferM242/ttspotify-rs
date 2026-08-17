fn main() {
    // Embed a Windows application manifest for modern theming,
    // dark mode support, and high-DPI awareness.
    #[cfg(windows)]
    {
        use embed_manifest::manifest::{ActiveCodePage, SupportedOS::*};
        use embed_manifest::{embed_manifest, new_manifest};

        let manifest = new_manifest("TTSpotify")
            .supported_os(Windows7..=Windows10)
            .active_code_page(ActiveCodePage::Utf8);
        embed_manifest(manifest).expect("unable to embed Windows manifest");

        // Application icon. Gives the exe an icon in Explorer and the taskbar,
        // and is what the tray loads by resource id at runtime.
        embed_resource::compile("assets/tray.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("unable to embed the application icon");
        println!("cargo:rerun-if-changed=assets/tray.rc");
        println!("cargo:rerun-if-changed=assets/tray.ico");

        // Generate Rust consts from the rc's #define block so dialog and
        // control ids exist in exactly one place. Code referring to an id the
        // template does not define fails to compile, instead of silently
        // talking to a control that is not there.
        let rc = std::fs::read_to_string("assets/tray.rc").expect("read tray.rc");
        let mut generated =
            String::from("// Generated from assets/tray.rc by build.rs. Do not edit.\n");
        for line in rc.lines() {
            let Some(rest) = line.strip_prefix("#define ") else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            if let (Some(name), Some(value)) = (parts.next(), parts.next()) {
                if !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) {
                    generated.push_str(&format!("pub const {name}: u16 = {value};\n"));
                }
            }
        }
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
        std::fs::write(
            std::path::Path::new(&out_dir).join("resource_ids.rs"),
            generated,
        )
        .expect("write resource_ids.rs");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
