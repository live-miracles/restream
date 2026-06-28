use std::ffi::{CStr, c_char};

use serde_json::{Value, json};

// SAFETY: mbedtls_version_get_string_full writes a NUL-terminated version
// string (e.g. "Mbed TLS 3.6.6") into the caller-provided buffer. The Mbed TLS
// docs guarantee the output never exceeds 18 bytes including the NUL, so the
// 32-byte buffer at the call site is always large enough.
unsafe extern "C" {
    fn mbedtls_version_get_string_full(string: *mut c_char);
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_sourceid() -> *const c_char;
}

const RUST_DEPENDENCIES_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/rust-runtime-dependencies.json"));

fn c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return "unknown".to_string();
    }
    // SAFETY: Caller guarantees pointer is either null or a valid
    // NUL-terminated C string obtained from a C library (Mbed TLS, FFmpeg,
    // or glibc). The null case is checked above.
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

fn av_version(version: u32) -> String {
    format!(
        "{}.{}.{}",
        (version >> 16) & 0xff,
        (version >> 8) & 0xff,
        version & 0xff
    )
}

fn license(expression: &str) -> Value {
    json!([{ "expression": expression }])
}

fn native_component(
    name: &str,
    version: String,
    license_expression: &str,
    version_source: &str,
    properties: Vec<Value>,
) -> Value {
    let linkage = "statically linked component";
    let bom_version = version
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || ".-_+".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut component = json!({
        "type": "library",
        "bom-ref": format!("native:{name}@{bom_version}"),
        "name": name,
        "version": version,
        "licenses": license(license_expression),
        "properties": [
            { "name": "restream:ecosystem", "value": "native" },
            { "name": "restream:versionSource", "value": version_source },
            { "name": "restream:linkage", "value": linkage }
        ]
    });
    if let Some(list) = component["properties"].as_array_mut() {
        list.extend(properties);
    }
    component
}

fn rust_components() -> Vec<Value> {
    let dependencies: Vec<Value> =
        serde_json::from_str(RUST_DEPENDENCIES_JSON).expect("embedded dependency inventory");
    dependencies
        .into_iter()
        .map(|dependency| {
            let name = dependency["name"].as_str().unwrap_or("unknown");
            let version = dependency["version"].as_str().unwrap_or("unknown");
            let purl = format!("pkg:cargo/{name}@{version}");
            let licenses = dependency["license"]
                .as_str()
                .map(license)
                .unwrap_or_else(|| json!([{ "license": { "name": "NOASSERTION" } }]));
            json!({
                "type": "library",
                "bom-ref": purl,
                "name": name,
                "version": version,
                "purl": purl,
                "licenses": licenses,
                "properties": [
                    { "name": "restream:ecosystem", "value": "cargo" },
                    { "name": "restream:versionSource", "value": "Cargo.lock" },
                    {
                        "name": "restream:source",
                        "value": dependency["source"].as_str().unwrap_or("path")
                    }
                ]
            })
        })
        .collect()
}

fn sqlite_runtime_info() -> (String, String) {
    (
        // SAFETY: sqlite3_libversion returns a valid static string owned by the
        // linked SQLite library for the process lifetime.
        c_string(unsafe { sqlite3_libversion() }),
        // SAFETY: sqlite3_sourceid returns a valid static string owned by the
        // linked SQLite library for the process lifetime.
        c_string(unsafe { sqlite3_sourceid() }),
    )
}

