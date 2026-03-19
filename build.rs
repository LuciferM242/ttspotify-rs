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
    }

    println!("cargo:rerun-if-changed=build.rs");
}
