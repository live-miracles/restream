//! Extract embedded FFmpeg binary to temp directory on startup.
//!
//! If the `FFMPEG_BIN_PATH` environment variable is set before startup the
//! embedded binary is **not** extracted — the provided path is used directly.
//! Set it to a system FFmpeg (e.g. `/usr/bin/ffmpeg`) to skip the temp-dir
//! extraction entirely, keeping RSS baseline low.
//!
//! When the env var is absent the embedded binary (via `rust-embed`) is written
//! to `/tmp/restream-ffmpeg/ffmpeg`, made executable, and the parent temp
//! directory is cleaned up on shutdown.
//!
//! The resolved path is cached in a [`OnceLock`] and served via [`ffmpeg_bin_path`]
//! so the external transcoder and other consumers don't need environment variables.

use crate::api::EmbeddedAssets;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::{info, warn};

static FFMPEG_BIN_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Resolve the FFmpeg binary path, extracting the embedded one only if needed.
///
/// If `FFMPEG_BIN_PATH` is set in the environment the provided path is used
/// as-is. Otherwise the embedded `public/bin/ffmpeg` is extracted to a per-PID
/// temp directory, made executable, and cached.
///
/// Subsequent calls return the cached path immediately.
///
/// Must be called before any consumer calls [`ffmpeg_bin_path`].
pub fn ensure_ffmpeg_extracted() -> &'static Path {
    if let Some(cached) = FFMPEG_BIN_PATH.get() {
        return cached;
    }

    let path = if let Ok(user_path) = std::env::var("FFMPEG_BIN_PATH") {
        let path = PathBuf::from(&user_path);
        if path.exists() && path.is_file() {
            info!(
                "[startup] FFMPEG_BIN_PATH is set — using external FFmpeg: {}",
                path.display()
            );
            path
        } else {
            warn!(path = %user_path, "FFMPEG_BIN_PATH set but file does not exist; using embedded FFmpeg");
            extract_embedded()
        }
    } else {
        extract_embedded()
    };

    FFMPEG_BIN_PATH
        .set(path)
        .expect("race initializing FFMPEG_BIN_PATH");
    FFMPEG_BIN_PATH.get().unwrap()
}

/// Extract the embedded FFmpeg binary to a per-PID temp directory.
fn extract_embedded() -> PathBuf {
    let ffmpeg_data = EmbeddedAssets::get("bin/ffmpeg")
        .expect("Embedded FFmpeg binary not found in public/bin/ffmpeg");

    // Fixed temp directory (no PID suffix) so crash loops don't leak orphaned
    // copies. Clean up any leftover from a previous crash at startup.
    let temp_dir = std::env::temp_dir().join("restream-ffmpeg");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp ffmpeg directory");

    let ffmpeg_path = temp_dir.join("ffmpeg");

    // Write binary from embedded data.
    std::fs::write(&ffmpeg_path, ffmpeg_data.data.as_ref())
        .expect("Failed to write extracted FFmpeg binary");

    // Make executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&ffmpeg_path, perms).expect("Failed to make FFmpeg executable");
    }

    info!(
        "[startup] Extracted embedded FFmpeg to {}",
        ffmpeg_path.display()
    );

    ffmpeg_path
}

/// Return the resolved FFmpeg binary path.
///
/// # Panics
/// Panics if [`ensure_ffmpeg_extracted`] has not been called first.
pub fn ffmpeg_bin_path() -> &'static Path {
    FFMPEG_BIN_PATH
        .get()
        .expect("ensure_ffmpeg_extracted() must be called before ffmpeg_bin_path()")
}

/// Remove the temp FFmpeg directory on shutdown.
///
/// This is a no-op when the user supplied `FFMPEG_BIN_PATH` before startup
/// (since the path won't be under the `restream-ffmpeg` temp directory).
pub fn cleanup_ffmpeg() {
    if let Some(path) = FFMPEG_BIN_PATH.get() {
        let parent = path.parent().unwrap();
        if parent.file_name().is_some_and(|n| n == "restream-ffmpeg") {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
