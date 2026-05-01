#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // GTK derives the Wayland app_id from the program name (or argv[0]
    // basename), and Wayland compositors look up `<app_id>.desktop` to
    // find the icon shown in Alt+Tab, the dock, etc. Set it explicitly
    // to the bundle identifier so it matches the installed desktop file.
    #[cfg(target_os = "linux")]
    glib::set_prgname(Some("com.athen.app"));

    athen_app::run();
}
