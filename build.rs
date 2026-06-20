fn main() {
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

    // Enforce static linking of the native C libraries via pkg-config
    let mut config = pkg_config::Config::new();
    config.statik(false);

    config
        .probe("libavcodec")
        .expect("libavcodec not found or static probe failed");
    config
        .probe("libavformat")
        .expect("libavformat not found or static probe failed");
    config
        .probe("libavfilter")
        .expect("libavfilter not found or static probe failed");
    config
        .probe("libswscale")
        .expect("libswscale not found or static probe failed");
    config
        .probe("libswresample")
        .expect("libswresample not found or static probe failed");
    config
        .probe("libavutil")
        .expect("libavutil not found or static probe failed");
    config
        .probe("srt")
        .expect("libsrt not found or static probe failed");
}
