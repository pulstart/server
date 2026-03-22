fn main() {
    #[cfg(target_os = "macos")]
    {
        // screencapturekit links libswift_Concurrency.dylib via @rpath,
        // but doesn't set the rpath. Point it to the system Swift runtime
        // (which lives in the dyld shared cache).
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

        // Link VideoToolbox and CoreFoundation for direct VTCompressionSession FFI
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
    }

    #[cfg(target_os = "linux")]
    {
        // Link libpulse-simple for PulseAudio audio capture
        println!("cargo:rustc-link-lib=pulse-simple");
        println!("cargo:rustc-link-lib=pulse");

        // Link X11, XShm, and XFixes for X11 screen + cursor capture
        println!("cargo:rustc-link-lib=X11");
        println!("cargo:rustc-link-lib=Xext");
        println!("cargo:rustc-link-lib=Xfixes");
        println!("cargo:rustc-link-lib=Xtst");

        // Link libwayland-client for Wayland screencopy capture
        println!("cargo:rustc-link-lib=wayland-client");
    }
}
