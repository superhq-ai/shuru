// The x264 bindgen setup is derived from voxl 0.3.5
// (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo, under the MIT
// license: see LICENSE-voxl. Trimmed to the Linux x264 + libyuv path;
// voxl's Windows/vcpkg, Opus, PulseAudio and web-bundle steps are dropped.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        // Other systems build only the "runs on Linux only" stub in main.rs.
        return;
    }
    let x264 = pkg_config::probe_library("x264").expect("x264 (Debian: libx264-dev)");
    // libyuv's CMake install writes no libyuv.pc on Debian, so link it by name
    // from the default library path (the package is libyuv-dev).
    match pkg_config::probe_library("libyuv") {
        Ok(_) => {}
        Err(_) => println!("cargo:rustc-link-lib=yuv"),
    }
    let mut b = bindgen::Builder::default()
        .header_contents("wrap.h", "#include <stdint.h>\n#include <x264.h>\n")
        .allowlist_function("x264_.*")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in &x264.include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut code = b.generate().expect("bindgen x264").to_string();
    // x264.h renames x264_encoder_open after the library's build number (a macro
    // bindgen can't follow); alias whichever version this x264 has.
    let open = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| {
            w.strip_prefix("x264_encoder_open_")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
        .expect("x264_encoder_open_<build> in x264.h")
        .to_string();
    code.push_str(&format!("\npub use {open} as x264_encoder_open;\n"));
    std::fs::write(out.join("x264.rs"), code).unwrap();
    // Embed x264's library dir so the binary runs outside the build shell.
    for p in &x264.link_paths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
