#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Headless daemon mode: full autonomous stack, no GUI. Selected via
    // flag or env so containers can bake it into the image. Checked
    // before any GTK/WebKit touchpoints — none of them exist headless.
    let headless = std::env::args().any(|a| a == "--headless")
        || std::env::var("ATHEN_HEADLESS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if headless {
        athen_app::run_headless();
        return;
    }

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

    // Cap tokio worker threads. Tauri's default global runtime spawns one
    // worker per CPU core, but Athen's workload is overwhelmingly I/O-bound
    // (sense polling, LLM HTTP), not CPU-parallel — so a high-core box would
    // get many mostly-idle worker threads + their stacks. clamp(2, 4) leaves
    // modest machines untouched (they already have few cores) while stopping
    // a 16-core box from spawning 16 idle workers. Registered BEFORE the
    // Tauri builder runs (and before any async_runtime block_on/spawn), since
    // `set` panics if the runtime is already initialized. `rt` is bound here
    // so it lives for the whole program — dropping it shuts the runtime down.
    let worker_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 4);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());

    athen_app::run();

    // Keep the runtime alive until the Tauri app loop returns.
    drop(rt);
}
