//! Binary entry point — delegates to `restream::run_app()`.
//! The tokio multi-threaded runtime is used for all async I/O.
//! CPU-bound FFmpeg work runs on dedicated OS threads (see `src/lib.rs` docs).

fn main() {
    // Extract embedded FFmpeg binary synchronously BEFORE the async runtime
    // spawns any threads. Must be called before ffmpeg_bin_path() consumers
    // run — this guarantees single-threaded initialization of the OnceLock
    // and eliminates any race between cached-path write and transcoder-stage
    // spawning.
    restream::ffmpeg_extract::ensure_ffmpeg_extracted();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(restream::run_app());

    restream::ffmpeg_extract::cleanup_ffmpeg();
}
