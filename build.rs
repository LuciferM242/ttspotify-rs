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
    }

    println!("cargo:rerun-if-changed=build.rs");
}
