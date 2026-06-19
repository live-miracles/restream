//! Binary entry point — delegates to `restream::run_app()`.
//! The tokio multi-threaded runtime is used for all async I/O.
//! CPU-bound FFmpeg work runs on dedicated OS threads (see `src/lib.rs` docs).

#[tokio::main]
async fn main() {
    restream::run_app().await;
}
