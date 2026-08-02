use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = new_manifest("GlazeTray")
        .dpi_awareness(DpiAwareness::PerMonitorV2)
        .long_path_aware(embed_manifest::manifest::Setting::Enabled);
    embed_manifest(manifest).expect("unable to embed manifest file");
}
