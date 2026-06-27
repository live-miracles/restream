fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=Cargo.toml");

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
    embed_toolchain_versions();
    embed_rust_dependency_inventory();

    // All native libraries (SRT, FFmpeg, OpenSSL, libstdc++) are always linked
    // statically from the repo-managed static prefix built by setup-static-build.sh.
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR missing"),
    );
    let prefix = manifest_dir.join(".build/static/prefix");
    let lib_dir = prefix.join("lib");
    let pkgconfig_dir = lib_dir.join("pkgconfig");

    // Prepend the static prefix to pkg-config's search path so FFmpeg, SRT, and
    // codec .pc files from the prefix take priority over any system-installed
    // counterparts, while OpenSSL (not built into the prefix) still resolves
    // from the system paths.
    let existing_pkg_config_path = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
    let new_pkg_config_path = if existing_pkg_config_path.is_empty() {
        pkgconfig_dir.display().to_string()
    } else {
        format!("{}:{existing_pkg_config_path}", pkgconfig_dir.display())
    };
    // SAFETY: build scripts are single-threaded at the point this runs.
    unsafe { std::env::set_var("PKG_CONFIG_PATH", new_pkg_config_path) };

    let archive = lib_dir.join("libsrt.a");
    let pc_path = pkgconfig_dir.join("srt.pc");

    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rerun-if-changed={}", pc_path.display());

    if !archive.exists() || !pc_path.exists() {
        panic!(
            "repo static libsrt is missing at {}. Run `scripts/resource-limit ./scripts/setup-static-build.sh` first.",
            prefix.display()
        );
    }

    let srt_version = std::fs::read_to_string(&pc_path)
        .ok()
        .and_then(|pc| {
            pc.lines()
                .find_map(|line| line.strip_prefix("Version: ").map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RESTREAM_BUILD_SRT_VERSION={srt_version}");

    // SRT is C++; place libstdc++ after all Rust/native objects so GNU ld
    // resolves C++ symbols from SRT before closing the static archive group.
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    let output = std::process::Command::new("c++")
        .arg("-print-file-name=libstdc++.a")
        .output()
        .expect("failed to ask C++ compiler for libstdc++.a");
    let archive_path = String::from_utf8(output.stdout)
        .expect("C++ compiler returned a non-UTF-8 libstdc++.a path");
    let archive_path = std::path::Path::new(archive_path.trim());
    let stdcxx_dir = archive_path
        .parent()
        .filter(|p| archive_path.is_absolute() && p.exists())
        .expect("C++ compiler did not return an absolute libstdc++.a path");
    println!("cargo:rustc-link-search=native={}", stdcxx_dir.display());

    // OpenSSL is not built into the static prefix — resolve its library
    // directory from the system pkg-config so the linker can find libssl.a.
    if let Ok(openssl) = pkg_config::Config::new()
        .statik(true)
        .cargo_metadata(false)
        .probe("openssl")
    {
        for path in &openssl.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    }

    println!("cargo:rustc-link-lib=static=srt");
    println!("cargo:rustc-link-lib=static=ssl");
    println!("cargo:rustc-link-lib=static=crypto");
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

    let mut ffmpeg = pkg_config::Config::new();
    ffmpeg.statik(true);
    ffmpeg
        .probe("libavcodec")
        .expect("libavcodec not found; run setup-static-build.sh first");
    ffmpeg
        .probe("libavformat")
        .expect("libavformat not found; run setup-static-build.sh first");
    ffmpeg
        .probe("libavfilter")
        .expect("libavfilter not found; run setup-static-build.sh first");
    ffmpeg
        .probe("libswscale")
        .expect("libswscale not found; run setup-static-build.sh first");
    ffmpeg
        .probe("libswresample")
        .expect("libswresample not found; run setup-static-build.sh first");
    ffmpeg
        .probe("libavutil")
        .expect("libavutil not found; run setup-static-build.sh first");

    embed_pkg_version("RESTREAM_BUILD_X264_VERSION", "x264");
    embed_pkg_version("RESTREAM_BUILD_X265_VERSION", "x265");
    embed_pkg_version("RESTREAM_BUILD_OPENSSL_VERSION", "openssl");
}

fn embed_pkg_version(env_name: &str, package: &str) {
    let version = pkg_config::Config::new()
        .statik(true)
        .cargo_metadata(false)
        .probe(package)
        .map(|library| library.version)
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env={env_name}={version}");
}

fn embed_toolchain_versions() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = std::process::Command::new(rustc)
        .args(["--version", "--verbose"])
        .output()
        .expect("failed to query rustc version");
    let text = String::from_utf8(output.stdout).expect("rustc version was not UTF-8");

    for (key, env_name) in [
        ("release", "RESTREAM_RUSTC_VERSION"),
        ("host", "RESTREAM_RUSTC_HOST"),
        ("LLVM version", "RESTREAM_LLVM_VERSION"),
    ] {
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}: ")))
            .unwrap_or("unknown");
        println!("cargo:rustc-env={env_name}={value}");
    }

    let cxx = std::env::var("CXX").unwrap_or_else(|_| "c++".to_string());
    let output = std::process::Command::new(cxx)
        .args(["-dumpfullversion", "-dumpversion"])
        .output()
        .expect("failed to query C++ compiler version");
    let version = String::from_utf8(output.stdout).expect("C++ compiler version was not UTF-8");
    println!(
        "cargo:rustc-env=RESTREAM_GCC_RUNTIME_VERSION={}",
        version.trim()
    );
}

