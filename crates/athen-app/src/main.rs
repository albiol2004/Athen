#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // GTK derives the Wayland app_id from the program name (or argv[0]
    // basename), and Wayland compositors look up `<app_id>.desktop` to
    // find the icon shown in Alt+Tab, the dock, etc. Set it explicitly
    // to the bundle identifier so it matches the installed desktop file.
    #[cfg(target_os = "linux")]
    glib::set_prgname(Some("com.athen.app"));

    // WORKAROUND: WebKitGTK 2.44+ DMABUF renderer + Mesa/RADV (AMD) stalls GPU
    // command submission, causing system-wide compositor stutter (visible even
    // when Athen is unfocused). Forcing the older GLX path avoids it. Confirmed
    // on Fedora 44 + AMD iGPU, 2026-05-03. Revisit when WebKitGTK or Mesa ship
    // a fix; on Intel/NVIDIA this just costs a small amount of perf.
    // Must be set before Tauri/WebKitGTK initializes.
    #[cfg(target_os = "linux")]
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    athen_app::run();
}
