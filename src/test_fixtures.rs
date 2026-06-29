//! Canonical fixture contract for tests and benchmarks.
//!
//! Non-integration fixtures live in git under `test/fixtures/`. Integration
//! media that exercises the public media-library path stays under `media/`.
//! Tests and benches should resolve them through this module so fixture drift
//! fails loudly instead of silently depending on whatever happens to be local.

use std::path::PathBuf;

pub const REQUIRED_CHECKED_IN_FIXTURES: &[&str] = &[
    "test/fixtures/correctness-h264.ts",
    "test/fixtures/correctness-h265.ts",
    "media/colorbar-timer-2v16a.mp4",
    "test/mediamtx-sink.yml",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn checked_in_fixture(relative_path: &str) -> Result<PathBuf, String> {
    let path = repo_root().join(relative_path);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "required checked-in fixture missing at {}; restore it from git",
            path.display()
        ))
    }
}

pub fn canonical_h264_ts_fixture() -> Result<PathBuf, String> {
    checked_in_fixture("test/fixtures/correctness-h264.ts")
}

pub fn canonical_h265_ts_fixture() -> Result<PathBuf, String> {
    checked_in_fixture("test/fixtures/correctness-h265.ts")
}

pub fn canonical_ts_fixture(codec: &str) -> Result<PathBuf, String> {
    match codec {
        "h264" | "avc" => canonical_h264_ts_fixture(),
        "h265" | "hevc" => canonical_h265_ts_fixture(),
        other => Err(format!("unsupported transport fixture codec {other:?}")),
    }
}
