fn main() {
    slint_build::compile("app.slint").unwrap();
    // The exe's own icon (Explorer, taskbar, Alt+Tab) and version info.
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/fast-explorer.ico");
    res.set("FileDescription", "Fast Explorer");
    res.set("ProductName", "Fast Explorer");
    res.compile().unwrap();
}
