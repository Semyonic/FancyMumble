//! System-audio capture for screen sharing.
//!
//! `WebKit`'s `getDisplayMedia` (used by the macOS webview) never returns an
//! audio track, so on macOS the screen share's sound is captured natively
//! and streamed to the frontend, which turns it into a `MediaStreamTrack`.
//!
//! - `macos` - Core Audio process tap (macOS 14.2+).
//!
//! Other platforms get their audio from `getDisplayMedia` itself and have
//! no native backend.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(crate) use macos::SystemAudioCapture;

/// PCM format of the blocks delivered to the frontend.
///
/// Every block is interleaved little-endian `f32` samples with
/// `channels` samples per frame.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureFormat {
    /// Frames per second of the captured stream.
    pub(crate) sample_rate: f64,
    /// Interleaved channels per frame.
    pub(crate) channels: u32,
}
