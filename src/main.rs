//! Binary entry point — delegates to `restream::run_app()`.
//! The tokio multi-threaded runtime is used for all async I/O.
//! CPU-bound FFmpeg work runs on dedicated OS threads (see `src/lib.rs` docs).

fn main() {
    // Extract embedded FFmpeg binary synchronously BEFORE the async runtime
    // spawns any threads. std::env::set_var is not safe to call from a
    // multi-threaded context; doing it here guarantees single-threaded
    // execution and eliminates the race between env-var write and
    // transcoder-stage spawning.
    restream::ffmpeg_extract::ensure_ffmpeg_extracted();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(restream::run_app());
}
