//! Extract embedded FFmpeg binary to temp directory on startup.
//!
//! This module extracts the embedded FFmpeg binary (via rust-embed) to a
//! temporary directory on startup, then updates FFMPEG_BIN_PATH so the
//! external transcoder can use it. This achieves single-binary deployment
//! while keeping RSS baseline low (binary decompressed and freed after extract).

use crate::api::EmbeddedAssets;
use std::path::PathBuf;

/// Extract embedded FFmpeg binary to temp directory and return its path.
/// 
/// On first call, this writes the embedded `public/bin/ffmpeg` to
/// `/tmp/restream-ffmpeg-<timestamp>`, makes it executable, and caches
/// the path in FFMPEG_BIN_PATH environment variable.
///
/// Subsequent calls use the cached path.
pub fn ensure_ffmpeg_extracted() -> PathBuf {
    // Check if already extracted
    if let Ok(cached) = std::env::var("FFMPEG_BIN_PATH") {
        let path = PathBuf::from(&cached);
        if path.exists() && path.is_file() {
            return path;
        }
    }

    // Extract embedded FFmpeg
    let ffmpeg_data = EmbeddedAssets::get("bin/ffmpeg")
        .expect("Embedded FFmpeg binary not found in public/bin/ffmpeg");

    // Create temp directory
    let temp_dir = std::env::temp_dir().join(format!(
        "restream-ffmpeg-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&temp_dir)
        .expect("Failed to create temp ffmpeg directory");

    let ffmpeg_path = temp_dir.join("ffmpeg");

    // Write binary
    std::fs::write(&ffmpeg_path, ffmpeg_data.data.as_ref())
        .expect("Failed to write extracted FFmpeg binary");

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&ffmpeg_path, perms)
            .expect("Failed to make FFmpeg executable");
    }

    println!(
        "[startup] Extracted embedded FFmpeg to {}",
        ffmpeg_path.display()
    );

    // Safe: called before the tokio runtime spawns any threads (see main.rs).
    // POSIX requires single-threaded context for setenv; the call site
    // guarantees this by running before Runtime::build().
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("FFMPEG_BIN_PATH", &ffmpeg_path);
    }

    ffmpeg_path
}

/// Cleanup temp FFmpeg directory on shutdown (optional, called via atexit).
#[allow(dead_code)]
pub fn cleanup_ffmpeg() {
    if let Ok(cached) = std::env::var("FFMPEG_BIN_PATH") {
        let path = PathBuf::from(&cached);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
