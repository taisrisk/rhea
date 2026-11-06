//! Embeds the application icon into the Windows executable (Explorer, taskbar, installer).
fn main() {
    println!("cargo:rerun-if-changed=assets/rhea.ico");
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/rhea.ico");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=could not embed the Windows icon: {error}");
        }
    }
}
