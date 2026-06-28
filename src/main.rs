//! Binary entry point — delegates to `restream::run_app()`.
//! The tokio multi-threaded runtime is used for all async I/O.
//! CPU-bound FFmpeg work runs on dedicated OS threads (see `src/lib.rs` docs).

fn main() {
    let mut args = std::env::args_os();
    let _program = args.next();
    if let Some(flag) = args.next() {
        if flag == "--emit-sbom" {
            let Some(path) = args.next() else {
                eprintln!("usage: restream --emit-sbom <path>");
                std::process::exit(2);
            };
            if args.next().is_some() {
                eprintln!("usage: restream --emit-sbom <path>");
                std::process::exit(2);
            }
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build tokio runtime")
                .block_on(restream::emit_repo_sbom(std::path::Path::new(&path)));
            match result {
                Ok(true) => {
                    println!("updated {}", std::path::Path::new(&path).display());
                    return;
                }
                Ok(false) => {
                    println!("unchanged {}", std::path::Path::new(&path).display());
                    return;
                }
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        eprintln!("usage: restream [--emit-sbom <path>]");
        std::process::exit(2);
    }

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
