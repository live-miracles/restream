fn main() {
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
