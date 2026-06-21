fn main() {
    println!("cargo:rerun-if-env-changed=RESTREAM_STATIC_SRT");
    println!("cargo:rerun-if-env-changed=RESTREAM_STATIC_FFMPEG");
    println!("cargo:rerun-if-env-changed=RESTREAM_FULLY_STATIC");
    println!("cargo:rerun-if-env-changed=RESTREAM_NATIVE_BUILD_ID");

    // Embed git commit hash at build time
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    let commit = output
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=GIT_COMMIT_HASH={}", commit.trim());

    // Release tooling can point PKG_CONFIG_PATH at a bonding-enabled static
    // libsrt prefix and set RESTREAM_STATIC_SRT=1. Whole-archiving is
    // intentional: distro FFmpeg may itself depend on a shared libsrt build
    // with bonding disabled, and otherwise that shared object can satisfy our
    // SRT symbols before the selected archive is pulled in.
    let static_srt = std::env::var_os("RESTREAM_STATIC_SRT").is_some();
    let fully_static = std::env::var_os("RESTREAM_FULLY_STATIC").is_some();
    if static_srt {
        let mut config = pkg_config::Config::new();
        config.statik(true).cargo_metadata(false);
        let library = config
            .probe("srt")
            .expect("static libsrt not found; point PKG_CONFIG_PATH at its prefix");
        for path in library.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        if fully_static {
            let output = std::process::Command::new("c++")
                .arg("-print-file-name=libstdc++.a")
                .output()
                .expect("failed to ask C++ compiler for libstdc++.a");
            let archive = String::from_utf8(output.stdout)
                .expect("C++ compiler returned a non-UTF-8 libstdc++.a path");
            let archive = std::path::Path::new(archive.trim());
            let directory = archive
                .parent()
                .filter(|path| archive.is_absolute() && path.exists())
                .expect("C++ compiler did not return an absolute libstdc++.a path");
            println!("cargo:rustc-link-search=native={}", directory.display());
        }
        if fully_static {
            println!("cargo:rustc-link-lib=static=srt");
        } else {
            println!("cargo:rustc-link-lib=static:+whole-archive=srt");
        }
        println!("cargo:rustc-link-lib=static=ssl");
        println!("cargo:rustc-link-lib=static=crypto");
        if fully_static {
            // GNU ld resolves static archives left-to-right. SRT is C++, so
            // place libstdc++ after all Rust/native objects instead of letting
            // rustc put it before the SRT archive.
            println!("cargo:rustc-link-arg=-Wl,-Bstatic");
            println!("cargo:rustc-link-arg=-Wl,--start-group");
            println!("cargo:rustc-link-arg=-lstdc++");
            println!("cargo:rustc-link-arg=-lm");
            println!("cargo:rustc-link-arg=-lpthread");
            println!("cargo:rustc-link-arg=-ldl");
            println!("cargo:rustc-link-arg=-lc");
            println!("cargo:rustc-link-arg=-lgcc_eh");
            println!("cargo:rustc-link-arg=-lgcc");
            println!("cargo:rustc-link-arg=-Wl,--end-group");
        } else {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            println!("cargo:rustc-link-lib=dylib=dl");
            println!("cargo:rustc-link-lib=dylib=pthread");
            println!("cargo:rustc-link-lib=dylib=m");
        }
    }

    // FFmpeg remains dynamically selected for normal development builds and
    // switches to the local minimal static build for release packaging.
    let static_ffmpeg = std::env::var_os("RESTREAM_STATIC_FFMPEG").is_some();
    let mut ffmpeg = pkg_config::Config::new();
    ffmpeg.statik(static_ffmpeg);
    ffmpeg
        .probe("libavcodec")
        .expect("libavcodec not found or static probe failed");
    ffmpeg
        .probe("libavformat")
        .expect("libavformat not found or static probe failed");
    ffmpeg
        .probe("libavfilter")
        .expect("libavfilter not found or static probe failed");
    ffmpeg
        .probe("libswscale")
        .expect("libswscale not found or static probe failed");
    ffmpeg
        .probe("libswresample")
        .expect("libswresample not found or static probe failed");
    ffmpeg
        .probe("libavutil")
        .expect("libavutil not found or static probe failed");

    if !static_srt {
        pkg_config::Config::new()
            .statik(false)
            .probe("srt")
            .expect("libsrt not found; set PKG_CONFIG_PATH to the selected SRT prefix");
    }
}