fn ffmpeg_components() -> (Vec<Value>, String, String) {
    // SAFETY: avcodec_configuration and avcodec_license are FFmpeg C API
    // functions that return NUL-terminated static strings valid for the
    // process lifetime. No ownership transfer; caller must not free.
    let configuration = c_string(unsafe { ffmpeg_next::ffi::avcodec_configuration() });
    let license_text = c_string(unsafe { ffmpeg_next::ffi::avcodec_license() });
    let license_expression = if license_text.to_ascii_lowercase().contains("gpl") {
        "GPL-2.0-or-later"
    } else {
        "LGPL-2.1-or-later"
    };
    let common_properties = || {
        vec![
            json!({ "name": "restream:ffmpegConfiguration", "value": configuration }),
            json!({ "name": "restream:runtimeLicenseText", "value": license_text }),
        ]
    };

    // SAFETY: All ffmpeg_next::ffi::*_version() functions are FFmpeg C API
    // calls that return a u32 version integer. No pointer arguments, no
    // memory allocation; they are pure functions callable from any context.
    let components = unsafe {
        vec![
            native_component(
                "libavcodec",
                av_version(ffmpeg_next::ffi::avcodec_version()),
                license_expression,
                "runtime API",
                common_properties(),
            ),
            native_component(
                "libavformat",
                av_version(ffmpeg_next::ffi::avformat_version()),
                license_expression,
                "runtime API",
                common_properties(),
            ),
            native_component(
                "libavfilter",
                av_version(ffmpeg_next::ffi::avfilter_version()),
                license_expression,
                "runtime API",
                common_properties(),
            ),
            native_component(
                "libswscale",
                av_version(ffmpeg_next::ffi::swscale_version()),
                license_expression,
                "runtime API",
                common_properties(),
            ),
            native_component(
                "libswresample",
                av_version(ffmpeg_next::ffi::swresample_version()),
                license_expression,
                "runtime API",
                common_properties(),
            ),
            native_component(
                "libavutil",
                av_version(ffmpeg_next::ffi::avutil_version()),
                license_expression,
                "runtime API",
                common_properties(),
            ),
        ]
    };

    (components, configuration, license_text)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn libc_component() -> Option<Value> {
    // SAFETY: gnu_get_libc_version is a glibc extension returning a
    // NUL-terminated static string. The returned pointer is valid for
    // the process lifetime. Only compiled on linux+gnu targets.
    unsafe extern "C" {
        fn gnu_get_libc_version() -> *const c_char;
    }
    Some(native_component(
        "glibc",
        // SAFETY: gnu_get_libc_version returns a valid static string pointer.
        c_string(unsafe { gnu_get_libc_version() }),
        "LGPL-2.1-or-later",
        "runtime API",
        Vec::new(),
    ))
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn libc_component() -> Option<Value> {
    None
}

pub fn status_and_sbom(bonding_available: bool) -> (Value, Value) {
    let (sqlite_version, sqlite_source_id) = sqlite_runtime_info();
    // SAFETY: mbedtls_version_get_string_full writes at most 18 bytes
    // (including the NUL) into the 32-byte buffer, then c_string reads it back
    // as a NUL-terminated C string. The buffer outlives the read.
    let mbedtls_version = {
        let mut buffer = [0 as c_char; 32];
        unsafe { mbedtls_version_get_string_full(buffer.as_mut_ptr()) };
        c_string(buffer.as_ptr())
    };
    let srt_version = crate::media::srt::linked_srt_version();
    let x264_version = env!("RESTREAM_BUILD_X264_VERSION").to_string();
    let x265_version = env!("RESTREAM_BUILD_X265_VERSION").to_string();
    let (mut native_components, ffmpeg_configuration, ffmpeg_license) = ffmpeg_components();

    native_components.extend([
        native_component(
            "libsrt",
            srt_version.clone(),
            "MPL-2.0",
            "runtime API",
            vec![
                json!({
                    "name": "restream:bondingAvailable",
                    "value": bonding_available.to_string()
                }),
                json!({
                    "name": "restream:buildResolvedVersion",
                    "value": env!("RESTREAM_BUILD_SRT_VERSION")
                }),
            ],
        ),
        native_component(
            "libmbedcrypto",
            mbedtls_version.clone(),
            "Apache-2.0",
            "runtime API",
            vec![json!({
                "name": "restream:buildResolvedVersion",
                "value": env!("RESTREAM_BUILD_MBEDTLS_VERSION")
            })],
        ),
        native_component(
            "SQLite",
            sqlite_version.clone(),
            "blessing",
            "runtime SQL function",
            vec![json!({
                "name": "restream:sqliteSourceId",
                "value": sqlite_source_id
            })],
        ),
        native_component(
            "x264",
            x264_version.clone(),
            "GPL-2.0-or-later",
            "linked pkg-config metadata at build time",
            vec![json!({
                "name": "restream:runtimeDispatch",
                "value": "x86 assembly enabled"
            })],
        ),
        native_component(
            "x265",
            x265_version.clone(),
            "GPL-2.0-or-later",
            "linked pkg-config metadata at build time",
            vec![json!({
                "name": "restream:runtimeDispatch",
                "value": "x86 assembly enabled"
            })],
        ),
        native_component(
            "libstdc++",
            env!("RESTREAM_GCC_RUNTIME_VERSION").to_string(),
            "GPL-3.0-or-later WITH GCC-exception-3.1",
            "C++ compiler used for linking",
            Vec::new(),
        ),
        native_component(
            "libgcc",
            env!("RESTREAM_GCC_RUNTIME_VERSION").to_string(),
            "GPL-3.0-or-later WITH GCC-exception-3.1",
            "C/C++ compiler used for linking",
            Vec::new(),
        ),
        native_component(
            "Rust standard library",
            env!("RESTREAM_RUSTC_VERSION").to_string(),
            "MIT OR Apache-2.0",
            "rustc toolchain used for linking",
            Vec::new(),
        ),
    ]);
    if let Some(component) = libc_component() {
        native_components.push(component);
    }
    let native_component_names: Vec<String> = native_components
        .iter()
        .filter_map(|component| component["name"].as_str().map(str::to_string))
        .collect();

    let mut components = rust_components();
    let rust_component_count = components.len();
    let native_component_count = native_components.len();
    components.extend(native_components);
    components.sort_by(|left, right| {
        left["name"]
            .as_str()
            .cmp(&right["name"].as_str())
            .then_with(|| left["version"].as_str().cmp(&right["version"].as_str()))
    });

    let application = json!({
        "type": "application",
        "bom-ref": format!("pkg:cargo/restream@{}", env!("CARGO_PKG_VERSION")),
        "name": "restream",
        "version": env!("CARGO_PKG_VERSION"),
        "purl": format!("pkg:cargo/restream@{}", env!("CARGO_PKG_VERSION")),
        "licenses": [{ "license": { "name": "NOASSERTION" } }],
        "properties": [
            { "name": "restream:gitCommit", "value": env!("GIT_COMMIT_HASH") },
            {
                "name": "restream:nativeBuildId",
                "value": env!("GIT_COMMIT_HASH")
            }
        ]
    });

    let sbom = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "component": application,
            "tools": {
                "components": [{
                    "type": "application",
                    "name": "restream-runtime-sbom",
                    "version": env!("CARGO_PKG_VERSION")
                }]
            },
            "properties": [
                { "name": "restream:generatedBy", "value": "running process" },
                { "name": "restream:rustDependencySource", "value": "resolved normal Cargo dependency closure" }
            ]
        },
        "components": components
    });

    // SAFETY: av_version_info returns a NUL-terminated static string
    // owned by FFmpeg, valid for the process lifetime.
    let ffmpeg_version = c_string(unsafe { ffmpeg_next::ffi::av_version_info() });
    let status = json!({
        "restream": {
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("GIT_COMMIT_HASH"),
            "nativeBuildId": env!("GIT_COMMIT_HASH"),
        },
        "toolchain": {
            "rustc": env!("RESTREAM_RUSTC_VERSION"),
            "target": env!("RESTREAM_RUSTC_HOST"),
            "llvm": env!("RESTREAM_LLVM_VERSION"),
            "gccRuntime": env!("RESTREAM_GCC_RUNTIME_VERSION"),
        },
        "nativeLibraries": {
            "ffmpeg": {
                "version": ffmpeg_version,
                "configuration": ffmpeg_configuration,
                "license": ffmpeg_license,
                "x86Assembly": ffmpeg_configuration.contains("--enable-x86asm"),
            },
            "srt": {
                "version": srt_version,
                "buildVersion": env!("RESTREAM_BUILD_SRT_VERSION"),
                "license": "MPL-2.0",
                "bondingAvailable": bonding_available,
            },
            "mbedtls": {
                "version": mbedtls_version,
                "buildVersion": env!("RESTREAM_BUILD_MBEDTLS_VERSION"),
                "license": "Apache-2.0",
            },
            "sqlite": {
                "version": sqlite_version,
                "sourceId": sqlite_source_id,
                "license": "blessing",
            },
            "x264": {
                "version": x264_version,
                "license": "GPL-2.0-or-later",
                "versionSource": "linked pkg-config metadata at build time",
            },
            "x265": {
                "version": x265_version,
                "license": "GPL-2.0-or-later",
                "versionSource": "linked pkg-config metadata at build time",
            }
        },
        "sbom": {
            "format": "CycloneDX",
            "specVersion": "1.5",
            "endpoint": "/api/status/sbom",
            "componentCount": rust_component_count + native_component_count,
            "rustComponentCount": rust_component_count,
            "nativeComponentCount": native_component_count,
            "nativeComponents": native_component_names,
            "licensesIncluded": true,
        }
    });

    (status, sbom)
}
