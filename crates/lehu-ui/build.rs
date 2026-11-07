fn main() {
    // Glyphs are rendered at build time for the software renderer, so the
    // board needs no font files and no fontconfig (one with no fonts is
    // fine too: vendor/fontique skips what it cannot match).
    let cfg = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/app.slint", cfg).expect("the screens compile");
}
