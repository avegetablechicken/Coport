//! Linux WebKit compositing, ported from Copool.
use tauri::{App, Manager};

pub fn configure(app: &mut App) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(crate::panel::LABEL) {
        window.with_webview(|webview| {
            use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebViewExt};
            // WebKit reports `Never` when it cannot composite on this system.
            let policy = WebViewExt::settings(&webview.inner())
                .map(|settings| settings.hardware_acceleration_policy());
            crate::backdrop::set_composited(
                policy.is_some_and(|policy| policy != HardwareAccelerationPolicy::Never),
            );
        })?;
    }
    Ok(())
}

/// WebKitGTK cannot share GPU buffers with NVIDIA's proprietary driver, so it
/// draws without compositing, which ignores `backdrop-filter`. Compositing on
/// the GPU and handing frames over in shared memory keeps it. Settings the
/// user made for these renderers win.
pub fn enable_compositing() {
    const SETTINGS: [&str; 4] = [
        "WEBKIT_DISABLE_COMPOSITING_MODE",
        "WEBKIT_DISABLE_DMABUF_RENDERER",
        "WEBKIT_FORCE_DMABUF_RENDERER",
        "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
    ];
    if !std::path::Path::new("/sys/module/nvidia").exists()
        || SETTINGS.iter().any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    // Called first in main, before any thread or WebKit process starts.
    // SAFETY: called first in main, before starting any threads (Rust 2024).
    unsafe {
        std::env::set_var("WEBKIT_FORCE_DMABUF_RENDERER", "1");
        std::env::set_var("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1");
    }
}
