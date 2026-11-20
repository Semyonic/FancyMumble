//! Native system-audio capture for screen sharing.
//!
//! The macOS webview's `getDisplayMedia` returns no audio track, so the
//! frontend calls [`start_system_audio_capture`] when a share has no audio
//! and receives the Mac's output as raw PCM blocks over `on_audio`.

use tauri::ipc::{Channel, InvokeResponseBody};

use crate::platform::system_audio::CaptureFormat;

/// The single active capture; replaced or dropped (stopped) as a whole.
#[cfg(target_os = "macos")]
static ACTIVE: std::sync::Mutex<Option<crate::platform::system_audio::SystemAudioCapture>> =
    std::sync::Mutex::new(None);

/// Starts streaming system audio (everything except this app) to
/// `on_audio` as interleaved little-endian `f32` blocks and returns their
/// format.  A capture that is already running is stopped first.
///
/// Fails on platforms without a native backend (everything but macOS)
/// and on macOS older than 14.2.
#[cfg_attr(
    not(target_os = "macos"),
    allow(clippy::needless_pass_by_value, reason = "Tauri command signature")
)]
#[tauri::command(async)]
pub(crate) fn start_system_audio_capture(
    on_audio: Channel<InvokeResponseBody>,
) -> Result<CaptureFormat, String> {
    #[cfg(target_os = "macos")]
    {
        let mut active = ACTIVE.lock().map_err(|e| e.to_string())?;
        // Release the previous tap before creating a new one.
        drop(active.take());
        let capture = crate::platform::system_audio::SystemAudioCapture::start(on_audio)?;
        let format = capture.format();
        *active = Some(capture);
        Ok(format)
    }
    #[cfg(not(target_os = "macos"))]
    {
        drop(on_audio);
        Err("native system audio capture is only available on macOS".into())
    }
}

/// Stops the active system-audio capture, if any.
#[tauri::command(async)]
pub(crate) fn stop_system_audio_capture() {
    #[cfg(target_os = "macos")]
    if let Ok(mut active) = ACTIVE.lock() {
        drop(active.take());
    }
}
