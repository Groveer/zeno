//! Clipboard image reading for vision paste (Alt+V).
//!
//! Supports Wayland (wl-paste), X11 (xclip), and macOS (osascript).

/// Result of reading an image from the clipboard.
pub struct ClipboardImage {
    /// MIME type (e.g. "image/png").
    pub media_type: String,
    /// Base64-encoded image data.
    pub base64_data: String,
    /// Size in bytes (decoded).
    pub size_bytes: usize,
}

impl ClipboardImage {
    /// Convert to a `(media_type, base64_data)` tuple for the API.
    pub fn into_tuple(self) -> (String, String) {
        (self.media_type, self.base64_data)
    }
}

/// Read an image from the clipboard.
///
/// Tries, in order:
/// 1. `wl-paste --type image/png` (Wayland)
/// 2. `xclip -selection clipboard -t image/png -o` (X11)
/// 3. `osascript` (macOS)
///
/// Returns `None` if no image is found or clipboard tools are unavailable.
pub async fn read_clipboard_image() -> Option<ClipboardImage> {
    // Try Wayland (wl-paste)
    if let Some(result) = try_wl_paste("image/png").await {
        return Some(result);
    }

    // Try X11 (xclip)
    if let Some(result) = try_xclip("image/png").await {
        return Some(result);
    }

    // Try macOS (osascript)
    if let Some(result) = try_macos_clipboard().await {
        return Some(result);
    }

    None
}

async fn try_wl_paste(mime: &str) -> Option<ClipboardImage> {
    let output = tokio::process::Command::new("wl-paste")
        .arg("--type")
        .arg(mime)
        .output()
        .await
        .ok()?;

    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    let bytes = output.stdout;
    let base64_data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    let size_bytes = bytes.len();

    if size_bytes > 10 * 1024 * 1024 {
        return None; // > 10 MB
    }

    Some(ClipboardImage {
        media_type: mime.to_string(),
        base64_data,
        size_bytes,
    })
}

async fn try_xclip(mime: &str) -> Option<ClipboardImage> {
    let output = tokio::process::Command::new("xclip")
        .arg("-selection")
        .arg("clipboard")
        .arg("-t")
        .arg(mime)
        .arg("-o")
        .output()
        .await
        .ok()?;

    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    let bytes = output.stdout;
    let base64_data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    let size_bytes = bytes.len();

    if size_bytes > 10 * 1024 * 1024 {
        return None;
    }

    Some(ClipboardImage {
        media_type: mime.to_string(),
        base64_data,
        size_bytes,
    })
}

async fn try_macos_clipboard() -> Option<ClipboardImage> {
    let output = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg("get the clipboard as «class PNGf»")
        .output()
        .await
        .ok()?;

    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    let bytes = output.stdout;
    let base64_data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    let size_bytes = bytes.len();

    if size_bytes > 10 * 1024 * 1024 {
        return None;
    }

    Some(ClipboardImage {
        media_type: "image/png".to_string(),
        base64_data,
        size_bytes,
    })
}
