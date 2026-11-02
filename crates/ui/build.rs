fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");
    // Icon of the .exe file itself; the window icon is set in app.slint.
    if let Err(e) = winresource::WindowsResource::new().set_icon("ui/lume.ico").compile() {
        println!("cargo:warning=exe icon not embedded: {e}");
    }
}
