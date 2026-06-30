//! Extract embedded FFmpeg binary to temp directory on startup.
//!
//! If the `FFMPEG_BIN_PATH` environment variable is set before startup the
//! embedded binary is **not** extracted — the provided path is used directly.
//! Set it to a system FFmpeg (e.g. `/usr/bin/ffmpeg`) to skip the temp-dir
//! extraction entirely, keeping RSS baseline low.
//!
//! When the env var is absent the embedded binary (via `rust-embed`) is written
//! to a versioned shared cache under `/tmp/restream-ffmpeg/`, made executable,
//! and then reused across processes. Startup is intentionally atomic and
//! multi-process safe so correctness harness modes can boot in parallel.
//!
//! The resolved path is cached in a [`OnceLock`] and served via [`ffmpeg_bin_path`]
//! so the external transcoder and other consumers don't need environment variables.

use crate::api::EmbeddedAssets;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::{info, warn};

static FFMPEG_BIN_PATH: OnceLock<PathBuf> = OnceLock::new();

fn resolve_ffmpeg_bin_path() -> PathBuf {
    if let Ok(user_path) = std::env::var("FFMPEG_BIN_PATH") {
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
    }
}

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
    FFMPEG_BIN_PATH
        .get_or_init(resolve_ffmpeg_bin_path)
        .as_path()
}

fn embedded_cache_root() -> PathBuf {
    std::env::temp_dir().join("restream-ffmpeg")
}

fn embedded_cache_key(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut key = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut key, "{byte:02x}");
    }
    key
}

fn embedded_cache_dir(bytes: &[u8]) -> PathBuf {
    embedded_cache_root().join(embedded_cache_key(bytes))
}

fn set_executable(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Extract the embedded FFmpeg binary into a versioned cache directory.
///
/// This is intentionally multi-process safe:
/// - a content-hash directory avoids cross-version clobbering
/// - we never remove the shared cache root at startup
/// - installation uses a unique temp file + atomic rename
fn extract_embedded() -> PathBuf {
    let ffmpeg_data = EmbeddedAssets::get("bin/ffmpeg")
        .expect("Embedded FFmpeg binary not found in public/bin/ffmpeg");
    let ffmpeg_bytes = ffmpeg_data.data.as_ref();
    let temp_dir = embedded_cache_dir(ffmpeg_bytes);
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp ffmpeg directory");

    let ffmpeg_path = temp_dir.join("ffmpeg");
    if ffmpeg_path.exists() {
        let _ = set_executable(&ffmpeg_path);
        return ffmpeg_path;
    }

    let temp_path = temp_dir.join(format!(
        "ffmpeg.tmp.{}.{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::write(&temp_path, ffmpeg_bytes).expect("Failed to write extracted FFmpeg binary");
    if let Err(error) = set_executable(&temp_path) {
        let _ = std::fs::remove_file(&temp_path);
        panic!("Failed to make FFmpeg executable: {error}");
    }

    match std::fs::rename(&temp_path, &ffmpeg_path) {
        Ok(()) => {}
        Err(error) if ffmpeg_path.exists() => {
            let _ = std::fs::remove_file(&temp_path);
            warn!(
                path = %ffmpeg_path.display(),
                err = %error,
                "another process finished embedded ffmpeg install first; reusing cached binary"
            );
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            panic!("Failed to install extracted FFmpeg binary: {error}");
        }
    }

    info!(
        "[startup] Extracted embedded FFmpeg to {}",
        ffmpeg_path.display()
    );

    ffmpeg_path
}

/// Return the resolved FFmpeg binary path.
///
pub fn ffmpeg_bin_path() -> &'static Path {
    ensure_ffmpeg_extracted()
}

/// Remove the temp FFmpeg directory on shutdown.
///
/// This is intentionally a no-op for embedded binaries since the shared cache
/// may be in use by concurrent processes. User-supplied `FFMPEG_BIN_PATH`
/// values are external and likewise left untouched.
pub fn cleanup_ffmpeg() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_cache_dir_is_stable_for_same_bytes() {
        let bytes = b"same-binary";
        assert_eq!(embedded_cache_dir(bytes), embedded_cache_dir(bytes));
    }

    #[test]
    fn embedded_cache_dir_changes_when_bytes_change() {
        assert_ne!(
            embedded_cache_dir(b"binary-a"),
            embedded_cache_dir(b"binary-b")
        );
    }
}
