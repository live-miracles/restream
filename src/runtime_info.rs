use std::ffi::{CStr, c_char, c_int};

use serde_json::{Value, json};
use sqlx::SqlitePool;

unsafe extern "C" {
    fn OpenSSL_version(kind: c_int) -> *const c_char;
}

const OPENSSL_VERSION: c_int = 0;
const RUST_DEPENDENCIES_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/rust-runtime-dependencies.json"));

fn c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return "unknown".to_string();
    }
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
    let linkage = if option_env!("RESTREAM_NATIVE_BUILD_ID").is_some() {
        "statically linked release component"
    } else {
        "system-selected development component"
    };
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
                .map(|expression| license(expression))
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

fn ffmpeg_components() -> (Vec<Value>, String, String) {
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
    unsafe extern "C" {
        fn gnu_get_libc_version() -> *const c_char;
    }
    Some(native_component(
        "glibc",
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

pub async fn status_and_sbom(db: &SqlitePool, bonding_available: bool) -> (Value, Value) {
    let sqlite_version = sqlx::query_scalar::<_, String>("SELECT sqlite_version()")
        .fetch_one(db)
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    let sqlite_source_id = sqlx::query_scalar::<_, String>("SELECT sqlite_source_id()")
        .fetch_one(db)
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    let openssl_version = c_string(unsafe { OpenSSL_version(OPENSSL_VERSION) });
    let srt_version = crate::media::srt::linked_srt_version();
    let x264_version = env!("RESTREAM_BUILD_X264_VERSION").to_string();
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
            "libssl",
            openssl_version.clone(),
            "Apache-2.0",
            "runtime API",
            vec![json!({
                "name": "restream:buildResolvedVersion",
                "value": env!("RESTREAM_BUILD_OPENSSL_VERSION")
            })],
        ),
        native_component(
            "libcrypto",
            openssl_version.clone(),
            "Apache-2.0",
            "runtime API",
            vec![json!({
                "name": "restream:buildResolvedVersion",
                "value": env!("RESTREAM_BUILD_OPENSSL_VERSION")
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
                "value": option_env!("RESTREAM_NATIVE_BUILD_ID").unwrap_or("system-libraries")
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

    let ffmpeg_version = c_string(unsafe { ffmpeg_next::ffi::av_version_info() });
    let status = json!({
        "restream": {
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("GIT_COMMIT_HASH"),
            "nativeBuildId": option_env!("RESTREAM_NATIVE_BUILD_ID"),
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
                "bondingAvailable": bonding_available,
            },
            "openssl": {
                "version": openssl_version,
                "buildVersion": env!("RESTREAM_BUILD_OPENSSL_VERSION"),
            },
            "sqlite": {
                "version": sqlite_version,
                "sourceId": sqlite_source_id,
            },
            "x264": {
                "version": x264_version,
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
            "licensesIncluded": true,
        }
    });

    (status, sbom)
}
