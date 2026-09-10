//! Puts the icon into the executable.
//!
//! Windows reads an application's icon out of the binary itself -- the taskbar,
//! alt-tab, Explorer and the Start menu all ask the file, not the installer. An
//! MSI can point a shortcut at an `.ico`, and that still leaves a generic gear
//! everywhere the shortcut is not.
//!
//! `assets/ira.ico` is rendered from the orb by an ignored test in `orb.rs`, so
//! the icon and the thing on screen cannot drift apart.

fn main() {
    // The host, which is what decides whether there is a resource compiler on
    // this machine -- and it is also the only platform that has resources at
    // all. A Linux build has neither and wants neither.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/ira.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/ira.ico");
        // Warned about rather than fatal. This needs rc.exe from the Windows
        // SDK, and a toolchain without one should produce an IRA with a plain
        // icon, not no IRA at all.
        if let Err(e) = res.compile() {
            println!("cargo:warning=no icon embedded ({e})");
        }
    }
}
