// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "linux")]
    pick_webkit_renderer();

    spool_lib::run()
}

/// Keep WebKitGTK on the GPU without hitting the NVIDIA black-window bug.
///
/// On NVIDIA + Wayland the DMABUF renderer fails to share buffers and the
/// window comes up black. Turning that renderer off fixes it but drops WebKit
/// to CPU rendering, which made scrolling and progress updates lag (15 fps at
/// ~95% CPU vs 24 fps at ~60% on the GPU in a MiniBrowser A/B on a GTX 1650 +
/// Vega laptop). So:
/// - no NVIDIA GPU: leave WebKit alone;
/// - NVIDIA plus another GPU: point EGL at Mesa, so WebKit renders on the other
///   GPU and never loads the NVIDIA driver;
/// - NVIDIA only: fall back to turning the DMABUF renderer off.
///
/// Either variable already set in the environment wins.
#[cfg(target_os = "linux")]
fn pick_webkit_renderer() {
    const MESA_EGL: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some()
        || std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_some()
    {
        return;
    }
    let drivers: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .filter_map(|e| std::fs::read_link(e.path().join("device/driver")).ok())
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    if !drivers.iter().any(|d| d == "nvidia") {
        return;
    }
    if drivers.iter().any(|d| d != "nvidia") && std::path::Path::new(MESA_EGL).exists() {
        std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", MESA_EGL);
    } else {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}
