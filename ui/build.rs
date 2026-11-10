fn main() {
    println!("cargo:rerun-if-changed=lang");
    println!("cargo:rerun-if-changed=assets");
    let cfg = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("app.slint", cfg).unwrap();
    // The exe's own icon (Explorer, taskbar, Alt+Tab) and version info.
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/fast-explorer.ico");
    res.set("FileDescription", "Fast Explorer");
    res.set("ProductName", "Fast Explorer");
    res.compile().unwrap();
}