fn embed_rust_dependency_inventory() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target = std::env::var("TARGET").expect("TARGET missing");
    let output = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            &target,
        ])
        .output()
        .expect("failed to run cargo metadata");
    if !output.status.success() {
        panic!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("invalid cargo metadata JSON");
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata packages missing");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("cargo metadata resolve graph missing");
    let root_id = metadata["resolve"]["root"]
        .as_str()
        .expect("cargo metadata root missing");

    let mut package_by_id = std::collections::HashMap::new();
    for package in packages {
        if let Some(id) = package["id"].as_str() {
            package_by_id.insert(id, package);
        }
    }

    let mut node_by_id = std::collections::HashMap::new();
    for node in nodes {
        if let Some(id) = node["id"].as_str() {
            node_by_id.insert(id, node);
        }
    }

    let mut pending = vec![root_id.to_string()];
    let mut visited = std::collections::HashSet::new();
    let mut dependencies = Vec::new();

    while let Some(id) = pending.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if id != root_id {
            let package = package_by_id
                .get(id.as_str())
                .expect("resolved package missing from metadata");
            let is_runtime_library = package["targets"]
                .as_array()
                .map(|targets| {
                    targets.iter().any(|target| {
                        target["kind"].as_array().is_some_and(|kinds| {
                            kinds.iter().any(|kind| {
                                matches!(
                                    kind.as_str(),
                                    Some("lib" | "rlib" | "dylib" | "staticlib" | "cdylib")
                                )
                            })
                        })
                    })
                })
                .unwrap_or(false);
            if !is_runtime_library {
                // Procedural macros and build tools execute during compilation
                // but are not linked into the shipped runtime artifact.
                continue;
            }
            dependencies.push(serde_json::json!({
                "name": package["name"],
                "version": package["version"],
                "source": package["source"],
                "license": package["license"],
            }));
        }

        let Some(node) = node_by_id.get(id.as_str()) else {
            continue;
        };
        let Some(deps) = node["deps"].as_array() else {
            continue;
        };
        for dep in deps {
            let is_runtime = dep["dep_kinds"]
                .as_array()
                .map(|kinds| {
                    kinds
                        .iter()
                        .any(|kind| kind["kind"].is_null() || kind["kind"] == "normal")
                })
                .unwrap_or(true);
            if is_runtime && let Some(package_id) = dep["pkg"].as_str() {
                pending.push(package_id.to_string());
            }
        }
    }

    dependencies.sort_by(|left, right| {
        left["name"]
            .as_str()
            .cmp(&right["name"].as_str())
            .then_with(|| left["version"].as_str().cmp(&right["version"].as_str()))
    });

    let output_path =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR missing"))
            .join("rust-runtime-dependencies.json");
    std::fs::write(
        output_path,
        serde_json::to_vec(&dependencies).expect("failed to serialize dependency inventory"),
    )
    .expect("failed to write dependency inventory");
}
