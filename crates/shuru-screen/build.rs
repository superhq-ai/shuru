fn main() {
    println!("cargo:rerun-if-changed=src/x264.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    cc::Build::new()
        .file("src/x264.c")
        .warnings(true)
        .compile("screen_x264");
    // Both from the system, linked dynamically: libx264-dev and libyuv-dev
    // on Debian.
    println!("cargo:rustc-link-lib=dylib=x264");
    println!("cargo:rustc-link-lib=dylib=yuv");
}
