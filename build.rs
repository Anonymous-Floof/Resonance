//! Embed the Windows resources: the application icon and the version block.
//!
//! Without this the executable gets the generic Windows placeholder icon in
//! Explorer, and its Properties dialog is blank — which for an unsigned binary
//! is the difference between "unknown publisher" and "no information at all".

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set("FileDescription", "Resonance");
        resource.set("ProductName", "Resonance");
        resource.set("OriginalFilename", "resonance.exe");
        resource.set("LegalCopyright", "MIT licensed");

        // A missing resource compiler is not a reason to fail the build. It
        // costs the icon and nothing else, and someone building from source
        // without the Windows SDK would rather have a working binary than an
        // error about `rc.exe`.
        if let Err(err) = resource.compile() {
            println!("cargo:warning=building without an icon: {err}");
        }
    }
}
