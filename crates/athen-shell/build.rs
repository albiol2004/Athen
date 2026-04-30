fn main() {
    // Expose the build-time target triple so the runtime can locate the
    // Tauri-bundled `nu-<triple>(.exe)` sidecar next to the app binary.
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=ATHEN_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
