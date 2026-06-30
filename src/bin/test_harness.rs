use axum::Router;
use axum::extract::{DefaultBodyLimit, OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use bytes::Bytes;
use chrono::Utc;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

const SUITE_DEFAULT_MODES: &[&str] = &[
    "api-smoke",
    "ramp-family",
    "mixed-h264-rtmp",
    "mixed-anchor",
    "mixed-h265-srt",
    "mixed-h264-srt-multi",
    "mixed-h265-srt-multi",
    "bframe-rtmp",
    "correctness-srt-rtmp",
    "correctness-srt-policy",
    "correctness-hevc-rtmp",
    "correctness-hevc-srt",
    "fault-resilience",
    "mixed-file-h264",
    "resource-sweep",
    "bitrate-sweep",
    "branch-matrix",
    "srt-crypto-matrix",
];

const SINK_PORT: u16 = 12935;

fn path_profile(path: &Path) -> Option<&'static str> {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == "target" {
            return components
                .next()
                .and_then(|value| value.as_os_str().to_str())
                .and_then(|value| match value {
                    "debug" => Some("debug"),
                    "release" => Some("release"),
                    "bench" => Some("bench"),
                    _ => None,
                });
        }
    }
    None
}

fn is_bench_profile(path: &Path) -> bool {
    matches!(path_profile(path), Some("bench"))
}

fn default_work_db_path(work_dir: &Path, file_name: &str) -> PathBuf {
    // Keep mutable harness state scoped to each WORK_DIR so long suites do not
    // contend through a shared repo-root SQLite database.
    work_dir.join(file_name)
}

fn command_requires_port_namespace(command: &str) -> bool {
    matches!(
        command,
        "api-smoke"
            | "correctness"
            | "correctness-rtmp"
            | "correctness-srt"
            | "correctness-srt-rtmp"
            | "correctness-srt-policy"
            | "bframe-rtmp"
            | "ramp-family"
            | "mixed-h264-rtmp"
            | "mixed-anchor"
            | "mixed-h265-srt"
            | "mixed-h264-srt-multi"
            | "mixed-h265-srt-multi"
            | "egress"
            | "correctness-hevc-rtmp"
            | "correctness-hevc-srt"
            | "fault-egress-retry"
            | "fault-resilience"
            | "recovery"
            | "mixed-file-h264"
            | "resource-sweep"
            | "bitrate-sweep"
            | "branch-matrix"
            | "srt-crypto-matrix"
    )
}

fn command_uses_host_net(raw: &[String]) -> bool {
    raw.iter().any(|arg| arg == "--no-netns")
        || std::env::var("TEST_HARNESS_USE_HOST_NET")
            .ok()
            .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn strip_netns_opt(raw: &[String]) -> Vec<String> {
    raw.iter()
        .filter(|arg| arg.as_str() != "--no-netns")
        .cloned()
        .collect()
}

fn netns_available() -> bool {
    std::process::Command::new("unshare")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn maybe_reexec_in_port_namespace() -> Result<(), String> {
    if std::env::var_os("RESTREAM_HARNESS_IN_NETNS").is_some() {
        return Ok(());
    }

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let command = raw.first().map(String::as_str).unwrap_or("suite");
    if command == "suite"
        || command == "preflight"
        || !command_requires_port_namespace(command)
        || command_uses_host_net(&raw)
    {
        return Ok(());
    }

    if !netns_available() {
        return Err(format!(
            "{command} requires a network namespace by default; install `unshare` support or rerun with --no-netns"
        ));
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = std::process::Command::new("unshare")
        .args(["--net", "--user", "--map-root-user"])
        .arg(&exe)
        .args(strip_netns_opt(&raw))
        .env("RESTREAM_HARNESS_IN_NETNS", "1")
        .status()
        .map_err(|e| format!("failed to re-exec {command} inside a network namespace: {e}"))?;

    let code = status.code().unwrap_or(1);
    unsafe { libc::_exit(code) };
}

// Measurement-oriented modes are only meaningful when both binaries come from
// the lightweight bench profile, so we fail fast instead of recording skewed
// numbers from debug or release builds.
fn measurement_mode_requires_bench_profile(mode: &str) -> bool {
    matches!(
        mode,
        "ramp-family"
            | "mixed-h264-rtmp"
            | "mixed-anchor"
            | "mixed-h265-srt"
            | "mixed-h264-srt-multi"
            | "mixed-h265-srt-multi"
            | "resource-sweep"
            | "bitrate-sweep"
            | "branch-matrix"
            | "srt-crypto-matrix"
    )
}

fn suite_modes_require_bench_profile(raw: &[String]) -> Result<bool, String> {
    let mut modes: Vec<String> = SUITE_DEFAULT_MODES.iter().map(|s| s.to_string()).collect();
    let mut preflight_only = false;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--only-modes" => {
                i += 1;
                modes = raw
                    .get(i)
                    .ok_or("--only-modes requires a value")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--run-id" | "--work-root" => {
                i += 1;
                raw.get(i)
                    .ok_or_else(|| format!("{} requires a value", raw[i - 1]))?;
            }
            "--no-netns" => {}
            "--continue-on-fail" => {}
            "--preflight-only" => preflight_only = true,
            other => return Err(format!("unknown suite option: {other}")),
        }
        i += 1;
    }

    if modes.is_empty() {
        return Err("--only-modes produced an empty mode list".to_string());
    }

    Ok(preflight_only
        || modes
            .iter()
            .any(|mode| measurement_mode_requires_bench_profile(mode)))
}

fn ensure_measurement_profile(command: &str, raw: &[String]) -> Result<(), String> {
    let needs_bench = if command == "suite" {
        suite_modes_require_bench_profile(raw)?
    } else {
        command == "preflight" || measurement_mode_requires_bench_profile(command)
    };
    if !needs_bench {
        return Ok(());
    }

    let harness_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let restream_path = default_restream_bin();
    if is_bench_profile(&harness_path) && is_bench_profile(&restream_path) {
        return Ok(());
    }

    Err(format!(
        "{command} requires bench-profile binaries for valid measurements; build them with `scripts/build-bench-harness.sh` and run `target/bench/test_harness`"
    ))
}

fn default_restream_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("RESTREAM_BIN").map(PathBuf::from) {
        return path;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bin_dir) = exe.parent()
    {
        let sibling = bin_dir.join("restream");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("target/release/restream")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(error) = maybe_reexec_in_port_namespace() {
        eprintln!("test harness failed: {error}");
        unsafe { libc::_exit(1) };
    }
    if let Err(error) = run().await {
        eprintln!("test harness failed: {error}");
        // Native FFmpeg/libsrt worker threads can still be alive on a failed
        // test. Avoid process-global C teardown while those threads exist.
        unsafe { libc::_exit(1) };
    }
}

fn ensure_loopback() {
    let _ = std::process::Command::new("ip")
        .args(["link", "set", "lo", "up"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

async fn run() -> Result<(), String> {
    ensure_loopback();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let command = raw.first().cloned().unwrap_or_else(|| "suite".to_string());
    ensure_measurement_profile(&command, &raw[1..])?;
    let result = match command.as_str() {
        "api-smoke" => api_smoke().await,
        "correctness" => correctness().await,
        "correctness-rtmp" => correctness_rtmp().await,
        "correctness-srt" => correctness_srt().await,
        "correctness-srt-rtmp" => srt_to_rtmp_correctness().await,
        "correctness-srt-policy" => srt_policy_correctness().await,
        "bframe-rtmp" => bframe_rtmp_correctness().await,
        "ramp-family" => ramp_family_correctness().await,
        "mixed-h264-rtmp" => mixed_h264_rtmp_correctness().await,
        "mixed-anchor" => mixed_anchor_correctness().await,
        "mixed-h265-srt" => mixed_h265_srt_correctness().await,
        "mixed-h264-srt-multi" => mixed_h264_srt_multi_correctness().await,
        "mixed-h265-srt-multi" => mixed_h265_srt_multi_correctness().await,
        "suite" => suite_run().await,
        "preflight" => preflight_check().await,
        "egress" => egress_correctness().await,
        "correctness-hevc-rtmp" => hevc_rtmp_egress_correctness().await,
        "correctness-hevc-srt" => hevc_srt_passthrough_correctness().await,
        "fault-egress-retry" => fault_egress_retry().await,
        "fault-resilience" => fault_resilience().await,
        "recovery" => recovery().await,
        "mixed-file-h264" => mixed_file_h264_correctness().await,
        "resource-sweep" => resource_sweep().await,
        "bitrate-sweep" => bitrate_sweep().await,
        "branch-matrix" => branch_matrix().await,
        "srt-crypto-matrix" => srt_crypto_matrix().await,
        other => Err(format!(
            "unknown command {other:?}; use suite, preflight, api-smoke, correctness, \
              correctness-rtmp, correctness-srt, correctness-srt-rtmp, correctness-srt-policy, \
              bframe-rtmp, ramp-family, mixed-h264-rtmp, mixed-anchor, \
              mixed-h265-srt, mixed-h264-srt-multi, mixed-h265-srt-multi, \
              egress, correctness-hevc-rtmp, correctness-hevc-srt, \
              fault-egress-retry, fault-resilience, recovery, mixed-file-h264, resource-sweep, bitrate-sweep, \
              branch-matrix, or srt-crypto-matrix"
        )),
    };

    match result {
        Ok(value) => {
            let path = artifact_path(&format!("{command}.json"));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap())
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            println!("artifact={}", path.display());
            // Skip runtime teardown — OS threads holding FFmpeg/SRT C contexts
            // race with global cleanup and cause spurious segfaults on exit.
            // Use _exit to also skip atexit handlers (FFmpeg codec deregistration
            // can deadlock with OS threads).
            unsafe { libc::_exit(0) };
        }
        Err(error) => Err(error),
    }
}

fn artifact_path(name: &str) -> PathBuf {
    std::env::var_os("TEST_HARNESS_ARTIFACT_DIR")
        .or_else(|| std::env::var_os("WORK_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test/artifacts/latest"))
        .join(name)
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn harness_srt_passphrase() -> Option<String> {
    std::env::var("HARNESS_SRT_PASSPHRASE")
        .ok()
        .filter(|value| !value.is_empty())
}

fn harness_srt_pbkeylen() -> Option<String> {
    std::env::var("HARNESS_SRT_PBKEYLEN")
        .ok()
        .filter(|value| !value.is_empty())
}

#[derive(Clone, Debug)]
struct HarnessSrtCrypto {
    label: String,
    passphrase: Option<String>,
    pbkeylen: Option<String>,
}

impl HarnessSrtCrypto {
    fn plaintext() -> Self {
        Self {
            label: "plaintext".to_string(),
            passphrase: None,
            pbkeylen: None,
        }
    }

    fn encrypted(pbkeylen: u32) -> Self {
        Self {
            label: format!("encrypted-{pbkeylen}"),
            passphrase: Some("0123456789abcd".to_string()),
            pbkeylen: Some(pbkeylen.to_string()),
        }
    }

    fn transport_label(&self) -> String {
        match (&self.passphrase, &self.pbkeylen) {
            (None, _) => "plaintext".to_string(),
            (Some(_), Some(len)) => format!("encrypted-{len}"),
            (Some(_), None) => "encrypted".to_string(),
        }
    }
}

fn harness_srt_crypto_from_env() -> HarnessSrtCrypto {
    match harness_srt_passphrase() {
        Some(passphrase) => HarnessSrtCrypto {
            label: match harness_srt_pbkeylen() {
                Some(len) => format!("encrypted-{len}"),
                None => "encrypted".to_string(),
            },
            passphrase: Some(passphrase),
            pbkeylen: harness_srt_pbkeylen(),
        },
        None => HarnessSrtCrypto::plaintext(),
    }
}

fn parse_srt_crypto_variants(name: &str, default: &str) -> Result<Vec<HarnessSrtCrypto>, String> {
    let mut out = Vec::new();
    for part in std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let variant = match part.to_ascii_lowercase().as_str() {
            "plaintext" | "plain" => HarnessSrtCrypto::plaintext(),
            "encrypted-16" | "enc16" | "aes128" | "128" => HarnessSrtCrypto::encrypted(16),
            "encrypted-24" | "enc24" | "aes192" | "192" => HarnessSrtCrypto::encrypted(24),
            "encrypted-32" | "enc32" | "aes256" | "256" => HarnessSrtCrypto::encrypted(32),
            other => {
                return Err(format!(
                    "{name} contains unsupported SRT crypto variant '{other}'"
                ));
            }
        };
        if out
            .iter()
            .all(|existing: &HarnessSrtCrypto| existing.label != variant.label)
        {
            out.push(variant);
        }
    }
    if out.is_empty() {
        return Err(format!("{name} did not resolve to any SRT crypto variants"));
    }
    Ok(out)
}

fn append_srt_crypto(url: String, crypto: &HarnessSrtCrypto) -> String {
    let Some(passphrase) = crypto.passphrase.as_deref() else {
        return url;
    };
    let separator = if url.contains('?') { '&' } else { '?' };
    let mut out = format!("{url}{separator}passphrase={passphrase}");
    if let Some(pbkeylen) = crypto.pbkeylen.as_deref() {
        out.push_str(&format!("&pbkeylen={pbkeylen}"));
    }
    out
}

fn apply_srt_listener_env(cmd: &mut Command, crypto: &HarnessSrtCrypto) {
    if let Some(passphrase) = crypto.passphrase.as_deref() {
        cmd.env("RESTREAM_SRT_PASSPHRASE", passphrase);
        if let Some(pbkeylen) = crypto.pbkeylen.as_deref() {
            cmd.env("RESTREAM_SRT_PBKEYLEN", pbkeylen);
        }
    } else {
        cmd.env_remove("RESTREAM_SRT_PASSPHRASE");
        cmd.env_remove("RESTREAM_SRT_PBKEYLEN");
    }
}

fn apply_harness_srt_listener_env(cmd: &mut Command) {
    apply_srt_listener_env(cmd, &harness_srt_crypto_from_env());
}

// ── Shared test infrastructure (Phase 1) ────────────────────────────────────
//
// `TestPorts` + `start_restream_child` de-duplicate the port and child-process
// setup that was previously inlined in `start_ramp_restream` and
// `start_mixed_restream`.

struct TestPorts {
    http: u16,
    rtmp: u16,
    srt: u16,
}

#[derive(Clone, Copy)]
struct HarnessPortDefaults {
    restream_http: u16,
    restream_rtmp: u16,
    restream_srt: u16,
    mtx_rtmp: u16,
    mtx_srt: u16,
    mtx_hls: u16,
    mtx_api: u16,
}

static HARNESS_PORT_DEFAULTS: OnceLock<HarnessPortDefaults> = OnceLock::new();

impl TestPorts {
    fn from_env() -> Self {
        let ports = harness_port_defaults();
        Self {
            http: ports.restream_http,
            rtmp: ports.restream_rtmp,
            srt: ports.restream_srt,
        }
    }
}

async fn start_restream_child(
    bin: &Path,
    ports: &TestPorts,
    db_path: &Path,
    log_path: &Path,
) -> Result<Child, String> {
    start_restream_child_opts(bin, ports, db_path, log_path, true).await
}

async fn start_restream_child_opts(
    bin: &Path,
    ports: &TestPorts,
    db_path: &Path,
    log_path: &Path,
    clean_db: bool,
) -> Result<Child, String> {
    if !bin.exists() {
        return Err(format!("restream binary not found at {}", bin.display()));
    }
    if clean_db {
        cleanup_ramp_db(db_path);
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let log_dir = log_path
        .parent()
        .map(|parent| parent.join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"));
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let stderr_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new(bin)
        .env("RESTREAM_HTTP_PORT", ports.http.to_string())
        .env("RESTREAM_RTMP_PORT", ports.rtmp.to_string())
        .env("RESTREAM_SRT_PORT", ports.srt.to_string())
        .env("RESTREAM_LOG_DIR", &log_dir)
        .env("RESTREAM_DB_PATH", db_path.to_string_lossy().to_string())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/healthz", ports.http),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut child).await;
        return Err(format!("restream did not become ready: {err}"));
    }
    Ok(child)
}

// ── Generalized harness sink (Phase 1) ──────────────────────────────────────
//
// Extends the existing SinkMetrics from byte-counting to packet-level tracking
// with timestamps, format, keyframe flags, and counts — the single source of
// truth for egress correctness in live tests.

struct SinkPacket {
    media_type: &'static str,
    timestamp_ms: u64,
}

struct GeneralizedSinkMetrics {
    connections: AtomicUsize,
    publishing: AtomicUsize,
    messages: AtomicU64,
    bytes: AtomicU64,
    video_count: AtomicU64,
    audio_count: AtomicU64,
    keyframe_count: AtomicU64,
    packets: Mutex<Vec<SinkPacket>>,
    video_codec: Mutex<Option<String>>,
    audio_codec: Mutex<Option<String>>,
}

impl Default for GeneralizedSinkMetrics {
    fn default() -> Self {
        Self {
            connections: AtomicUsize::new(0),
            publishing: AtomicUsize::new(0),
            messages: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            video_count: AtomicU64::new(0),
            audio_count: AtomicU64::new(0),
            keyframe_count: AtomicU64::new(0),
            packets: Mutex::new(Vec::new()),
            video_codec: Mutex::new(None),
            audio_codec: Mutex::new(None),
        }
    }
}

impl GeneralizedSinkMetrics {
    fn dts_monotone(&self) -> bool {
        let packets = self.packets.lock().unwrap();
        let mut last_video_ts: Option<u64> = None;
        for pkt in packets.iter() {
            if pkt.media_type == "video" {
                if let Some(prev) = last_video_ts {
                    if pkt.timestamp_ms < prev {
                        return false;
                    }
                }
                last_video_ts = Some(pkt.timestamp_ms);
            }
        }
        true
    }

    fn summary(&self) -> Value {
        json!({
            "connections": self.connections.load(Ordering::Relaxed),
            "publishing": self.publishing.load(Ordering::Relaxed),
            "messages": self.messages.load(Ordering::Relaxed),
            "bytes": self.bytes.load(Ordering::Relaxed),
            "videoCount": self.video_count.load(Ordering::Relaxed),
            "audioCount": self.audio_count.load(Ordering::Relaxed),
            "keyframeCount": self.keyframe_count.load(Ordering::Relaxed),
            "dtsMonotone": self.dts_monotone(),
        })
    }
}

async fn handle_generalized_sink_client(
    mut socket: TcpStream,
    metrics: Arc<GeneralizedSinkMetrics>,
) -> Result<(), String> {
    metrics.connections.fetch_add(1, Ordering::Relaxed);
    let mut handshake = Handshake::new(PeerType::Server);
    let mut buffer = vec![0u8; 8_192];
    let remaining = loop {
        let n = socket.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("socket closed during handshake".to_string());
        }
        match handshake
            .process_bytes(&buffer[..n])
            .map_err(|e| format!("handshake: {e:?}"))?
        {
            HandshakeProcessResult::InProgress { response_bytes } => {
                socket
                    .write_all(&response_bytes)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                socket
                    .write_all(&response_bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                break remaining_bytes;
            }
        }
    };

    let (mut session, initial) =
        ServerSession::new(ServerSessionConfig::new()).map_err(|e| format!("{e:?}"))?;
    write_generalized_sink_results(&mut socket, &mut session, initial, &metrics).await?;
    if !remaining.is_empty() {
        let results = session
            .handle_input(&remaining)
            .map_err(|e| format!("{e:?}"))?;
        write_generalized_sink_results(&mut socket, &mut session, results, &metrics).await?;
    }

    loop {
        let n = socket.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        let results = session
            .handle_input(&buffer[..n])
            .map_err(|e| format!("{e:?}"))?;
        write_generalized_sink_results(&mut socket, &mut session, results, &metrics).await?;
    }
}

async fn write_generalized_sink_results(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    results: Vec<ServerSessionResult>,
    metrics: &GeneralizedSinkMetrics,
) -> Result<(), String> {
    let mut pending = results;
    while let Some(result) = pending.pop() {
        match result {
            ServerSessionResult::OutboundResponse(packet) => {
                socket
                    .write_all(&packet.bytes)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            ServerSessionResult::RaisedEvent(event) => match event {
                ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                    let mut accepted = session
                        .accept_request(request_id)
                        .map_err(|e| format!("{e:?}"))?;
                    pending.append(&mut accepted);
                }
                ServerSessionEvent::PublishStreamRequested { request_id, .. } => {
                    let mut accepted = session
                        .accept_request(request_id)
                        .map_err(|e| format!("{e:?}"))?;
                    metrics.publishing.fetch_add(1, Ordering::Relaxed);
                    pending.append(&mut accepted);
                }
                ServerSessionEvent::VideoDataReceived {
                    data, timestamp, ..
                } => {
                    metrics.messages.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    metrics.video_count.fetch_add(1, Ordering::Relaxed);
                    let tag = data.first().copied().unwrap_or(0);
                    let is_keyframe = (tag & 0xF0) == 0x10 || tag == 0x90;
                    if is_keyframe {
                        metrics.keyframe_count.fetch_add(1, Ordering::Relaxed);
                    }
                    if metrics.video_codec.lock().unwrap().is_none() {
                        let codec = if tag & 0x80 != 0 {
                            if data.len() >= 5 {
                                match &data[1..5] {
                                    b"hvc1" => Some("hevc"),
                                    b"av01" => Some("av1"),
                                    b"vp09" => Some("vp9"),
                                    _ => Some("h264"),
                                }
                            } else {
                                None
                            }
                        } else {
                            match tag & 0x0F {
                                7 => Some("h264"),
                                12 => Some("hevc"),
                                _ => None,
                            }
                        };
                        if let Some(c) = codec {
                            *metrics.video_codec.lock().unwrap() = Some(c.to_string());
                        }
                    }
                    if let Ok(mut pkts) = metrics.packets.lock() {
                        pkts.push(SinkPacket {
                            media_type: "video",
                            timestamp_ms: timestamp.value as u64,
                        });
                    }
                }
                ServerSessionEvent::AudioDataReceived {
                    data, timestamp, ..
                } => {
                    metrics.messages.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    metrics.audio_count.fetch_add(1, Ordering::Relaxed);
                    if metrics.audio_codec.lock().unwrap().is_none() {
                        if let Some(&tag) = data.first() {
                            let codec = match (tag >> 4) & 0x0F {
                                10 => Some("aac"),
                                2 => Some("mp3"),
                                _ => None,
                            };
                            if let Some(c) = codec {
                                *metrics.audio_codec.lock().unwrap() = Some(c.to_string());
                            }
                        }
                    }
                    if let Ok(mut pkts) = metrics.packets.lock() {
                        pkts.push(SinkPacket {
                            media_type: "audio",
                            timestamp_ms: timestamp.value as u64,
                        });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

// ── Harness sink probe (Phase 4) ──────────────────────────────────────────
//
// Spins up a generalized sink, creates an output pointed at it, waits for
// packets, asserts DTS monotonicity / video+audio presence / keyframes,
// then tears down. Returns the sink summary for embedding in test results.

struct SinkProbeResult {
    passed: bool,
    summary: Value,
    output_id: String,
}

async fn run_sink_probe(
    api: &RampApi,
    pipeline_id: &str,
    label: &str,
    encoding: &str,
    sink_port: u16,
    min_video: u64,
) -> Result<SinkProbeResult, String> {
    let sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/sink-probe-{label}");
    let output_id = create_mixed_output(
        api,
        pipeline_id,
        &format!("sink-{label}"),
        &sink_url,
        encoding,
    )
    .await?;
    start_mixed_output(api, pipeline_id, &output_id).await?;

    let metrics = Arc::new(GeneralizedSinkMetrics::default());
    let listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink bind {sink_port}: {e}"))?;
    let m = metrics.clone();
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let m = m.clone();
            tokio::spawn(handle_generalized_sink_client(socket, m));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    while metrics.video_count.load(Ordering::Relaxed) < min_video {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dts_ok = metrics.dts_monotone();
    let video = metrics.video_count.load(Ordering::Relaxed);
    let audio = metrics.audio_count.load(Ordering::Relaxed);
    let keyframes = metrics.keyframe_count.load(Ordering::Relaxed);
    let summary = metrics.summary();
    task.abort();

    // Stop the output
    let _ = api
        .post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
            json!({}),
        )
        .await;

    let passed = video >= min_video && audio > 0 && keyframes > 0 && dts_ok;
    if !passed {
        eprintln!(
            "[sink-probe:{label}] FAIL: video={video} audio={audio} keyframes={keyframes} dts_monotone={dts_ok}"
        );
    } else {
        println!(
            "[sink-probe:{label}] ok: video={video} audio={audio} keyframes={keyframes} dts_monotone={dts_ok}"
        );
    }

    Ok(SinkProbeResult {
        passed,
        summary,
        output_id,
    })
}

struct HlsPutProbeResult {
    passed: bool,
    summary: Value,
    output_id: String,
}

async fn run_hls_put_probe(
    api: &RampApi,
    pipeline_id: &str,
    label: &str,
    put_port: u16,
) -> Result<HlsPutProbeResult, String> {
    let sink_dir = artifact_path(&format!("hls-put-probe-{label}"));
    let _ = std::fs::remove_dir_all(&sink_dir);
    std::fs::create_dir_all(&sink_dir).map_err(|e| e.to_string())?;

    let (sink_cancel, sink_handle) = start_hls_put_sink(put_port, sink_dir.clone()).await?;

    let put_url =
        format!("http://127.0.0.1:{put_port}/upload?cid=probe-{label}&copy=0&file=out.m3u8");
    let output_id = create_mixed_output(
        api,
        pipeline_id,
        &format!("hls-put-{label}"),
        &put_url,
        "source",
    )
    .await?;
    start_mixed_output(api, pipeline_id, &output_id).await?;

    let artifacts = wait_for_hls_put_artifacts(&sink_dir, Duration::from_secs(30)).await;
    let mut playlist_ok = false;
    let mut content_types_ok = false;
    let mut segment_ok = false;

    if let Ok(ref arts) = artifacts {
        playlist_ok = validate_hls_playlist(&arts.youtube_playlist, "probe").is_ok();

        if let Ok(requests) = read_hls_put_requests(&sink_dir) {
            let playlist_ct = request_seen(&requests, |r| {
                r["file"] == "out.m3u8" && r["contentType"] == "application/vnd.apple.mpegurl"
            });
            let segment_ct = request_seen(&requests, |r| {
                r["file"]
                    .as_str()
                    .is_some_and(|f| is_segment_file(f, "seg"))
                    && r["contentType"] == "video/mp2t"
            });
            content_types_ok = playlist_ct && segment_ct;
        }

        if let Ok(probe) = ffprobe(&arts.youtube_segment.to_string_lossy()).await {
            let has_video = probe["streams"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["codec_type"] == "video"));
            let has_audio = probe["streams"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["codec_type"] == "audio"));
            segment_ok = has_video && has_audio;
        }
    }

    let status = api
        .get_json(&format!(
            "/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/status"
        ))
        .await
        .ok();
    let status_ok = status
        .as_ref()
        .is_some_and(|s| s["bytesOut"].as_u64().unwrap_or(0) > 0);

    let _ = api
        .post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
            json!({}),
        )
        .await;

    sink_cancel.cancel();
    let _ = sink_handle.await;

    let passed = playlist_ok && content_types_ok && segment_ok && status_ok;
    let summary = json!({
        "playlistValid": playlist_ok,
        "contentTypesCorrect": content_types_ok,
        "segmentDecodable": segment_ok,
        "artifactsFound": artifacts.is_ok(),
        "outputStatus": status,
    });

    if !passed {
        eprintln!(
            "[hls-put-probe:{label}] FAIL: playlist={playlist_ok} content_types={content_types_ok} segment={segment_ok} status={status_ok}"
        );
    } else {
        println!(
            "[hls-put-probe:{label}] ok: playlist={playlist_ok} content_types={content_types_ok} segment={segment_ok} status={status_ok}"
        );
    }

    Ok(HlsPutProbeResult {
        passed,
        summary,
        output_id,
    })
}

async fn run_burst_graph_check(api: &RampApi, pipeline_id: &str) -> Result<(bool, Value), String> {
    let graph = api
        .get_json(&format!("/api/v1/pipelines/{pipeline_id}/graph"))
        .await?;
    let readers = graph_ring_readers(&graph);
    let burst_ok = readers
        .iter()
        .filter(|r| {
            r["burstCount"].as_u64().unwrap_or(0) > 0
                && r["avgBurstSize"].as_f64().unwrap_or(0.0) > 0.0
        })
        .count();
    let passed = !readers.is_empty() && burst_ok == readers.len();
    let summary = json!({
        "readerCount": readers.len(),
        "burstOk": burst_ok,
    });
    Ok((passed, summary))
}

#[derive(Clone, Copy)]
struct RampConfig {
    name: &'static str,
    ingest_proto: &'static str,
    out_proto: &'static str,
    encoding: &'static str,
}

struct RampEnv {
    work_dir: PathBuf,
    scale_log: PathBuf,
    summary_log: PathBuf,
    restream_log: PathBuf,
    mediamtx_log: PathBuf,
    mediamtx_config: PathBuf,
    restream_bin: PathBuf,
    restream_db_path: PathBuf,
    restream_http: u16,
    restream_rtmp: u16,
    restream_srt: u16,
    mtx_rtmp: u16,
    mtx_srt: u16,
    mtx_api: u16,
    n_outputs: usize,
    snap_every: usize,
    snapshot_sleep: Duration,
    cleanup_sleep: Duration,
}

impl RampEnv {
    fn from_env() -> Self {
        let work_dir = std::env::var_os("WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test/artifacts/ramp"));
        let ports = harness_port_defaults();
        Self {
            scale_log: std::env::var_os("SCALE_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("scale.csv")),
            summary_log: std::env::var_os("SUMMARY_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("summary.txt")),
            restream_log: std::env::var_os("RAMP_RESTREAM_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("restream.log")),
            mediamtx_log: std::env::var_os("RAMP_MEDIAMTX_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("mediamtx.log")),
            mediamtx_config: std::env::var_os("RAMP_MEDIAMTX_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("mediamtx.yml")),
            restream_bin: default_restream_bin(),
            restream_db_path: std::env::var_os("RESTREAM_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| default_work_db_path(&work_dir, "ramp.db")),
            restream_http: ports.restream_http,
            restream_rtmp: ports.restream_rtmp,
            restream_srt: ports.restream_srt,
            mtx_rtmp: ports.mtx_rtmp,
            mtx_srt: ports.mtx_srt,
            mtx_api: ports.mtx_api,
            n_outputs: env_usize("N_OUTPUTS", 10),
            snap_every: env_usize("SNAP_EVERY", 1).max(1),
            snapshot_sleep: Duration::from_secs(env_secs("SNAPSHOT_SLEEP_SECS", 3)),
            cleanup_sleep: Duration::from_secs(env_secs("RAMP_CONFIG_CLEANUP_SECS", 8)),
            work_dir,
        }
    }
}

struct RampApi {
    client: reqwest::Client,
    base_url: String,
    cookie: Option<String>,
}

impl RampApi {
    fn new(http_port: u16) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{http_port}"),
            cookie: None,
        }
    }

    async fn login(&mut self) -> Result<(), String> {
        let response = self
            .client
            .post(format!("{}/api/v1/auth/login", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(r#"{"password":"admin"}"#)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("login failed with HTTP {}", response.status()));
        }
        self.cookie = response
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::to_string);
        if self.cookie.is_none() {
            return Err("login response did not include a session cookie".to_string());
        }
        Ok(())
    }

    async fn get_json(&self, path: &str) -> Result<Value, String> {
        let mut request = self.client.get(format!("{}{}", self.base_url, path));
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        json_response(request).await
    }

    async fn get_text_response(&self, path: &str) -> Result<(reqwest::StatusCode, String), String> {
        let mut request = self.client.get(format!("{}{}", self.base_url, path));
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok((status, body))
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, String> {
        let mut request = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        json_response(request).await
    }

    async fn patch_json(&self, path: &str, body: Value) -> Result<Value, String> {
        let mut request = self
            .client
            .patch(format!("{}{}", self.base_url, path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        json_response(request).await
    }

    async fn put_json(&self, path: &str, body: Value) -> Result<Value, String> {
        let mut request = self
            .client
            .put(format!("{}{}", self.base_url, path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        json_response(request).await
    }

    async fn delete_json(&self, path: &str) -> Result<Value, String> {
        let mut request = self.client.delete(format!("{}{}", self.base_url, path));
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        json_response(request).await
    }
}

async fn get_logs(api: &RampApi, query: &str) -> Result<Vec<Value>, String> {
    let response = api.get_json(&format!("/api/v1/logs?{query}")).await?;
    response["logs"]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("logs response missing array for query: {query}"))
}

fn log_event_type(log: &Value) -> Option<&str> {
    log["eventType"].as_str()
}

fn log_target(log: &Value) -> Option<&str> {
    log["target"].as_str()
}

fn log_message(log: &Value) -> Option<&str> {
    log["message"].as_str()
}

fn log_pipeline_id(log: &Value) -> Option<&str> {
    log["pipelineId"].as_str()
}

fn parse_log_fields(log: &Value) -> Option<Value> {
    let fields = log.get("fields")?;
    match fields {
        Value::Object(_) => Some(fields.clone()),
        Value::String(raw) if !raw.trim().is_empty() => serde_json::from_str(raw).ok(),
        _ => None,
    }
}

fn log_has_correlation_id(log: &Value) -> bool {
    parse_log_fields(log)
        .and_then(|fields| {
            fields
                .get("correlation_id")
                .and_then(|value| value.as_str())
                .or_else(|| fields.get("correlationId").and_then(|value| value.as_str()))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .is_some()
}

fn logs_contain_event(logs: &[Value], event_type: &str) -> bool {
    logs.iter()
        .any(|log| log_event_type(log) == Some(event_type))
}

async fn verify_api_smoke_history_contract(api: &RampApi) -> Result<Value, String> {
    let lifecycle_logs = get_logs(api, "event_class=lifecycle&limit=50&order=desc").await?;

    Ok(json!({
        "logsEndpointOk": true,
        "logCount": lifecycle_logs.len(),
    }))
}

async fn verify_live_history_contract(
    api: &RampApi,
    expected_event_types: &[&str],
) -> Result<Value, String> {
    let all_logs = get_logs(api, "limit=2000&order=desc").await?;

    let pipeline_logs: Vec<Value> = all_logs
        .iter()
        .filter(|log| log_pipeline_id(log).is_some())
        .cloned()
        .collect();
    if pipeline_logs.is_empty() {
        return Err("live history contract found no pipeline-scoped logs".to_string());
    }

    let missing_event_types: Vec<&str> = expected_event_types
        .iter()
        .copied()
        .filter(|event_type| !logs_contain_event(&pipeline_logs, event_type))
        .collect();
    if !missing_event_types.is_empty() {
        return Err(format!(
            "live history contract missing lifecycle events: {}",
            missing_event_types.join(", ")
        ));
    }

    let correlated_pipeline_log_count = pipeline_logs
        .iter()
        .filter(|log| log_has_correlation_id(log))
        .count();

    let ext_transcoder_logs: Vec<Value> = pipeline_logs
        .iter()
        .filter(|log| {
            log_target(log).is_some_and(|target| target.contains("external_transcoder"))
                || log_message(log).is_some_and(|message| message.contains("[ext-transcoder]"))
        })
        .cloned()
        .collect();
    let ext_transcoder_correlated = ext_transcoder_logs.iter().any(log_has_correlation_id);

    Ok(json!({
        "pipelineLogCount": pipeline_logs.len(),
        "expectedEventTypes": expected_event_types,
        "correlatedPipelineLogCount": correlated_pipeline_log_count,
        "externalTranscoderLogCount": ext_transcoder_logs.len(),
        "externalTranscoderCorrelated": ext_transcoder_correlated,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResourceSweepLifecycle {
    Isolated,
    Continuous,
    Cumulative,
}

impl ResourceSweepLifecycle {
    fn from_env() -> Result<Self, String> {
        match std::env::var("RESOURCE_SWEEP_LIFECYCLE")
            .unwrap_or_else(|_| "isolated".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "isolated" => Ok(Self::Isolated),
            "continuous" => Ok(Self::Continuous),
            "cumulative" => Ok(Self::Cumulative),
            other => Err(format!(
                "RESOURCE_SWEEP_LIFECYCLE must be isolated, continuous, or cumulative (got {other})"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Continuous => "continuous",
            Self::Cumulative => "cumulative",
        }
    }
}

#[derive(Clone)]
struct ResourceSweepEnv {
    work_dir: PathBuf,
    summary_json: PathBuf,
    summary_csv: PathBuf,
    samples_jsonl: PathBuf,
    restream_log: PathBuf,
    mediamtx_log: PathBuf,
    mediamtx_config: PathBuf,
    restream_bin: PathBuf,
    restream_db_path: PathBuf,
    restream_http: u16,
    restream_rtmp: u16,
    restream_srt: u16,
    mtx_rtmp: u16,
    mtx_srt: u16,
    mtx_api: u16,
    sample_secs: u64,
    sample_interval_ms: u64,
    settle_secs: u64,
    ingest_counts: Vec<usize>,
    egress_counts: Vec<usize>,
    scenario_filter: Option<HashSet<String>>,
    lifecycle: ResourceSweepLifecycle,
    no_cleanup: bool,
    srt_crypto: HarnessSrtCrypto,
}

impl ResourceSweepEnv {
    fn from_env() -> Result<Self, String> {
        Self::from_env_with_default_dir("test/artifacts/resource-sweep")
    }

    fn from_env_with_default_dir(default_dir: &str) -> Result<Self, String> {
        let work_dir = std::env::var_os("WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(default_dir));
        let ports = harness_port_defaults();
        Ok(Self {
            summary_json: work_dir.join("resource-sweep-results.json"),
            summary_csv: work_dir.join("resource-sweep-results.csv"),
            samples_jsonl: work_dir.join("resource-sweep-samples.jsonl"),
            restream_log: work_dir.join("restream.log"),
            mediamtx_log: work_dir.join("mediamtx.log"),
            mediamtx_config: work_dir.join("mediamtx.yml"),
            restream_bin: default_restream_bin(),
            restream_db_path: std::env::var_os("RESTREAM_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| default_work_db_path(&work_dir, "resource-sweep.db")),
            restream_http: ports.restream_http,
            restream_rtmp: ports.restream_rtmp,
            restream_srt: ports.restream_srt,
            mtx_rtmp: ports.mtx_rtmp,
            mtx_srt: ports.mtx_srt,
            mtx_api: ports.mtx_api,
            sample_secs: env_secs("RESOURCE_SWEEP_SAMPLE_SECS", 6),
            sample_interval_ms: env_secs("RESOURCE_SWEEP_SAMPLE_INTERVAL_MS", 1000),
            settle_secs: env_secs("RESOURCE_SWEEP_SETTLE_SECS", 4),
            ingest_counts: parse_usize_list("RESOURCE_SWEEP_INGEST_COUNTS", "1,3,5"),
            egress_counts: parse_usize_list("RESOURCE_SWEEP_EGRESS_COUNTS", "1,5,10"),
            scenario_filter: parse_string_set("RESOURCE_SWEEP_SCENARIOS"),
            lifecycle: ResourceSweepLifecycle::from_env()?,
            no_cleanup: std::env::var("RESOURCE_SWEEP_NO_CLEANUP")
                .ok()
                .is_some_and(|v| v == "1"),
            srt_crypto: harness_srt_crypto_from_env(),
            work_dir,
        })
    }

    fn scenario_enabled(&self, scenario: &str) -> bool {
        self.scenario_filter
            .as_ref()
            .is_none_or(|filter| filter.contains(scenario))
    }
}

fn parse_usize_list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .filter_map(|part| part.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .collect()
}

fn parse_string_set(name: &str) -> Option<HashSet<String>> {
    let values: HashSet<String> = std::env::var(name)
        .ok()?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    (!values.is_empty()).then_some(values)
}

fn parse_bitrate_specs(name: &str, default: &str) -> Result<Vec<BitrateSpec>, String> {
    let mut out = Vec::new();
    for part in std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let normalized = part.to_ascii_uppercase();
        let mbps = if let Some(value) = normalized.strip_suffix('M') {
            value
                .parse::<f64>()
                .map_err(|_| format!("invalid Mbps bitrate {part:?}"))?
        } else if let Some(value) = normalized.strip_suffix('K') {
            value
                .parse::<f64>()
                .map_err(|_| format!("invalid Kbps bitrate {part:?}"))?
                / 1000.0
        } else {
            normalized
                .parse::<f64>()
                .map_err(|_| format!("invalid bitrate {part:?}"))?
        };
        out.push(BitrateSpec {
            label: part.to_string(),
            mbps,
        });
    }
    if out.is_empty() {
        return Err(format!("{name} produced no bitrate values"));
    }
    Ok(out)
}

fn parse_sweep_configs(name: &str) -> Result<Vec<SweepConfig>, String> {
    let raw = std::env::var(name).unwrap_or_else(|_| {
        SWEEP_CONFIGS
            .iter()
            .map(|cfg| cfg.name)
            .collect::<Vec<_>>()
            .join(",")
    });
    let mut out = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let config = SWEEP_CONFIGS
            .iter()
            .copied()
            .find(|cfg| cfg.name == part)
            .ok_or_else(|| format!("unknown sweep config {part:?}"))?;
        out.push(config);
    }
    if out.is_empty() {
        return Err(format!("{name} produced no configs"));
    }
    Ok(out)
}

#[derive(Clone, Copy)]
struct SweepConfig {
    name: &'static str,
    ingest_proto: &'static str,
    video_codec: &'static str,
    multi_audio: bool,
}

const SWEEP_CONFIGS: &[SweepConfig] = &[
    SweepConfig {
        name: "h264-rtmp",
        ingest_proto: "rtmp",
        video_codec: "h264",
        multi_audio: false,
    },
    SweepConfig {
        name: "h264-srt",
        ingest_proto: "srt",
        video_codec: "h264",
        multi_audio: false,
    },
    SweepConfig {
        name: "h265-srt",
        ingest_proto: "srt",
        video_codec: "h265",
        multi_audio: false,
    },
    SweepConfig {
        name: "h264-srt-multi",
        ingest_proto: "srt",
        video_codec: "h264",
        multi_audio: true,
    },
    SweepConfig {
        name: "h265-srt-multi",
        ingest_proto: "srt",
        video_codec: "h265",
        multi_audio: true,
    },
];

#[derive(Clone, Copy)]
enum SweepOutputKind {
    RtmpSource,
    SrtSource,
    Rtmp720p,
    Srt720p,
    Rtmp1080p,
    Srt1080p,
}

impl SweepOutputKind {
    fn label(self) -> &'static str {
        match self {
            Self::RtmpSource => "rtmp-source",
            Self::SrtSource => "srt-source",
            Self::Rtmp720p => "rtmp-720p",
            Self::Srt720p => "srt-720p",
            Self::Rtmp1080p => "rtmp-1080p",
            Self::Srt1080p => "srt-1080p",
        }
    }
}

struct ResourceSweepStack {
    mediamtx: Child,
    restream: Child,
    api: RampApi,
    restream_pid: u32,
}

#[derive(Clone)]
struct BranchMatrixEnv {
    resource: ResourceSweepEnv,
    summary_json: PathBuf,
    summary_csv: PathBuf,
    summary_md: PathBuf,
    backend: String,
    srt_variants: Vec<HarnessSrtCrypto>,
    scenario_filter: Option<HashSet<String>>,
}

impl BranchMatrixEnv {
    fn from_env() -> Result<Self, String> {
        let mut resource =
            ResourceSweepEnv::from_env_with_default_dir("test/artifacts/branch-matrix")?;
        let work_dir = resource.work_dir.clone();
        let egress_count = env_usize("BRANCH_MATRIX_EGRESS_COUNT", 10).max(1);
        resource.egress_counts = vec![egress_count];
        resource.ingest_counts = vec![1];
        resource.summary_json = work_dir.join("branch-matrix-results.json");
        resource.summary_csv = work_dir.join("branch-matrix-results.csv");
        resource.samples_jsonl = work_dir.join("branch-matrix-samples.jsonl");
        if std::env::var_os("RESTREAM_DB_PATH").is_none() {
            resource.restream_db_path = work_dir.join("branch-matrix.db");
        }
        Ok(Self {
            summary_json: work_dir.join("branch-matrix-results.json"),
            summary_csv: work_dir.join("branch-matrix-results.csv"),
            summary_md: work_dir.join("branch-matrix-summary.md"),
            backend: if std::env::var("RESTREAM_USE_INTERNAL_TRANSCODER")
                .ok()
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            {
                "internal".to_string()
            } else {
                "external".to_string()
            },
            srt_variants: vec![harness_srt_crypto_from_env()],
            scenario_filter: parse_string_set("BRANCH_MATRIX_SCENARIOS"),
            resource,
        })
    }

    fn scenario_enabled(&self, scenario: &str) -> bool {
        self.scenario_filter
            .as_ref()
            .is_none_or(|filter| filter.contains(scenario))
    }
}

struct BitrateSweepEnv {
    work_dir: PathBuf,
    summary_json: PathBuf,
    summary_csv: PathBuf,
    samples_jsonl: PathBuf,
    restream_log: PathBuf,
    mediamtx_log: PathBuf,
    mediamtx_config: PathBuf,
    restream_bin: PathBuf,
    restream_db_path: PathBuf,
    restream_http: u16,
    restream_rtmp: u16,
    restream_srt: u16,
    mtx_rtmp: u16,
    mtx_srt: u16,
    mtx_api: u16,
    stabilize_secs: u64,
    sample_interval_secs: u64,
    output_groups: usize,
    no_cleanup: bool,
    bitrates: Vec<BitrateSpec>,
    configs: Vec<SweepConfig>,
}

impl BitrateSweepEnv {
    fn from_env() -> Result<Self, String> {
        let work_dir = std::env::var_os("WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test/artifacts/bitrate-sweep"));
        let ports = harness_port_defaults();
        Ok(Self {
            summary_json: work_dir.join("bitrate-sweep-results.json"),
            summary_csv: work_dir.join("bitrate-sweep-results.csv"),
            samples_jsonl: work_dir.join("bitrate-sweep-samples.jsonl"),
            restream_log: work_dir.join("restream.log"),
            mediamtx_log: work_dir.join("mediamtx.log"),
            mediamtx_config: work_dir.join("mediamtx.yml"),
            restream_bin: default_restream_bin(),
            restream_db_path: std::env::var_os("RESTREAM_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| default_work_db_path(&work_dir, "bitrate-sweep.db")),
            restream_http: ports.restream_http,
            restream_rtmp: ports.restream_rtmp,
            restream_srt: ports.restream_srt,
            mtx_rtmp: ports.mtx_rtmp,
            mtx_srt: ports.mtx_srt,
            mtx_api: ports.mtx_api,
            stabilize_secs: env_secs("BITRATE_SWEEP_STABILIZE_SECS", 30),
            sample_interval_secs: env_secs("BITRATE_SWEEP_SAMPLE_INTERVAL_SECS", 5).max(1),
            output_groups: env_usize("BITRATE_SWEEP_OUTPUT_GROUPS", 1).max(1),
            no_cleanup: std::env::var("BITRATE_SWEEP_NO_CLEANUP")
                .ok()
                .is_some_and(|v| v == "1"),
            bitrates: parse_bitrate_specs("BITRATE_SWEEP_BITRATES", "1.5M,4M,8M")?,
            configs: parse_sweep_configs("BITRATE_SWEEP_CONFIGS")?,
            work_dir,
        })
    }
}

#[derive(Clone)]
struct BitrateSpec {
    label: String,
    mbps: f64,
}

#[derive(Clone)]
struct BitrateSweepSample {
    config: String,
    bitrate_label: String,
    bitrate_mbps: f64,
    elapsed_secs: u64,
    restream_cpu_pct: f64,
    ffmpeg_cpu_pct: f64,
    total_cpu_pct: f64,
    restream_rss_kb: u64,
    ffmpeg_count: u64,
    ffmpeg_rss_kb: u64,
    total_rss_kb: u64,
    retained_payload_kb: u64,
    source_ring_kb: u64,
    transcoder_ring_kb: u64,
    tsmux_ring_kb: u64,
    avio_len_kb: u64,
    avio_hwm_kb: u64,
    overflow_count: u64,
}

struct BitrateSweepCase {
    config: String,
    ingest_proto: String,
    video_codec: String,
    multi_audio: bool,
    bitrate_label: String,
    bitrate_mbps: f64,
    output_groups: usize,
    outputs_total: usize,
    restream_rss_base_kb: u64,
    restream_rss_final_kb: u64,
    restream_rss_delta_kb: u64,
    restream_rss_peak_kb: u64,
    ffmpeg_count_peak: u64,
    ffmpeg_rss_peak_kb: u64,
    total_rss_peak_kb: u64,
    restream_cpu_avg_pct: f64,
    restream_cpu_peak_pct: f64,
    ffmpeg_cpu_avg_pct: f64,
    ffmpeg_cpu_peak_pct: f64,
    total_cpu_avg_pct: f64,
    total_cpu_peak_pct: f64,
    retained_payload_min_kb: u64,
    retained_payload_max_kb: u64,
    retained_payload_final_kb: u64,
    retained_growth_kb_per_min: f64,
    source_ring_peak_kb: u64,
    transcoder_ring_peak_kb: u64,
    tsmux_ring_peak_kb: u64,
    avio_len_peak_kb: u64,
    avio_hwm_peak_kb: u64,
    overflow_count_final: u64,
    correctness_ok: bool,
    correctness_failures: Vec<String>,
}

#[derive(Clone)]
struct ResourceSample {
    scenario: String,
    label: String,
    lifecycle: String,
    pipelines: usize,
    outputs: usize,
    ingest_types: String,
    egress_mix: String,
    transcode: String,
    restream_cpu_pct: f64,
    ffmpeg_cpu_pct: f64,
    total_cpu_pct: f64,
    rss_kb: u64,
    ffmpeg_count: u64,
    ffmpeg_rss_kb: u64,
    anonymous_kb: u64,
    private_dirty_kb: u64,
    private_clean_kb: u64,
    shared_clean_kb: u64,
    shared_dirty_kb: u64,
    pss_kb: u64,
    swap_kb: u64,
    retained_kb: u64,
    source_ring_kb: u64,
    transcoder_ring_kb: u64,
    tsmux_ring_kb: u64,
    avio_len_kb: u64,
    avio_hwm_kb: u64,
    active_transcoder_buffers: u64,
    ingests: usize,
    egresses: usize,
    stages: usize,
    pipeline_count: usize,
    unattributed_kb: u64,
}

struct ResourceAggregate {
    scenario: String,
    label: String,
    lifecycle: String,
    pipelines: usize,
    outputs: usize,
    ingest_types: String,
    egress_mix: String,
    transcode: String,
    sample_count: usize,
    restream_cpu_avg_pct: f64,
    restream_cpu_peak_pct: f64,
    ffmpeg_cpu_avg_pct: f64,
    ffmpeg_cpu_peak_pct: f64,
    total_cpu_avg_pct: f64,
    total_cpu_peak_pct: f64,
    rss_avg_kb: f64,
    rss_peak_kb: u64,
    ffmpeg_rss_peak_kb: u64,
    retained_peak_kb: u64,
    source_ring_peak_kb: u64,
    transcoder_ring_peak_kb: u64,
    tsmux_ring_peak_kb: u64,
    avio_len_peak_kb: u64,
    avio_hwm_peak_kb: u64,
    anonymous_peak_kb: u64,
    private_dirty_peak_kb: u64,
    shared_clean_peak_kb: u64,
    pss_peak_kb: u64,
    unattributed_peak_kb: u64,
    active_transcoder_buffers_peak: u64,
    ingests_peak: usize,
    egresses_peak: usize,
    stages_peak: usize,
    pipeline_count_peak: usize,
}

struct ResourceScenarioMeta<'a> {
    scenario: &'a str,
    label: String,
    pipelines: usize,
    outputs: usize,
    ingest_types: String,
    egress_mix: String,
    transcode: &'a str,
}

struct ProcMemRollup {
    anonymous_kb: u64,
    private_dirty_kb: u64,
    private_clean_kb: u64,
    shared_clean_kb: u64,
    shared_dirty_kb: u64,
    pss_kb: u64,
    swap_kb: u64,
}

async fn resource_sweep() -> Result<Value, String> {
    let env = ResourceSweepEnv::from_env()?;
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&env.summary_csv);
    let _ = std::fs::remove_file(&env.summary_json);
    let _ = std::fs::remove_file(&env.samples_jsonl);

    let mut stack = if env.lifecycle == ResourceSweepLifecycle::Isolated {
        None
    } else {
        Some(start_resource_sweep_stack(&env).await?)
    };
    let mut retained_publishers: Vec<Child> = Vec::new();
    let mut aggregates = Vec::new();

    if env.scenario_enabled("baseline-empty") {
        aggregates.push(run_resource_baseline(&env, &mut stack, &mut retained_publishers).await?);
    }
    if env.scenario_enabled("ingest-only") {
        for config in SWEEP_CONFIGS {
            aggregates.push(
                run_resource_ingest_only(&env, &mut stack, &mut retained_publishers, *config)
                    .await?,
            );
        }
    }
    if env.scenario_enabled("ingest-growth-same") {
        aggregates.extend(
            run_resource_ingest_growth(&env, &mut stack, &mut retained_publishers, false).await?,
        );
    }
    if env.scenario_enabled("ingest-growth-mixed") {
        aggregates.extend(
            run_resource_ingest_growth(&env, &mut stack, &mut retained_publishers, true).await?,
        );
    }
    if env.scenario_enabled("egress-growth-source-same") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-source-same",
                SWEEP_CONFIGS[1],
                &[SweepOutputKind::RtmpSource],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-source-mixed") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-source-mixed",
                SWEEP_CONFIGS[1],
                &[SweepOutputKind::RtmpSource, SweepOutputKind::SrtSource],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-transcode-same") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-transcode-same",
                SWEEP_CONFIGS[1],
                &[SweepOutputKind::Rtmp720p],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-transcode-mixed") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-transcode-mixed",
                SWEEP_CONFIGS[1],
                &[SweepOutputKind::Rtmp720p, SweepOutputKind::Srt720p],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-source-plus-transcode-mixed") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-source-plus-transcode-mixed",
                SWEEP_CONFIGS[1],
                &[
                    SweepOutputKind::RtmpSource,
                    SweepOutputKind::SrtSource,
                    SweepOutputKind::Rtmp720p,
                    SweepOutputKind::Srt720p,
                ],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-transcode-dual-mixed") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-transcode-dual-mixed",
                SWEEP_CONFIGS[1],
                &[
                    SweepOutputKind::Rtmp720p,
                    SweepOutputKind::Srt720p,
                    SweepOutputKind::Rtmp1080p,
                    SweepOutputKind::Srt1080p,
                ],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-source-plus-transcode-dual-mixed") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-source-plus-transcode-dual-mixed",
                SWEEP_CONFIGS[1],
                &[
                    SweepOutputKind::RtmpSource,
                    SweepOutputKind::SrtSource,
                    SweepOutputKind::Rtmp720p,
                    SweepOutputKind::Srt720p,
                    SweepOutputKind::Rtmp1080p,
                    SweepOutputKind::Srt1080p,
                ],
            )
            .await?,
        );
    }
    if env.scenario_enabled("egress-growth-hevc-bridge") {
        aggregates.extend(
            run_resource_egress_growth(
                &env,
                &mut stack,
                &mut retained_publishers,
                "egress-growth-hevc-bridge",
                SWEEP_CONFIGS[2],
                &[SweepOutputKind::RtmpSource],
            )
            .await?,
        );
    }

    write_resource_sweep_csv(&env.summary_csv, &aggregates)?;
    let result = json!({
        "mode": "resource-sweep",
        "lifecycle": env.lifecycle.as_str(),
        "artifacts": {
            "summaryJson": env.summary_json,
            "summaryCsv": env.summary_csv,
            "samplesJsonl": env.samples_jsonl,
            "restreamLog": env.restream_log,
            "mediamtxLog": env.mediamtx_log,
        },
        "aggregates": aggregates.iter().map(resource_aggregate_json).collect::<Vec<_>>(),
    });
    std::fs::write(
        &env.summary_json,
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .map_err(|e| e.to_string())?;

    if env.no_cleanup {
        println!("resource-sweep no-cleanup: leaving final stack running");
    } else {
        for child in &mut retained_publishers {
            stop_child(child).await;
        }
        if let Some(stack) = stack.as_mut() {
            stop_child(&mut stack.restream).await;
            stop_child(&mut stack.mediamtx).await;
        }
    }
    Ok(result)
}

async fn branch_matrix() -> Result<Value, String> {
    let env = BranchMatrixEnv::from_env()?;
    run_branch_matrix_variant(&env).await
}

async fn srt_crypto_matrix() -> Result<Value, String> {
    let mut env = BranchMatrixEnv::from_env()?;
    env.srt_variants =
        parse_srt_crypto_variants("SRT_CRYPTO_MATRIX_VARIANTS", "plaintext,enc16,enc24,enc32")?;

    let parent_work_dir = env.resource.work_dir.clone();
    let mut runs = Vec::new();
    for crypto in env.srt_variants.clone() {
        let mut variant_env = env.clone();
        variant_env.resource.srt_crypto = crypto.clone();
        variant_env.resource.work_dir = parent_work_dir.join(&crypto.label);
        variant_env.resource.summary_json = variant_env
            .resource
            .work_dir
            .join("branch-matrix-results.json");
        variant_env.resource.summary_csv = variant_env
            .resource
            .work_dir
            .join("branch-matrix-results.csv");
        variant_env.resource.samples_jsonl = variant_env
            .resource
            .work_dir
            .join("branch-matrix-samples.jsonl");
        variant_env.resource.restream_log = variant_env.resource.work_dir.join("restream.log");
        variant_env.resource.mediamtx_log = variant_env.resource.work_dir.join("mediamtx.log");
        variant_env.resource.mediamtx_config = variant_env.resource.work_dir.join("mediamtx.yml");
        variant_env.resource.restream_db_path =
            variant_env.resource.work_dir.join("branch-matrix.db");
        variant_env.summary_json = variant_env
            .resource
            .work_dir
            .join("branch-matrix-results.json");
        variant_env.summary_csv = variant_env
            .resource
            .work_dir
            .join("branch-matrix-results.csv");
        variant_env.summary_md = variant_env
            .resource
            .work_dir
            .join("branch-matrix-summary.md");
        runs.push(run_branch_matrix_variant(&variant_env).await?);
    }

    Ok(json!({
        "mode": "srt-crypto-matrix",
        "variants": runs,
    }))
}

async fn run_branch_matrix_variant(env: &BranchMatrixEnv) -> Result<Value, String> {
    let resource = &env.resource;
    std::fs::create_dir_all(&resource.work_dir).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&env.summary_csv);
    let _ = std::fs::remove_file(&env.summary_json);
    let _ = std::fs::remove_file(&env.summary_md);
    let _ = std::fs::remove_file(&resource.samples_jsonl);

    let mut stack = if resource.lifecycle == ResourceSweepLifecycle::Isolated {
        None
    } else {
        Some(start_resource_sweep_stack(resource).await?)
    };
    let mut retained_publishers: Vec<Child> = Vec::new();
    let mut aggregates = Vec::new();

    for (scenario_name, output_kinds) in [
        (
            "egress-growth-source-mixed",
            vec![SweepOutputKind::RtmpSource, SweepOutputKind::SrtSource],
        ),
        (
            "egress-growth-transcode-mixed",
            vec![SweepOutputKind::Rtmp720p, SweepOutputKind::Srt720p],
        ),
        (
            "egress-growth-source-plus-transcode-mixed",
            vec![
                SweepOutputKind::RtmpSource,
                SweepOutputKind::SrtSource,
                SweepOutputKind::Rtmp720p,
                SweepOutputKind::Srt720p,
            ],
        ),
        (
            "egress-growth-transcode-dual-mixed",
            vec![
                SweepOutputKind::Rtmp720p,
                SweepOutputKind::Srt720p,
                SweepOutputKind::Rtmp1080p,
                SweepOutputKind::Srt1080p,
            ],
        ),
        (
            "egress-growth-source-plus-transcode-dual-mixed",
            vec![
                SweepOutputKind::RtmpSource,
                SweepOutputKind::SrtSource,
                SweepOutputKind::Rtmp720p,
                SweepOutputKind::Srt720p,
                SweepOutputKind::Rtmp1080p,
                SweepOutputKind::Srt1080p,
            ],
        ),
    ] {
        if !env.scenario_enabled(scenario_name) {
            continue;
        }
        aggregates.extend(
            run_resource_egress_growth(
                resource,
                &mut stack,
                &mut retained_publishers,
                scenario_name,
                SWEEP_CONFIGS[1],
                &output_kinds,
            )
            .await?,
        );
    }

    write_resource_sweep_csv(&env.summary_csv, &aggregates)?;
    write_branch_matrix_markdown(
        &env.summary_md,
        &env.backend,
        &resource.srt_crypto.transport_label(),
        &aggregates,
    )?;
    let result = json!({
        "mode": "branch-matrix",
        "backend": env.backend,
        "srtIngestTransport": resource.srt_crypto.transport_label(),
        "lifecycle": resource.lifecycle.as_str(),
        "artifacts": {
            "summaryJson": env.summary_json,
            "summaryCsv": env.summary_csv,
            "summaryMarkdown": env.summary_md,
            "samplesJsonl": resource.samples_jsonl,
            "restreamLog": resource.restream_log,
            "mediamtxLog": resource.mediamtx_log,
        },
        "aggregates": aggregates.iter().map(resource_aggregate_json).collect::<Vec<_>>(),
    });
    std::fs::write(
        &env.summary_json,
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .map_err(|e| e.to_string())?;

    if resource.no_cleanup {
        println!("branch-matrix no-cleanup: leaving final stack running");
    } else {
        for child in &mut retained_publishers {
            stop_child(child).await;
        }
        if let Some(stack) = stack.as_mut() {
            stop_child(&mut stack.restream).await;
            stop_child(&mut stack.mediamtx).await;
        }
    }
    Ok(result)
}

async fn bitrate_sweep() -> Result<Value, String> {
    let env = BitrateSweepEnv::from_env()?;
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&env.summary_csv);
    let _ = std::fs::remove_file(&env.summary_json);
    let _ = std::fs::remove_file(&env.samples_jsonl);

    let mut rows = Vec::new();
    for config in &env.configs {
        for bitrate in &env.bitrates {
            let row = run_bitrate_case(&env, *config, bitrate).await?;
            rows.push(row);
        }
    }

    write_bitrate_sweep_csv(&env.summary_csv, &rows)?;
    let result = json!({
        "mode": "bitrate-sweep",
        "artifacts": {
            "summaryJson": env.summary_json,
            "summaryCsv": env.summary_csv,
            "samplesJsonl": env.samples_jsonl,
            "restreamLog": env.restream_log,
            "mediamtxLog": env.mediamtx_log,
        },
        "cases": rows.iter().map(bitrate_sweep_case_json).collect::<Vec<_>>(),
    });
    std::fs::write(
        &env.summary_json,
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    Ok(result)
}

async fn run_bitrate_case(
    env: &BitrateSweepEnv,
    config: SweepConfig,
    bitrate: &BitrateSpec,
) -> Result<BitrateSweepCase, String> {
    let mut stack = start_bitrate_sweep_stack(env).await?;
    let stream_key = format!(
        "bitrate-{}-{}",
        config.name,
        bitrate.label.to_ascii_lowercase().replace('.', "_")
    );
    let pipeline_id = create_resource_pipeline(&stack.api, config.name, &stream_key).await?;
    let srt_crypto = harness_srt_crypto_from_env();
    let mut publisher = spawn_resource_publisher_with_bitrate(
        env.restream_rtmp,
        env.restream_srt,
        &env.work_dir,
        &srt_crypto,
        config,
        &stream_key,
        &bitrate.label,
    )?;
    wait_for_api_input_live(&stack.api, &pipeline_id, Duration::from_secs(45)).await?;
    let restream_rss_base_kb =
        read_proc_status_kb_checked(stack.restream_pid, "VmRSS", &env.restream_log)?;

    let mut output_ids = Vec::new();
    let mut probe_specs = Vec::new();
    for index in 1..=env.output_groups {
        let names = bitrate_case_output_names(config.name, &bitrate.label, index);
        for (kind, name, expected) in [
            (SweepOutputKind::RtmpSource, names.rtmp_source, "1920x1080"),
            (SweepOutputKind::Rtmp720p, names.rtmp_720p, "1280x720"),
            (SweepOutputKind::SrtSource, names.srt_source, "1920x1080"),
            (SweepOutputKind::Srt720p, names.srt_720p, "1280x720"),
        ] {
            let (url, encoding) = bitrate_output_url(env, config, kind, &name);
            let output_id =
                create_mixed_output(&stack.api, &pipeline_id, &name, &url, &encoding).await?;
            start_mixed_output(&stack.api, &pipeline_id, &output_id).await?;
            output_ids.push(output_id);
            probe_specs.push((kind, name, expected.to_string()));
        }
    }
    wait_for_outputs_progress(
        &stack.api,
        &pipeline_id,
        &output_ids,
        Duration::from_secs(45),
    )
    .await?;

    let samples = sample_bitrate_window(env, &mut stack, config, bitrate, &pipeline_id).await?;
    let mut correctness_ok = true;
    let mut correctness_failures = Vec::new();
    for (kind, name, expected) in &probe_specs {
        let url = bitrate_probe_url(env, *kind, name);
        if let Some(observed) =
            check_bitrate_stream(name, &url, expected, Duration::from_secs(20)).await?
        {
            correctness_ok = false;
            correctness_failures.push(format!("{name}: expected {expected}, observed {observed}"));
        }
    }

    let restream_rss_final_kb =
        read_proc_status_kb_checked(stack.restream_pid, "VmRSS", &env.restream_log).unwrap_or(0);
    let ffmpeg = ffmpeg_children_stats(stack.restream_pid)?;

    stop_child(&mut publisher).await;
    delete_resource_pipeline(&stack.api, &pipeline_id).await;
    if !env.no_cleanup {
        stop_child(&mut stack.restream).await;
        stop_child(&mut stack.mediamtx).await;
    }

    summarize_bitrate_case(
        config,
        bitrate,
        env.output_groups,
        restream_rss_base_kb,
        restream_rss_final_kb,
        ffmpeg,
        correctness_ok,
        correctness_failures,
        &samples,
    )
}

async fn start_bitrate_sweep_stack(env: &BitrateSweepEnv) -> Result<ResourceSweepStack, String> {
    if !env.restream_bin.exists() {
        return Err(format!(
            "restream binary not found at {}",
            env.restream_bin.display()
        ));
    }
    std::fs::create_dir_all(env.work_dir.join("logs")).map_err(|e| e.to_string())?;
    cleanup_ramp_db(&env.restream_db_path);
    let mediamtx_log = std::fs::File::create(&env.mediamtx_log).map_err(|e| e.to_string())?;
    let mediamtx_err = mediamtx_log.try_clone().map_err(|e| e.to_string())?;
    std::fs::write(
        &env.mediamtx_config,
        format!(
            "logLevel: warn\nrtmp: yes\nrtmpAddress: :{}\nrtmpEncryption: \"no\"\nrtsp: no\nsrt: yes\nsrtAddress: :{}\nhls: no\nwebrtc: no\napi: yes\napiAddress: :{}\nmetrics: no\npaths:\n  all:\n",
            env.mtx_rtmp, env.mtx_srt, env.mtx_api
        ),
    )
    .map_err(|e| e.to_string())?;
    let mut mediamtx = Command::new("mediamtx")
        .arg(&env.mediamtx_config)
        .stdout(Stdio::from(mediamtx_log))
        .stderr(Stdio::from(mediamtx_err))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/v3/paths/list", env.mtx_api),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut mediamtx).await;
        return Err(format!("mediamtx did not become ready: {err}"));
    }

    let restream_log = std::fs::File::create(&env.restream_log).map_err(|e| e.to_string())?;
    let restream_err = restream_log.try_clone().map_err(|e| e.to_string())?;
    let mut restream_cmd = Command::new(&env.restream_bin);
    restream_cmd
        .env("RESTREAM_HTTP_PORT", env.restream_http.to_string())
        .env("RESTREAM_RTMP_PORT", env.restream_rtmp.to_string())
        .env("RESTREAM_SRT_PORT", env.restream_srt.to_string())
        .env("RESTREAM_LOG_DIR", env.work_dir.join("logs"))
        .env(
            "RESTREAM_DB_PATH",
            env.restream_db_path.to_string_lossy().to_string(),
        )
        .stdout(Stdio::from(restream_log))
        .stderr(Stdio::from(restream_err))
        .kill_on_drop(true);
    apply_harness_srt_listener_env(&mut restream_cmd);
    let mut restream = restream_cmd.spawn().map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/healthz", env.restream_http),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut restream).await;
        stop_child(&mut mediamtx).await;
        return Err(format!("restream did not become ready: {err}"));
    }
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;
    let restream_pid = restream.id().ok_or("restream pid missing")?;
    Ok(ResourceSweepStack {
        mediamtx,
        restream,
        api,
        restream_pid,
    })
}

struct BitrateOutputNames {
    rtmp_source: String,
    rtmp_720p: String,
    srt_source: String,
    srt_720p: String,
}

fn bitrate_case_output_names(
    config_name: &str,
    bitrate_label: &str,
    index: usize,
) -> BitrateOutputNames {
    let suffix = bitrate_label.to_ascii_lowercase().replace('.', "_");
    BitrateOutputNames {
        rtmp_source: format!("{config_name}-{suffix}-rtmp-src-{index}"),
        rtmp_720p: format!("{config_name}-{suffix}-rtmp-720p-{index}"),
        srt_source: format!("{config_name}-{suffix}-srt-src-{index}"),
        srt_720p: format!("{config_name}-{suffix}-srt-720p-{index}"),
    }
}

fn bitrate_output_url(
    env: &BitrateSweepEnv,
    config: SweepConfig,
    kind: SweepOutputKind,
    name: &str,
) -> (String, String) {
    match kind {
        SweepOutputKind::RtmpSource => (
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp),
            "source".to_string(),
        ),
        SweepOutputKind::SrtSource => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{name}",
                env.mtx_srt
            ),
            "source".to_string(),
        ),
        SweepOutputKind::Rtmp720p => (
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp),
            if config.multi_audio {
                "720p+atrack:0".to_string()
            } else {
                "720p".to_string()
            },
        ),
        SweepOutputKind::Srt720p => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{name}",
                env.mtx_srt
            ),
            if config.multi_audio {
                "720p+atrack:0,1".to_string()
            } else {
                "720p".to_string()
            },
        ),
        SweepOutputKind::Rtmp1080p => (
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp),
            if config.multi_audio {
                "1080p+atrack:0".to_string()
            } else {
                "1080p".to_string()
            },
        ),
        SweepOutputKind::Srt1080p => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{name}",
                env.mtx_srt
            ),
            if config.multi_audio {
                "1080p+atrack:0,1".to_string()
            } else {
                "1080p".to_string()
            },
        ),
    }
}

fn bitrate_probe_url(env: &BitrateSweepEnv, kind: SweepOutputKind, name: &str) -> String {
    match kind {
        SweepOutputKind::RtmpSource | SweepOutputKind::Rtmp720p | SweepOutputKind::Rtmp1080p => {
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp)
        }
        SweepOutputKind::SrtSource | SweepOutputKind::Srt720p | SweepOutputKind::Srt1080p => {
            format!(
                "srt://127.0.0.1:{}?streamid=read:live/{name}&timeout=30000000",
                env.mtx_srt
            )
        }
    }
}

async fn sample_bitrate_window(
    env: &BitrateSweepEnv,
    stack: &mut ResourceSweepStack,
    config: SweepConfig,
    bitrate: &BitrateSpec,
    pipeline_id: &str,
) -> Result<Vec<BitrateSweepSample>, String> {
    let mut samples = Vec::new();
    let mut prev_ticks = read_proc_stat_ticks(stack.restream_pid)?;
    let mut prev_ffmpeg_ticks: HashMap<u32, u64> = HashMap::new();
    let mut prev_instant = Instant::now();
    let mut elapsed_secs = 0u64;
    let deadline = Instant::now() + Duration::from_secs(env.stabilize_secs);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(env.sample_interval_secs)).await;
        elapsed_secs += env.sample_interval_secs;
        let ffmpeg = ffmpeg_children_stats(stack.restream_pid)?;
        let ticks = read_proc_stat_ticks(stack.restream_pid)?;
        let interval_secs = prev_instant.elapsed().as_secs_f64().max(0.001);
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 };
        let restream_cpu_pct =
            100.0 * (ticks.saturating_sub(prev_ticks)) as f64 / clk_tck / interval_secs;
        let mut ffmpeg_delta_ticks = 0u64;
        let mut next_ffmpeg_ticks = HashMap::new();
        for pid in &ffmpeg.pids {
            if let Ok(current_ticks) = read_proc_stat_ticks(*pid) {
                let previous_ticks = prev_ffmpeg_ticks.get(pid).copied().unwrap_or(current_ticks);
                ffmpeg_delta_ticks += current_ticks.saturating_sub(previous_ticks);
                next_ffmpeg_ticks.insert(*pid, current_ticks);
            }
        }
        let ffmpeg_cpu_pct = 100.0 * ffmpeg_delta_ticks as f64 / clk_tck / interval_secs;
        let total_cpu_pct = restream_cpu_pct + ffmpeg_cpu_pct;
        prev_ticks = ticks;
        prev_ffmpeg_ticks = next_ffmpeg_ticks;
        prev_instant = Instant::now();

        let telemetry = stack.api.get_json("/api/v1/engine/telemetry").await?;
        let pipeline_telemetry = stack
            .api
            .get_json(&format!("/api/v1/pipelines/{pipeline_id}/telemetry"))
            .await?;
        let accounting = &telemetry["memoryAccounting"];
        let avio = &accounting["avioQueues"];
        let overflow_count = pipeline_telemetry["sourceRing"]["readers"]
            .as_array()
            .map(|readers| {
                readers
                    .iter()
                    .map(|reader| reader["overflowCount"].as_u64().unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);
        let sample = BitrateSweepSample {
            config: config.name.to_string(),
            bitrate_label: bitrate.label.clone(),
            bitrate_mbps: bitrate.mbps,
            elapsed_secs,
            restream_cpu_pct,
            ffmpeg_cpu_pct,
            total_cpu_pct,
            restream_rss_kb: read_proc_status_kb_checked(
                stack.restream_pid,
                "VmRSS",
                &env.restream_log,
            )?,
            ffmpeg_count: ffmpeg.count,
            ffmpeg_rss_kb: ffmpeg.rss_kb,
            total_rss_kb: read_proc_status_kb_checked(
                stack.restream_pid,
                "VmRSS",
                &env.restream_log,
            )? + ffmpeg.rss_kb,
            retained_payload_kb: accounting["retainedPayloadBytes"].as_u64().unwrap_or(0) / 1024,
            source_ring_kb: accounting["sourceRings"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|ring| ring["payloadStats"]["payloadBytes"].as_u64().unwrap_or(0))
                .sum::<u64>()
                / 1024,
            transcoder_ring_kb: accounting["transcoderRings"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|ring| ring["payloadStats"]["payloadBytes"].as_u64().unwrap_or(0))
                .sum::<u64>()
                / 1024,
            tsmux_ring_kb: accounting["tsMuxerRings"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|ring| ring["payloadStats"]["payloadBytes"].as_u64().unwrap_or(0))
                .sum::<u64>()
                / 1024,
            avio_len_kb: avio["totalLenBytes"].as_u64().unwrap_or(0) / 1024,
            avio_hwm_kb: avio["inputQueues"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(avio["egressQueues"].as_array().into_iter().flatten())
                .map(|queue| queue["highWaterBytes"].as_u64().unwrap_or(0))
                .sum::<u64>()
                / 1024,
            overflow_count,
        };
        append_line(
            &env.samples_jsonl,
            &format!(
                "{}\n",
                serde_json::to_string(&bitrate_sweep_sample_json(&sample)).unwrap()
            ),
        )?;
        samples.push(sample);
    }
    Ok(samples)
}

async fn check_bitrate_stream(
    label: &str,
    url: &str,
    expected: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let deadline = Instant::now() + timeout;
    let mut last_observed = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match probe_dims_ramp(url).await {
            Ok(dimensions) if dimensions == expected => return Ok(None),
            Ok(dimensions) if !dimensions.is_empty() => last_observed = Some(dimensions),
            Ok(_) => {}
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let observed = last_observed
        .or(last_error)
        .unwrap_or_else(|| "none".to_string());
    println!("[bitrate-sweep] probe mismatch {label}: expected {expected}, observed {observed}");
    Ok(Some(observed))
}

fn summarize_bitrate_case(
    config: SweepConfig,
    bitrate: &BitrateSpec,
    output_groups: usize,
    restream_rss_base_kb: u64,
    restream_rss_final_kb: u64,
    ffmpeg: FfmpegStats,
    correctness_ok: bool,
    correctness_failures: Vec<String>,
    samples: &[BitrateSweepSample],
) -> Result<BitrateSweepCase, String> {
    if samples.is_empty() {
        return Err("bitrate sweep produced no samples".to_string());
    }
    let retained_min_kb = samples
        .iter()
        .map(|sample| sample.retained_payload_kb)
        .min()
        .unwrap_or(0);
    let retained_max_kb = samples
        .iter()
        .map(|sample| sample.retained_payload_kb)
        .max()
        .unwrap_or(0);
    let retained_final_kb = samples
        .last()
        .map(|sample| sample.retained_payload_kb)
        .unwrap_or(0);
    let elapsed_min = (samples
        .last()
        .map(|sample| sample.elapsed_secs)
        .unwrap_or(0) as f64)
        / 60.0;
    Ok(BitrateSweepCase {
        config: config.name.to_string(),
        ingest_proto: config.ingest_proto.to_string(),
        video_codec: config.video_codec.to_string(),
        multi_audio: config.multi_audio,
        bitrate_label: bitrate.label.clone(),
        bitrate_mbps: bitrate.mbps,
        output_groups,
        outputs_total: output_groups * 4,
        restream_rss_base_kb,
        restream_rss_final_kb,
        restream_rss_delta_kb: restream_rss_final_kb.saturating_sub(restream_rss_base_kb),
        restream_rss_peak_kb: samples
            .iter()
            .map(|sample| sample.restream_rss_kb)
            .max()
            .unwrap_or(0),
        ffmpeg_count_peak: samples
            .iter()
            .map(|sample| sample.ffmpeg_count)
            .max()
            .unwrap_or(ffmpeg.count),
        ffmpeg_rss_peak_kb: samples
            .iter()
            .map(|sample| sample.ffmpeg_rss_kb)
            .max()
            .unwrap_or(ffmpeg.rss_kb),
        total_rss_peak_kb: samples
            .iter()
            .map(|sample| sample.total_rss_kb)
            .max()
            .unwrap_or(restream_rss_final_kb + ffmpeg.rss_kb),
        restream_cpu_avg_pct: round2(
            samples
                .iter()
                .map(|sample| sample.restream_cpu_pct)
                .sum::<f64>()
                / samples.len() as f64,
        ),
        restream_cpu_peak_pct: round2(
            samples
                .iter()
                .map(|sample| sample.restream_cpu_pct)
                .fold(0.0, f64::max),
        ),
        ffmpeg_cpu_avg_pct: round2(
            samples
                .iter()
                .map(|sample| sample.ffmpeg_cpu_pct)
                .sum::<f64>()
                / samples.len() as f64,
        ),
        ffmpeg_cpu_peak_pct: round2(
            samples
                .iter()
                .map(|sample| sample.ffmpeg_cpu_pct)
                .fold(0.0, f64::max),
        ),
        total_cpu_avg_pct: round2(
            samples
                .iter()
                .map(|sample| sample.total_cpu_pct)
                .sum::<f64>()
                / samples.len() as f64,
        ),
        total_cpu_peak_pct: round2(
            samples
                .iter()
                .map(|sample| sample.total_cpu_pct)
                .fold(0.0, f64::max),
        ),
        retained_payload_min_kb: retained_min_kb,
        retained_payload_max_kb: retained_max_kb,
        retained_payload_final_kb: retained_final_kb,
        retained_growth_kb_per_min: if elapsed_min > 0.0 {
            round2((retained_final_kb.saturating_sub(retained_min_kb)) as f64 / elapsed_min)
        } else {
            0.0
        },
        source_ring_peak_kb: samples
            .iter()
            .map(|sample| sample.source_ring_kb)
            .max()
            .unwrap_or(0),
        transcoder_ring_peak_kb: samples
            .iter()
            .map(|sample| sample.transcoder_ring_kb)
            .max()
            .unwrap_or(0),
        tsmux_ring_peak_kb: samples
            .iter()
            .map(|sample| sample.tsmux_ring_kb)
            .max()
            .unwrap_or(0),
        avio_len_peak_kb: samples
            .iter()
            .map(|sample| sample.avio_len_kb)
            .max()
            .unwrap_or(0),
        avio_hwm_peak_kb: samples
            .iter()
            .map(|sample| sample.avio_hwm_kb)
            .max()
            .unwrap_or(0),
        overflow_count_final: samples
            .last()
            .map(|sample| sample.overflow_count)
            .unwrap_or(0),
        correctness_ok,
        correctness_failures,
    })
}

fn bitrate_sweep_sample_json(sample: &BitrateSweepSample) -> Value {
    json!({
        "config": sample.config,
        "bitrateLabel": sample.bitrate_label,
        "bitrateMbps": sample.bitrate_mbps,
        "elapsedSecs": sample.elapsed_secs,
        "restreamCpuPct": sample.restream_cpu_pct,
        "ffmpegCpuPct": sample.ffmpeg_cpu_pct,
        "totalCpuPct": sample.total_cpu_pct,
        "restreamRssKb": sample.restream_rss_kb,
        "ffmpegCount": sample.ffmpeg_count,
        "ffmpegRssKb": sample.ffmpeg_rss_kb,
        "totalRssKb": sample.total_rss_kb,
        "retainedPayloadKb": sample.retained_payload_kb,
        "sourceRingKb": sample.source_ring_kb,
        "transcoderRingKb": sample.transcoder_ring_kb,
        "tsmuxRingKb": sample.tsmux_ring_kb,
        "avioLenKb": sample.avio_len_kb,
        "avioHwmKb": sample.avio_hwm_kb,
        "overflowCount": sample.overflow_count,
    })
}

fn bitrate_sweep_case_json(case: &BitrateSweepCase) -> Value {
    json!({
        "config": case.config,
        "ingestProto": case.ingest_proto,
        "videoCodec": case.video_codec,
        "multiAudio": case.multi_audio,
        "bitrateLabel": case.bitrate_label,
        "bitrateMbps": case.bitrate_mbps,
        "outputGroups": case.output_groups,
        "outputsTotal": case.outputs_total,
        "restreamRssBaseKb": case.restream_rss_base_kb,
        "restreamRssFinalKb": case.restream_rss_final_kb,
        "restreamRssDeltaKb": case.restream_rss_delta_kb,
        "restreamRssPeakKb": case.restream_rss_peak_kb,
        "ffmpegCountPeak": case.ffmpeg_count_peak,
        "ffmpegRssPeakKb": case.ffmpeg_rss_peak_kb,
        "totalRssPeakKb": case.total_rss_peak_kb,
        "restreamCpuAvgPct": case.restream_cpu_avg_pct,
        "restreamCpuPeakPct": case.restream_cpu_peak_pct,
        "ffmpegCpuAvgPct": case.ffmpeg_cpu_avg_pct,
        "ffmpegCpuPeakPct": case.ffmpeg_cpu_peak_pct,
        "totalCpuAvgPct": case.total_cpu_avg_pct,
        "totalCpuPeakPct": case.total_cpu_peak_pct,
        "retainedPayloadMinKb": case.retained_payload_min_kb,
        "retainedPayloadMaxKb": case.retained_payload_max_kb,
        "retainedPayloadFinalKb": case.retained_payload_final_kb,
        "retainedGrowthKbPerMin": case.retained_growth_kb_per_min,
        "sourceRingPeakKb": case.source_ring_peak_kb,
        "transcoderRingPeakKb": case.transcoder_ring_peak_kb,
        "tsmuxRingPeakKb": case.tsmux_ring_peak_kb,
        "avioLenPeakKb": case.avio_len_peak_kb,
        "avioHwmPeakKb": case.avio_hwm_peak_kb,
        "overflowCountFinal": case.overflow_count_final,
        "correctnessOk": case.correctness_ok,
        "correctnessFailures": case.correctness_failures,
    })
}

fn write_bitrate_sweep_csv(path: &Path, rows: &[BitrateSweepCase]) -> Result<(), String> {
    let mut text = String::from(
        "config,ingest_proto,video_codec,multi_audio,bitrate_label,bitrate_mbps,output_groups,outputs_total,restream_rss_base_kb,restream_rss_final_kb,restream_rss_delta_kb,restream_rss_peak_kb,ffmpeg_count_peak,ffmpeg_rss_peak_kb,total_rss_peak_kb,restream_cpu_avg_pct,restream_cpu_peak_pct,ffmpeg_cpu_avg_pct,ffmpeg_cpu_peak_pct,total_cpu_avg_pct,total_cpu_peak_pct,retained_payload_min_kb,retained_payload_max_kb,retained_payload_final_kb,retained_growth_kb_per_min,source_ring_peak_kb,transcoder_ring_peak_kb,tsmux_ring_peak_kb,avio_len_peak_kb,avio_hwm_peak_kb,overflow_count_final,correctness_ok\n",
    );
    for row in rows {
        text.push_str(&format!(
            "{},{},{},{},{},{:.2},{},{},{},{},{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{},{},{},{:.2},{},{},{},{},{},{},{}\n",
            csv_escape(&row.config),
            csv_escape(&row.ingest_proto),
            csv_escape(&row.video_codec),
            row.multi_audio,
            csv_escape(&row.bitrate_label),
            row.bitrate_mbps,
            row.output_groups,
            row.outputs_total,
            row.restream_rss_base_kb,
            row.restream_rss_final_kb,
            row.restream_rss_delta_kb,
            row.restream_rss_peak_kb,
            row.ffmpeg_count_peak,
            row.ffmpeg_rss_peak_kb,
            row.total_rss_peak_kb,
            row.restream_cpu_avg_pct,
            row.restream_cpu_peak_pct,
            row.ffmpeg_cpu_avg_pct,
            row.ffmpeg_cpu_peak_pct,
            row.total_cpu_avg_pct,
            row.total_cpu_peak_pct,
            row.retained_payload_min_kb,
            row.retained_payload_max_kb,
            row.retained_payload_final_kb,
            row.retained_growth_kb_per_min,
            row.source_ring_peak_kb,
            row.transcoder_ring_peak_kb,
            row.tsmux_ring_peak_kb,
            row.avio_len_peak_kb,
            row.avio_hwm_peak_kb,
            row.overflow_count_final,
            row.correctness_ok,
        ));
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

async fn start_resource_sweep_stack(env: &ResourceSweepEnv) -> Result<ResourceSweepStack, String> {
    if !env.restream_bin.exists() {
        return Err(format!(
            "restream binary not found at {}",
            env.restream_bin.display()
        ));
    }
    std::fs::create_dir_all(env.work_dir.join("logs")).map_err(|e| e.to_string())?;
    cleanup_ramp_db(&env.restream_db_path);
    let mediamtx_log = std::fs::File::create(&env.mediamtx_log).map_err(|e| e.to_string())?;
    let mediamtx_err = mediamtx_log.try_clone().map_err(|e| e.to_string())?;
    std::fs::write(
        &env.mediamtx_config,
        format!(
            "logLevel: warn\nrtmp: yes\nrtmpAddress: :{}\nrtmpEncryption: \"no\"\nrtsp: no\nsrt: yes\nsrtAddress: :{}\nhls: no\nwebrtc: no\napi: yes\napiAddress: :{}\nmetrics: no\npaths:\n  all:\n",
            env.mtx_rtmp, env.mtx_srt, env.mtx_api
        ),
    )
    .map_err(|e| e.to_string())?;
    let mut mediamtx = Command::new("mediamtx")
        .arg(&env.mediamtx_config)
        .stdout(Stdio::from(mediamtx_log))
        .stderr(Stdio::from(mediamtx_err))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/v3/paths/list", env.mtx_api),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut mediamtx).await;
        return Err(format!("mediamtx did not become ready: {err}"));
    }

    let restream_log = std::fs::File::create(&env.restream_log).map_err(|e| e.to_string())?;
    let restream_err = restream_log.try_clone().map_err(|e| e.to_string())?;
    let mut restream_cmd = Command::new(&env.restream_bin);
    restream_cmd
        .env("RESTREAM_HTTP_PORT", env.restream_http.to_string())
        .env("RESTREAM_RTMP_PORT", env.restream_rtmp.to_string())
        .env("RESTREAM_SRT_PORT", env.restream_srt.to_string())
        .env("RESTREAM_LOG_DIR", env.work_dir.join("logs"))
        .env(
            "RESTREAM_DB_PATH",
            env.restream_db_path.to_string_lossy().to_string(),
        )
        .stdout(Stdio::from(restream_log))
        .stderr(Stdio::from(restream_err))
        .kill_on_drop(true);
    apply_srt_listener_env(&mut restream_cmd, &env.srt_crypto);
    let mut restream = restream_cmd.spawn().map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/healthz", env.restream_http),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut restream).await;
        stop_child(&mut mediamtx).await;
        return Err(format!("restream did not become ready: {err}"));
    }
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;
    let restream_pid = restream.id().ok_or("restream pid missing")?;
    Ok(ResourceSweepStack {
        mediamtx,
        restream,
        api,
        restream_pid,
    })
}

async fn ensure_resource_stack<'a>(
    env: &ResourceSweepEnv,
    stack: &'a mut Option<ResourceSweepStack>,
) -> Result<&'a mut ResourceSweepStack, String> {
    if stack.is_none() {
        *stack = Some(start_resource_sweep_stack(env).await?);
    }
    stack
        .as_mut()
        .ok_or("resource sweep stack missing".to_string())
}

async fn run_resource_baseline(
    env: &ResourceSweepEnv,
    stack: &mut Option<ResourceSweepStack>,
    retained_publishers: &mut Vec<Child>,
) -> Result<ResourceAggregate, String> {
    let local_only = env.lifecycle == ResourceSweepLifecycle::Isolated;
    let mut local_stack = if local_only {
        Some(start_resource_sweep_stack(env).await?)
    } else {
        None
    };
    let active = if local_only {
        local_stack.as_mut().unwrap()
    } else {
        ensure_resource_stack(env, stack).await?
    };
    let meta = ResourceScenarioMeta {
        scenario: "baseline-empty",
        label: "empty".to_string(),
        pipelines: 0,
        outputs: 0,
        ingest_types: "none".to_string(),
        egress_mix: "none".to_string(),
        transcode: "none",
    };
    let aggregate = sample_resource_window(env, active, meta).await?;
    if local_only {
        stop_child(&mut local_stack.as_mut().unwrap().restream).await;
        stop_child(&mut local_stack.as_mut().unwrap().mediamtx).await;
    }
    let _ = retained_publishers;
    Ok(aggregate)
}

async fn run_resource_ingest_only(
    env: &ResourceSweepEnv,
    stack: &mut Option<ResourceSweepStack>,
    retained_publishers: &mut Vec<Child>,
    config: SweepConfig,
) -> Result<ResourceAggregate, String> {
    let local_only = env.lifecycle == ResourceSweepLifecycle::Isolated;
    let mut local_stack = if local_only {
        Some(start_resource_sweep_stack(env).await?)
    } else {
        None
    };
    let active = if local_only {
        local_stack.as_mut().unwrap()
    } else {
        ensure_resource_stack(env, stack).await?
    };
    let stream_key = format!("resource-{}", config.name);
    let pipeline_id = create_resource_pipeline(&active.api, config.name, &stream_key).await?;
    let mut publisher = spawn_resource_publisher(env, config, &stream_key)?;
    wait_for_api_input_live(&active.api, &pipeline_id, Duration::from_secs(45)).await?;
    let meta = ResourceScenarioMeta {
        scenario: "ingest-only",
        label: config.name.to_string(),
        pipelines: 1,
        outputs: 0,
        ingest_types: config.name.to_string(),
        egress_mix: "none".to_string(),
        transcode: "none",
    };
    let aggregate = sample_resource_window(env, active, meta).await?;
    if env.lifecycle == ResourceSweepLifecycle::Cumulative {
        retained_publishers.push(publisher);
    } else {
        stop_child(&mut publisher).await;
        delete_resource_pipeline(&active.api, &pipeline_id).await;
    }
    if local_only {
        stop_child(&mut local_stack.as_mut().unwrap().restream).await;
        stop_child(&mut local_stack.as_mut().unwrap().mediamtx).await;
    }
    Ok(aggregate)
}

async fn run_resource_ingest_growth(
    env: &ResourceSweepEnv,
    stack: &mut Option<ResourceSweepStack>,
    retained_publishers: &mut Vec<Child>,
    mixed: bool,
) -> Result<Vec<ResourceAggregate>, String> {
    let local_only = env.lifecycle == ResourceSweepLifecycle::Isolated;
    let mut local_stack = if local_only {
        Some(start_resource_sweep_stack(env).await?)
    } else {
        None
    };
    let active = if local_only {
        local_stack.as_mut().unwrap()
    } else {
        ensure_resource_stack(env, stack).await?
    };

    let mut publishers = Vec::new();
    let mut pipeline_ids = Vec::new();
    let max_ingests = *env.ingest_counts.iter().max().unwrap_or(&1);
    let mut out = Vec::new();
    for index in 1..=max_ingests {
        let config = if mixed {
            SWEEP_CONFIGS[index - 1]
        } else {
            SWEEP_CONFIGS[1]
        };
        let stream_key = format!("resource-growth-{index}-{}", config.name);
        let pipeline_id = create_resource_pipeline(
            &active.api,
            &format!("{}-{index}", config.name),
            &stream_key,
        )
        .await?;
        let publisher = spawn_resource_publisher(env, config, &stream_key)?;
        wait_for_api_input_live(&active.api, &pipeline_id, Duration::from_secs(45)).await?;
        publishers.push(publisher);
        pipeline_ids.push(pipeline_id);
        if env.ingest_counts.contains(&index) {
            let ingest_types = if mixed {
                SWEEP_CONFIGS
                    .iter()
                    .take(index)
                    .map(|cfg| cfg.name)
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                "h264-srt".to_string()
            };
            out.push(
                sample_resource_window(
                    env,
                    active,
                    ResourceScenarioMeta {
                        scenario: if mixed {
                            "ingest-growth-mixed"
                        } else {
                            "ingest-growth-same"
                        },
                        label: format!("{index}-pipelines"),
                        pipelines: index,
                        outputs: 0,
                        ingest_types,
                        egress_mix: "none".to_string(),
                        transcode: "none",
                    },
                )
                .await?,
            );
        }
    }
    if env.lifecycle == ResourceSweepLifecycle::Cumulative {
        retained_publishers.extend(publishers);
    } else {
        for child in &mut publishers {
            stop_child(child).await;
        }
        for pipeline_id in pipeline_ids {
            delete_resource_pipeline(&active.api, &pipeline_id).await;
        }
    }
    if local_only {
        stop_child(&mut local_stack.as_mut().unwrap().restream).await;
        stop_child(&mut local_stack.as_mut().unwrap().mediamtx).await;
    }
    Ok(out)
}

async fn run_resource_egress_growth(
    env: &ResourceSweepEnv,
    stack: &mut Option<ResourceSweepStack>,
    retained_publishers: &mut Vec<Child>,
    scenario_name: &'static str,
    config: SweepConfig,
    output_kinds: &[SweepOutputKind],
) -> Result<Vec<ResourceAggregate>, String> {
    let local_only = env.lifecycle == ResourceSweepLifecycle::Isolated;
    let mut local_stack = if local_only {
        Some(start_resource_sweep_stack(env).await?)
    } else {
        None
    };
    let active = if local_only {
        local_stack.as_mut().unwrap()
    } else {
        ensure_resource_stack(env, stack).await?
    };
    let stream_key = format!("resource-{scenario_name}");
    let pipeline_id = create_resource_pipeline(&active.api, scenario_name, &stream_key).await?;
    let mut publisher = spawn_resource_publisher(env, config, &stream_key)?;
    wait_for_api_input_live(&active.api, &pipeline_id, Duration::from_secs(45)).await?;
    let mut output_ids = Vec::new();
    let max_outputs = *env.egress_counts.iter().max().unwrap_or(&1);
    let mut out = Vec::new();
    for index in 1..=max_outputs {
        for kind in output_kinds {
            let name = format!("{scenario_name}-{}-{index}", kind.label());
            let (url, encoding) = resource_output_url(env, config, *kind, &name);
            let output_id =
                create_mixed_output(&active.api, &pipeline_id, &name, &url, &encoding).await?;
            start_mixed_output(&active.api, &pipeline_id, &output_id).await?;
            output_ids.push(output_id);
        }
        if env.egress_counts.contains(&index) {
            wait_for_outputs_progress(
                &active.api,
                &pipeline_id,
                &output_ids,
                Duration::from_secs(30),
            )
            .await?;
            out.push(
                sample_resource_window(
                    env,
                    active,
                    ResourceScenarioMeta {
                        scenario: scenario_name,
                        label: format!("{index}-per-group"),
                        pipelines: 1,
                        outputs: output_ids.len(),
                        ingest_types: config.name.to_string(),
                        egress_mix: output_kinds
                            .iter()
                            .map(|kind| kind.label())
                            .collect::<Vec<_>>()
                            .join(","),
                        transcode: if output_kinds.iter().any(|kind| {
                            matches!(
                                kind,
                                SweepOutputKind::Rtmp720p
                                    | SweepOutputKind::Srt720p
                                    | SweepOutputKind::Rtmp1080p
                                    | SweepOutputKind::Srt1080p
                            )
                        }) {
                            "yes"
                        } else {
                            "no"
                        },
                    },
                )
                .await?,
            );
        }
    }
    if env.lifecycle == ResourceSweepLifecycle::Cumulative && env.no_cleanup {
        retained_publishers.push(publisher);
    } else if env.lifecycle == ResourceSweepLifecycle::Cumulative {
        retained_publishers.push(publisher);
    } else {
        stop_child(&mut publisher).await;
        delete_resource_pipeline(&active.api, &pipeline_id).await;
    }
    if local_only {
        stop_child(&mut local_stack.as_mut().unwrap().restream).await;
        stop_child(&mut local_stack.as_mut().unwrap().mediamtx).await;
    }
    Ok(out)
}

async fn create_resource_pipeline(
    api: &RampApi,
    name: &str,
    stream_key: &str,
) -> Result<String, String> {
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": name, "streamKey": stream_key}),
        )
        .await?;
    pipeline["pipeline"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or("pipeline create response missing pipeline.id".to_string())
}

async fn delete_resource_pipeline(api: &RampApi, pipeline_id: &str) {
    let _ = api
        .delete_json(&format!("/api/v1/pipelines/{pipeline_id}"))
        .await;
}

fn spawn_resource_publisher(
    env: &ResourceSweepEnv,
    config: SweepConfig,
    stream_key: &str,
) -> Result<Child, String> {
    spawn_resource_publisher_with_bitrate(
        env.restream_rtmp,
        env.restream_srt,
        &env.work_dir,
        &env.srt_crypto,
        config,
        stream_key,
        "1.5M",
    )
}

fn spawn_resource_publisher_with_bitrate(
    restream_rtmp: u16,
    restream_srt: u16,
    work_dir: &Path,
    srt_crypto: &HarnessSrtCrypto,
    config: SweepConfig,
    stream_key: &str,
    bitrate: &str,
) -> Result<Child, String> {
    let log_path = work_dir.join(format!("publisher-{stream_key}.log"));
    let fixture = sweep_fixture(config, bitrate)?;
    let (url, format, selection) = if config.ingest_proto == "rtmp" {
        (
            format!("rtmp://127.0.0.1:{restream_rtmp}/live/{stream_key}"),
            "flv",
            PublishTrackSelection::PrimaryAv,
        )
    } else {
        (
            append_srt_crypto(
                format!(
                    "srt://127.0.0.1:{restream_srt}?streamid=publish:live/{stream_key}&latency=200000"
                ),
                srt_crypto,
            ),
            "mpegts",
            if config.multi_audio {
                PublishTrackSelection::AllStreams
            } else {
                PublishTrackSelection::PrimaryAv
            },
        )
    };
    spawn_publisher_with_selection(&fixture, &url, format, selection, Some(&log_path))
}

fn resource_output_url(
    env: &ResourceSweepEnv,
    config: SweepConfig,
    kind: SweepOutputKind,
    name: &str,
) -> (String, String) {
    match kind {
        SweepOutputKind::RtmpSource => (
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp),
            "source".to_string(),
        ),
        SweepOutputKind::SrtSource => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{name}",
                env.mtx_srt
            ),
            "source".to_string(),
        ),
        SweepOutputKind::Rtmp720p => (
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp),
            if config.multi_audio {
                "720p+atrack:0".to_string()
            } else {
                "720p".to_string()
            },
        ),
        SweepOutputKind::Srt720p => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{name}",
                env.mtx_srt
            ),
            if config.multi_audio {
                "720p+atrack:0,1".to_string()
            } else {
                "720p".to_string()
            },
        ),
        SweepOutputKind::Rtmp1080p => (
            format!("rtmp://127.0.0.1:{}/live/{name}", env.mtx_rtmp),
            if config.multi_audio {
                "1080p+atrack:0".to_string()
            } else {
                "1080p".to_string()
            },
        ),
        SweepOutputKind::Srt1080p => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{name}",
                env.mtx_srt
            ),
            if config.multi_audio {
                "1080p+atrack:0,1".to_string()
            } else {
                "1080p".to_string()
            },
        ),
    }
}

async fn wait_for_outputs_progress(
    api: &RampApi,
    pipeline_id: &str,
    output_ids: &[String],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let health = api.get_json("/api/v1/engine/health").await?;
        let mut progressed = 0usize;
        let mut stalled = Vec::new();
        for output_id in output_ids {
            let entry = &health["pipelines"][pipeline_id]["outputs"][output_id];
            let bytes_out = entry["bytesOut"].as_u64().unwrap_or(0);
            let metrics_bytes = entry["metrics"]["bytesOut"].as_u64().unwrap_or(0);
            let packets_out = entry["metrics"]["packetsOut"].as_u64().unwrap_or(0);
            if bytes_out > 0 || metrics_bytes > 0 || packets_out > 0 {
                progressed += 1;
            } else {
                let phase = entry["phase"].as_str().unwrap_or("unknown");
                let state = entry["state"].as_str().unwrap_or("unknown");
                let last_error = entry["lastError"].as_str().unwrap_or("");
                stalled.push(format!(
                    "{output_id}[phase={phase},state={state},bytesOut={bytes_out},metricsBytesOut={metrics_bytes},packetsOut={packets_out},lastError={last_error}]"
                ));
            }
        }
        if progressed == output_ids.len() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "outputs did not make progress for pipeline {pipeline_id}: {progressed}/{}; stalled={}",
                output_ids.len(),
                stalled.join(", ")
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn sample_resource_window(
    env: &ResourceSweepEnv,
    stack: &mut ResourceSweepStack,
    meta: ResourceScenarioMeta<'_>,
) -> Result<ResourceAggregate, String> {
    tokio::time::sleep(Duration::from_secs(env.settle_secs)).await;
    let mut samples = Vec::new();
    let mut prev_ticks = read_proc_stat_ticks(stack.restream_pid)?;
    let mut prev_ffmpeg_ticks: HashMap<u32, u64> = HashMap::new();
    let mut prev_instant = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(env.sample_secs);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(env.sample_interval_ms)).await;
        let now = Instant::now();
        let ticks = read_proc_stat_ticks(stack.restream_pid)?;
        let ffmpeg = ffmpeg_children_stats(stack.restream_pid)?;
        let interval_secs = prev_instant.elapsed().as_secs_f64().max(0.001);
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 };
        let restream_cpu_pct =
            100.0 * (ticks.saturating_sub(prev_ticks)) as f64 / clk_tck / interval_secs;
        let mut ffmpeg_delta_ticks = 0u64;
        let mut next_ffmpeg_ticks = HashMap::new();
        for pid in &ffmpeg.pids {
            if let Ok(current_ticks) = read_proc_stat_ticks(*pid) {
                let previous_ticks = prev_ffmpeg_ticks.get(pid).copied().unwrap_or(current_ticks);
                ffmpeg_delta_ticks += current_ticks.saturating_sub(previous_ticks);
                next_ffmpeg_ticks.insert(*pid, current_ticks);
            }
        }
        let ffmpeg_cpu_pct = 100.0 * ffmpeg_delta_ticks as f64 / clk_tck / interval_secs;
        let total_cpu_pct = restream_cpu_pct + ffmpeg_cpu_pct;
        prev_ticks = ticks;
        prev_ffmpeg_ticks = next_ffmpeg_ticks;
        prev_instant = now;
        let rss_kb = read_proc_status_kb_checked(stack.restream_pid, "VmRSS", &env.restream_log)?;
        let rollup = read_smaps_rollup(stack.restream_pid)?;
        let telemetry = stack.api.get_json("/api/v1/engine/telemetry").await?;
        let health = stack.api.get_json("/api/v1/engine/health").await?;
        let accounting = &telemetry["memoryAccounting"];
        let retained_kb = accounting["retainedPayloadBytes"].as_u64().unwrap_or(0) / 1024;
        let source_ring_kb = accounting["sourceRings"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|ring| ring["payloadStats"]["payloadBytes"].as_u64().unwrap_or(0))
            .sum::<u64>()
            / 1024;
        let transcoder_ring_kb = accounting["transcoderRings"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|ring| ring["payloadStats"]["payloadBytes"].as_u64().unwrap_or(0))
            .sum::<u64>()
            / 1024;
        let tsmux_ring_kb = accounting["tsMuxerRings"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|ring| ring["payloadStats"]["payloadBytes"].as_u64().unwrap_or(0))
            .sum::<u64>()
            / 1024;
        let avio_queues = &accounting["avioQueues"];
        let avio_len_kb = avio_queues["totalLenBytes"].as_u64().unwrap_or(0) / 1024;
        let avio_hwm_kb = avio_queues["inputQueues"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(avio_queues["egressQueues"].as_array().into_iter().flatten())
            .map(|queue| queue["highWaterBytes"].as_u64().unwrap_or(0))
            .sum::<u64>()
            / 1024;
        let sample = ResourceSample {
            scenario: meta.scenario.to_string(),
            label: meta.label.clone(),
            lifecycle: env.lifecycle.as_str().to_string(),
            pipelines: meta.pipelines,
            outputs: meta.outputs,
            ingest_types: meta.ingest_types.clone(),
            egress_mix: meta.egress_mix.clone(),
            transcode: meta.transcode.to_string(),
            restream_cpu_pct,
            ffmpeg_cpu_pct,
            total_cpu_pct,
            rss_kb,
            ffmpeg_count: ffmpeg.count,
            ffmpeg_rss_kb: ffmpeg.rss_kb,
            anonymous_kb: rollup.anonymous_kb,
            private_dirty_kb: rollup.private_dirty_kb,
            private_clean_kb: rollup.private_clean_kb,
            shared_clean_kb: rollup.shared_clean_kb,
            shared_dirty_kb: rollup.shared_dirty_kb,
            pss_kb: rollup.pss_kb,
            swap_kb: rollup.swap_kb,
            retained_kb,
            source_ring_kb,
            transcoder_ring_kb,
            tsmux_ring_kb,
            avio_len_kb,
            avio_hwm_kb,
            active_transcoder_buffers: telemetry["activeTranscoderBuffers"].as_u64().unwrap_or(0),
            ingests: telemetry["ingests"]
                .as_array()
                .map(|v| v.len())
                .unwrap_or(0),
            egresses: telemetry["egresses"]
                .as_array()
                .map(|v| v.len())
                .unwrap_or(0),
            stages: telemetry["stages"].as_array().map(|v| v.len()).unwrap_or(0),
            pipeline_count: health["pipelines"]
                .as_object()
                .map(|v| v.len())
                .unwrap_or(0),
            unattributed_kb: rss_kb.saturating_sub(retained_kb + avio_len_kb),
        };
        append_line(
            &env.samples_jsonl,
            &format!(
                "{}\n",
                serde_json::to_string(&resource_sample_json(&sample)).unwrap()
            ),
        )?;
        samples.push(sample);
    }
    Ok(summarize_resource_samples(meta, env.lifecycle, &samples))
}

fn summarize_resource_samples(
    meta: ResourceScenarioMeta<'_>,
    lifecycle: ResourceSweepLifecycle,
    samples: &[ResourceSample],
) -> ResourceAggregate {
    let restream_cpu_sum: f64 = samples.iter().map(|s| s.restream_cpu_pct).sum();
    let ffmpeg_cpu_sum: f64 = samples.iter().map(|s| s.ffmpeg_cpu_pct).sum();
    let total_cpu_sum: f64 = samples.iter().map(|s| s.total_cpu_pct).sum();
    let rss_sum: u64 = samples.iter().map(|s| s.rss_kb).sum();
    ResourceAggregate {
        scenario: meta.scenario.to_string(),
        label: meta.label,
        lifecycle: lifecycle.as_str().to_string(),
        pipelines: meta.pipelines,
        outputs: meta.outputs,
        ingest_types: meta.ingest_types,
        egress_mix: meta.egress_mix,
        transcode: meta.transcode.to_string(),
        sample_count: samples.len(),
        restream_cpu_avg_pct: round2(restream_cpu_sum / samples.len().max(1) as f64),
        restream_cpu_peak_pct: round2(
            samples
                .iter()
                .map(|s| s.restream_cpu_pct)
                .fold(0.0, f64::max),
        ),
        ffmpeg_cpu_avg_pct: round2(ffmpeg_cpu_sum / samples.len().max(1) as f64),
        ffmpeg_cpu_peak_pct: round2(samples.iter().map(|s| s.ffmpeg_cpu_pct).fold(0.0, f64::max)),
        total_cpu_avg_pct: round2(total_cpu_sum / samples.len().max(1) as f64),
        total_cpu_peak_pct: round2(samples.iter().map(|s| s.total_cpu_pct).fold(0.0, f64::max)),
        rss_avg_kb: round2(rss_sum as f64 / samples.len().max(1) as f64),
        rss_peak_kb: samples.iter().map(|s| s.rss_kb).max().unwrap_or(0),
        ffmpeg_rss_peak_kb: samples.iter().map(|s| s.ffmpeg_rss_kb).max().unwrap_or(0),
        retained_peak_kb: samples.iter().map(|s| s.retained_kb).max().unwrap_or(0),
        source_ring_peak_kb: samples.iter().map(|s| s.source_ring_kb).max().unwrap_or(0),
        transcoder_ring_peak_kb: samples
            .iter()
            .map(|s| s.transcoder_ring_kb)
            .max()
            .unwrap_or(0),
        tsmux_ring_peak_kb: samples.iter().map(|s| s.tsmux_ring_kb).max().unwrap_or(0),
        avio_len_peak_kb: samples.iter().map(|s| s.avio_len_kb).max().unwrap_or(0),
        avio_hwm_peak_kb: samples.iter().map(|s| s.avio_hwm_kb).max().unwrap_or(0),
        anonymous_peak_kb: samples.iter().map(|s| s.anonymous_kb).max().unwrap_or(0),
        private_dirty_peak_kb: samples
            .iter()
            .map(|s| s.private_dirty_kb)
            .max()
            .unwrap_or(0),
        shared_clean_peak_kb: samples.iter().map(|s| s.shared_clean_kb).max().unwrap_or(0),
        pss_peak_kb: samples.iter().map(|s| s.pss_kb).max().unwrap_or(0),
        unattributed_peak_kb: samples.iter().map(|s| s.unattributed_kb).max().unwrap_or(0),
        active_transcoder_buffers_peak: samples
            .iter()
            .map(|s| s.active_transcoder_buffers)
            .max()
            .unwrap_or(0),
        ingests_peak: samples.iter().map(|s| s.ingests).max().unwrap_or(0),
        egresses_peak: samples.iter().map(|s| s.egresses).max().unwrap_or(0),
        stages_peak: samples.iter().map(|s| s.stages).max().unwrap_or(0),
        pipeline_count_peak: samples.iter().map(|s| s.pipeline_count).max().unwrap_or(0),
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn read_proc_stat_ticks(pid: u32) -> Result<u64, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let utime = fields
        .get(13)
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("proc stat missing utime")?;
    let stime = fields
        .get(14)
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("proc stat missing stime")?;
    Ok(utime + stime)
}

fn read_proc_status_kb(pid: u32, key: &str) -> Result<u64, String> {
    let status =
        std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| e.to_string())?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            return value
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| format!("failed to parse {key}"));
        }
    }
    Err(format!("{key} missing in /proc/{pid}/status"))
}

fn read_proc_status_kb_checked(pid: u32, key: &str, log_path: &Path) -> Result<u64, String> {
    read_proc_status_kb(pid, key).map_err(|error| {
        let tail = file_tail_lines(log_path, 20);
        if tail.is_empty() {
            format!("restream pid {pid} unavailable while reading {key}: {error}")
        } else {
            format!(
                "restream pid {pid} unavailable while reading {key}: {error}\nrestream log tail:\n{}",
                tail.join("\n")
            )
        }
    })
}

fn read_smaps_rollup(pid: u32) -> Result<ProcMemRollup, String> {
    let text =
        std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).map_err(|e| e.to_string())?;
    let value_for = |name: &str| -> u64 {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Ok(ProcMemRollup {
        anonymous_kb: value_for("Anonymous"),
        private_dirty_kb: value_for("Private_Dirty"),
        private_clean_kb: value_for("Private_Clean"),
        shared_clean_kb: value_for("Shared_Clean"),
        shared_dirty_kb: value_for("Shared_Dirty"),
        pss_kb: value_for("Pss"),
        swap_kb: value_for("Swap"),
    })
}

fn ffmpeg_children_stats(parent_pid: u32) -> Result<FfmpegStats, String> {
    let mut count = 0u64;
    let mut rss_kb = 0u64;
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        let status_path = format!("/proc/{pid}/status");
        let Ok(status) = std::fs::read_to_string(&status_path) else {
            continue;
        };
        let ppid = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if ppid != parent_pid {
            continue;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let text = String::from_utf8_lossy(&cmdline);
        if text.contains("ffmpeg") {
            count += 1;
            rss_kb += read_proc_status_kb(pid, "VmRSS").unwrap_or(0);
            pids.push(pid);
        }
    }
    Ok(FfmpegStats {
        count,
        rss_kb,
        pids,
    })
}

fn resource_sample_json(sample: &ResourceSample) -> Value {
    json!({
        "scenario": sample.scenario,
        "label": sample.label,
        "lifecycle": sample.lifecycle,
        "pipelines": sample.pipelines,
        "outputs": sample.outputs,
        "ingestTypes": sample.ingest_types,
        "egressMix": sample.egress_mix,
        "transcode": sample.transcode,
        "restreamCpuPct": sample.restream_cpu_pct,
        "ffmpegCpuPct": sample.ffmpeg_cpu_pct,
        "totalCpuPct": sample.total_cpu_pct,
        "rssKb": sample.rss_kb,
        "ffmpegCount": sample.ffmpeg_count,
        "ffmpegRssKb": sample.ffmpeg_rss_kb,
        "anonymousKb": sample.anonymous_kb,
        "privateDirtyKb": sample.private_dirty_kb,
        "privateCleanKb": sample.private_clean_kb,
        "sharedCleanKb": sample.shared_clean_kb,
        "sharedDirtyKb": sample.shared_dirty_kb,
        "pssKb": sample.pss_kb,
        "swapKb": sample.swap_kb,
        "retainedKb": sample.retained_kb,
        "sourceRingKb": sample.source_ring_kb,
        "transcoderRingKb": sample.transcoder_ring_kb,
        "tsmuxRingKb": sample.tsmux_ring_kb,
        "avioLenKb": sample.avio_len_kb,
        "avioHwmKb": sample.avio_hwm_kb,
        "activeTranscoderBuffers": sample.active_transcoder_buffers,
        "ingests": sample.ingests,
        "egresses": sample.egresses,
        "stages": sample.stages,
        "pipelineCount": sample.pipeline_count,
        "unattributedKb": sample.unattributed_kb,
    })
}

fn resource_aggregate_json(aggregate: &ResourceAggregate) -> Value {
    json!({
        "scenario": aggregate.scenario,
        "label": aggregate.label,
        "lifecycle": aggregate.lifecycle,
        "pipelines": aggregate.pipelines,
        "outputs": aggregate.outputs,
        "ingestTypes": aggregate.ingest_types,
        "egressMix": aggregate.egress_mix,
        "transcode": aggregate.transcode,
        "sampleCount": aggregate.sample_count,
        "restreamCpuAvgPct": aggregate.restream_cpu_avg_pct,
        "restreamCpuPeakPct": aggregate.restream_cpu_peak_pct,
        "ffmpegCpuAvgPct": aggregate.ffmpeg_cpu_avg_pct,
        "ffmpegCpuPeakPct": aggregate.ffmpeg_cpu_peak_pct,
        "totalCpuAvgPct": aggregate.total_cpu_avg_pct,
        "totalCpuPeakPct": aggregate.total_cpu_peak_pct,
        "rssAvgKb": aggregate.rss_avg_kb,
        "rssPeakKb": aggregate.rss_peak_kb,
        "ffmpegRssPeakKb": aggregate.ffmpeg_rss_peak_kb,
        "retainedPeakKb": aggregate.retained_peak_kb,
        "sourceRingPeakKb": aggregate.source_ring_peak_kb,
        "transcoderRingPeakKb": aggregate.transcoder_ring_peak_kb,
        "tsmuxRingPeakKb": aggregate.tsmux_ring_peak_kb,
        "avioLenPeakKb": aggregate.avio_len_peak_kb,
        "avioHwmPeakKb": aggregate.avio_hwm_peak_kb,
        "anonymousPeakKb": aggregate.anonymous_peak_kb,
        "privateDirtyPeakKb": aggregate.private_dirty_peak_kb,
        "sharedCleanPeakKb": aggregate.shared_clean_peak_kb,
        "pssPeakKb": aggregate.pss_peak_kb,
        "unattributedPeakKb": aggregate.unattributed_peak_kb,
        "activeTranscoderBuffersPeak": aggregate.active_transcoder_buffers_peak,
        "ingestsPeak": aggregate.ingests_peak,
        "egressesPeak": aggregate.egresses_peak,
        "stagesPeak": aggregate.stages_peak,
        "pipelineCountPeak": aggregate.pipeline_count_peak,
    })
}

fn write_resource_sweep_csv(path: &Path, rows: &[ResourceAggregate]) -> Result<(), String> {
    let mut text = String::from(
        "scenario,label,lifecycle,pipelines,outputs,ingest_types,egress_mix,transcode,sample_count,restream_cpu_avg_pct,restream_cpu_peak_pct,ffmpeg_cpu_avg_pct,ffmpeg_cpu_peak_pct,total_cpu_avg_pct,total_cpu_peak_pct,rss_avg_kb,rss_peak_kb,ffmpeg_rss_peak_kb,retained_peak_kb,source_ring_peak_kb,transcoder_ring_peak_kb,tsmux_ring_peak_kb,avio_len_peak_kb,avio_hwm_peak_kb,anonymous_peak_kb,private_dirty_peak_kb,shared_clean_peak_kb,pss_peak_kb,unattributed_peak_kb,active_transcoder_buffers_peak,ingests_peak,egresses_peak,stages_peak,pipeline_count_peak\n",
    );
    for row in rows {
        text.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            csv_escape(&row.scenario),
            csv_escape(&row.label),
            csv_escape(&row.lifecycle),
            row.pipelines,
            row.outputs,
            csv_escape(&row.ingest_types),
            csv_escape(&row.egress_mix),
            csv_escape(&row.transcode),
            row.sample_count,
            row.restream_cpu_avg_pct,
            row.restream_cpu_peak_pct,
            row.ffmpeg_cpu_avg_pct,
            row.ffmpeg_cpu_peak_pct,
            row.total_cpu_avg_pct,
            row.total_cpu_peak_pct,
            row.rss_avg_kb,
            row.rss_peak_kb,
            row.ffmpeg_rss_peak_kb,
            row.retained_peak_kb,
            row.source_ring_peak_kb,
            row.transcoder_ring_peak_kb,
            row.tsmux_ring_peak_kb,
            row.avio_len_peak_kb,
            row.avio_hwm_peak_kb,
            row.anonymous_peak_kb,
            row.private_dirty_peak_kb,
            row.shared_clean_peak_kb,
            row.pss_peak_kb,
            row.unattributed_peak_kb,
            row.active_transcoder_buffers_peak,
            row.ingests_peak,
            row.egresses_peak,
            row.stages_peak,
            row.pipeline_count_peak,
        ));
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn write_branch_matrix_markdown(
    path: &Path,
    backend: &str,
    srt_ingest_transport: &str,
    rows: &[ResourceAggregate],
) -> Result<(), String> {
    let mut selected: Vec<&ResourceAggregate> = rows.iter().collect();
    selected.sort_by_key(|row| match row.scenario.as_str() {
        "egress-growth-source-mixed" => 0,
        "egress-growth-transcode-mixed" => 1,
        "egress-growth-source-plus-transcode-mixed" => 2,
        "egress-growth-transcode-dual-mixed" => 3,
        "egress-growth-source-plus-transcode-dual-mixed" => 4,
        _ => 99,
    });

    let mut text = String::new();
    text.push_str("# Branch Matrix\n\n");
    text.push_str(&format!("- Backend: `{backend}`\n"));
    text.push_str(&format!(
        "- SRT ingest transport: `{srt_ingest_transport}`\n"
    ));
    if let Some(row) = selected.first() {
        text.push_str(&format!("- Lifecycle: `{}`\n", row.lifecycle));
        text.push_str(&format!("- Fanout per group: `{}`\n", row.label));
    }
    text.push('\n');
    text.push_str("| Shape | Outputs | Restream MB | Child FFmpeg MB | Combined MB | Total CPU % | Stages |\n");
    text.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for row in &selected {
        let combined_mb = (row.rss_peak_kb + row.ffmpeg_rss_peak_kb) as f64 / 1024.0;
        text.push_str(&format!(
            "| {} | {} | {:.1} | {:.1} | {:.1} | {:.2} | {} |\n",
            branch_shape_label(&row.scenario),
            row.outputs,
            row.rss_peak_kb as f64 / 1024.0,
            row.ffmpeg_rss_peak_kb as f64 / 1024.0,
            combined_mb,
            row.total_cpu_avg_pct,
            row.stages_peak,
        ));
    }

    if let (Some(single), Some(single_plus_source), Some(dual), Some(dual_plus_source)) = (
        selected
            .iter()
            .find(|row| row.scenario == "egress-growth-transcode-mixed"),
        selected
            .iter()
            .find(|row| row.scenario == "egress-growth-source-plus-transcode-mixed"),
        selected
            .iter()
            .find(|row| row.scenario == "egress-growth-transcode-dual-mixed"),
        selected
            .iter()
            .find(|row| row.scenario == "egress-growth-source-plus-transcode-dual-mixed"),
    ) {
        text.push_str("\n## Deltas\n\n");
        text.push_str("| Comparison | Output Delta | Combined MB Delta | Total CPU Delta |\n");
        text.push_str("|---|---:|---:|---:|\n");
        text.push_str(&format!(
            "| Add passthrough on top of one transcode family | {} | {:.1} | {:.2} |\n",
            single_plus_source.outputs.saturating_sub(single.outputs),
            ((single_plus_source.rss_peak_kb + single_plus_source.ffmpeg_rss_peak_kb)
                .saturating_sub(single.rss_peak_kb + single.ffmpeg_rss_peak_kb)) as f64
                / 1024.0,
            single_plus_source.total_cpu_avg_pct - single.total_cpu_avg_pct,
        ));
        text.push_str(&format!(
            "| Add a second transcode family | {} | {:.1} | {:.2} |\n",
            dual.outputs.saturating_sub(single.outputs),
            ((dual.rss_peak_kb + dual.ffmpeg_rss_peak_kb)
                .saturating_sub(single.rss_peak_kb + single.ffmpeg_rss_peak_kb)) as f64
                / 1024.0,
            dual.total_cpu_avg_pct - single.total_cpu_avg_pct,
        ));
        text.push_str(&format!(
            "| Add passthrough on top of two transcode families | {} | {:.1} | {:.2} |\n",
            dual_plus_source.outputs.saturating_sub(dual.outputs),
            ((dual_plus_source.rss_peak_kb + dual_plus_source.ffmpeg_rss_peak_kb)
                .saturating_sub(dual.rss_peak_kb + dual.ffmpeg_rss_peak_kb)) as f64
                / 1024.0,
            dual_plus_source.total_cpu_avg_pct - dual.total_cpu_avg_pct,
        ));
    }

    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn branch_shape_label(scenario: &str) -> &'static str {
    match scenario {
        "egress-growth-source-mixed" => "source only",
        "egress-growth-transcode-mixed" => "one transcode family (720p)",
        "egress-growth-source-plus-transcode-mixed" => "source + one transcode family",
        "egress-growth-transcode-dual-mixed" => "two transcode families (720p + 1080p)",
        "egress-growth-source-plus-transcode-dual-mixed" => "source + two transcode families",
        _ => "custom",
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

async fn json_response(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn harness_port_defaults() -> HarnessPortDefaults {
    *HARNESS_PORT_DEFAULTS.get_or_init(|| {
        let mut reserved = HashSet::new();
        HarnessPortDefaults {
            restream_http: env_or_allocated_port("RESTREAM_HTTP", 3030, &mut reserved),
            restream_rtmp: env_or_allocated_port("RESTREAM_RTMP", 1935, &mut reserved),
            restream_srt: env_or_allocated_port("RESTREAM_SRT", 10080, &mut reserved),
            mtx_rtmp: env_or_allocated_port("MTX_RTMP", 1936, &mut reserved),
            mtx_srt: env_or_allocated_port("MTX_SRT", 8891, &mut reserved),
            mtx_hls: env_or_allocated_port("MTX_HLS", 8890, &mut reserved),
            mtx_api: env_or_allocated_port("MTX_API", 9997, &mut reserved),
        }
    })
}

fn env_or_allocated_port(name: &str, default: u16, reserved: &mut HashSet<u16>) -> u16 {
    if let Some(port) = std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
    {
        reserved.insert(port);
        return port;
    }

    let port = synthesized_harness_port(name, reserved).unwrap_or(default);
    reserved.insert(port);
    port
}

fn synthesized_harness_port(name: &str, reserved: &HashSet<u16>) -> Option<u16> {
    // Do not probe-bind here: some restricted runners deny ad hoc socket
    // creation before the harness re-execs into its private loopback namespace.
    // A per-process high-port bundle is enough to avoid host collisions by
    // default while still allowing explicit env overrides when needed.
    let pid = std::process::id();
    let name_hash = name.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(byte as u32)
    });
    let base = 20_000u32 + pid.wrapping_mul(97).wrapping_add(name_hash) % 30_000u32;
    for step in 0..1024u32 {
        let candidate = 20_000u32 + (base - 20_000u32 + step * 37) % 30_000u32;
        let candidate = candidate as u16;
        if !reserved.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

// ── api-smoke (Phase 3) ─────────────────────────────────────────────────────
//
// Lightweight live test for the API/DB/lifecycle layer. No media — just spin up
// the binary, walk the API (auth, pipeline/output CRUD, start/stop), restart
// the child, and assert pipelines survived (DB persistence).

async fn api_smoke() -> Result<Value, String> {
    let work_dir = std::env::var_os("WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test/artifacts/api-smoke"));
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("api-smoke.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    // ── First boot: CRUD ────────────────────────────────────────────
    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;
    println!("[api-smoke] authenticated");

    // Health endpoint
    let health = api.get_json("/healthz").await?;
    if health.is_null() {
        return Err("healthz returned null".to_string());
    }
    println!("[api-smoke] healthz ok");

    // Create pipeline
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "smoke-test", "streamKey": "sk-smoke"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();
    println!("[api-smoke] created pipeline {pipeline_id}");

    // Create output
    let output = api
        .post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs"),
            json!({"name": "smoke-out", "url": "rtmp://127.0.0.1:19350/live/nowhere", "encoding": "source"}),
        )
        .await?;
    let output_id = output["output"]["id"]
        .as_str()
        .ok_or("output create missing id")?
        .to_string();
    println!("[api-smoke] created output {output_id}");

    // Read back pipeline list
    let pipelines = api.get_json("/api/v1/pipelines").await?;
    let list = pipelines["pipelines"]
        .as_array()
        .ok_or("pipelines list not an array")?;
    if !list.iter().any(|p| p["id"] == pipeline_id.as_str()) {
        return Err(format!("created pipeline {pipeline_id} not found in list"));
    }
    println!("[api-smoke] pipeline appears in list");

    // Health shows pipeline
    let health = api.get_json("/api/v1/engine/health").await?;
    if health["pipelines"][&pipeline_id].is_null() {
        return Err("pipeline not in health snapshot".to_string());
    }
    println!("[api-smoke] pipeline in health snapshot");

    // ── Restart: DB persistence ─────────────────────────────────────
    stop_child(&mut child).await;
    println!("[api-smoke] stopped first instance");

    let log2_path = work_dir.join("restream-2.log");
    let mut child2 = start_restream_child_opts(&restream_bin, &ports, &db_path, &log2_path, false)
        .await
        .map_err(|e| format!("restart failed: {e}"))?;
    let mut api2 = RampApi::new(ports.http);
    api2.login().await?;
    println!("[api-smoke] restarted and authenticated");

    let pipelines2 = api2.get_json("/api/v1/pipelines").await?;
    let list2 = pipelines2["pipelines"]
        .as_array()
        .ok_or("pipelines list after restart not an array")?;
    let survived = list2.iter().any(|p| p["id"] == pipeline_id.as_str());
    if !survived {
        stop_child(&mut child2).await;
        return Err(format!("pipeline {pipeline_id} did not survive restart"));
    }
    println!("[api-smoke] pipeline survived restart (DB persistence confirmed)");

    let history_contract = verify_api_smoke_history_contract(&api2).await?;
    println!("[api-smoke] history contract verified");

    // Cleanup
    stop_child(&mut child2).await;

    Ok(json!({
        "passed": true,
        "mode": "api-smoke",
        "pipelineId": pipeline_id,
        "outputId": output_id,
        "dbPersistence": survived,
        "historyContract": history_contract,
    }))
}

async fn ramp_family_correctness() -> Result<Value, String> {
    let env = RampEnv::from_env();
    if env.n_outputs == 0 {
        return Err("N_OUTPUTS must be greater than zero".to_string());
    }
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    ensure_ramp_artifacts(&env)?;

    let configs = selected_ramp_configs();
    if configs.is_empty() {
        return Err("RAMP_FAMILY_CONFIGS selected no ramp-family configs".to_string());
    }

    let mut mediamtx = start_ramp_mediamtx(&env).await?;
    let mut restream = start_ramp_restream(&env).await?;
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;

    let mut case_results = Vec::with_capacity(configs.len());
    for config in configs {
        case_results.push(run_ramp_config(config, &env, &api, restream.id().unwrap_or(0)).await?);
    }

    stop_child(&mut restream).await;
    stop_child(&mut mediamtx).await;

    Ok(json!({
        "passed": true,
        "mode": "ramp-family",
        "configs": case_results,
        "artifacts": {
            "scaleCsv": env.scale_log,
            "summary": env.summary_log,
            "restreamLog": env.restream_log,
            "mediamtxLog": env.mediamtx_log,
        }
    }))
}

fn selected_ramp_configs() -> Vec<RampConfig> {
    const DEFAULTS: &[RampConfig] = &[
        RampConfig {
            name: "rtmp-rtmp-src",
            ingest_proto: "rtmp",
            out_proto: "rtmp",
            encoding: "source",
        },
        RampConfig {
            name: "rtmp-rtmp-720p",
            ingest_proto: "rtmp",
            out_proto: "rtmp",
            encoding: "720p",
        },
        RampConfig {
            name: "rtmp-srt-src",
            ingest_proto: "rtmp",
            out_proto: "srt",
            encoding: "source",
        },
        RampConfig {
            name: "rtmp-srt-720p",
            ingest_proto: "rtmp",
            out_proto: "srt",
            encoding: "720p",
        },
        RampConfig {
            name: "srt-rtmp-src",
            ingest_proto: "srt",
            out_proto: "rtmp",
            encoding: "source",
        },
        RampConfig {
            name: "srt-rtmp-720p",
            ingest_proto: "srt",
            out_proto: "rtmp",
            encoding: "720p",
        },
        RampConfig {
            name: "srt-srt-src",
            ingest_proto: "srt",
            out_proto: "srt",
            encoding: "source",
        },
        RampConfig {
            name: "srt-srt-720p",
            ingest_proto: "srt",
            out_proto: "srt",
            encoding: "720p",
        },
    ];
    let allow = std::env::var("RAMP_FAMILY_CONFIGS").ok().map(|value| {
        value
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    DEFAULTS
        .iter()
        .copied()
        .filter(|config| {
            allow
                .as_ref()
                .is_none_or(|items| items.iter().any(|item| item == config.name))
        })
        .collect()
}

fn ensure_ramp_artifacts(env: &RampEnv) -> Result<(), String> {
    if !env.scale_log.exists() {
        std::fs::write(
            &env.scale_log,
            "config,step,label,cpu_pct,rss_kb,ffmpeg_n,ffmpeg_rss_kb,total_rss_kb\n",
        )
        .map_err(|e| e.to_string())?;
    }
    if !env.summary_log.exists() {
        std::fs::write(&env.summary_log, "").map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn start_ramp_restream(env: &RampEnv) -> Result<Child, String> {
    start_restream_child(
        &env.restream_bin,
        &TestPorts {
            http: env.restream_http,
            rtmp: env.restream_rtmp,
            srt: env.restream_srt,
        },
        &env.restream_db_path,
        &env.restream_log,
    )
    .await
}

fn cleanup_ramp_db(path: &Path) {
    let path_string = path.to_string_lossy();
    let db_path = path_string
        .strip_prefix("sqlite:")
        .unwrap_or(path_string.as_ref())
        .split('?')
        .next()
        .unwrap_or("data.db");
    let db_path = PathBuf::from(db_path);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-shm", db_path.display()));
    let _ = std::fs::remove_file(format!("{}-wal", db_path.display()));
}

async fn start_ramp_mediamtx(env: &RampEnv) -> Result<Child, String> {
    std::fs::write(
        &env.mediamtx_config,
        format!(
            "logLevel: warn\nrtmp: yes\nrtmpAddress: :{}\nrtmpEncryption: \"no\"\nrtsp: no\nsrt: yes\nsrtAddress: :{}\nhls: no\nwebrtc: no\napi: yes\napiAddress: :{}\nmetrics: no\npaths:\n  all:\n",
            env.mtx_rtmp, env.mtx_srt, env.mtx_api
        ),
    )
    .map_err(|e| e.to_string())?;
    let log = std::fs::File::create(&env.mediamtx_log).map_err(|e| e.to_string())?;
    let stderr_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new("mediamtx")
        .arg(&env.mediamtx_config)
        .env_remove("MTX_RTMP")
        .env_remove("MTX_SRT")
        .env_remove("MTX_HLS")
        .env_remove("MTX_API")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/v3/paths/list", env.mtx_api),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut child).await;
        return Err(format!("mediamtx did not become ready: {err}"));
    }
    Ok(child)
}

async fn wait_for_http_ok(url: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::Client::new();
    loop {
        if let Ok(response) = client.get(url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {url}"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn run_ramp_config(
    config: RampConfig,
    env: &RampEnv,
    api: &RampApi,
    restream_pid: u32,
) -> Result<Value, String> {
    println!(
        "\n[ramp-family] {} {} ingest -> {} {} x{} outputs",
        config.name, config.ingest_proto, config.out_proto, config.encoding, env.n_outputs
    );
    let stream_key = format!("sk-{}", config.name);
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": config.name, "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    let mut publisher = spawn_ramp_publisher(config, env, &stream_key).await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;
    let baseline_snapshot = snapshot_ramp(env, restream_pid, config.name, 0, "baseline").await?;
    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);

    let mut output_ids = Vec::with_capacity(env.n_outputs);
    for n in 1..=env.n_outputs {
        let url = match config.out_proto {
            "rtmp" => format!("rtmp://127.0.0.1:{}/live/{}-{n}", env.mtx_rtmp, config.name),
            "srt" => format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{}-{n}",
                env.mtx_srt, config.name
            ),
            other => return Err(format!("unsupported ramp output protocol {other}")),
        };
        let output = api
            .post_json(
                &format!("/api/v1/pipelines/{pipeline_id}/outputs"),
                json!({"name": format!("out{n}"), "url": url, "encoding": config.encoding}),
            )
            .await?;
        let output_id = output["output"]["id"]
            .as_str()
            .ok_or("output create response missing output.id")?
            .to_string();
        api.post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/start"),
            Value::Null,
        )
        .await?;
        output_ids.push(output_id);
        if n == 1 || n % env.snap_every == 0 {
            snapshot_ramp(env, restream_pid, config.name, n, &format!("out{n}")).await?;
        }
    }

    let rss_final = process_rss_kb(restream_pid).await.unwrap_or(0);
    let ffmpeg = ffmpeg_pipe1_stats().await;
    let rss_delta = rss_final.saturating_sub(rss_baseline);
    let per_output = rss_delta / env.n_outputs as u64;
    append_line(
        &env.summary_log,
        &format!(
            "{},rss_delta_kb={},per_output_kb={},ffmpeg_n={},ffmpeg_rss_kb={}\n",
            config.name, rss_delta, per_output, ffmpeg.count, ffmpeg.rss_kb
        ),
    )?;

    let expected = if config.encoding == "source" {
        "1920x1080"
    } else {
        "1280x720"
    };
    let first_url = read_url(config, env, 1);
    let last_url = read_url(config, env, env.n_outputs);
    let first_dims = check_ramp_stream("out1", &first_url, expected, 10).await;
    let last_dims =
        check_ramp_stream(&format!("out{}", env.n_outputs), &last_url, expected, 10).await;

    stop_child(&mut publisher).await;
    for output_id in &output_ids {
        let _ = api
            .post_json(
                &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
                Value::Null,
            )
            .await;
    }
    tokio::time::sleep(env.cleanup_sleep).await;

    Ok(json!({
        "config": config.name,
        "pipelineId": pipeline_id,
        "outputs": output_ids.len(),
        "baseline": baseline_snapshot,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "ffmpegCount": ffmpeg.count,
        "ffmpegRssKb": ffmpeg.rss_kb,
        "spotChecks": {
            "first": {"expected": expected, "got": first_dims},
            "last": {"expected": expected, "got": last_dims},
        }
    }))
}

async fn spawn_ramp_publisher(
    config: RampConfig,
    env: &RampEnv,
    stream_key: &str,
) -> Result<Child, String> {
    let fixture = ramp_fixture()?;
    let (url, format) = match config.ingest_proto {
        "rtmp" => (
            format!("rtmp://127.0.0.1:{}/live/{stream_key}", env.restream_rtmp),
            "flv",
        ),
        "srt" => (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
                env.restream_srt
            ),
            "mpegts",
        ),
        other => return Err(format!("unsupported ramp ingest protocol {other}")),
    };
    spawn_publisher_with_selection(
        &fixture,
        &url,
        format,
        PublishTrackSelection::PrimaryAv,
        None,
    )
}

async fn wait_for_api_input_live(
    api: &RampApi,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(health) = api.get_json("/api/v1/engine/health").await
            && health["pipelines"][pipeline_id]["input"]["status"] == "on"
            && health["pipelines"][pipeline_id]["input"]["bytesReceived"]
                .as_u64()
                .unwrap_or(0)
                > 0
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{pipeline_id}: ingest did not go live within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_api_input_media_ready(
    api: &RampApi,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let mut last_snapshot = Value::Null;

    loop {
        if let Ok(health) = api.get_json("/api/v1/engine/health").await {
            let snapshot = health["pipelines"][pipeline_id].clone();
            if !snapshot.is_null() {
                last_snapshot = snapshot.clone();
                let input = &snapshot["input"];
                let input_live =
                    input["status"] == "on" && input["bytesReceived"].as_u64().unwrap_or(0) > 0;
                let has_video = !input["video"].is_null();
                let has_audio = input["audioTracks"]
                    .as_array()
                    .map(|tracks| !tracks.is_empty())
                    .unwrap_or(false);
                if input_live && has_video && has_audio {
                    return Ok(snapshot);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{pipeline_id}: ingest went live but media probe was incomplete within {}s; last snapshot={}",
                timeout.as_secs(),
                last_snapshot
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_api_input_off(
    api: &RampApi,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(health) = api.get_json("/api/v1/engine/health").await {
            let status = health["pipelines"][pipeline_id]["input"]["status"]
                .as_str()
                .unwrap_or("unknown");
            if status == "off" {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{pipeline_id}: ingest did not go off within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_api_recording_state(
    api: &RampApi,
    pipeline_id: &str,
    expected_active: bool,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let health = api.get_json("/api/v1/engine/health").await?;
        let recording = &health["pipelines"][pipeline_id]["recording"];
        let enabled = recording["enabled"].as_bool().unwrap_or(false);
        let active = recording["active"].as_bool().unwrap_or(false);
        if active == expected_active {
            return Ok(json!({
                "enabled": enabled,
                "active": active,
            }));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "recording state for pipeline {pipeline_id} did not reach active={expected_active}; enabled={enabled} active={active}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_api_hls_preview_state(
    api: &RampApi,
    pipeline_id: &str,
    expected_active: bool,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let health = api.get_json("/api/v1/engine/health").await?;
        let preview = &health["pipelines"][pipeline_id]["hlsPreview"];
        let active = preview["active"].as_bool().unwrap_or(false);
        if active == expected_active {
            return Ok(preview.clone());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "HLS preview state for pipeline {pipeline_id} did not reach active={expected_active}; preview={preview}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_pipeline_file_ingest_running_state(
    api: &RampApi,
    pipeline_id: &str,
    expected_running: bool,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let ingest = api
            .get_json(&format!("/api/v1/pipelines/{pipeline_id}/file-ingest"))
            .await?;
        let running = ingest["running"].as_bool().unwrap_or(false);
        if running == expected_running {
            return Ok(ingest);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "file ingest state for pipeline {pipeline_id} did not reach running={expected_running}; ingest={ingest}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_hls_playlist_ready(
    api: &RampApi,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<(reqwest::StatusCode, String), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let (status, body) = api
            .get_text_response(&format!("/hls/{pipeline_id}/master.m3u8"))
            .await?;
        if status.is_success() && body.contains("#EXTM3U") {
            return Ok((status, body));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "HLS playlist for pipeline {pipeline_id} did not become ready within {}s; last_status={} body={body}",
                timeout.as_secs(),
                status
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

struct RampSnapshot {
    cpu_pct: String,
    rss_kb: u64,
    ffmpeg_count: u64,
    ffmpeg_rss_kb: u64,
}

async fn snapshot_ramp(
    env: &RampEnv,
    restream_pid: u32,
    config: &str,
    step: usize,
    label: &str,
) -> Result<Value, String> {
    if !env.snapshot_sleep.is_zero() {
        tokio::time::sleep(env.snapshot_sleep).await;
    }
    let ffmpeg = ffmpeg_pipe1_stats().await;
    let snapshot = RampSnapshot {
        cpu_pct: process_cpu_pct(restream_pid)
            .await
            .unwrap_or_else(|| "0".to_string()),
        rss_kb: process_rss_kb(restream_pid).await.unwrap_or(0),
        ffmpeg_count: ffmpeg.count,
        ffmpeg_rss_kb: ffmpeg.rss_kb,
    };
    let total = snapshot.rss_kb + snapshot.ffmpeg_rss_kb;
    append_line(
        &env.scale_log,
        &format!(
            "{config},{step},\"{label}\",{},{},{},{},{}\n",
            snapshot.cpu_pct, snapshot.rss_kb, snapshot.ffmpeg_count, snapshot.ffmpeg_rss_kb, total
        ),
    )?;
    println!(
        "  {step:<4} {label:<20} cpu={} rss={} KB ffmpeg#={} ffmpeg_rss={} KB total={} KB",
        snapshot.cpu_pct, snapshot.rss_kb, snapshot.ffmpeg_count, snapshot.ffmpeg_rss_kb, total
    );
    Ok(json!({
        "step": step,
        "label": label,
        "cpuPct": snapshot.cpu_pct,
        "rssKb": snapshot.rss_kb,
        "ffmpegCount": snapshot.ffmpeg_count,
        "ffmpegRssKb": snapshot.ffmpeg_rss_kb,
        "totalRssKb": total,
    }))
}

fn append_line(path: &Path, line: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

#[derive(Clone)]
struct FfmpegStats {
    count: u64,
    rss_kb: u64,
    pids: Vec<u32>,
}

async fn ffmpeg_pipe1_stats() -> FfmpegStats {
    let output = Command::new("ps").arg("aux").output().await;
    let Ok(output) = output else {
        return FfmpegStats {
            count: 0,
            rss_kb: 0,
            pids: Vec::new(),
        };
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut count = 0;
    let mut rss_kb = 0;
    for line in text.lines() {
        if line.contains("ffmpeg") && line.contains("pipe:1") {
            count += 1;
            rss_kb += line
                .split_whitespace()
                .nth(5)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    FfmpegStats {
        count,
        rss_kb,
        pids: Vec::new(),
    }
}

async fn process_cpu_pct(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "%cpu="])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(if value.is_empty() {
        "0".to_string()
    } else {
        value
    })
}

async fn process_rss_kb(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "rss="])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn read_url(config: RampConfig, env: &RampEnv, output_index: usize) -> String {
    match config.out_proto {
        "rtmp" => format!(
            "rtmp://127.0.0.1:{}/live/{}-{output_index}",
            env.mtx_rtmp, config.name
        ),
        "srt" => format!(
            "srt://127.0.0.1:{}?streamid=read:live/{}-{output_index}&timeout=30000000",
            env.mtx_srt, config.name
        ),
        _ => String::new(),
    }
}

async fn check_ramp_stream(
    label: &str,
    url: &str,
    expected: &str,
    retries: usize,
) -> Option<String> {
    let mut last = None;
    for _ in 0..retries {
        if let Ok(dimensions) = probe_dims_ramp(url).await {
            if dimensions == expected {
                println!("  ok   {label:<45} -> {dimensions}");
                return Some(dimensions);
            }
            if !dimensions.is_empty() {
                last = Some(dimensions);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!(
        "  FAIL {label:<45} expected={expected} got={}",
        last.as_deref().unwrap_or("none")
    );
    last
}

async fn probe_dims_ramp(url: &str) -> Result<String, String> {
    probe_dims_ramp_with_cookie(url, None).await
}

async fn probe_dims_ramp_with_cookie(url: &str, cookie: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-probesize",
        "10000000",
        "-analyzeduration",
        "10000000",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height",
        "-of",
        "csv=p=0",
    ]);
    if let Some(cookie) = cookie {
        command.args(["-headers", &format!("Cookie: {cookie}\r\n")]);
    }
    let child = command
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .map_err(|_| format!("ffprobe timed out: {url}"))?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffprobe failed: {url}: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .replace(',', "x"))
}

struct MixedEnv {
    work_dir: PathBuf,
    scale_log: PathBuf,
    rss_summary: PathBuf,
    summary_log: PathBuf,
    restream_log: PathBuf,
    mediamtx_log: PathBuf,
    mediamtx_config: PathBuf,
    restream_bin: PathBuf,
    restream_db_path: PathBuf,
    assertion_log: Option<PathBuf>,
    only_checks: Option<Vec<String>>,
    resume_from: Option<String>,
    skip_load: bool,
    restream_http: u16,
    restream_rtmp: u16,
    restream_srt: u16,
    mtx_rtmp: u16,
    mtx_srt: u16,
    mtx_hls: u16,
    mtx_api: u16,
    n_per_group: usize,
    snapshot_sleep: Duration,
}

impl MixedEnv {
    fn from_env(log_stem: &str) -> Self {
        let work_dir = std::env::var_os("WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test/artifacts/mixed-scale"));
        let ports = harness_port_defaults();
        Self {
            scale_log: std::env::var_os("SCALE_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("scale.csv")),
            rss_summary: std::env::var_os("RSS_SUMMARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("rss-summary.csv")),
            summary_log: std::env::var_os("SUMMARY_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join("summary.txt")),
            restream_log: std::env::var_os("MIXED_RESTREAM_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join(format!("{log_stem}-restream.log"))),
            mediamtx_log: std::env::var_os("MIXED_MEDIAMTX_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join(format!("{log_stem}-mediamtx.log"))),
            mediamtx_config: std::env::var_os("MIXED_MEDIAMTX_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|| work_dir.join(format!("{log_stem}-mediamtx.yml"))),
            restream_bin: default_restream_bin(),
            restream_db_path: std::env::var_os("RESTREAM_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| default_work_db_path(&work_dir, &format!("{log_stem}.db"))),
            assertion_log: std::env::var_os("ASSERTION_LOG")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            only_checks: std::env::var("ONLY_CHECKS")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    value
                        .split(',')
                        .map(|item| item.trim().replace('_', "-"))
                        .filter(|item| !item.is_empty())
                        .collect()
                }),
            resume_from: std::env::var("RESUME_FROM")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            skip_load: std::env::var("SKIP_LOAD")
                .ok()
                .is_some_and(|value| value == "1"),
            restream_http: ports.restream_http,
            restream_rtmp: ports.restream_rtmp,
            restream_srt: ports.restream_srt,
            mtx_rtmp: ports.mtx_rtmp,
            mtx_srt: ports.mtx_srt,
            mtx_hls: ports.mtx_hls,
            mtx_api: ports.mtx_api,
            n_per_group: env_usize("N_PER_GROUP", 25),
            snapshot_sleep: Duration::from_secs(env_secs("SNAPSHOT_SLEEP_SECS", 3)),
            work_dir,
        }
    }

    fn check_selected(&self, check: &str) -> bool {
        self.only_checks
            .as_ref()
            .is_none_or(|items| items.iter().any(|item| item == check))
    }
}

struct MixedResume {
    target: Option<String>,
    active: bool,
}

impl MixedResume {
    fn new(target: Option<String>) -> Self {
        Self {
            active: target.is_none(),
            target,
        }
    }

    fn allows(&mut self, id: &str) -> bool {
        if self.active {
            return true;
        }
        if self.target.as_deref() == Some(id) {
            self.active = true;
            return true;
        }
        false
    }
}

async fn mixed_anchor_correctness() -> Result<Value, String> {
    let env = MixedEnv::from_env("mixed-anchor");
    if env.n_per_group == 0 {
        return Err("N_PER_GROUP must be greater than zero".to_string());
    }
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    ensure_mixed_artifacts(&env)?;

    let mut mediamtx = start_mixed_mediamtx(&env).await?;
    let mut restream = start_mixed_restream(&env).await?;
    let restream_pid = restream.id().unwrap_or(0);
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;

    let mut resume = MixedResume::new(env.resume_from.clone());
    let result = run_mixed_anchor_config(&env, &api, restream_pid, &mut resume).await;

    stop_child(&mut restream).await;
    stop_child(&mut mediamtx).await;

    result.map(|config| {
        json!({
            "passed": true,
            "mode": "mixed-anchor",
            "configs": [config],
            "artifacts": {
                "scaleCsv": env.scale_log,
                "rssSummary": env.rss_summary,
                "summary": env.summary_log,
                "restreamLog": env.restream_log,
                "mediamtxLog": env.mediamtx_log,
            }
        })
    })
}

async fn mixed_h265_srt_correctness() -> Result<Value, String> {
    let env = MixedEnv::from_env("mixed-h265-srt");
    if env.n_per_group == 0 {
        return Err("N_PER_GROUP must be greater than zero".to_string());
    }
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    ensure_mixed_artifacts(&env)?;

    let mut mediamtx = start_mixed_mediamtx(&env).await?;
    let mut restream = start_mixed_restream(&env).await?;
    let restream_pid = restream.id().unwrap_or(0);
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;

    let mut resume = MixedResume::new(env.resume_from.clone());
    let result = run_mixed_h265_srt_config(&env, &api, restream_pid, &mut resume).await;

    stop_child(&mut restream).await;
    stop_child(&mut mediamtx).await;

    result.map(|config| {
        json!({
            "passed": true,
            "mode": "mixed-h265-srt",
            "configs": [config],
            "artifacts": {
                "scaleCsv": env.scale_log,
                "rssSummary": env.rss_summary,
                "summary": env.summary_log,
                "restreamLog": env.restream_log,
                "mediamtxLog": env.mediamtx_log,
            }
        })
    })
}

async fn mixed_h264_rtmp_correctness() -> Result<Value, String> {
    let env = MixedEnv::from_env("mixed-h264-rtmp");
    if env.n_per_group == 0 {
        return Err("N_PER_GROUP must be greater than zero".to_string());
    }
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    ensure_mixed_artifacts(&env)?;

    let mut mediamtx = start_mixed_mediamtx(&env).await?;
    let mut restream = start_mixed_restream(&env).await?;
    let restream_pid = restream.id().unwrap_or(0);
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;

    let mut resume = MixedResume::new(env.resume_from.clone());
    let config = run_mixed_h264_rtmp_config(&env, &api, restream_pid, &mut resume).await;

    stop_child(&mut restream).await;
    stop_child(&mut mediamtx).await;

    config.map(|config| {
        json!({
            "passed": true,
            "mode": "mixed-h264-rtmp",
            "configs": [config],
            "artifacts": {
                "scaleCsv": env.scale_log,
                "rssSummary": env.rss_summary,
                "summary": env.summary_log,
                "restreamLog": env.restream_log,
                "mediamtxLog": env.mediamtx_log,
            }
        })
    })
}

async fn mixed_h264_srt_multi_correctness() -> Result<Value, String> {
    mixed_srt_multi_correctness("mixed-h264-srt-multi", "h264-srt-multi", false).await
}

async fn mixed_h265_srt_multi_correctness() -> Result<Value, String> {
    mixed_srt_multi_correctness("mixed-h265-srt-multi", "h265-srt-multi", true).await
}

async fn mixed_srt_multi_correctness(
    log_stem: &str,
    cfg: &str,
    h265: bool,
) -> Result<Value, String> {
    let env = MixedEnv::from_env(log_stem);
    if env.n_per_group == 0 {
        return Err("N_PER_GROUP must be greater than zero".to_string());
    }
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    ensure_mixed_artifacts(&env)?;

    let mut mediamtx = start_mixed_mediamtx(&env).await?;
    let mut restream = start_mixed_restream(&env).await?;
    let restream_pid = restream.id().unwrap_or(0);
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;

    let mut resume = MixedResume::new(env.resume_from.clone());
    let config = run_mixed_srt_multi_config(&env, &api, restream_pid, cfg, h265, &mut resume).await;

    stop_child(&mut restream).await;
    stop_child(&mut mediamtx).await;

    config.map(|config| {
        json!({
            "passed": true,
            "mode": log_stem,
            "configs": [config],
            "artifacts": {
                "scaleCsv": env.scale_log,
                "rssSummary": env.rss_summary,
                "summary": env.summary_log,
                "restreamLog": env.restream_log,
                "mediamtxLog": env.mediamtx_log,
            }
        })
    })
}

fn ensure_mixed_artifacts(env: &MixedEnv) -> Result<(), String> {
    if !env.scale_log.exists() {
        std::fs::write(
            &env.scale_log,
            "config,label,cpu_pct,rss_kb,ext_ffmpeg_n,ext_ffmpeg_rss_kb\n",
        )
        .map_err(|e| e.to_string())?;
    }
    if !env.rss_summary.exists() {
        std::fs::write(&env.rss_summary, "").map_err(|e| e.to_string())?;
    }
    if !env.summary_log.exists() {
        std::fs::write(&env.summary_log, "").map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn start_mixed_restream(env: &MixedEnv) -> Result<Child, String> {
    start_restream_child(
        &env.restream_bin,
        &TestPorts {
            http: env.restream_http,
            rtmp: env.restream_rtmp,
            srt: env.restream_srt,
        },
        &env.restream_db_path,
        &env.restream_log,
    )
    .await
}

async fn start_mixed_mediamtx(env: &MixedEnv) -> Result<Child, String> {
    std::fs::write(
        &env.mediamtx_config,
        format!(
            "logLevel: warn\nrtmp: yes\nrtmpAddress: :{}\nrtmpEncryption: \"no\"\nrtsp: no\nsrt: yes\nsrtAddress: :{}\nhls: yes\nhlsAddress: :{}\nhlsPartDuration: 200ms\nhlsSegmentDuration: 2s\nwebrtc: no\napi: yes\napiAddress: :{}\nmetrics: no\npaths:\n  all:\n",
            env.mtx_rtmp, env.mtx_srt, env.mtx_hls, env.mtx_api
        ),
    )
    .map_err(|e| e.to_string())?;
    let log = std::fs::File::create(&env.mediamtx_log).map_err(|e| e.to_string())?;
    let stderr_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new("mediamtx")
        .arg(&env.mediamtx_config)
        .env_remove("MTX_RTMP")
        .env_remove("MTX_SRT")
        .env_remove("MTX_HLS")
        .env_remove("MTX_API")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/v3/paths/list", env.mtx_api),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut child).await;
        return Err(format!("mediamtx did not become ready: {err}"));
    }
    Ok(child)
}

async fn run_mixed_anchor_config(
    env: &MixedEnv,
    api: &RampApi,
    restream_pid: u32,
    resume: &mut MixedResume,
) -> Result<Value, String> {
    let cfg = "h264-srt";
    let n = env.n_per_group;
    let total = n * 4;
    let stream_key = format!("sk-{cfg}");

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": cfg, "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    let mut publisher = spawn_mixed_anchor_publisher(env, &stream_key).await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;
    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, "baseline (input live, 0 outputs)").await?;
    }

    let mut output_ids = Vec::with_capacity(total + 1);
    let hls_output = create_mixed_output(
        api,
        &pipeline_id,
        "hls-preview",
        &format!("hls://{cfg}-preview"),
        "source",
    )
    .await?;
    start_mixed_output(api, &pipeline_id, &hls_output).await?;
    output_ids.push(hls_output);

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP source")).await?;
    }

    if env.check_selected("smoke") && resume.allows("MS-smoke") {
        let started = Instant::now();
        let launches =
            count_log_matches(&env.restream_log, "[external-transcoder] Launching ffmpeg");
        if launches != 0 {
            emit_mixed_result(
                env,
                cfg,
                "MS-smoke",
                "fail",
                started.elapsed(),
                Some(json!({
                    "message": format!("smoke: external transcoder fired before 720p outputs ({launches} launches)"),
                    "external_transcoder_launches": launches,
                })),
            )?;
            return Err(format!(
                "smoke: external transcoder fired before 720p outputs ({launches} launches)"
            ));
        }
        emit_mixed_result(
            env,
            cfg,
            "MS-smoke",
            "pass",
            started.elapsed(),
            Some(json!({
                "external_transcoder_launches": launches,
            })),
        )?;
        log_mixed_ok(env, "smoke: no external transcoder for source outputs")?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-720p",
            count: n,
            encoding: "720p",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP 720p")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-src-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} SRT source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-720p",
            count: n,
            encoding: "720p",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-720p-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(
            env,
            restream_pid,
            cfg,
            &format!("after all {total} outputs"),
        )
        .await?;
    }

    let rss_final = process_rss_kb(restream_pid).await.unwrap_or(0);
    let ffmpeg = ffmpeg_pipe1_stats().await;
    let rss_delta = rss_final.saturating_sub(rss_baseline);
    let per_output = rss_delta / total as u64;
    append_line(
        &env.rss_summary,
        &format!(
            "{cfg},rss_delta_kb={rss_delta},per_output_kb={per_output},ext_ffmpeg_n={},ext_ffmpeg_rss_kb={}\n",
            ffmpeg.count, ffmpeg.rss_kb
        ),
    )?;
    if !env.skip_load && env.check_selected("load") {
        emit_mixed_result(
            env,
            cfg,
            "MS-load-h264-srt",
            "pass",
            Duration::ZERO,
            Some(json!({
                "rss_delta_kb": rss_delta,
                "per_output_kb": per_output,
                "ext_ffmpeg_n": ffmpeg.count,
                "ext_ffmpeg_rss_kb": ffmpeg.rss_kb,
            })),
        )?;
    }

    if env.check_selected("ffprobe") {
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-rtmp-src",
                label: &format!("RTMP-src  out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{n}", env.mtx_rtmp),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-rtmp-720p",
                label: &format!("RTMP-720p out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-srt-src",
                label: &format!("SRT-src   out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-src-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-srt-720p",
                label: &format!("SRT-720p  out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-720p-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
    } else if env.check_selected("lifecycle") {
        warm_mixed_stream(
            &format!("RTMP-720p out{n} lifecycle warmup"),
            &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
            "1280x720",
            None,
        )
        .await;
    }

    if env.check_selected("hls") {
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-hls-mtx",
                label: "HLS/mtx",
                url: &format!(
                    "http://127.0.0.1:{}/live/{cfg}-rtmp-src-{n}/index.m3u8",
                    env.mtx_hls
                ),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-hls-restream",
                label: "HLS/restream",
                url: &format!(
                    "http://127.0.0.1:{}/hls/{pipeline_id}/index.m3u8",
                    env.restream_http
                ),
                expected: "1920x1080",
                cookie: api.cookie.as_deref(),
            },
            resume,
        )
        .await?;
    }

    // Phase 4: harness sink probe — assert DTS monotonicity, video+audio
    // presence, and keyframe cadence on the live egress.
    let mut sink_probe_result = None;
    if env.check_selected("sink-probe") && resume.allows("MS-sink-probe") {
        let started = Instant::now();
        let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
        match run_sink_probe(api, &pipeline_id, cfg, "source", sink_port, 30).await {
            Ok(probe) => {
                let status = if probe.passed { "pass" } else { "fail" };
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-sink-probe",
                    status,
                    started.elapsed(),
                    Some(probe.summary.clone()),
                )?;
                output_ids.push(probe.output_id.clone());
                sink_probe_result = Some(probe);
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-sink-probe",
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    let mut hls_put_probe_result = None;
    if env.check_selected("hls-put-probe") && resume.allows("MS-hls-put-probe") {
        let started = Instant::now();
        let put_port: u16 = env_u16("HLS_PUT_PORT", 8990);
        match run_hls_put_probe(api, &pipeline_id, cfg, put_port).await {
            Ok(probe) => {
                let status = if probe.passed { "pass" } else { "fail" };
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-hls-put-probe",
                    status,
                    started.elapsed(),
                    Some(probe.summary.clone()),
                )?;
                output_ids.push(probe.output_id.clone());
                hls_put_probe_result = Some(probe);
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-hls-put-probe",
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    let mut burst_graph_result = None;
    if env.check_selected("burst-graph") && resume.allows("MS-burst-graph") {
        let started = Instant::now();
        match run_burst_graph_check(api, &pipeline_id).await {
            Ok((passed, summary)) => {
                let status = if passed { "pass" } else { "fail" };
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-burst-graph",
                    status,
                    started.elapsed(),
                    Some(summary.clone()),
                )?;
                burst_graph_result = Some((passed, summary));
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-burst-graph",
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    stop_child(&mut publisher).await;
    stop_mixed_outputs(api, &pipeline_id, &output_ids).await;
    let lifecycle_started = Instant::now();
    let lifecycle_result =
        wait_for_outputs_stopped(api, &pipeline_id, &output_ids, Duration::from_secs(60)).await;
    if env.check_selected("lifecycle") && resume.allows("MS-lifecycle") {
        if let Err(error) = lifecycle_result {
            emit_mixed_result(
                env,
                cfg,
                "MS-lifecycle",
                "fail",
                lifecycle_started.elapsed(),
                Some(json!({
                    "message": error,
                    "stopped": false,
                    "requested": output_ids.len(),
                })),
            )?;
            return Err("lifecycle: outputs did not all stop within 60 s".to_string());
        }
        emit_mixed_result(
            env,
            cfg,
            "MS-lifecycle",
            "pass",
            lifecycle_started.elapsed(),
            Some(json!({
                "stopped": output_ids.len(),
            })),
        )?;
        log_mixed_ok(env, "lifecycle: all outputs stopped")?;
    } else if lifecycle_result.is_err() {
        tokio::time::sleep(Duration::from_secs(3)).await;
    } else {
        log_mixed_ok(env, "lifecycle: all outputs stopped")?;
    }

    let mut result = json!({
        "config": cfg,
        "pipelineId": pipeline_id,
        "nPerGroup": n,
        "totalOutputs": total,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "extFfmpegCount": ffmpeg.count,
        "extFfmpegRssKb": ffmpeg.rss_kb,
    });
    if let Some(probe) = sink_probe_result {
        result["sinkProbe"] = probe.summary;
        result["sinkProbePassed"] = json!(probe.passed);
    }
    if let Some(probe) = hls_put_probe_result {
        result["hlsPutProbe"] = probe.summary;
        result["hlsPutProbePassed"] = json!(probe.passed);
    }
    if let Some((passed, summary)) = burst_graph_result {
        result["burstGraph"] = summary;
        result["burstGraphPassed"] = json!(passed);
    }
    Ok(result)
}

async fn run_mixed_h265_srt_config(
    env: &MixedEnv,
    api: &RampApi,
    restream_pid: u32,
    resume: &mut MixedResume,
) -> Result<Value, String> {
    let cfg = "h265-srt";
    let n = env.n_per_group;
    let total = n * 4;
    let stream_key = format!("sk-{cfg}");

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": cfg, "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    let mut publisher = spawn_mixed_h265_srt_publisher(env, &stream_key).await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;
    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, "baseline (input live, 0 outputs)").await?;
    }

    let mut output_ids = Vec::with_capacity(total);
    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-720p",
            count: n,
            encoding: "720p",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP 720p")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-src-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} SRT source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-720p",
            count: n,
            encoding: "720p",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-720p-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(
            env,
            restream_pid,
            cfg,
            &format!("after all {total} outputs"),
        )
        .await?;
    }

    let rss_final = process_rss_kb(restream_pid).await.unwrap_or(0);
    let ffmpeg = ffmpeg_pipe1_stats().await;
    let rss_delta = rss_final.saturating_sub(rss_baseline);
    let per_output = rss_delta / total as u64;
    append_line(
        &env.rss_summary,
        &format!(
            "{cfg},rss_delta_kb={rss_delta},per_output_kb={per_output},ext_ffmpeg_n={},ext_ffmpeg_rss_kb={}\n",
            ffmpeg.count, ffmpeg.rss_kb
        ),
    )?;
    if !env.skip_load && env.check_selected("load") {
        emit_mixed_result(
            env,
            cfg,
            "MS-load-h265-srt",
            "pass",
            Duration::ZERO,
            Some(json!({
                "rss_delta_kb": rss_delta,
                "per_output_kb": per_output,
                "ext_ffmpeg_n": ffmpeg.count,
                "ext_ffmpeg_rss_kb": ffmpeg.rss_kb,
            })),
        )?;
    }

    if env.check_selected("ffprobe") {
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h265-srt-rtmp-src",
                label: &format!("RTMP-src  out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{n}", env.mtx_rtmp),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h265-srt-rtmp-720p",
                label: &format!("RTMP-720p out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h265-srt-srt-src",
                label: &format!("SRT-src   out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-src-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h265-srt-srt-720p",
                label: &format!("SRT-720p  out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-720p-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
    }

    if env.check_selected("tc-spawns") && resume.allows("MS-tc-spawns") {
        let started = Instant::now();
        let graph = api
            .get_json(&format!("/api/v1/pipelines/{pipeline_id}/graph"))
            .await?;
        let tc_stages = graph_active_node_count(&graph, "codec_edge");
        let ffmpeg = ffmpeg_pipe1_stats().await;
        let tc_max = ffmpeg.count + 1;
        if tc_stages < 1 || tc_stages as u64 > tc_max {
            let message = format!(
                "{cfg}: expected 1..{tc_max} active HEVC->H.264 codec-edge stages (got {tc_stages}; N={n} outputs - sharing broken if >{tc_max})"
            );
            emit_mixed_result(
                env,
                cfg,
                "MS-tc-spawns",
                "fail",
                started.elapsed(),
                Some(json!({
                    "message": message,
                    "tc_stages": tc_stages,
                    "bound": tc_max,
                    "graph": graph,
                })),
            )?;
            stop_child(&mut publisher).await;
            stop_mixed_outputs(api, &pipeline_id, &output_ids).await;
            return Err(message);
        }
        emit_mixed_result(
            env,
            cfg,
            "MS-tc-spawns",
            "pass",
            started.elapsed(),
            Some(json!({
                "tc_stages": tc_stages,
                "bound": tc_max,
            })),
        )?;
        log_mixed_ok(
            env,
            &format!(
                "{cfg}: TC_STAGES={tc_stages} <= {tc_max} (stage sharing confirmed for {total} outputs)"
            ),
        )?;
    }

    let mut sink_probe_result = None;
    if env.check_selected("sink-probe") && resume.allows("MS-sink-probe-h265-srt") {
        let started = Instant::now();
        let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
        match run_sink_probe(api, &pipeline_id, cfg, "source", sink_port, 30).await {
            Ok(probe) => {
                let status = if probe.passed { "pass" } else { "fail" };
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-sink-probe-h265-srt",
                    status,
                    started.elapsed(),
                    Some(probe.summary.clone()),
                )?;
                output_ids.push(probe.output_id.clone());
                sink_probe_result = Some(probe);
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-sink-probe-h265-srt",
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    stop_child(&mut publisher).await;
    stop_mixed_outputs(api, &pipeline_id, &output_ids).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    let mut result = json!({
        "config": cfg,
        "pipelineId": pipeline_id,
        "nPerGroup": n,
        "totalOutputs": total,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "extFfmpegCount": ffmpeg.count,
        "extFfmpegRssKb": ffmpeg.rss_kb,
    });
    if let Some(probe) = sink_probe_result {
        result["sinkProbe"] = probe.summary;
        result["sinkProbePassed"] = json!(probe.passed);
    }
    Ok(result)
}

async fn run_mixed_h264_rtmp_config(
    env: &MixedEnv,
    api: &RampApi,
    restream_pid: u32,
    resume: &mut MixedResume,
) -> Result<Value, String> {
    let cfg = "h264-rtmp";
    let n = env.n_per_group;
    let total = n * 4;
    let stream_key = format!("sk-{cfg}");

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": cfg, "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    let mut publisher = spawn_mixed_h264_rtmp_publisher(env, &stream_key).await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;
    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, "baseline (input live, 0 outputs)").await?;
    }

    let mut output_ids = Vec::with_capacity(total);
    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-720p",
            count: n,
            encoding: "720p",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP 720p")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-src-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} SRT source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-720p",
            count: n,
            encoding: "720p",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-720p-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(
            env,
            restream_pid,
            cfg,
            &format!("after all {total} outputs"),
        )
        .await?;
    }

    let rss_final = process_rss_kb(restream_pid).await.unwrap_or(0);
    let ffmpeg = ffmpeg_pipe1_stats().await;
    let rss_delta = rss_final.saturating_sub(rss_baseline);
    let per_output = rss_delta / total as u64;
    append_line(
        &env.rss_summary,
        &format!(
            "{cfg},rss_delta_kb={rss_delta},per_output_kb={per_output},ext_ffmpeg_n={},ext_ffmpeg_rss_kb={}\n",
            ffmpeg.count, ffmpeg.rss_kb
        ),
    )?;
    if !env.skip_load && env.check_selected("load") {
        emit_mixed_result(
            env,
            cfg,
            "MS-load-h264-rtmp",
            "pass",
            Duration::ZERO,
            Some(json!({
                "rss_delta_kb": rss_delta,
                "per_output_kb": per_output,
                "ext_ffmpeg_n": ffmpeg.count,
                "ext_ffmpeg_rss_kb": ffmpeg.rss_kb,
            })),
        )?;
    }

    if env.check_selected("ffprobe") {
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h264-rtmp-rtmp-src",
                label: &format!("RTMP-src  out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{n}", env.mtx_rtmp),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h264-rtmp-rtmp-720p",
                label: &format!("RTMP-720p out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h264-rtmp-srt-src",
                label: &format!("SRT-src   out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-src-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: "MS-ffprobe-h264-rtmp-srt-720p",
                label: &format!("SRT-720p  out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-720p-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
    }

    let mut sink_probe_result = None;
    if env.check_selected("sink-probe") && resume.allows("MS-sink-probe-h264-rtmp") {
        let started = Instant::now();
        let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
        match run_sink_probe(api, &pipeline_id, cfg, "source", sink_port, 30).await {
            Ok(probe) => {
                let status = if probe.passed { "pass" } else { "fail" };
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-sink-probe-h264-rtmp",
                    status,
                    started.elapsed(),
                    Some(probe.summary.clone()),
                )?;
                output_ids.push(probe.output_id.clone());
                sink_probe_result = Some(probe);
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    "MS-sink-probe-h264-rtmp",
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    stop_child(&mut publisher).await;
    stop_mixed_outputs(api, &pipeline_id, &output_ids).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    let mut result = json!({
        "config": cfg,
        "pipelineId": pipeline_id,
        "nPerGroup": n,
        "totalOutputs": total,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "extFfmpegCount": ffmpeg.count,
        "extFfmpegRssKb": ffmpeg.rss_kb,
    });
    if let Some(probe) = sink_probe_result {
        result["sinkProbe"] = probe.summary;
        result["sinkProbePassed"] = json!(probe.passed);
    }
    Ok(result)
}

async fn run_mixed_srt_multi_config(
    env: &MixedEnv,
    api: &RampApi,
    restream_pid: u32,
    cfg: &str,
    h265: bool,
    resume: &mut MixedResume,
) -> Result<Value, String> {
    let n = env.n_per_group;
    let total = n * 4;
    let stream_key = format!("sk-{cfg}");

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": cfg, "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    let mut publisher = spawn_mixed_srt_multi_publisher(env, &stream_key, cfg, h265).await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;

    // Give the probe time to fire and adaptive ring resize to complete (≤ 5 s).
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Verify adaptive ring sizing: 2-audio-track SRT stream → 100+ pkt/s →
    // ring must have grown beyond the 1024-slot default and hold ≥ 5 s of depth.
    let ring_check_id = format!("MS-adaptive-ring-{cfg}");
    if env.check_selected("ffprobe") || resume.allows(&ring_check_id) {
        let started = std::time::Instant::now();
        let telem_path = format!("/api/v1/pipelines/{pipeline_id}/telemetry");
        match api.get_json(&telem_path).await {
            Ok(telem) => {
                let cap = telem["sourceRing"]["capacity"].as_u64().unwrap_or(0);
                let depth = telem["sourceRing"]["bufferDepthSecs"]
                    .as_f64()
                    .unwrap_or(0.0);
                let overflows: u64 = telem["sourceRing"]["readers"]
                    .as_array()
                    .map(|rs| {
                        rs.iter()
                            .map(|r| r["overflowCount"].as_u64().unwrap_or(0))
                            .sum()
                    })
                    .unwrap_or(0);
                // 2 audio tracks × 50 pkt/s + video ≈ 130 pkt/s → needed ≈ 780
                // Any capacity > 1024 confirms adaptive resize fired correctly
                let resized = cap > 1024;
                let adequate = depth >= 5.0 || cap >= 780;
                let no_overflow = overflows == 0;
                let passed = adequate && no_overflow;
                emit_mixed_result(
                    env,
                    cfg,
                    &ring_check_id,
                    if passed { "pass" } else { "fail" },
                    started.elapsed(),
                    Some(json!({
                        "ringCapacity": cap,
                        "bufferDepthSecs": depth,
                        "ringResized": resized,
                        "adequate": adequate,
                        "overflows": overflows,
                    })),
                )?;
                if passed {
                    log_mixed_ok(
                        env,
                        &format!(
                            "adaptive-ring {cfg}: cap={cap} depth={depth:.1}s \
                             overflows={overflows}{}",
                            if resized { " [resized]" } else { "" }
                        ),
                    )?;
                } else {
                    return Err(format!(
                        "adaptive ring check failed for {cfg}: cap={cap} depth={depth:.1}s overflows={overflows}"
                    ));
                }
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    &ring_check_id,
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, "baseline (input live, 0 outputs)").await?;
    }

    let mut output_ids = Vec::with_capacity(total);
    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-720p",
            count: n,
            encoding: "720p+atrack:0",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP 720p")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-src-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} SRT source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-720p",
            count: n,
            encoding: "720p+atrack:0,1",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-720p-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(
            env,
            restream_pid,
            cfg,
            &format!("after all {total} outputs"),
        )
        .await?;
    }

    let rss_final = process_rss_kb(restream_pid).await.unwrap_or(0);
    let ffmpeg = ffmpeg_pipe1_stats().await;
    let rss_delta = rss_final.saturating_sub(rss_baseline);
    let per_output = rss_delta / total as u64;
    append_line(
        &env.rss_summary,
        &format!(
            "{cfg},rss_delta_kb={rss_delta},per_output_kb={per_output},ext_ffmpeg_n={},ext_ffmpeg_rss_kb={}\n",
            ffmpeg.count, ffmpeg.rss_kb
        ),
    )?;
    if !env.skip_load && env.check_selected("load") {
        emit_mixed_result(
            env,
            cfg,
            &format!("MS-load-{cfg}"),
            "pass",
            Duration::ZERO,
            Some(json!({
                "rss_delta_kb": rss_delta,
                "per_output_kb": per_output,
                "ext_ffmpeg_n": ffmpeg.count,
                "ext_ffmpeg_rss_kb": ffmpeg.rss_kb,
                "audio_tracks": 2,
            })),
        )?;
    }

    if env.check_selected("ffprobe") {
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: &format!("MS-ffprobe-{cfg}-rtmp-src"),
                label: &format!("RTMP-src  out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{n}", env.mtx_rtmp),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: &format!("MS-ffprobe-{cfg}-rtmp-720p"),
                label: &format!("RTMP-720p out{n}"),
                url: &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: &format!("MS-ffprobe-{cfg}-srt-src"),
                label: &format!("SRT-src   out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-src-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1920x1080",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_stream(
            env,
            MixedProbeSpec {
                cfg,
                id: &format!("MS-ffprobe-{cfg}-srt-720p"),
                label: &format!("SRT-720p  out{n}"),
                url: &format!(
                    "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-720p-{n}&timeout=30000000",
                    env.mtx_srt
                ),
                expected: "1280x720",
                cookie: None,
            },
            resume,
        )
        .await?;
        verify_mixed_audio_route(
            env,
            cfg,
            &format!("MS-audio-{cfg}-rtmp-720p"),
            &format!("RTMP-720p audio out{n}"),
            &format!("rtmp://127.0.0.1:{}/live/{cfg}-rtmp-720p-{n}", env.mtx_rtmp),
            "1280x720",
            1,
            resume,
        )
        .await?;
        verify_mixed_audio_route(
            env,
            cfg,
            &format!("MS-audio-{cfg}-srt-720p"),
            &format!("SRT-720p audio out{n}"),
            &format!(
                "srt://127.0.0.1:{}?streamid=read:live/{cfg}-srt-720p-{n}&timeout=30000000",
                env.mtx_srt
            ),
            "1280x720",
            2,
            resume,
        )
        .await?;
    }

    let mut sink_probe_result = None;
    let probe_id = format!("MS-sink-probe-{cfg}");
    if env.check_selected("sink-probe") && resume.allows(&probe_id) {
        let started = Instant::now();
        let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
        match run_sink_probe(api, &pipeline_id, cfg, "source", sink_port, 30).await {
            Ok(probe) => {
                let status = if probe.passed { "pass" } else { "fail" };
                emit_mixed_result(
                    env,
                    cfg,
                    &probe_id,
                    status,
                    started.elapsed(),
                    Some(probe.summary.clone()),
                )?;
                output_ids.push(probe.output_id.clone());
                sink_probe_result = Some(probe);
            }
            Err(e) => {
                emit_mixed_result(
                    env,
                    cfg,
                    &probe_id,
                    "fail",
                    started.elapsed(),
                    Some(json!({"error": e})),
                )?;
            }
        }
    }

    stop_child(&mut publisher).await;
    stop_mixed_outputs(api, &pipeline_id, &output_ids).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    let mut result = json!({
        "config": cfg,
        "pipelineId": pipeline_id,
        "nPerGroup": n,
        "totalOutputs": total,
        "rssDeltaKb": rss_delta,
        "perOutputKb": per_output,
        "extFfmpegCount": ffmpeg.count,
        "extFfmpegRssKb": ffmpeg.rss_kb,
        "audioTracks": 2,
        "rtmp720pEncoding": "720p+atrack:0",
        "srt720pEncoding": "720p+atrack:0,1",
    });
    if let Some(probe) = sink_probe_result {
        result["sinkProbe"] = probe.summary;
        result["sinkProbePassed"] = json!(probe.passed);
    }
    Ok(result)
}

async fn spawn_mixed_anchor_publisher(env: &MixedEnv, stream_key: &str) -> Result<Child, String> {
    let log_path = env.work_dir.join("mixed-anchor-publisher.log");
    let fixture = restream::test_fixtures::bench_transport_fixture("h264", "1.5M", false)?;
    spawn_publisher_with_selection(
        &fixture,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
            env.restream_srt
        ),
        "mpegts",
        PublishTrackSelection::PrimaryAv,
        Some(&log_path),
    )
}

async fn spawn_mixed_h265_srt_publisher(env: &MixedEnv, stream_key: &str) -> Result<Child, String> {
    let log_path = env.work_dir.join("mixed-h265-srt-publisher.log");
    let fixture = restream::test_fixtures::bench_transport_fixture("h265", "1.5M", false)?;
    spawn_publisher_with_selection(
        &fixture,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
            env.restream_srt
        ),
        "mpegts",
        PublishTrackSelection::PrimaryAv,
        Some(&log_path),
    )
}

async fn spawn_mixed_h264_rtmp_publisher(
    env: &MixedEnv,
    stream_key: &str,
) -> Result<Child, String> {
    let log_path = env.work_dir.join("mixed-h264-rtmp-publisher.log");
    let fixture = restream::test_fixtures::bench_transport_fixture("h264", "1.5M", false)?;
    spawn_publisher_with_selection(
        &fixture,
        &format!("rtmp://127.0.0.1:{}/live/{stream_key}", env.restream_rtmp),
        "flv",
        PublishTrackSelection::PrimaryAv,
        Some(&log_path),
    )
}

async fn spawn_mixed_srt_multi_publisher(
    env: &MixedEnv,
    stream_key: &str,
    cfg: &str,
    h265: bool,
) -> Result<Child, String> {
    let log_path = env.work_dir.join(format!("{cfg}-publisher.log"));
    let fixture = restream::test_fixtures::bench_transport_fixture(
        if h265 { "h265" } else { "h264" },
        "1.5M",
        true,
    )?;
    spawn_publisher_with_selection(
        &fixture,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
            env.restream_srt
        ),
        "mpegts",
        PublishTrackSelection::AllStreams,
        Some(&log_path),
    )
}

async fn create_mixed_output(
    api: &RampApi,
    pipeline_id: &str,
    name: &str,
    url: &str,
    encoding: &str,
) -> Result<String, String> {
    let output = api
        .post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs"),
            json!({"name": name, "url": url, "encoding": encoding}),
        )
        .await?;
    output["output"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or("output create response missing output.id".to_string())
}

async fn start_mixed_output(
    api: &RampApi,
    pipeline_id: &str,
    output_id: &str,
) -> Result<(), String> {
    api.post_json(
        &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/start"),
        Value::Null,
    )
    .await
    .map(|_| ())
}

struct MixedGroupSpec<'a> {
    cfg: &'a str,
    group: &'a str,
    count: usize,
    encoding: &'a str,
}

async fn add_mixed_group<F>(
    api: &RampApi,
    pipeline_id: &str,
    spec: MixedGroupSpec<'_>,
    url_for: F,
    output_ids: &mut Vec<String>,
) -> Result<(), String>
where
    F: Fn(usize) -> String,
{
    for index in 1..=spec.count {
        let output_id = create_mixed_output(
            api,
            pipeline_id,
            &format!("{}-{index}", spec.group),
            &url_for(index),
            spec.encoding,
        )
        .await?;
        start_mixed_output(api, pipeline_id, &output_id).await?;
        output_ids.push(output_id);
    }
    println!(
        "[mixed-scale] added {} {} outputs for {}",
        spec.count, spec.group, spec.cfg
    );
    Ok(())
}

async fn snapshot_mixed(
    env: &MixedEnv,
    restream_pid: u32,
    cfg: &str,
    label: &str,
) -> Result<(), String> {
    if !env.snapshot_sleep.is_zero() {
        tokio::time::sleep(env.snapshot_sleep).await;
    }
    let cpu = process_cpu_pct(restream_pid)
        .await
        .unwrap_or_else(|| "0".to_string());
    let rss = process_rss_kb(restream_pid).await.unwrap_or(0);
    let ffmpeg = ffmpeg_pipe1_stats().await;
    append_line(
        &env.scale_log,
        &format!(
            "{cfg},\"{label}\",{cpu},{rss},{},{}\n",
            ffmpeg.count, ffmpeg.rss_kb
        ),
    )?;
    println!(
        "  {label:<45} cpu={cpu}% rss={rss} KB ext_ffmpeg#={} ext_ffmpeg_rss={} KB",
        ffmpeg.count, ffmpeg.rss_kb
    );
    Ok(())
}

struct MixedProbeSpec<'a> {
    cfg: &'a str,
    id: &'a str,
    label: &'a str,
    url: &'a str,
    expected: &'a str,
    cookie: Option<&'a str>,
}

async fn verify_mixed_stream(
    env: &MixedEnv,
    spec: MixedProbeSpec<'_>,
    resume: &mut MixedResume,
) -> Result<(), String> {
    if !resume.allows(spec.id) {
        return Ok(());
    }
    let started = Instant::now();
    let mut last = String::new();
    let mut last_error = String::new();
    for _attempt in 1..=30 {
        match probe_dims_ramp_with_cookie(spec.url, spec.cookie).await {
            Ok(dimensions) if dimensions == spec.expected => {
                emit_mixed_result(
                    env,
                    spec.cfg,
                    spec.id,
                    "pass",
                    started.elapsed(),
                    Some(json!({
                        "label": spec.label,
                        "expected": spec.expected,
                        "got": dimensions,
                        "url": spec.url,
                    })),
                )?;
                log_mixed_ok(env, &format!("ffprobe: {} -> {dimensions}", spec.label))?;
                return Ok(());
            }
            Ok(dimensions) => {
                if !dimensions.is_empty() {
                    last = dimensions;
                }
            }
            Err(error) => {
                last_error = error.clone();
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let message = format!(
        "ffprobe: {} - expected {}, got '{}'",
        spec.label,
        spec.expected,
        if last.is_empty() {
            "<no output>"
        } else {
            &last
        }
    );
    emit_mixed_result(
        env,
        spec.cfg,
        spec.id,
        "fail",
        started.elapsed(),
        Some(json!({
            "message": message,
            "label": spec.label,
            "expected": spec.expected,
            "got": last,
            "url": spec.url,
            "ffprobe_stderr": last_error,
        })),
    )?;
    Err(message)
}

async fn warm_mixed_stream(label: &str, url: &str, expected: &str, cookie: Option<&str>) {
    for _attempt in 1..=15 {
        match probe_dims_ramp_with_cookie(url, cookie).await {
            Ok(dimensions) if dimensions == expected => {
                println!("  warmup: {label} -> {dimensions}");
                return;
            }
            Ok(_) | Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!(
        "    warmup: {label} did not reach {expected}; lifecycle will report if stop state is unhealthy"
    );
}

#[allow(clippy::too_many_arguments)]
async fn verify_mixed_audio_route(
    env: &MixedEnv,
    cfg: &str,
    id: &str,
    label: &str,
    url: &str,
    expected_dimensions: &str,
    expected_audio_tracks: usize,
    resume: &mut MixedResume,
) -> Result<(), String> {
    if !resume.allows(id) {
        return Ok(());
    }
    let started = Instant::now();
    let mut last_dimensions = String::new();
    let mut last_audio_tracks = None;
    let mut last_error = String::new();
    for _attempt in 1..=15 {
        match ffprobe(url).await {
            Ok(probe) => {
                let dimensions = video_dimensions(&probe).unwrap_or_default();
                let audio_tracks = probe_audio_track_count(&probe);
                if dimensions == expected_dimensions && audio_tracks == expected_audio_tracks {
                    emit_mixed_result(
                        env,
                        cfg,
                        id,
                        "pass",
                        started.elapsed(),
                        Some(json!({
                            "label": label,
                            "expected": expected_dimensions,
                            "got": dimensions,
                            "expected_audio_tracks": expected_audio_tracks,
                            "audio_tracks": audio_tracks,
                            "url": url,
                        })),
                    )?;
                    log_mixed_ok(
                        env,
                        &format!("{label}: {dimensions}, audio_tracks={audio_tracks}"),
                    )?;
                    return Ok(());
                }
                if !dimensions.is_empty() {
                    last_dimensions = dimensions;
                }
                last_audio_tracks = Some(audio_tracks);
            }
            Err(error) => {
                last_error = error.clone();
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let message = format!(
        "{label}: expected {expected_dimensions} with {expected_audio_tracks} audio tracks, got '{}' with {} audio tracks",
        if last_dimensions.is_empty() {
            "<no video>"
        } else {
            &last_dimensions
        },
        last_audio_tracks
            .map(|count| count.to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    );
    emit_mixed_result(
        env,
        cfg,
        id,
        "fail",
        started.elapsed(),
        Some(json!({
            "message": message,
            "label": label,
            "expected": expected_dimensions,
            "got": last_dimensions,
            "expected_audio_tracks": expected_audio_tracks,
            "audio_tracks": last_audio_tracks,
            "url": url,
            "ffprobe_stderr": last_error,
        })),
    )?;
    Err(message)
}

async fn stop_mixed_outputs(api: &RampApi, pipeline_id: &str, output_ids: &[String]) {
    for output_id in output_ids {
        let _ = api
            .post_json(
                &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
                Value::Null,
            )
            .await;
    }
}

async fn wait_for_outputs_stopped(
    api: &RampApi,
    pipeline_id: &str,
    output_ids: &[String],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let config = api.get_json("/api/v1/settings").await?;
        let all_stopped = output_ids.iter().all(|output_id| {
            config["jobs"]
                .as_array()
                .and_then(|jobs| {
                    jobs.iter().find(|job| {
                        job["pipelineId"] == pipeline_id && job["outputId"] == output_id.as_str()
                    })
                })
                .and_then(|job| job["status"].as_str())
                .is_none_or(|status| status == "stopped")
        });
        if all_stopped {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("lifecycle: outputs did not all stop within 60 s".to_string());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn emit_mixed_result(
    env: &MixedEnv,
    cfg: &str,
    id: &str,
    status: &str,
    elapsed: Duration,
    extra: Option<Value>,
) -> Result<(), String> {
    let Some(path) = &env.assertion_log else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), json!(id));
    object.insert("mode".to_string(), json!("mixed-scale"));
    object.insert("config".to_string(), json!(cfg));
    object.insert("status".to_string(), json!(status));
    object.insert("ms".to_string(), json!(elapsed.as_millis()));
    if let Some(Value::Object(extra)) = extra {
        object.extend(extra);
    }
    append_line(path, &format!("{}\n", Value::Object(object))).map_err(|e| e.to_string())
}

fn log_mixed_ok(env: &MixedEnv, message: &str) -> Result<(), String> {
    append_line(&env.summary_log, &format!("ok: {message}\n"))
}

fn effective_log_paths(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return vec![path.to_path_buf()];
    };
    let logs_dir = parent.join("logs");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&logs_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("restream.log"))
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        vec![path.to_path_buf()]
    } else {
        entries
    }
}

fn count_log_matches(path: &Path, needle: &str) -> usize {
    effective_log_paths(path)
        .into_iter()
        .filter_map(|candidate| std::fs::read_to_string(candidate).ok())
        .map(|content| content.matches(needle).count())
        .sum()
}

fn file_tail_lines(path: &Path, lines: usize) -> Vec<String> {
    let Some(target) = effective_log_paths(path).into_iter().last() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(target) else {
        return Vec::new();
    };
    let mut tail = content.lines().rev().take(lines).collect::<Vec<_>>();
    tail.reverse();
    tail.into_iter().map(str::to_string).collect()
}

async fn correctness() -> Result<Value, String> {
    let work_dir = artifact_path("correctness");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let rtmp_pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "RTMP test", "streamKey": "e2e-rtmp"}),
        )
        .await?;
    let rtmp_id = rtmp_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("RTMP pipeline create missing id")?
        .to_string();

    let srt_pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "SRT test", "streamKey": "e2e-srt"}),
        )
        .await?;
    let srt_id = srt_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("SRT pipeline create missing id")?
        .to_string();
    println!("[correctness] created pipelines {rtmp_id}, {srt_id}");

    let rtmp_fixture = checked_h264_fixture()?;
    let srt_fixture = checked_h265_fixture()?;

    let rtmp_publish = format!("rtmp://127.0.0.1:{}/live/e2e-rtmp", ports.rtmp);
    let srt_publish = format!(
        "srt://127.0.0.1:{}?streamid=publish:live/e2e-srt&pkt_size=1316",
        ports.srt
    );
    let rtmp_read = rtmp_publish.clone();
    let srt_read = format!(
        "srt://127.0.0.1:{}?streamid=read:live/e2e-srt&mode=caller&transtype=live&latency=100",
        ports.srt
    );

    let mut rtmp_publisher = spawn_publisher(&rtmp_fixture, &rtmp_publish, "flv", false).await?;
    let mut srt_publisher = spawn_publisher(&srt_fixture, &srt_publish, "mpegts", true).await?;

    wait_for_api_input_live(&api, &rtmp_id, Duration::from_secs(15)).await?;
    wait_for_api_input_live(&api, &srt_id, Duration::from_secs(15)).await?;

    let health = api.get_json("/api/v1/engine/health").await?;
    let rtmp_snapshot = health["pipelines"][&rtmp_id].clone();
    let srt_snapshot = health["pipelines"][&srt_id].clone();
    if rtmp_snapshot.is_null() || srt_snapshot.is_null() {
        stop_child(&mut rtmp_publisher).await;
        stop_child(&mut srt_publisher).await;
        stop_child(&mut child).await;
        return Err("missing snapshot in /health".to_string());
    }

    let rtmp_probe = ffprobe(&rtmp_read).await?;
    let srt_probe = ffprobe(&srt_read).await?;

    assert_media_only(&rtmp_probe, "RTMP read")?;
    assert_media_only(&srt_probe, "SRT read")?;
    let rtmp_media = normalized_streams(&rtmp_probe)?;
    let srt_media = normalized_streams(&srt_probe)?;
    assert_snapshot_matches_probe(&rtmp_snapshot, &rtmp_media, "RTMP")?;
    assert_snapshot_matches_probe(&srt_snapshot, &srt_media, "SRT")?;

    stop_child(&mut rtmp_publisher).await;
    stop_child(&mut srt_publisher).await;
    stop_child(&mut child).await;

    Ok(json!({
        "passed": true,
        "rtmp": {
            "fixture": rtmp_fixture,
            "publishUrl": rtmp_publish,
            "readUrl": rtmp_read,
            "snapshot": rtmp_snapshot,
            "probe": rtmp_probe,
            "normalizedStreams": rtmp_media,
        },
        "srt": {
            "fixture": srt_fixture,
            "publishUrl": srt_publish,
            "readUrl": srt_read,
            "snapshot": srt_snapshot,
            "probe": srt_probe,
            "normalizedStreams": srt_media,
        },
    }))
}

async fn correctness_rtmp() -> Result<Value, String> {
    correctness_one_protocol("rtmp").await
}

async fn correctness_srt() -> Result<Value, String> {
    correctness_one_protocol("srt").await
}

/// Test: SRT H.264/AAC ingest → RTMP source egress.
///
/// Validates the direct Raw Annex-B/ADTS to RTMP FLV/AVCC/AAC packetization
/// path without involving a transcoder.
async fn srt_to_rtmp_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("correctness-srt-rtmp");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "H.264 SRT source", "streamKey": "e2e-srt-rtmp"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    // Create RTMP egress output pointed at the generalized sink
    let sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/e2e-srt-rtmp-sink");
    let output = api
        .post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs"),
            json!({"name": "rtmp-sink", "url": sink_url, "encoding": "source"}),
        )
        .await?;
    let output_id = output["output"]["id"]
        .as_str()
        .ok_or("output create missing id")?
        .to_string();

    // Start the generalized sink to receive egress
    let sink_metrics = Arc::new(GeneralizedSinkMetrics::default());
    let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink bind: {e}"))?;
    let sink_m = sink_metrics.clone();
    let sink_task = tokio::spawn(async move {
        while let Ok((socket, _)) = sink_listener.accept().await {
            let m = sink_m.clone();
            tokio::spawn(handle_generalized_sink_client(socket, m));
        }
    });

    let fixture = checked_h264_fixture()?;

    let mut publisher = spawn_publisher(
        &fixture,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/e2e-srt-rtmp&pkt_size=1316",
            ports.srt
        ),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;
    println!("[srt-rtmp] Source ingest established (H.264 via SRT)");

    // Start the output
    api.post_json(
        &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/start"),
        json!({}),
    )
    .await?;

    // Wait for sink to receive data
    let deadline = Instant::now() + Duration::from_secs(15);
    while sink_metrics.video_count.load(Ordering::Relaxed) < 10 {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let sink_summary = sink_metrics.summary();
    let dts_ok = sink_metrics.dts_monotone();
    let video_count = sink_metrics.video_count.load(Ordering::Relaxed);
    let audio_count = sink_metrics.audio_count.load(Ordering::Relaxed);

    stop_child(&mut publisher).await;
    sink_task.abort();
    stop_child(&mut child).await;

    let passed = video_count > 0 && audio_count > 0 && dts_ok;
    let results = json!({
        "passed": passed,
        "dtsMonotone": dts_ok,
        "videoCount": video_count,
        "audioCount": audio_count,
        "sink": sink_summary,
    });

    let path = work_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    if passed {
        Ok(results)
    } else {
        Err(format!("SRT to RTMP direct egress failed: {results}"))
    }
}

fn srt_publish_url(port: u16, stream_key: &str, crypto: Option<(&str, u32)>) -> String {
    let mut url =
        format!("srt://127.0.0.1:{port}?streamid=publish:live/{stream_key}&pkt_size=1316");
    if let Some((passphrase, pbkeylen)) = crypto {
        url.push_str(&format!("&passphrase={passphrase}&pbkeylen={pbkeylen}"));
    }
    url
}

fn srt_read_url(port: u16, stream_key: &str, crypto: Option<(&str, u32)>) -> String {
    let mut url = format!(
        "srt://127.0.0.1:{port}?streamid=read:live/{stream_key}&mode=caller&transtype=live&latency=100"
    );
    if let Some((passphrase, pbkeylen)) = crypto {
        url.push_str(&format!("&passphrase={passphrase}&pbkeylen={pbkeylen}"));
    }
    url
}

async fn expect_ingest_rejected(
    api: &RampApi,
    pipeline_id: &str,
    fixture: &Path,
    publish_url: &str,
    label: &str,
) -> Result<Value, String> {
    let mut publisher = spawn_publisher(fixture, publish_url, "mpegts", true).await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let live = wait_for_api_input_live(api, pipeline_id, Duration::from_secs(1))
        .await
        .is_ok();
    stop_child(&mut publisher).await;
    if live {
        return Err(format!("{label}: ingest unexpectedly went live"));
    }
    wait_for_api_input_off(api, pipeline_id, Duration::from_secs(5)).await?;
    Ok(json!({"passed": true, "label": label}))
}

async fn expect_srt_read_failure(url: &str, label: &str) -> Result<Value, String> {
    match ffprobe(url).await {
        Ok(probe) => Err(format!("{label}: read unexpectedly succeeded: {probe}")),
        Err(error) => Ok(json!({"passed": true, "label": label, "error": error})),
    }
}

async fn srt_policy_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("correctness-srt-policy");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let fixture = checked_h264_fixture()?;

    let mut results = serde_json::Map::new();

    api.patch_json(
        "/api/v1/settings",
        json!({"srtIngest": {"mode": "plaintext", "pbkeylen": 16, "passphrase": null}}),
    )
    .await?;
    let plain_inherit = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "policy-plain-inherit", "streamKey": "policy-plain-inherit", "srtIngestPolicy": {"mode": "inherit"}}),
        )
        .await?;
    let plain_inherit_id = plain_inherit["pipeline"]["id"]
        .as_str()
        .ok_or("plain inherit pipeline id missing")?
        .to_string();
    let mut plain_pub = spawn_publisher(
        &fixture,
        &srt_publish_url(ports.srt, "policy-plain-inherit", None),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(&api, &plain_inherit_id, Duration::from_secs(15)).await?;
    let plain_read_probe = ffprobe(&srt_read_url(ports.srt, "policy-plain-inherit", None)).await?;
    assert_media_only(&plain_read_probe, "plain inherit read")?;
    stop_child(&mut plain_pub).await;
    wait_for_api_input_off(&api, &plain_inherit_id, Duration::from_secs(10)).await?;
    results.insert(
        "globalPlaintextInherit".to_string(),
        json!({"passed": true, "readProbe": plain_read_probe}),
    );

    api.patch_json(
        "/api/v1/settings",
        json!({"srtIngest": {"mode": "encrypted", "passphrase": "globalpass123", "pbkeylen": 16}}),
    )
    .await?;
    let global_enc = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "policy-global-enc", "streamKey": "policy-global-enc", "srtIngestPolicy": {"mode": "inherit"}}),
        )
        .await?;
    let global_enc_id = global_enc["pipeline"]["id"]
        .as_str()
        .ok_or("global enc pipeline id missing")?
        .to_string();
    let mut global_enc_pub = spawn_publisher(
        &fixture,
        &srt_publish_url(ports.srt, "policy-global-enc", Some(("globalpass123", 16))),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(&api, &global_enc_id, Duration::from_secs(15)).await?;
    let global_enc_read = ffprobe(&srt_read_url(
        ports.srt,
        "policy-global-enc",
        Some(("globalpass123", 16)),
    ))
    .await?;
    assert_media_only(&global_enc_read, "global encrypted read")?;
    let global_enc_read_fail = expect_srt_read_failure(
        &srt_read_url(ports.srt, "policy-global-enc", None),
        "global encrypted plaintext read",
    )
    .await?;
    stop_child(&mut global_enc_pub).await;
    wait_for_api_input_off(&api, &global_enc_id, Duration::from_secs(10)).await?;
    let global_enc_publish_fail = expect_ingest_rejected(
        &api,
        &global_enc_id,
        &fixture,
        &srt_publish_url(ports.srt, "policy-global-enc", None),
        "global encrypted plaintext publish",
    )
    .await?;
    results.insert(
        "globalEncrypted16Inherit".to_string(),
        json!({
            "passed": true,
            "readProbe": global_enc_read,
            "plaintextReadRejected": global_enc_read_fail,
            "plaintextPublishRejected": global_enc_publish_fail,
        }),
    );

    let plain_override = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "policy-plain-override", "streamKey": "policy-plain-override", "srtIngestPolicy": {"mode": "plaintext"}}),
        )
        .await?;
    let plain_override_id = plain_override["pipeline"]["id"]
        .as_str()
        .ok_or("plain override pipeline id missing")?
        .to_string();
    let mut plain_override_pub = spawn_publisher(
        &fixture,
        &srt_publish_url(ports.srt, "policy-plain-override", None),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(&api, &plain_override_id, Duration::from_secs(15)).await?;
    let plain_override_read =
        ffprobe(&srt_read_url(ports.srt, "policy-plain-override", None)).await?;
    assert_media_only(&plain_override_read, "plain override read")?;
    stop_child(&mut plain_override_pub).await;
    wait_for_api_input_off(&api, &plain_override_id, Duration::from_secs(10)).await?;
    results.insert(
        "globalEncrypted16PipelinePlaintext".to_string(),
        json!({"passed": true, "readProbe": plain_override_read}),
    );

    api.patch_json(
        "/api/v1/settings",
        json!({"srtIngest": {"mode": "plaintext", "pbkeylen": 16, "passphrase": null}}),
    )
    .await?;
    for (label, stream_key, passphrase, pbkeylen) in [
        (
            "pipelineEncrypted24",
            "policy-enc-24",
            "pipepass1234",
            24u32,
        ),
        (
            "pipelineEncrypted32",
            "policy-enc-32",
            "pipepass12345",
            32u32,
        ),
    ] {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({
                    "name": label,
                    "streamKey": stream_key,
                    "srtIngestPolicy": {"mode": "encrypted", "passphrase": passphrase, "pbkeylen": pbkeylen}
                }),
            )
            .await?;
        let pipeline_id = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("encrypted override pipeline id missing")?
            .to_string();
        let mut pub_ok = spawn_publisher(
            &fixture,
            &srt_publish_url(ports.srt, stream_key, Some((passphrase, pbkeylen))),
            "mpegts",
            true,
        )
        .await?;
        wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;
        let read_ok = ffprobe(&srt_read_url(
            ports.srt,
            stream_key,
            Some((passphrase, pbkeylen)),
        ))
        .await?;
        assert_media_only(&read_ok, label)?;
        let read_plain_fail = expect_srt_read_failure(
            &srt_read_url(ports.srt, stream_key, None),
            &format!("{label} plaintext read"),
        )
        .await?;
        let read_wrong_pass_fail = expect_srt_read_failure(
            &srt_read_url(ports.srt, stream_key, Some(("wrongpass123", pbkeylen))),
            &format!("{label} wrong passphrase read"),
        )
        .await?;
        stop_child(&mut pub_ok).await;
        wait_for_api_input_off(&api, &pipeline_id, Duration::from_secs(10)).await?;
        let publish_plain_fail = expect_ingest_rejected(
            &api,
            &pipeline_id,
            &fixture,
            &srt_publish_url(ports.srt, stream_key, None),
            &format!("{label} plaintext publish"),
        )
        .await?;
        results.insert(
            label.to_string(),
            json!({
                "passed": true,
                "readProbe": read_ok,
                "plaintextReadRejected": read_plain_fail,
                "wrongPassphraseReadRejected": read_wrong_pass_fail,
                "plaintextPublishRejected": publish_plain_fail,
            }),
        );
    }

    stop_child(&mut child).await;
    let value = Value::Object(results);
    let path = work_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
    Ok(value)
}

/// Test: SRT ingest -> HLS HTTP PUT upload for YouTube-style and path-style sinks.
struct HlsPutArtifacts {
    youtube_playlist: PathBuf,
    youtube_segment: PathBuf,
}

struct HlsPutSinkState {
    root: PathBuf,
    requests_path: PathBuf,
    write_lock: Mutex<()>,
}

#[derive(Clone)]
struct HlsPutHangSinkState {
    cancel: CancellationToken,
    delay: Duration,
}

async fn start_hls_put_sink(
    port: u16,
    root: PathBuf,
) -> Result<(CancellationToken, tokio::task::JoinHandle<()>), String> {
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let state = Arc::new(HlsPutSinkState {
        requests_path: root.join("requests.jsonl"),
        root,
        write_lock: Mutex::new(()),
    });
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/*path", put(hls_put_sink_put))
        .layer(DefaultBodyLimit::disable())
        .with_state(state);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(server_cancel.cancelled_owned())
            .await
        {
            eprintln!("[hls-put-sink] server failed: {err}");
        }
    });
    Ok((cancel, handle))
}

async fn start_hls_put_hang_sink(
    port: u16,
    delay: Duration,
) -> Result<(CancellationToken, tokio::task::JoinHandle<()>), String> {
    let cancel = CancellationToken::new();
    let state = HlsPutHangSinkState {
        cancel: cancel.clone(),
        delay,
    };
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/*path",
            put(
                |State(state): State<HlsPutHangSinkState>,
                 OriginalUri(_uri): OriginalUri,
                 _headers: HeaderMap,
                 _body: Bytes| async move {
                    tokio::select! {
                        _ = state.cancel.cancelled() => StatusCode::SERVICE_UNAVAILABLE,
                        _ = tokio::time::sleep(state.delay) => StatusCode::NO_CONTENT,
                    }
                },
            ),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(state);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let server_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(server_cancel.cancelled_owned())
            .await
        {
            eprintln!("[hls-put-hang-sink] server failed: {err}");
        }
    });
    Ok((cancel, handle))
}

async fn hls_put_sink_put(
    State(state): State<Arc<HlsPutSinkState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let name =
        hls_put_sink_file_name(uri.path(), uri.query()).unwrap_or_else(|| "index.m3u8".to_string());
    let name = name.replace('\\', "/").trim_start_matches('/').to_string();
    if name.is_empty() || name.split('/').any(|part| part == "..") {
        return StatusCode::BAD_REQUEST;
    }

    let target = state.root.join(&name);
    if let Some(parent) = target.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "[hls-put-sink] failed to create {}: {err}",
            parent.display()
        );
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    if let Err(err) = std::fs::write(&target, &body) {
        eprintln!("[hls-put-sink] failed to write {}: {err}", target.display());
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE.as_str())
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let record = json!({
        "path": uri.to_string(),
        "file": name,
        "contentType": content_type,
        "bytes": body.len(),
    });
    let _guard = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.requests_path)
    {
        Ok(mut file) => {
            if let Err(err) = writeln!(file, "{record}") {
                eprintln!(
                    "[hls-put-sink] failed to append {}: {err}",
                    state.requests_path.display()
                );
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
        Err(err) => {
            eprintln!(
                "[hls-put-sink] failed to open {}: {err}",
                state.requests_path.display()
            );
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    StatusCode::NO_CONTENT
}

fn hls_put_sink_file_name(path: &str, query: Option<&str>) -> Option<String> {
    query
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "file").then(|| value.to_string())
            })
        })
        .or_else(|| {
            let trimmed = path.trim_start_matches('/');
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
}

async fn wait_for_hls_put_artifacts(
    sink_dir: &Path,
    timeout: Duration,
) -> Result<HlsPutArtifacts, String> {
    let deadline = Instant::now() + timeout;
    let youtube_playlist = sink_dir.join("out.m3u8");
    loop {
        let youtube_segment = first_segment_in(sink_dir);
        if youtube_playlist.is_file()
            && file_nonempty(&youtube_playlist)
            && let Some(youtube_segment) = youtube_segment
        {
            return Ok(HlsPutArtifacts {
                youtube_playlist,
                youtube_segment,
            });
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for HLS PUT playlist/segment artifacts in {}",
                sink_dir.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn first_segment_in(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| is_segment_file(name, "seg"))
                && file_nonempty(path)
        })
        .collect();
    entries.sort();
    entries.into_iter().next()
}

fn file_nonempty(path: &Path) -> bool {
    path.metadata().map(|meta| meta.len() > 0).unwrap_or(false)
}

fn validate_hls_playlist(path: &Path, label: &str) -> Result<(), String> {
    let playlist = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if !playlist.contains("#EXTM3U") {
        return Err(format!("{label} HLS PUT playlist missing EXTM3U header"));
    }
    if !playlist.contains(".ts") {
        return Err(format!(
            "{label} HLS PUT playlist missing segment reference"
        ));
    }
    Ok(())
}

fn read_hls_put_requests(sink_dir: &Path) -> Result<Vec<Value>, String> {
    let path = sink_dir.join("requests.jsonl");
    let body = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|e| e.to_string()))
        .collect()
}

fn request_seen(requests: &[Value], predicate: impl Fn(&Value) -> bool) -> bool {
    requests.iter().any(predicate)
}

fn is_segment_file(file: &str, prefix: &str) -> bool {
    file.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(".ts"))
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

fn probe_audio_track_count(probe: &Value) -> usize {
    probe["streams"]
        .as_array()
        .map(|streams| {
            streams
                .iter()
                .filter(|s| s["codec_type"] == "audio")
                .count()
        })
        .unwrap_or(0)
}

fn video_dimensions(probe: &Value) -> Option<String> {
    let stream = probe["streams"]
        .as_array()?
        .iter()
        .find(|stream| stream["codec_type"] == "video")?;
    Some(format!(
        "{}x{}",
        stream["width"].as_i64()?,
        stream["height"].as_i64()?
    ))
}

fn graph_ring_readers(graph: &Value) -> Vec<Value> {
    graph["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node["type"] == "ring_buffer")
        .flat_map(|node| {
            node["details"]["readers"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn graph_active_node_count(graph: &Value, node_type: &str) -> usize {
    graph["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node["type"] == node_type && node["active"].as_bool().unwrap_or(false))
        .count()
}

/// Test: RTMP B-frame ingest -> RTMP egress timestamp round-trip.
///
/// Publishes B-frame H.264/AAC over RTMP, sends egress to the generalized
/// harness sink, and verifies ffprobe observes composition offsets (PTS > DTS)
/// while DTS stays monotone.
async fn bframe_rtmp_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("bframe-rtmp");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "B-frame RTMP source", "streamKey": "e2e-bframe-src"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    // Create RTMP egress output pointed at the harness sink
    let sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/e2e-bframe-sink");
    let output = api
        .post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs"),
            json!({"name": "bframe-sink", "url": sink_url, "encoding": "source"}),
        )
        .await?;
    let output_id = output["output"]["id"]
        .as_str()
        .ok_or("output create missing id")?
        .to_string();

    // Start generalized sink
    let sink_metrics = Arc::new(GeneralizedSinkMetrics::default());
    let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink bind: {e}"))?;
    let sink_m = sink_metrics.clone();
    let sink_task = tokio::spawn(async move {
        while let Ok((socket, _)) = sink_listener.accept().await {
            let m = sink_m.clone();
            tokio::spawn(handle_generalized_sink_client(socket, m));
        }
    });

    let fixture = checked_h264_fixture()?;

    let mut publisher = spawn_publisher(
        &fixture,
        &format!("rtmp://127.0.0.1:{}/live/e2e-bframe-src", ports.rtmp),
        "flv",
        false,
    )
    .await?;
    wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;
    println!("[bframe-rtmp] Source ingest established");

    // Start the output
    api.post_json(
        &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/start"),
        json!({}),
    )
    .await?;

    // Wait for sink to accumulate packets
    let deadline = Instant::now() + Duration::from_secs(15);
    while sink_metrics.video_count.load(Ordering::Relaxed) < 30 {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Also probe via ffprobe for B-frame packet-level analysis
    let packets_path = work_dir.join("bframe-packets.json");
    let read_url = format!("rtmp://127.0.0.1:{}/live/e2e-bframe-src", ports.rtmp);
    let packet_probe = ffprobe_video_packets(&read_url, &packets_path).await?;
    let packet_count = count_video_packets(&packet_probe);
    let bframe_count = count_bframe_packets(&packet_probe);
    let ffprobe_dts_monotone = video_dts_monotone(&packet_probe);

    let sink_dts_monotone = sink_metrics.dts_monotone();
    let video_count = sink_metrics.video_count.load(Ordering::Relaxed);
    let sink_summary = sink_metrics.summary();

    stop_child(&mut publisher).await;
    sink_task.abort();
    stop_child(&mut child).await;

    let passed =
        packet_count >= 30 && bframe_count > 0 && ffprobe_dts_monotone && sink_dts_monotone;

    let mut results = json!({
        "passed": passed,
        "packetCount": packet_count,
        "bframeCount": bframe_count,
        "ffprobeDtsMonotone": ffprobe_dts_monotone,
        "sinkDtsMonotone": sink_dts_monotone,
        "sinkVideoCount": video_count,
        "sink": sink_summary,
    });
    if packet_count < 30 {
        results["error"] = json!(format!(
            "expected at least 30 video packets, got {packet_count}"
        ));
    } else if bframe_count == 0 {
        results["error"] = json!("RTMP egress did not expose any packets with PTS > DTS");
    } else if !ffprobe_dts_monotone || !sink_dts_monotone {
        results["error"] = json!("RTMP egress DTS values are not monotone");
    }

    let path = work_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    if passed {
        Ok(results)
    } else {
        Err(format!("RTMP B-frame round-trip failed: {results}"))
    }
}

async fn correctness_one_protocol(protocol: &str) -> Result<Value, String> {
    let work_dir = artifact_path(&format!("correctness-{protocol}"));
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let stream_key = format!("e2e-{protocol}");
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": format!("{protocol} test"), "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();
    println!("[correctness-{protocol}] created pipeline {pipeline_id}");

    let fixture = if protocol == "rtmp" {
        checked_h264_fixture()?
    } else {
        checked_h265_fixture()?
    };
    let (publish_url, read_url, format) = if protocol == "rtmp" {
        (
            format!("rtmp://127.0.0.1:{}/live/{stream_key}", ports.rtmp),
            format!("rtmp://127.0.0.1:{}/live/{stream_key}", ports.rtmp),
            "flv",
        )
    } else {
        (
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&pkt_size=1316",
                ports.srt
            ),
            format!(
                "srt://127.0.0.1:{}?streamid=read:live/{stream_key}&mode=caller&transtype=live&latency=100",
                ports.srt
            ),
            "mpegts",
        )
    };
    let map_all = protocol == "srt";
    let mut publisher = spawn_publisher(&fixture, &publish_url, format, map_all).await?;
    wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;

    let snapshot =
        match wait_for_api_input_media_ready(&api, &pipeline_id, Duration::from_secs(15)).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                stop_child(&mut publisher).await;
                stop_child(&mut child).await;
                return Err(err);
            }
        };

    let probe = ffprobe(&read_url).await?;
    assert_media_only(&probe, &format!("{protocol} read"))?;
    let normalized = normalized_streams(&probe)?;
    assert_snapshot_matches_probe(&snapshot, &normalized, protocol)?;

    stop_child(&mut publisher).await;
    stop_child(&mut child).await;

    Ok(json!({
        "passed": true,
        "protocol": protocol,
        "publishUrl": publish_url,
        "readUrl": read_url,
        "snapshot": snapshot,
        "probe": probe,
        "normalizedStreams": normalized,
    }))
}

async fn egress_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("egress");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "Egress source", "streamKey": "e2e-src"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    let fixture = checked_h264_fixture()?;

    let mut publisher = spawn_publisher(
        &fixture,
        &format!("rtmp://127.0.0.1:{}/live/e2e-src", ports.rtmp),
        "flv",
        false,
    )
    .await?;
    wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;
    println!("[egress] Source ingest established");

    // RTMP egress — use generalized sink for DTS/packet assertions
    let rtmp_sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/e2e-rtmp-sink");
    let rtmp_output_id =
        create_mixed_output(&api, &pipeline_id, "rtmp-egress", &rtmp_sink_url, "source").await?;

    let sink_metrics = Arc::new(GeneralizedSinkMetrics::default());
    let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink bind: {e}"))?;
    let sink_m = sink_metrics.clone();
    let sink_task = tokio::spawn(async move {
        while let Ok((socket, _)) = sink_listener.accept().await {
            let m = sink_m.clone();
            tokio::spawn(handle_generalized_sink_client(socket, m));
        }
    });

    start_mixed_output(&api, &pipeline_id, &rtmp_output_id).await?;

    // SRT egress — create a second output to a real SRT listener pipeline
    let srt_egress_url = format!(
        "srt://127.0.0.1:{}?streamid=publish:live/e2e-srt-sink&pkt_size=1316",
        ports.srt
    );
    let srt_pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "SRT egress sink", "streamKey": "e2e-srt-sink"}),
        )
        .await?;
    let srt_pipeline_id = srt_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("SRT pipeline create missing id")?
        .to_string();
    let srt_output_id =
        create_mixed_output(&api, &pipeline_id, "srt-egress", &srt_egress_url, "source").await?;
    start_mixed_output(&api, &pipeline_id, &srt_output_id).await?;

    // Wait for sink to receive enough data
    let deadline = Instant::now() + Duration::from_secs(15);
    while sink_metrics.video_count.load(Ordering::Relaxed) < 30 {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut results = json!({});

    // RTMP egress validation via sink
    let dts_ok = sink_metrics.dts_monotone();
    let video_count = sink_metrics.video_count.load(Ordering::Relaxed);
    let audio_count = sink_metrics.audio_count.load(Ordering::Relaxed);
    let rtmp_passed = video_count >= 30 && audio_count > 0 && dts_ok;
    results["rtmpEgress"] = json!({
        "passed": rtmp_passed,
        "dtsMonotone": dts_ok,
        "videoCount": video_count,
        "audioCount": audio_count,
        "sink": sink_metrics.summary(),
    });

    // SRT egress validation — check the sink pipeline received ingest
    let srt_health = api
        .get_json("/api/v1/engine/health")
        .await
        .unwrap_or(json!({}));
    let srt_has_input = srt_health["pipelines"]
        .as_array()
        .and_then(|pipes| {
            pipes
                .iter()
                .find(|p| p["id"].as_str() == Some(&srt_pipeline_id))
        })
        .and_then(|p| p["activeIngest"].as_bool())
        .unwrap_or(false);
    results["srtEgress"] = json!({
        "passed": srt_has_input,
        "srtSinkPipelineId": srt_pipeline_id,
        "activeIngest": srt_has_input,
    });

    // Recording via API
    let media_dir = work_dir.join("media");
    std::fs::create_dir_all(&media_dir).map_err(|e| e.to_string())?;
    api.post_json(
        &format!("/api/v1/pipelines/{pipeline_id}/recording/start"),
        json!({}),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(6)).await;
    api.post_json(
        &format!("/api/v1/pipelines/{pipeline_id}/recording/stop"),
        json!({}),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Find recording files in the child's media directory
    let rec_dir = work_dir.join("media");
    let mut rec_file: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&rec_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "ts") {
                rec_file = Some(path);
                break;
            }
        }
    }
    let recording_result = match rec_file {
        Some(ref path) => {
            let output = tokio::time::timeout(
                Duration::from_secs(12),
                Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-probesize",
                        "2M",
                        "-analyzeduration",
                        "2M",
                        "-show_entries",
                        "stream=index,codec_name,codec_type",
                        "-of",
                        "json",
                        path.to_string_lossy().as_ref(),
                    ])
                    .output(),
            )
            .await;
            match output {
                Ok(Ok(out)) if out.status.success() => {
                    match serde_json::from_slice::<Value>(&out.stdout) {
                        Ok(probe) => {
                            if let Some(streams) = probe["streams"].as_array() {
                                let has_video = streams.iter().any(|s| s["codec_type"] == "video");
                                let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
                                if has_video && has_audio {
                                    json!({"passed": true, "file": path.to_string_lossy(), "streamCount": streams.len()})
                                } else {
                                    json!({"passed": false, "error": format!("missing streams: video={has_video} audio={has_audio}")})
                                }
                            } else {
                                json!({"passed": false, "error": "no streams in ffprobe output"})
                            }
                        }
                        Err(e) => {
                            json!({"passed": false, "error": format!("ffprobe parse failed: {e}")})
                        }
                    }
                }
                Ok(Ok(out)) => {
                    json!({"passed": false, "error": format!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr))})
                }
                Ok(Err(e)) => json!({"passed": false, "error": format!("ffprobe error: {e}")}),
                Err(_) => json!({"passed": false, "error": "ffprobe timed out"}),
            }
        }
        None => json!({"passed": false, "error": "recording file not found in media dir"}),
    };
    results["recording"] = recording_result;

    let passed = results["rtmpEgress"]["passed"].as_bool().unwrap_or(false)
        && results["srtEgress"]["passed"].as_bool().unwrap_or(false)
        && results["recording"]["passed"].as_bool().unwrap_or(false);
    results["passed"] = json!(passed);

    stop_child(&mut publisher).await;
    sink_task.abort();
    stop_child(&mut child).await;

    let path = work_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());

    if passed {
        Ok(results)
    } else {
        Err(format!("egress correctness failed: {results}"))
    }
}

/// Test: SRT ingest of H.265 → RTMP egress with inline H.265→H.264 transcoding.
///
/// Validates that the RTMP output stream contains valid H.264 video + AAC audio
/// (proving the transcoder works correctly end-to-end). Uses the generalized sink
/// for DTS assertions and ffprobe for codec identity.
async fn hevc_rtmp_egress_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("correctness-hevc-rtmp");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "H.265 SRT source", "streamKey": "e2e-hevc"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    let fixture = checked_h265_fixture()?;

    let mut publisher = spawn_publisher(
        &fixture,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/e2e-hevc&pkt_size=1316",
            ports.srt
        ),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;
    println!("[hevc-rtmp] Source ingest established (H.265 via SRT)");

    // Verify source is HEVC via /probe
    let probe = api
        .get_json(&format!("/api/v1/pipelines/{pipeline_id}/probe"))
        .await
        .unwrap_or(json!({}));
    let source_codec = probe["video"]["codec"].as_str().unwrap_or("unknown");
    if source_codec != "hevc" {
        stop_child(&mut publisher).await;
        stop_child(&mut child).await;
        return Err(format!("source codec is {source_codec}, expected hevc"));
    }

    // Create RTMP output with transcoding (encoding=h264 triggers transcode)
    let sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/e2e-hevc-sink");
    let output_id = create_mixed_output(&api, &pipeline_id, "hevc-rtmp", &sink_url, "h264").await?;

    let sink_metrics = Arc::new(GeneralizedSinkMetrics::default());
    let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink bind: {e}"))?;
    let sink_m = sink_metrics.clone();
    let sink_task = tokio::spawn(async move {
        while let Ok((socket, _)) = sink_listener.accept().await {
            let m = sink_m.clone();
            tokio::spawn(handle_generalized_sink_client(socket, m));
        }
    });

    start_mixed_output(&api, &pipeline_id, &output_id).await?;

    let deadline = Instant::now() + Duration::from_secs(20);
    while sink_metrics.video_count.load(Ordering::Relaxed) < 30 {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let dts_ok = sink_metrics.dts_monotone();
    let video_count = sink_metrics.video_count.load(Ordering::Relaxed);
    let audio_count = sink_metrics.audio_count.load(Ordering::Relaxed);
    let sink_summary = sink_metrics.summary();

    let detected_video = sink_metrics.video_codec.lock().unwrap().clone();
    let detected_audio = sink_metrics.audio_codec.lock().unwrap().clone();
    let video_h264 = detected_video.as_deref() == Some("h264");
    let audio_aac = detected_audio.as_deref() == Some("aac");

    stop_child(&mut publisher).await;
    sink_task.abort();
    stop_child(&mut child).await;

    let passed = video_count >= 30 && audio_count > 0 && dts_ok && video_h264 && audio_aac;
    let results = json!({
        "passed": passed,
        "dtsMonotone": dts_ok,
        "videoCount": video_count,
        "audioCount": audio_count,
        "videoCodec": detected_video,
        "audioCodec": detected_audio,
        "sink": sink_summary,
    });

    let path = work_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());

    if passed {
        Ok(results)
    } else {
        Err(format!("HEVC RTMP egress failed: {results}"))
    }
}

/// Test: SRT ingest of H.265 → SRT egress passthrough.
///
/// Validates that native SRT egress preserves HEVC video identity while carrying
/// AAC audio, so the H.265 path is not silently mislabeled or transcoded.
async fn hevc_srt_passthrough_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("correctness-hevc-srt");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    // Source pipeline
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "H.265 SRT source", "streamKey": "e2e-hevc-srt"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    // Sink pipeline (SRT egress will publish here)
    let sink_pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "H.265 SRT passthrough sink", "streamKey": "e2e-hevc-srt-sink"}),
        )
        .await?;
    let sink_pipeline_id = sink_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("sink pipeline create missing id")?
        .to_string();

    let fixture = checked_h265_fixture()?;

    let mut publisher = spawn_publisher(
        &fixture,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/e2e-hevc-srt&pkt_size=1316",
            ports.srt
        ),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(&api, &pipeline_id, Duration::from_secs(15)).await?;
    println!("[hevc-srt] Source ingest established (H.265 via SRT)");

    // Create SRT egress output (passthrough — encoding=source)
    let srt_sink_url = format!(
        "srt://127.0.0.1:{}?streamid=publish:live/e2e-hevc-srt-sink&pkt_size=1316",
        ports.srt
    );
    let output_id = create_mixed_output(
        &api,
        &pipeline_id,
        "srt-passthrough",
        &srt_sink_url,
        "source",
    )
    .await?;
    start_mixed_output(&api, &pipeline_id, &output_id).await?;

    // Wait for the egress to publish to the sink pipeline
    wait_for_api_input_live(&api, &sink_pipeline_id, Duration::from_secs(15)).await?;
    println!("[hevc-srt] Sink ingest established (H.265 via SRT egress)");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ffprobe the SRT read URL to verify codec identity
    let srt_read_url = format!(
        "srt://127.0.0.1:{}?streamid=read:live/e2e-hevc-srt-sink&mode=caller&transtype=live&latency=100",
        ports.srt
    );
    let probe = ffprobe(&srt_read_url).await?;
    let media_check = assert_media_only(&probe, "HEVC SRT passthrough");
    let streams = normalized_streams(&probe).ok();
    let video_hevc = probe["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "video"))
        .and_then(|s| s["codec_name"].as_str())
        .is_some_and(|codec| codec == "hevc" || codec == "h265");
    let audio_aac = probe["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "audio"))
        .and_then(|s| s["codec_name"].as_str())
        == Some("aac");

    stop_child(&mut publisher).await;
    stop_child(&mut child).await;

    let passed = media_check.is_ok() && video_hevc && audio_aac;
    let mut results = json!({
        "passed": passed,
        "videoCodec": if video_hevc { "hevc" } else { "NOT_hevc" },
        "audioCodec": if audio_aac { "aac" } else { "NOT_aac" },
        "mediaCheck": media_check.is_ok(),
        "mediaError": media_check.err(),
        "probe": probe,
    });
    if let Some(s) = streams {
        results["streams"] = s;
    }
    if !video_hevc {
        results["error"] = json!("SRT output video codec is not HEVC — passthrough failed");
    }

    let path = work_dir.join("results.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&results).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&results).unwrap());

    if passed {
        Ok(results)
    } else {
        Err(format!("HEVC SRT passthrough failed: {results}"))
    }
}

#[derive(Clone, Copy)]
enum PublishTrackSelection {
    PrimaryAv,
    AllStreams,
}

impl PublishTrackSelection {
    fn needs_all_streams(self) -> bool {
        matches!(self, Self::AllStreams)
    }
}

fn sweep_fixture(config: SweepConfig, bitrate_label: &str) -> Result<PathBuf, String> {
    restream::test_fixtures::bench_transport_fixture(
        config.video_codec,
        bitrate_label,
        config.multi_audio,
    )
}

fn ramp_fixture() -> Result<PathBuf, String> {
    restream::test_fixtures::bench_transport_fixture("h264", "4M", false)
}

fn checked_h264_fixture() -> Result<PathBuf, String> {
    restream::test_fixtures::canonical_h264_ts_fixture()
}

fn checked_h265_fixture() -> Result<PathBuf, String> {
    restream::test_fixtures::canonical_h265_ts_fixture()
}

fn spawn_publisher_with_selection(
    path: &Path,
    url: &str,
    format: &str,
    selection: PublishTrackSelection,
    log_path: Option<&Path>,
) -> Result<Child, String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-stream_loop",
        "-1",
        "-i",
    ]);
    cmd.arg(path);
    if selection.needs_all_streams() {
        cmd.args(["-map", "0"]);
    } else {
        cmd.args(["-map", "0:v", "-map", "0:a:0"]);
    }
    cmd.args(["-c", "copy", "-f", format]).arg(url);
    if let Some(log_path) = log_path {
        let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
        let stderr = log.try_clone().map_err(|e| e.to_string())?;
        cmd.stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
    } else {
        // stderr must not be piped without a consumer — the 64KB pipe buffer
        // fills and blocks ffmpeg, hanging the test. Discard it when a fixture
        // publisher does not need a dedicated log file.
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
    }
    cmd.spawn().map_err(|e| e.to_string())
}

async fn spawn_publisher(
    path: &Path,
    url: &str,
    format: &str,
    map_all: bool,
) -> Result<Child, String> {
    spawn_publisher_with_selection(
        path,
        url,
        format,
        if map_all {
            PublishTrackSelection::AllStreams
        } else {
            PublishTrackSelection::PrimaryAv
        },
        None,
    )
}

/// Probe a live stream URL without buffering its contents into the harness.
async fn ffprobe(url: &str) -> Result<Value, String> {
    // kill_on_drop(true) ensures the subprocess is killed when the timeout
    // drops the future, preventing orphan ffprobe processes (T2 fix).
    let child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-probesize",
            "2M",
            "-analyzeduration",
            "2M",
            "-show_entries",
            "stream=index,codec_name,codec_type,width,height,sample_rate,channels",
            "-of",
            "json",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;

    let output = tokio::time::timeout(Duration::from_secs(12), child.wait_with_output())
        .await
        .map_err(|_| format!("ffprobe timed out: {url}"))?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
}

async fn ffprobe_video_packets(url: &str, output_path: &Path) -> Result<Value, String> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-read_intervals",
            "%+5",
            "-select_streams",
            "v:0",
            "-show_packets",
            "-show_entries",
            "packet=pts_time,dts_time",
            "-of",
            "json",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;

    let output = tokio::time::timeout(Duration::from_secs(25), child.wait_with_output())
        .await
        .map_err(|_| format!("ffprobe packet capture timed out: {url}"))?
        .map_err(|e| e.to_string())?;
    std::fs::write(output_path, &output.stdout).map_err(|e| e.to_string())?;
    let stderr_path = artifact_path("bframe-ffprobe.log");
    std::fs::write(&stderr_path, &output.stderr).map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe packet capture failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
}

fn packet_times(packet_probe: &Value) -> impl Iterator<Item = (Option<f64>, Option<f64>)> + '_ {
    packet_probe["packets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|packet| {
            (
                packet["pts_time"].as_str().and_then(parse_probe_time),
                packet["dts_time"].as_str().and_then(parse_probe_time),
            )
        })
}

fn parse_probe_time(value: &str) -> Option<f64> {
    if value == "N/A" {
        None
    } else {
        value.parse().ok()
    }
}

fn count_video_packets(packet_probe: &Value) -> usize {
    packet_times(packet_probe)
        .filter(|(_, dts)| dts.is_some())
        .count()
}

fn count_bframe_packets(packet_probe: &Value) -> usize {
    packet_times(packet_probe)
        .filter(|(pts, dts)| matches!((pts, dts), (Some(pts), Some(dts)) if pts > dts))
        .count()
}

fn video_dts_monotone(packet_probe: &Value) -> bool {
    let mut last = None;
    for (_, dts) in packet_times(packet_probe) {
        let Some(dts) = dts else {
            continue;
        };
        if last.is_some_and(|last| dts < last) {
            return false;
        }
        last = Some(dts);
    }
    true
}

fn normalized_streams(probe: &Value) -> Result<Value, String> {
    let streams = probe["streams"]
        .as_array()
        .ok_or("ffprobe output has no streams")?;
    let mut normalized: Vec<Value> = streams
        .iter()
        .filter_map(|stream| match stream["codec_type"].as_str() {
            Some("video") => Some(json!({
                "type": "video",
                "codec": stream["codec_name"],
                "width": stream["width"],
                "height": stream["height"],
            })),
            Some("audio") => Some(json!({
                "type": "audio",
                "codec": stream["codec_name"],
                "sampleRate": stream["sample_rate"],
                "channels": stream["channels"],
            })),
            _ => None,
        })
        .collect();
    normalized.sort_by_key(|entry| entry["type"].as_str().unwrap_or("").to_string());
    Ok(Value::Array(normalized))
}

fn assert_media_only(probe: &Value, label: &str) -> Result<(), String> {
    let streams = probe["streams"]
        .as_array()
        .ok_or_else(|| format!("{label}: ffprobe output has no streams"))?;
    let non_media: Vec<&str> = streams
        .iter()
        .filter_map(|stream| stream["codec_type"].as_str())
        .filter(|kind| !matches!(*kind, "video" | "audio"))
        .collect();
    let video_count = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "video")
        .count();
    let audio_count = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio")
        .count();
    if !non_media.is_empty() || video_count != 1 || audio_count < 1 {
        return Err(format!(
            "{label}: expected 1 video + >=1 audio, got video={video_count} \
             audio={audio_count} non_media={non_media:?}"
        ));
    }
    Ok(())
}

fn assert_snapshot_matches_probe(
    snapshot: &Value,
    normalized: &Value,
    label: &str,
) -> Result<(), String> {
    let input = &snapshot["input"];
    let streams = normalized
        .as_array()
        .ok_or_else(|| format!("{label}: normalized streams are not an array"))?;
    let video = streams
        .iter()
        .find(|stream| stream["type"] == "video")
        .ok_or_else(|| format!("{label}: missing normalized video"))?;
    let audio = streams
        .iter()
        .find(|stream| stream["type"] == "audio")
        .ok_or_else(|| format!("{label}: missing normalized audio"))?;
    let snapshot_audio = input["audioTracks"]
        .as_array()
        .and_then(|tracks| tracks.first())
        .ok_or_else(|| format!("{label}: snapshot missing audio"))?;
    let probe_sample_rate = audio["sampleRate"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| audio["sampleRate"].as_u64());

    let matches = input["video"]["codec"] == video["codec"]
        && input["video"]["width"] == video["width"]
        && input["video"]["height"] == video["height"]
        && snapshot_audio["codec"] == audio["codec"]
        && snapshot_audio["sampleRate"].as_u64() == probe_sample_rate
        && snapshot_audio["channels"] == audio["channels"];
    if !matches {
        return Err(format!(
            "{label}: engine snapshot does not match external probe: snapshot={} probe={}",
            snapshot, normalized
        ));
    }
    Ok(())
}

async fn mixed_file_h264_correctness() -> Result<Value, String> {
    let env = MixedEnv::from_env("mixed-file-h264");
    if env.n_per_group == 0 {
        return Err("N_PER_GROUP must be greater than zero".to_string());
    }
    std::fs::create_dir_all(&env.work_dir).map_err(|e| e.to_string())?;
    ensure_mixed_artifacts(&env)?;

    let mut mediamtx = start_mixed_mediamtx(&env).await?;
    let mut restream = start_mixed_restream(&env).await?;
    let restream_pid = restream.id().unwrap_or(0);
    let mut api = RampApi::new(env.restream_http);
    api.login().await?;

    let config = run_mixed_file_h264_config(&env, &api, restream_pid).await;

    stop_child(&mut restream).await;
    stop_child(&mut mediamtx).await;

    config.map(|config| {
        json!({
            "passed": true,
            "mode": "mixed-file-h264",
            "configs": [config],
            "artifacts": {
                "scaleCsv": env.scale_log,
                "rssSummary": env.rss_summary,
                "summary": env.summary_log,
                "restreamLog": env.restream_log,
                "mediamtxLog": env.mediamtx_log,
            }
        })
    })
}

async fn run_mixed_file_h264_config(
    env: &MixedEnv,
    api: &RampApi,
    restream_pid: u32,
) -> Result<Value, String> {
    let cfg = "file-h264";
    let n = env.n_per_group;
    let total = n * 2;
    let stream_key = format!("sk-{cfg}");

    let fixture = checked_h264_fixture()?;

    let fixture_name = fixture.file_name().unwrap().to_string_lossy().to_string();
    let media_dir =
        PathBuf::from(std::env::var("RESTREAM_MEDIA_DIR").unwrap_or_else(|_| "media".into()));
    let media_dest = media_dir.join(&fixture_name);
    if !media_dest.exists() {
        std::fs::copy(&fixture, &media_dest).map_err(|e| e.to_string())?;
    }

    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": cfg, "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    api.put_json(
        &format!("/api/v1/pipelines/{pipeline_id}/file-ingest"),
        json!({"filename": fixture_name, "loop": true}),
    )
    .await?;

    let ingest_list = api.get_json("/api/v1/ingests").await?;
    let ingest_id = ingest_list
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|i| i["streamKey"].as_str() == Some(&stream_key))
        })
        .and_then(|i| i["id"].as_str())
        .ok_or("file ingest not found in list")?
        .to_string();

    api.post_json(&format!("/api/v1/ingests/{ingest_id}/start"), json!({}))
        .await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;
    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);
    if !env.skip_load {
        snapshot_mixed(
            env,
            restream_pid,
            cfg,
            "baseline (file ingest live, 0 outputs)",
        )
        .await?;
    }

    let mut output_ids = Vec::with_capacity(total);
    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "rtmp-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "rtmp://127.0.0.1:{}/live/{cfg}-rtmp-src-{index}",
                env.mtx_rtmp
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} RTMP source")).await?;
    }

    add_mixed_group(
        api,
        &pipeline_id,
        MixedGroupSpec {
            cfg,
            group: "srt-src",
            count: n,
            encoding: "source",
        },
        |index| {
            format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{cfg}-srt-src-{index}",
                env.mtx_srt
            )
        },
        &mut output_ids,
    )
    .await?;
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, &format!("after {n} SRT source")).await?;
    }

    let duration_secs: u64 = 10;
    println!("[mixed-file-h264] sustaining {total} outputs for {duration_secs}s");
    tokio::time::sleep(Duration::from_secs(duration_secs)).await;
    if !env.skip_load {
        snapshot_mixed(
            env,
            restream_pid,
            cfg,
            &format!("after {duration_secs}s sustained"),
        )
        .await?;
    }

    let rss_peak = process_rss_kb(restream_pid).await.unwrap_or(0);
    let growth_kb = rss_peak.saturating_sub(rss_baseline);

    for (i, output_id) in output_ids.iter().enumerate() {
        api.post_json(
            &format!("/api/v1/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
            json!({}),
        )
        .await?;
        if i % 4 == 3 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    api.post_json(&format!("/api/v1/ingests/{ingest_id}/stop"), json!({}))
        .await?;

    println!(
        "[mixed-file-h264] done: {total} outputs, baseline={rss_baseline}kB peak={rss_peak}kB growth={growth_kb}kB"
    );

    Ok(json!({
        "config": cfg,
        "outputCount": total,
        "rssBaselineKb": rss_baseline,
        "rssPeakKb": rss_peak,
        "rssGrowthKb": growth_kb,
    }))
}

async fn fault_rtmp_egress_sink_disappear(
    api: &RampApi,
    ports: &TestPorts,
    fixture_h264: &Path,
    sink_port: u16,
    timeout: Duration,
) -> Result<Value, String> {
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "fault-egress-rtmp", "streamKey": "fault-egress-rtmp"}),
        )
        .await?;
    let pid = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("missing id")?
        .to_string();

    let sink_metrics = Arc::new(GeneralizedSinkMetrics::default());
    let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink bind: {e}"))?;
    let sink_cancel = CancellationToken::new();
    let reader_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let reader_handles_inner = reader_handles.clone();
    let sink_metrics_inner = sink_metrics.clone();
    let sink_cancel_inner = sink_cancel.clone();
    let sink_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = sink_listener.accept() => {
                    if let Ok((socket, _)) = result {
                        let metrics = sink_metrics_inner.clone();
                        let h = tokio::spawn(async move {
                            let _ = handle_generalized_sink_client(socket, metrics).await;
                        });
                        reader_handles_inner.lock().unwrap().push(h);
                    }
                }
                _ = sink_cancel_inner.cancelled() => break,
            }
        }
    });

    let sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/fault-egress-rtmp-sink");
    let output = api
        .post_json(
            &format!("/api/v1/pipelines/{pid}/outputs"),
            json!({"name": "rtmp-sink", "url": sink_url, "encoding": "source"}),
        )
        .await?;
    let oid = output["output"]["id"]
        .as_str()
        .ok_or("missing id")?
        .to_string();

    let mut pub_child = spawn_publisher(
        fixture_h264,
        &format!("rtmp://127.0.0.1:{}/live/fault-egress-rtmp", ports.rtmp),
        "flv",
        false,
    )
    .await?;
    wait_for_api_input_live(api, &pid, timeout).await?;

    api.post_json(
        &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
        json!({}),
    )
    .await?;

    let deadline = Instant::now() + timeout;
    while sink_metrics.video_count.load(Ordering::Relaxed) < 10 {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("[fault] RTMP egress delivering data");

    sink_cancel.cancel();
    sink_task.abort();
    {
        let handles = reader_handles.lock().unwrap();
        for h in handles.iter() {
            h.abort();
        }
    }

    let started = Instant::now();
    let poll_deadline = started + Duration::from_secs(10);
    let mut passed = false;
    let mut phase = String::from("unknown");
    let mut has_error = false;
    let mut saw_retrying = false;
    let mut retry_attempts: Option<u64> = None;
    let mut retry_backoff_ms: Option<u64> = None;
    let mut health_saw_retrying = false;
    while Instant::now() < poll_deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await;
        match &status {
            Err(_) => {
                phase = "cleaned-up".to_string();
                passed = true;
                break;
            }
            Ok(s) => {
                has_error = s["lastError"]
                    .as_str()
                    .map(|e| !e.is_empty())
                    .unwrap_or(false);
                phase = s["phase"].as_str().unwrap_or("unknown").to_string();
                if s["status"].as_str() == Some("retrying") {
                    saw_retrying = true;
                    retry_attempts = s["retryAttempts"].as_u64();
                    retry_backoff_ms = s["retryBackoffMs"].as_u64();
                }
                if let Ok(health) = api.get_json("/api/v1/engine/health").await
                    && health["pipelines"][&pid]["outputs"][&oid]["status"].as_str()
                        == Some("retrying")
                {
                    health_saw_retrying = true;
                }
                if saw_retrying && has_error {
                    passed = true;
                    break;
                }
            }
        }
    }
    let elapsed = started.elapsed();
    let recovery_metrics = Arc::new(GeneralizedSinkMetrics::default());
    let recovered_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
        .await
        .map_err(|e| format!("sink rebind: {e}"))?;
    let recovered_cancel = CancellationToken::new();
    let recovered_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let recovered_handles_inner = recovered_handles.clone();
    let recovery_metrics_inner = recovery_metrics.clone();
    let recovered_cancel_inner = recovered_cancel.clone();
    let recovered_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = recovered_listener.accept() => {
                    if let Ok((socket, _)) = result {
                        let metrics = recovery_metrics_inner.clone();
                        let h = tokio::spawn(async move {
                            let _ = handle_generalized_sink_client(socket, metrics).await;
                        });
                        recovered_handles_inner.lock().unwrap().push(h);
                    }
                }
                _ = recovered_cancel_inner.cancelled() => break,
            }
        }
    });

    let recovery_started = Instant::now();
    let recovery_deadline = recovery_started + Duration::from_secs(25);
    let mut recovered = false;
    let mut recovery_status = String::from("unknown");
    while Instant::now() < recovery_deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(status) = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
        {
            recovery_status = status["status"].as_str().unwrap_or("unknown").to_string();
            if recovery_status == "retrying" {
                saw_retrying = true;
            }
        }
        if recovery_metrics.video_count.load(Ordering::Relaxed) >= 10 {
            recovered = true;
            break;
        }
    }
    recovered_cancel.cancel();
    recovered_task.abort();
    {
        let handles = recovered_handles.lock().unwrap();
        for h in handles.iter() {
            h.abort();
        }
    }
    let final_status = api
        .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
        .await
        .ok();
    let final_retrying = final_status
        .as_ref()
        .and_then(|status| status["retrying"].as_bool())
        .unwrap_or(false);
    println!(
        "[fault] RTMP egress sink disappear: {} (phase={}, hasError={}, sawRetrying={}, healthSawRetrying={}, recovered={}, recoveryStatus={}, finalRetrying={}, {:.1}s)",
        if passed && recovered && saw_retrying && health_saw_retrying && !final_retrying {
            "PASS"
        } else {
            "FAIL"
        },
        phase,
        has_error,
        saw_retrying,
        health_saw_retrying,
        recovered,
        recovery_status,
        final_retrying,
        elapsed.as_secs_f64()
    );

    stop_child(&mut pub_child).await;

    Ok(json!({
        "test": "rtmp-egress-sink-disappear",
        "passed": passed && recovered && saw_retrying && health_saw_retrying && !final_retrying,
        "phase": phase,
        "hasError": has_error,
        "elapsedMs": elapsed.as_millis(),
        "sawRetrying": saw_retrying,
        "healthSawRetrying": health_saw_retrying,
        "retryAttempts": retry_attempts,
        "retryBackoffMs": retry_backoff_ms,
        "recovered": recovered,
        "recoveryStatus": recovery_status,
        "finalRetrying": final_retrying,
    }))
}

async fn fault_srt_egress_sink_disappear(
    api: &RampApi,
    ports: &TestPorts,
    fixture_h264: &Path,
    timeout: Duration,
) -> Result<Value, String> {
    let pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "fault-egress-srt", "streamKey": "fault-egress-srt"}),
        )
        .await?;
    let pid = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("missing id")?
        .to_string();

    let sink_pipeline = api
        .post_json(
            "/api/v1/pipelines",
            json!({"name": "srt-sink-target", "streamKey": "srt-sink-target"}),
        )
        .await?;
    let sink_pid = sink_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("missing id")?
        .to_string();

    let sink_url = format!(
        "srt://127.0.0.1:{}?streamid=publish:live/srt-sink-target&pkt_size=1316",
        ports.srt
    );
    let output = api
        .post_json(
            &format!("/api/v1/pipelines/{pid}/outputs"),
            json!({"name": "srt-sink", "url": sink_url, "encoding": "source"}),
        )
        .await?;
    let oid = output["output"]["id"]
        .as_str()
        .ok_or("missing id")?
        .to_string();

    let mut pub_child = spawn_publisher(
        fixture_h264,
        &format!(
            "srt://127.0.0.1:{}?streamid=publish:live/fault-egress-srt&pkt_size=1316",
            ports.srt
        ),
        "mpegts",
        true,
    )
    .await?;
    wait_for_api_input_live(api, &pid, timeout).await?;

    api.post_json(
        &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
        json!({}),
    )
    .await?;

    let deadline = Instant::now() + timeout;
    let mut sink_live = false;
    while Instant::now() < deadline {
        if let Ok(health) = api.get_json("/api/v1/engine/health").await {
            let status = health["pipelines"][&sink_pid]["input"]["status"]
                .as_str()
                .unwrap_or("off");
            if status == "on" {
                sink_live = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if sink_live {
        println!("[fault] SRT egress delivering to sink pipeline");
    }

    let delete_url = format!("{}/api/v1/pipelines/{sink_pid}", api.base_url);
    let mut request = api.client.delete(&delete_url);
    if let Some(cookie) = &api.cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    let _ = request.send().await;

    let started = Instant::now();
    let poll_deadline = started + Duration::from_secs(10);
    let mut passed = false;
    let mut phase = String::from("unknown");
    let mut has_error = false;
    let mut saw_retrying = false;
    let mut health_saw_retrying = false;
    let mut retry_attempts: Option<u64> = None;
    let mut retry_backoff_ms: Option<u64> = None;
    while Instant::now() < poll_deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await;
        match &status {
            Err(_) => {
                phase = "cleaned-up".to_string();
                passed = true;
                break;
            }
            Ok(s) => {
                has_error = s["lastError"]
                    .as_str()
                    .map(|e| !e.is_empty())
                    .unwrap_or(false);
                phase = s["phase"].as_str().unwrap_or("unknown").to_string();
                if s["status"].as_str() == Some("retrying") {
                    saw_retrying = true;
                    retry_attempts = s["retryAttempts"].as_u64();
                    retry_backoff_ms = s["retryBackoffMs"].as_u64();
                }
                if let Ok(health) = api.get_json("/api/v1/engine/health").await
                    && health["pipelines"][&pid]["outputs"][&oid]["status"].as_str()
                        == Some("retrying")
                {
                    health_saw_retrying = true;
                }
                if saw_retrying && has_error {
                    passed = true;
                    break;
                }
            }
        }
    }
    let elapsed = started.elapsed();
    let final_status = api
        .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
        .await
        .ok();
    let final_retrying = final_status
        .as_ref()
        .and_then(|status| status["retrying"].as_bool())
        .unwrap_or(false);
    println!(
        "[fault] SRT egress sink disappear: {} (phase={}, hasError={}, sawRetrying={}, healthSawRetrying={}, finalRetrying={}, {:.1}s)",
        if passed && saw_retrying && health_saw_retrying && final_retrying {
            "PASS"
        } else {
            "FAIL"
        },
        phase,
        has_error,
        saw_retrying,
        health_saw_retrying,
        final_retrying,
        elapsed.as_secs_f64()
    );

    stop_child(&mut pub_child).await;

    Ok(json!({
        "test": "srt-egress-sink-disappear",
        "passed": passed && saw_retrying && health_saw_retrying && final_retrying,
        "phase": phase,
        "hasError": has_error,
        "elapsedMs": elapsed.as_millis(),
        "sawRetrying": saw_retrying,
        "healthSawRetrying": health_saw_retrying,
        "retryAttempts": retry_attempts,
        "retryBackoffMs": retry_backoff_ms,
        "finalRetrying": final_retrying,
    }))
}

async fn fault_egress_retry() -> Result<Value, String> {
    let work_dir = artifact_path("fault-egress-retry");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();
    let timeout = Duration::from_secs(15);

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let fixture_h264 = checked_h264_fixture()?;
    let results = vec![
        fault_rtmp_egress_sink_disappear(&api, &ports, &fixture_h264, sink_port, timeout).await?,
        fault_srt_egress_sink_disappear(&api, &ports, &fixture_h264, timeout).await?,
    ];

    stop_child(&mut child).await;

    let all_passed = results.iter().all(|r| r["passed"] == true);
    let result = json!({
        "mode": "fault-egress-retry",
        "passed": all_passed,
        "tests": results,
    });

    let result_path = work_dir.join("fault-egress-retry.json");
    std::fs::write(&result_path, serde_json::to_string_pretty(&result).unwrap())
        .map_err(|e| e.to_string())?;
    println!("artifact={}", result_path.display());

    if !all_passed {
        return Err("fault-egress-retry: not all tests passed".to_string());
    }
    Ok(result)
}

async fn recovery_live_cases(
    api: &mut RampApi,
    ports: &TestPorts,
    fixture_h264: &Path,
    sink_port: u16,
    hls_put_port: u16,
    timeout: Duration,
) -> Result<Vec<Value>, String> {
    let mut results = Vec::new();

    // ── 1. Transient RTMP publisher drop does not tear down egress ─────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-rtmp-transient", "streamKey": "fault-rtmp-transient"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let metrics = Arc::new(GeneralizedSinkMetrics::default());
        let listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
            .await
            .map_err(|e| format!("sink bind {sink_port}: {e}"))?;
        let sink_metrics = metrics.clone();
        let sink_task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let sink_metrics = sink_metrics.clone();
                tokio::spawn(handle_generalized_sink_client(socket, sink_metrics));
            }
        });

        let output = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs"),
                json!({
                    "name": "rtmp-transient-sink",
                    "url": format!("rtmp://127.0.0.1:{sink_port}/live/fault-rtmp-transient-sink"),
                    "encoding": "source"
                }),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing output id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-rtmp-transient", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(api, &pid, timeout).await?;
        api.post_json(
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        let warm_deadline = Instant::now() + Duration::from_secs(15);
        while metrics.video_count.load(Ordering::Relaxed) < 10 {
            if Instant::now() >= warm_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let baseline_video = metrics.video_count.load(Ordering::Relaxed);
        let baseline_connections = metrics.connections.load(Ordering::Relaxed);

        stop_child(&mut pub_child).await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;

        let gap_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let gap_health = api.get_json("/api/v1/engine/health").await.ok();
        let gap_input = gap_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);
        let gap_connections = metrics.connections.load(Ordering::Relaxed);
        let gap_status_running = gap_status
            .as_ref()
            .and_then(|status| status["status"].as_str())
            == Some("running");
        let gap_retrying = gap_status
            .as_ref()
            .and_then(|status| status["retrying"].as_bool())
            .unwrap_or(false);
        let gap_has_error = gap_status
            .as_ref()
            .and_then(|status| status["lastError"].as_str())
            .map(|message| !message.is_empty())
            .unwrap_or(false);
        let gap_grace_active = gap_input["disconnectGraceActive"] == true;
        let gap_grace_remaining = gap_input["disconnectGraceRemainingMs"]
            .as_u64()
            .is_some_and(|remaining| remaining > 0 && remaining <= 5_000);
        let gap_preserved = gap_status.is_some()
            && gap_connections == baseline_connections
            && gap_status_running
            && !gap_retrying
            && !gap_has_error
            && gap_grace_active
            && gap_grace_remaining;

        let mut resumed_child = spawn_publisher(
            fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-rtmp-transient", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(api, &pid, Duration::from_secs(30)).await?;

        let resume_deadline = Instant::now() + Duration::from_secs(15);
        let mut resumed = false;
        while Instant::now() < resume_deadline {
            if metrics.video_count.load(Ordering::Relaxed) > baseline_video + 10 {
                resumed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let final_connections = metrics.connections.load(Ordering::Relaxed);
        let final_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let final_status_running = final_status
            .as_ref()
            .and_then(|status| status["status"].as_str())
            == Some("running");
        let final_retrying = final_status
            .as_ref()
            .and_then(|status| status["retrying"].as_bool())
            .unwrap_or(false);
        let final_health = api.get_json("/api/v1/engine/health").await.ok();
        let final_input = final_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);
        let final_disconnect_cleared = final_input["status"] == "on"
            && final_input["probeStatus"] == "ready"
            && final_input["lastSessionProtocol"].is_null()
            && final_input["lastDisconnectReason"].is_null()
            && final_input["lastFailurePhase"].is_null()
            && final_input["recentDisconnectError"] == false;
        let passed = baseline_video >= 10
            && baseline_connections == 1
            && gap_preserved
            && resumed
            && final_connections == baseline_connections
            && final_status_running
            && !final_retrying
            && final_disconnect_cleared;
        println!(
            "[fault] Transient RTMP publisher drop preserves egress: {} (connections={} resumed={} gapStatusRunning={} gapRetrying={} finalRetrying={} disconnectCleared={})",
            if passed { "PASS" } else { "FAIL" },
            final_connections,
            resumed,
            gap_status_running,
            gap_retrying,
            final_retrying,
            final_disconnect_cleared,
        );
        results.push(json!({
            "test": "transient-rtmp-drop-preserves-egress",
            "passed": passed,
            "baselineVideo": baseline_video,
            "baselineConnections": baseline_connections,
            "gapConnections": gap_connections,
            "gapStatusExists": gap_status.is_some(),
            "gapStatusRunning": gap_status_running,
            "gapRetrying": gap_retrying,
            "gapHasError": gap_has_error,
            "gapGraceActive": gap_grace_active,
            "gapGraceRemainingBounded": gap_grace_remaining,
            "gapInputSnapshot": gap_input,
            "resumed": resumed,
            "finalConnections": final_connections,
            "finalStatusRunning": final_status_running,
            "finalRetrying": final_retrying,
            "finalDisconnectCleared": final_disconnect_cleared,
            "finalInputSnapshot": final_input,
        }));

        let _ = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs/{oid}/stop"),
                json!({}),
            )
            .await;
        stop_child(&mut resumed_child).await;
        sink_task.abort();
    }

    // ── 2. Transient SRT publisher drop does not tear down egress ──────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-srt-transient", "streamKey": "fault-srt-transient"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let metrics = Arc::new(GeneralizedSinkMetrics::default());
        let listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
            .await
            .map_err(|e| format!("sink bind {sink_port}: {e}"))?;
        let sink_metrics = metrics.clone();
        let sink_task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let sink_metrics = sink_metrics.clone();
                tokio::spawn(handle_generalized_sink_client(socket, sink_metrics));
            }
        });

        let output = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs"),
                json!({
                    "name": "srt-transient-sink",
                    "url": format!("rtmp://127.0.0.1:{sink_port}/live/fault-srt-transient-sink"),
                    "encoding": "source"
                }),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing output id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            fixture_h264,
            &format!(
                "srt://127.0.0.1:{}?streamid=publish:live/fault-srt-transient&pkt_size=1316",
                ports.srt
            ),
            "mpegts",
            true,
        )
        .await?;
        wait_for_api_input_live(api, &pid, timeout).await?;
        api.post_json(
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        let warm_deadline = Instant::now() + Duration::from_secs(15);
        while metrics.video_count.load(Ordering::Relaxed) < 10 {
            if Instant::now() >= warm_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let baseline_video = metrics.video_count.load(Ordering::Relaxed);
        let baseline_connections = metrics.connections.load(Ordering::Relaxed);

        stop_child(&mut pub_child).await;
        let off_result = wait_for_api_input_off(api, &pid, Duration::from_secs(10)).await;
        let off_health = api.get_json("/api/v1/engine/health").await.ok();
        let off_input = off_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);

        let gap_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let gap_connections = metrics.connections.load(Ordering::Relaxed);
        let gap_status_running = gap_status
            .as_ref()
            .and_then(|status| status["status"].as_str())
            == Some("running");
        let gap_retrying = gap_status
            .as_ref()
            .and_then(|status| status["retrying"].as_bool())
            .unwrap_or(false);
        let gap_has_error = gap_status
            .as_ref()
            .and_then(|status| status["lastError"].as_str())
            .map(|message| !message.is_empty())
            .unwrap_or(false);
        let gap_input_off = off_result.is_ok() && off_input["status"] == "off";
        let gap_grace_active = off_input["disconnectGraceActive"] == true;
        let gap_grace_remaining = off_input["disconnectGraceRemainingMs"]
            .as_u64()
            .is_some_and(|remaining| remaining > 0 && remaining <= 5_000);
        let gap_preserved = gap_input_off
            && gap_status.is_some()
            && gap_connections == baseline_connections
            && gap_status_running
            && !gap_retrying
            && !gap_has_error
            && gap_grace_active
            && gap_grace_remaining;

        // SRT publishers can linger for a short teardown window after an
        // abrupt drop. Reconnect only after the prior session is fully off so
        // the test proves grace-window recovery instead of duplicate-publisher
        // rejection timing.
        let mut resumed_child = spawn_publisher(
            fixture_h264,
            &format!(
                "srt://127.0.0.1:{}?streamid=publish:live/fault-srt-transient&pkt_size=1316",
                ports.srt
            ),
            "mpegts",
            true,
        )
        .await?;
        let media_ready = wait_for_api_input_media_ready(api, &pid, Duration::from_secs(30)).await;

        let resume_deadline = Instant::now() + Duration::from_secs(15);
        let mut resumed = false;
        while Instant::now() < resume_deadline {
            if metrics.video_count.load(Ordering::Relaxed) > baseline_video + 10 {
                resumed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let final_connections = metrics.connections.load(Ordering::Relaxed);
        let final_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let final_status_running = final_status
            .as_ref()
            .and_then(|status| status["status"].as_str())
            == Some("running");
        let final_retrying = final_status
            .as_ref()
            .and_then(|status| status["retrying"].as_bool())
            .unwrap_or(false);
        let final_health = api.get_json("/api/v1/engine/health").await.ok();
        let final_input = final_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);
        let final_disconnect_cleared = final_input["status"] == "on"
            && final_input["probeStatus"] == "ready"
            && final_input["lastSessionProtocol"].is_null()
            && final_input["lastDisconnectReason"].is_null()
            && final_input["lastFailurePhase"].is_null()
            && final_input["recentDisconnectError"] == false;
        let passed = baseline_video >= 10
            && baseline_connections == 1
            && gap_preserved
            && resumed
            && final_connections == baseline_connections
            && final_status_running
            && !final_retrying
            && media_ready.is_ok()
            && final_disconnect_cleared;
        println!(
            "[fault] Transient SRT publisher drop preserves egress: {} (connections={} resumed={} gapInputOff={} gapStatusRunning={} gapRetrying={} finalRetrying={} mediaReady={} disconnectCleared={})",
            if passed { "PASS" } else { "FAIL" },
            final_connections,
            resumed,
            gap_input_off,
            gap_status_running,
            gap_retrying,
            final_retrying,
            media_ready.is_ok(),
            final_disconnect_cleared,
        );
        results.push(json!({
            "test": "transient-srt-drop-preserves-egress",
            "passed": passed,
            "baselineVideo": baseline_video,
            "baselineConnections": baseline_connections,
            "gapConnections": gap_connections,
            "gapInputOff": gap_input_off,
            "gapOffError": off_result.err(),
            "gapOffInputSnapshot": off_input,
            "gapStatusExists": gap_status.is_some(),
            "gapStatusRunning": gap_status_running,
            "gapRetrying": gap_retrying,
            "gapHasError": gap_has_error,
            "gapGraceActive": gap_grace_active,
            "gapGraceRemainingBounded": gap_grace_remaining,
            "resumed": resumed,
            "mediaReady": media_ready.is_ok(),
            "mediaReadyError": media_ready.err(),
            "finalConnections": final_connections,
            "finalStatusRunning": final_status_running,
            "finalRetrying": final_retrying,
            "finalDisconnectCleared": final_disconnect_cleared,
            "finalInputSnapshot": final_input,
        }));

        let _ = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs/{oid}/stop"),
                json!({}),
            )
            .await;
        stop_child(&mut resumed_child).await;
        sink_task.abort();
    }

    // ── 3. Egress retry survives transient ingest gap within grace ─────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-rtmp-retry-gap", "streamKey": "fault-rtmp-retry-gap"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let sink_metrics = Arc::new(GeneralizedSinkMetrics::default());
        let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
            .await
            .map_err(|e| format!("sink bind {sink_port}: {e}"))?;
        let sink_cancel = CancellationToken::new();
        let sink_reader_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sink_reader_handles_inner = sink_reader_handles.clone();
        let sink_metrics_inner = sink_metrics.clone();
        let sink_cancel_inner = sink_cancel.clone();
        let sink_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = sink_listener.accept() => {
                        if let Ok((socket, _)) = result {
                            let metrics = sink_metrics_inner.clone();
                            let handle = tokio::spawn(async move {
                                let _ = handle_generalized_sink_client(socket, metrics).await;
                            });
                            sink_reader_handles_inner.lock().unwrap().push(handle);
                        }
                    }
                    _ = sink_cancel_inner.cancelled() => break,
                }
            }
        });

        let output = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs"),
                json!({
                    "name": "rtmp-retry-gap-sink",
                    "url": format!("rtmp://127.0.0.1:{sink_port}/live/fault-rtmp-retry-gap-sink"),
                    "encoding": "source"
                }),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing output id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-rtmp-retry-gap", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(api, &pid, timeout).await?;
        api.post_json(
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        let warm_deadline = Instant::now() + Duration::from_secs(15);
        while sink_metrics.video_count.load(Ordering::Relaxed) < 10 {
            if Instant::now() >= warm_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let baseline_video = sink_metrics.video_count.load(Ordering::Relaxed);

        sink_cancel.cancel();
        sink_task.abort();
        {
            let handles = sink_reader_handles.lock().unwrap();
            for handle in handles.iter() {
                handle.abort();
            }
        }

        let retrying_deadline = Instant::now() + Duration::from_secs(10);
        let mut retry_phase = String::from("unknown");
        let mut retry_has_error = false;
        let mut retry_status_visible = false;
        let mut retry_health_visible = false;
        let mut retry_attempts: Option<u64> = None;
        let mut retry_backoff_ms: Option<u64> = None;
        while Instant::now() < retrying_deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let status = api
                .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
                .await
                .ok();
            if let Some(status) = status.as_ref() {
                retry_has_error = status["lastError"]
                    .as_str()
                    .map(|message| !message.is_empty())
                    .unwrap_or(false);
                retry_phase = status["phase"].as_str().unwrap_or("unknown").to_string();
                retry_status_visible = status["status"].as_str() == Some("retrying")
                    && status["retrying"].as_bool() == Some(true);
                if retry_status_visible {
                    retry_attempts = status["retryAttempts"].as_u64();
                    retry_backoff_ms = status["retryBackoffMs"].as_u64();
                }
            }
            if let Ok(health) = api.get_json("/api/v1/engine/health").await {
                let output_health = &health["pipelines"][&pid]["outputs"][&oid];
                retry_health_visible = output_health["status"].as_str() == Some("retrying")
                    && output_health["retrying"].as_bool() == Some(true);
            }
            if retry_status_visible && retry_health_visible && retry_has_error {
                break;
            }
        }

        stop_child(&mut pub_child).await;
        let input_off = wait_for_api_input_off(api, &pid, Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(1_500)).await;

        let gap_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let gap_health = api.get_json("/api/v1/engine/health").await.ok();
        let gap_output_retrying = gap_status
            .as_ref()
            .map(|status| {
                status["status"].as_str() == Some("retrying")
                    && status["retrying"].as_bool() == Some(true)
            })
            .unwrap_or(false);
        let gap_health_retrying = gap_health
            .as_ref()
            .map(|health| {
                let output = &health["pipelines"][&pid]["outputs"][&oid];
                output["status"].as_str() == Some("retrying")
                    && output["retrying"].as_bool() == Some(true)
            })
            .unwrap_or(false);
        let gap_input = gap_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);
        let gap_disconnect_visible = gap_input["status"] == "off"
            && gap_input["lastSessionProtocol"] == "rtmp"
            && gap_input["lastDisconnectReason"] == "publisher disconnected"
            && gap_input["lastFailurePhase"] == "disconnect"
            && gap_input["recentDisconnectError"] == false;
        let gap_grace_active = gap_input["disconnectGraceActive"] == true;
        let gap_grace_remaining = gap_input["disconnectGraceRemainingMs"]
            .as_u64()
            .is_some_and(|remaining| remaining > 0 && remaining <= 5_000);

        let recovery_metrics = Arc::new(GeneralizedSinkMetrics::default());
        let recovery_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
            .await
            .map_err(|e| format!("sink rebind {sink_port}: {e}"))?;
        let recovery_cancel = CancellationToken::new();
        let recovery_reader_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let recovery_reader_handles_inner = recovery_reader_handles.clone();
        let recovery_metrics_inner = recovery_metrics.clone();
        let recovery_cancel_inner = recovery_cancel.clone();
        let recovery_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = recovery_listener.accept() => {
                        if let Ok((socket, _)) = result {
                            let metrics = recovery_metrics_inner.clone();
                            let handle = tokio::spawn(async move {
                                let _ = handle_generalized_sink_client(socket, metrics).await;
                            });
                            recovery_reader_handles_inner.lock().unwrap().push(handle);
                        }
                    }
                    _ = recovery_cancel_inner.cancelled() => break,
                }
            }
        });

        let mut resumed_child = spawn_publisher(
            fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-rtmp-retry-gap", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        let media_ready = wait_for_api_input_media_ready(api, &pid, Duration::from_secs(30)).await;

        let recovery_deadline = Instant::now() + Duration::from_secs(25);
        let mut recovered = false;
        let mut recovery_status = String::from("unknown");
        while Instant::now() < recovery_deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(status) = api
                .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
                .await
            {
                recovery_status = status["status"].as_str().unwrap_or("unknown").to_string();
            }
            if recovery_metrics.video_count.load(Ordering::Relaxed) >= 10 {
                recovered = true;
                break;
            }
        }

        let final_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let final_status_running = final_status
            .as_ref()
            .and_then(|status| status["status"].as_str())
            == Some("running");
        let final_retrying = final_status
            .as_ref()
            .and_then(|status| status["retrying"].as_bool())
            .unwrap_or(false);
        let final_health = api.get_json("/api/v1/engine/health").await.ok();
        let final_input = final_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);
        let final_disconnect_cleared = final_input["status"] == "on"
            && final_input["probeStatus"] == "ready"
            && final_input["lastSessionProtocol"].is_null()
            && final_input["lastDisconnectReason"].is_null()
            && final_input["lastFailurePhase"].is_null()
            && final_input["recentDisconnectError"] == false;
        let passed = baseline_video >= 10
            && retry_status_visible
            && retry_health_visible
            && retry_has_error
            && input_off.is_ok()
            && gap_output_retrying
            && gap_health_retrying
            && gap_disconnect_visible
            && gap_grace_active
            && gap_grace_remaining
            && media_ready.is_ok()
            && recovered
            && final_status_running
            && !final_retrying
            && final_disconnect_cleared;
        println!(
            "[fault] Egress retry survives transient ingest gap: {} (retrying={} healthRetrying={} gapRetrying={} gapHealthRetrying={} recovered={} recoveryStatus={} finalRetrying={} disconnectCleared={})",
            if passed { "PASS" } else { "FAIL" },
            retry_status_visible,
            retry_health_visible,
            gap_output_retrying,
            gap_health_retrying,
            recovered,
            recovery_status,
            final_retrying,
            final_disconnect_cleared,
        );
        results.push(json!({
            "test": "egress-retry-survives-transient-ingest-gap",
            "passed": passed,
            "baselineVideo": baseline_video,
            "retryPhase": retry_phase,
            "retryHasError": retry_has_error,
            "retryStatusVisible": retry_status_visible,
            "retryHealthVisible": retry_health_visible,
            "retryAttempts": retry_attempts,
            "retryBackoffMs": retry_backoff_ms,
            "inputOffError": input_off.err(),
            "gapOutputRetrying": gap_output_retrying,
            "gapHealthRetrying": gap_health_retrying,
            "gapDisconnectVisible": gap_disconnect_visible,
            "gapGraceActive": gap_grace_active,
            "gapGraceRemainingBounded": gap_grace_remaining,
            "gapInputSnapshot": gap_input,
            "mediaReady": media_ready.is_ok(),
            "mediaReadyError": media_ready.err(),
            "recovered": recovered,
            "recoveryStatus": recovery_status,
            "finalStatusRunning": final_status_running,
            "finalRetrying": final_retrying,
            "finalDisconnectCleared": final_disconnect_cleared,
            "finalInputSnapshot": final_input,
        }));

        recovery_cancel.cancel();
        recovery_task.abort();
        {
            let handles = recovery_reader_handles.lock().unwrap();
            for handle in handles.iter() {
                handle.abort();
            }
        }

        let _ = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs/{oid}/stop"),
                json!({}),
            )
            .await;
        stop_child(&mut resumed_child).await;
    }

    // ── 4. Hung HLS PUT sink times out, retries, and recovers after restart ──
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-hls-put-timeout", "streamKey": "fault-hls-put-timeout"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();
        let sink_dir = artifact_path("recovery-hls-put-timeout");
        let _ = std::fs::remove_dir_all(&sink_dir);
        std::fs::create_dir_all(&sink_dir).map_err(|e| e.to_string())?;

        let (hang_cancel, hang_handle) =
            start_hls_put_hang_sink(hls_put_port, Duration::from_secs(30)).await?;
        let output = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs"),
                json!({
                    "name": "hls-put-timeout",
                    "url": format!("http://127.0.0.1:{hls_put_port}/upload?cid=fault-hls-put-timeout&copy=0&file=out.m3u8"),
                    "encoding": "source"
                }),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing output id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-hls-put-timeout", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(api, &pid, timeout).await?;
        api.post_json(
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        let retry_deadline = Instant::now() + Duration::from_secs(20);
        let mut retry_status_visible = false;
        let mut retry_health_visible = false;
        let mut retry_has_error = false;
        let mut retry_phase = String::from("unknown");
        let mut retry_failure_phase = String::from("unknown");
        let mut retry_error = String::new();
        while Instant::now() < retry_deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(status) = api
                .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
                .await
            {
                retry_status_visible = status["status"].as_str() == Some("retrying")
                    && status["retrying"].as_bool() == Some(true);
                retry_phase = status["phase"].as_str().unwrap_or("unknown").to_string();
                retry_failure_phase = status["failurePhase"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                retry_error = status["lastError"].as_str().unwrap_or("").to_string();
                retry_has_error = !retry_error.is_empty();
            }
            if let Ok(health) = api.get_json("/api/v1/engine/health").await {
                let output = &health["pipelines"][&pid]["outputs"][&oid];
                retry_health_visible = output["status"].as_str() == Some("retrying")
                    && output["retrying"].as_bool() == Some(true);
            }
            if retry_status_visible && retry_health_visible && retry_has_error {
                break;
            }
        }

        hang_cancel.cancel();
        let _ = hang_handle.await;

        let (sink_cancel, sink_handle) = start_hls_put_sink(hls_put_port, sink_dir.clone()).await?;
        let artifacts = wait_for_hls_put_artifacts(&sink_dir, Duration::from_secs(30)).await;
        let requests = read_hls_put_requests(&sink_dir).ok();
        let content_types_ok = requests.as_ref().is_some_and(|requests| {
            request_seen(requests, |r| {
                r["file"] == "out.m3u8" && r["contentType"] == "application/vnd.apple.mpegurl"
            }) && request_seen(requests, |r| {
                r["file"]
                    .as_str()
                    .is_some_and(|f| is_segment_file(f, "seg"))
                    && r["contentType"] == "video/mp2t"
            })
        });

        let recovery_deadline = Instant::now() + Duration::from_secs(20);
        let mut recovered = false;
        let mut recovery_status = String::from("unknown");
        let mut final_bytes_out = 0u64;
        while Instant::now() < recovery_deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(status) = api
                .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
                .await
            {
                recovery_status = status["status"].as_str().unwrap_or("unknown").to_string();
                final_bytes_out = status["bytesOut"].as_u64().unwrap_or(0);
                if recovery_status == "running"
                    && !status["retrying"].as_bool().unwrap_or(false)
                    && final_bytes_out > 0
                {
                    recovered = true;
                    break;
                }
            }
        }

        let final_status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await
            .ok();
        let final_status_running = final_status
            .as_ref()
            .and_then(|status| status["status"].as_str())
            == Some("running");
        let final_retrying = final_status
            .as_ref()
            .and_then(|status| status["retrying"].as_bool())
            .unwrap_or(false);
        let final_error_cleared = final_status
            .as_ref()
            .is_some_and(|status| status["lastError"].is_null());
        let timeout_error_visible = {
            let lower = retry_error.to_ascii_lowercase();
            lower.contains("timed out") || lower.contains("deadline")
        };
        let failure_phase_ok =
            retry_failure_phase == "upload_segment" || retry_failure_phase == "upload_playlist";
        let passed = retry_status_visible
            && retry_health_visible
            && retry_has_error
            && timeout_error_visible
            && retry_phase == "failed"
            && failure_phase_ok
            && artifacts.is_ok()
            && content_types_ok
            && recovered
            && final_status_running
            && !final_retrying
            && final_error_cleared
            && final_bytes_out > 0;
        println!(
            "[fault] Hung HLS PUT sink recovers after timeout: {} (retrying={} healthRetrying={} timeoutVisible={} failurePhase={} recovered={} recoveryStatus={} finalRetrying={} bytesOut={})",
            if passed { "PASS" } else { "FAIL" },
            retry_status_visible,
            retry_health_visible,
            timeout_error_visible,
            retry_failure_phase,
            recovered,
            recovery_status,
            final_retrying,
            final_bytes_out,
        );
        results.push(json!({
            "test": "hls-put-timeout-recovers-after-restart",
            "passed": passed,
            "retryStatusVisible": retry_status_visible,
            "retryHealthVisible": retry_health_visible,
            "retryHasError": retry_has_error,
            "retryPhase": retry_phase,
            "retryFailurePhase": retry_failure_phase,
            "retryError": retry_error,
            "timeoutErrorVisible": timeout_error_visible,
            "artifactsFound": artifacts.is_ok(),
            "contentTypesCorrect": content_types_ok,
            "recovered": recovered,
            "recoveryStatus": recovery_status,
            "finalStatusRunning": final_status_running,
            "finalRetrying": final_retrying,
            "finalErrorCleared": final_error_cleared,
            "finalBytesOut": final_bytes_out,
            "finalStatus": final_status,
        }));

        let _ = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs/{oid}/stop"),
                json!({}),
            )
            .await;
        stop_child(&mut pub_child).await;
        sink_cancel.cancel();
        let _ = sink_handle.await;
    }

    Ok(results)
}

async fn recovery() -> Result<Value, String> {
    let work_dir = artifact_path("recovery");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let hls_put_port: u16 = env_u16("HLS_PUT_PORT", 8990);
    let ports = TestPorts::from_env();
    let timeout = Duration::from_secs(15);

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let fixture_h264 = checked_h264_fixture()?;
    let results = recovery_live_cases(
        &mut api,
        &ports,
        &fixture_h264,
        sink_port,
        hls_put_port,
        timeout,
    )
    .await?;

    let history_contract = verify_live_history_contract(&api, &["egress.failed"]).await?;
    println!("[recovery] history contract verified");

    stop_child(&mut child).await;

    let all_passed = results.iter().all(|r| r["passed"] == true);
    let result = json!({
        "mode": "recovery",
        "passed": all_passed,
        "tests": results,
        "historyContract": history_contract,
    });

    let result_path = work_dir.join("recovery.json");
    std::fs::write(&result_path, serde_json::to_string_pretty(&result).unwrap())
        .map_err(|e| e.to_string())?;
    println!("artifact={}", result_path.display());

    if !all_passed {
        return Err("recovery: not all tests passed".to_string());
    }
    Ok(result)
}

async fn fault_resilience() -> Result<Value, String> {
    let work_dir = artifact_path("fault-resilience");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = default_restream_bin();
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let hls_put_port: u16 = env_u16("HLS_PUT_PORT", 8990);
    let ports = TestPorts::from_env();
    let timeout = Duration::from_secs(15);

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let fixture_h264 = checked_h264_fixture()?;

    let mut results: Vec<Value> = Vec::new();

    // ── 1. RTMP publisher disconnect ────────────────────────────────────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-rtmp", "streamKey": "fault-rtmp"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-rtmp", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;
        println!("[fault] RTMP publisher live");

        stop_child(&mut pub_child).await;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, timeout).await;
        let elapsed = started.elapsed();
        let off_health = api.get_json("/api/v1/engine/health").await.ok();
        let off_input = off_health
            .as_ref()
            .map(|health| health["pipelines"][&pid]["input"].clone())
            .unwrap_or(Value::Null);
        let disconnect_fields_ok = off_input["lastSessionProtocol"] == "rtmp"
            && off_input["lastDisconnectAt"].is_string()
            && off_input["lastDisconnectReason"] == "publisher disconnected"
            && off_input["lastFailurePhase"] == "disconnect"
            && off_input["recentDisconnectError"] == false;
        let passed = off_result.is_ok() && disconnect_fields_ok;
        println!(
            "[fault] RTMP publisher disconnect: {} ({:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "rtmp-publisher-disconnect",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "error": off_result.err(),
            "disconnectFieldsOk": disconnect_fields_ok,
            "inputSnapshot": off_input,
        }));
    }

    // ── 2. SRT publisher disconnect ─────────────────────────────────────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-srt", "streamKey": "fault-srt"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!(
                "srt://127.0.0.1:{}?streamid=publish:live/fault-srt&pkt_size=1316",
                ports.srt
            ),
            "mpegts",
            true,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;
        println!("[fault] SRT publisher live");

        stop_child(&mut pub_child).await;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, timeout).await;
        let elapsed = started.elapsed();
        let passed = off_result.is_ok();
        println!(
            "[fault] SRT publisher disconnect: {} ({:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "srt-publisher-disconnect",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "error": off_result.err(),
        }));
    }

    results.extend(
        recovery_live_cases(
            &mut api,
            &ports,
            &fixture_h264,
            sink_port,
            hls_put_port,
            timeout,
        )
        .await?,
    );

    // ── 4. File ingest stop ─────────────────────────────────────────────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-file", "streamKey": "fault-file"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let fixture_name = fixture_h264
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let media_dest =
            PathBuf::from(std::env::var("RESTREAM_MEDIA_DIR").unwrap_or_else(|_| "media".into()))
                .join(&fixture_name);
        if !media_dest.exists() {
            std::fs::copy(&fixture_h264, &media_dest).map_err(|e| e.to_string())?;
        }

        api.put_json(
            &format!("/api/v1/pipelines/{pid}/file-ingest"),
            json!({"filename": fixture_name, "loop": false}),
        )
        .await?;

        let ingest_list = api.get_json("/api/v1/ingests").await?;
        let ingest_id = ingest_list
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|i| i["streamKey"].as_str() == Some("fault-file"))
            })
            .and_then(|i| i["id"].as_str())
            .ok_or("file ingest not found in list")?
            .to_string();

        api.post_json(&format!("/api/v1/ingests/{ingest_id}/start"), json!({}))
            .await?;
        wait_for_api_input_live(&api, &pid, Duration::from_secs(30)).await?;
        println!("[fault] File ingest live");

        api.post_json(&format!("/api/v1/ingests/{ingest_id}/stop"), json!({}))
            .await?;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, timeout).await;
        let elapsed = started.elapsed();
        let passed = off_result.is_ok();
        println!(
            "[fault] File ingest stop: {} ({:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "file-ingest-stop",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "error": off_result.err(),
        }));
    }

    // ── 4. Recording stops and surfaces inactive after ingest disappears ──
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-recording", "streamKey": "fault-recording"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-recording", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;

        api.post_json(
            &format!("/api/v1/pipelines/{pid}/recording/start"),
            json!({}),
        )
        .await?;

        let active_result =
            wait_for_api_recording_state(&api, &pid, true, Duration::from_secs(10)).await;
        let active_ok = active_result.is_ok();
        tokio::time::sleep(Duration::from_secs(6)).await;

        stop_child(&mut pub_child).await;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, timeout).await;
        let inactive_result =
            wait_for_api_recording_state(&api, &pid, false, Duration::from_secs(10)).await;
        let elapsed = started.elapsed();

        let mut recording_file_found = false;
        if let Ok(entries) = std::fs::read_dir("media") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "ts") {
                    let file_name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    if file_name.contains("fault-recording") {
                        recording_file_found = true;
                        break;
                    }
                }
            }
        }

        let recording_enabled = inactive_result
            .as_ref()
            .ok()
            .and_then(|state| state["enabled"].as_bool())
            .unwrap_or(false);
        let recording_active = inactive_result
            .as_ref()
            .ok()
            .and_then(|state| state["active"].as_bool())
            .unwrap_or(true);
        let passed = active_ok
            && off_result.is_ok()
            && inactive_result.is_ok()
            && recording_enabled
            && !recording_active
            && recording_file_found;
        println!(
            "[fault] Recording follows ingest teardown: {} (enabled={}, active={}, fileFound={}, {:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            recording_enabled,
            recording_active,
            recording_file_found,
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "recording-stops-after-ingest-disconnect",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "inputOffError": off_result.err(),
            "recordingActiveError": active_result.err(),
            "recordingInactiveError": inactive_result.err(),
            "recordingEnabled": recording_enabled,
            "recordingActive": recording_active,
            "recordingFileFound": recording_file_found,
        }));
    }

    // ── 5. External transcoder tears down after ingest disappears ───────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-transcoder", "streamKey": "fault-transcoder"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let sink_bytes = Arc::new(AtomicU64::new(0));
        let sink_listener = TcpListener::bind(format!("127.0.0.1:{sink_port}"))
            .await
            .map_err(|e| format!("sink bind: {e}"))?;
        let sink_cancel = CancellationToken::new();
        let sink_bytes_inner = sink_bytes.clone();
        let sink_cancel_inner = sink_cancel.clone();
        let sink_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = sink_listener.accept() => {
                        if let Ok((socket, _)) = result {
                            let bytes = sink_bytes_inner.clone();
                            tokio::spawn(async move {
                                let mut buf = [0u8; 65536];
                                loop {
                                    match socket.readable().await {
                                        Ok(()) => match socket.try_read(&mut buf) {
                                            Ok(0) => break,
                                            Ok(n) => { bytes.fetch_add(n as u64, Ordering::Relaxed); }
                                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                                            Err(_) => break,
                                        },
                                        Err(_) => break,
                                    }
                                }
                            });
                        }
                    }
                    _ = sink_cancel_inner.cancelled() => break,
                }
            }
        });

        let sink_url = format!("rtmp://127.0.0.1:{sink_port}/live/fault-transcoder-sink");
        let output = api
            .post_json(
                &format!("/api/v1/pipelines/{pid}/outputs"),
                json!({"name": "rtmp-720p", "url": sink_url, "encoding": "720p"}),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-transcoder", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;

        api.post_json(
            &format!("/api/v1/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        let restream_pid = child.id().ok_or("restream pid missing")?;
        let warm_deadline = Instant::now() + Duration::from_secs(15);
        let mut ffmpeg_spawned = false;
        let mut peak_ffmpeg_children = 0u64;
        let mut peak_transcoder_buffers = 0u64;
        let mut saw_output_bytes = false;
        while Instant::now() < warm_deadline {
            let ffmpeg = ffmpeg_children_stats(restream_pid)?;
            let telemetry = api.get_json("/api/v1/engine/telemetry").await?;
            let active_transcoder_buffers =
                telemetry["activeTranscoderBuffers"].as_u64().unwrap_or(0);
            peak_ffmpeg_children = peak_ffmpeg_children.max(ffmpeg.count);
            peak_transcoder_buffers = peak_transcoder_buffers.max(active_transcoder_buffers);
            saw_output_bytes |= sink_bytes.load(Ordering::Relaxed) > 0;
            if (ffmpeg.count > 0 || active_transcoder_buffers > 0) && saw_output_bytes {
                ffmpeg_spawned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        stop_child(&mut pub_child).await;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, timeout).await;
        let cleanup_deadline = Instant::now() + Duration::from_secs(15);
        let mut cleanup_ok = false;
        let mut final_ffmpeg_count = u64::MAX;
        let mut final_transcoder_buffers = u64::MAX;
        while Instant::now() < cleanup_deadline {
            let ffmpeg = ffmpeg_children_stats(restream_pid)?;
            let telemetry = api.get_json("/api/v1/engine/telemetry").await?;
            let active_transcoder_buffers = telemetry["activeTranscoderBuffers"]
                .as_u64()
                .unwrap_or(u64::MAX);
            final_ffmpeg_count = ffmpeg.count;
            final_transcoder_buffers = active_transcoder_buffers;
            if ffmpeg.count == 0 && active_transcoder_buffers == 0 {
                cleanup_ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let status = api
            .get_json(&format!("/api/v1/pipelines/{pid}/outputs/{oid}/status"))
            .await;
        let output_cleaned_up = match &status {
            Err(_) => true,
            Ok(json) if json.get("error").is_some() => true,
            Ok(json) => {
                json["endedAt"].is_string()
                    && matches!(json["status"].as_str(), Some("stopped" | "failed"))
            }
        };
        let elapsed = started.elapsed();
        let passed = ffmpeg_spawned && off_result.is_ok() && cleanup_ok && output_cleaned_up;
        println!(
            "[fault] External transcoder tears down: {} (spawned={}, peakFfmpegChildren={}, peakTranscoderBuffers={}, finalFfmpegChildren={}, activeTranscoderBuffers={}, outputCleanedUp={}, {:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            ffmpeg_spawned,
            peak_ffmpeg_children,
            peak_transcoder_buffers,
            final_ffmpeg_count,
            final_transcoder_buffers,
            output_cleaned_up,
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "external-transcoder-stops-after-ingest-disconnect",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "inputOffError": off_result.err(),
            "ffmpegSpawned": ffmpeg_spawned,
            "peakFfmpegChildren": peak_ffmpeg_children,
            "peakTranscoderBuffers": peak_transcoder_buffers,
            "sawOutputBytes": saw_output_bytes,
            "finalFfmpegChildren": final_ffmpeg_count,
            "finalActiveTranscoderBuffers": final_transcoder_buffers,
            "outputCleanedUp": output_cleaned_up,
        }));

        sink_cancel.cancel();
        sink_task.abort();
    }

    // ── 6. RTMP egress sink disappears ──────────────────────────────────
    results.push(
        fault_rtmp_egress_sink_disappear(&api, &ports, &fixture_h264, sink_port, timeout).await?,
    );

    // ── 7. SRT egress sink disappears ───────────────────────────────────
    results.push(fault_srt_egress_sink_disappear(&api, &ports, &fixture_h264, timeout).await?);

    // ── 8. HLS preview tears down after ingest disappears ───────────────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-hls-preview", "streamKey": "fault-hls-preview"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-hls-preview", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;

        let playlist_result =
            wait_for_hls_playlist_ready(&api, &pid, Duration::from_secs(15)).await;
        let (playlist_status, playlist_ok, playlist_error) = match playlist_result {
            Ok((status, body)) => (status, body.contains("#EXTM3U"), None),
            Err(error) => (reqwest::StatusCode::NOT_FOUND, false, Some(error)),
        };
        let active_result =
            wait_for_api_hls_preview_state(&api, &pid, true, Duration::from_secs(10)).await;
        let active_ok = active_result.is_ok();

        stop_child(&mut pub_child).await;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, timeout).await;
        let inactive_result =
            wait_for_api_hls_preview_state(&api, &pid, false, Duration::from_secs(15)).await;
        let elapsed = started.elapsed();

        let shutdown_deadline = Instant::now() + Duration::from_secs(15);
        let mut final_playlist_status = reqwest::StatusCode::OK;
        let mut final_playlist_gone = false;
        while Instant::now() < shutdown_deadline {
            let (status, _) = api
                .get_text_response(&format!("/hls/{pid}/master.m3u8"))
                .await?;
            final_playlist_status = status;
            if status == reqwest::StatusCode::NOT_FOUND {
                final_playlist_gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let passed = playlist_ok
            && active_ok
            && off_result.is_ok()
            && inactive_result.is_ok()
            && final_playlist_gone;
        println!(
            "[fault] HLS preview tears down with ingest: {} (playlistOk={}, finalStatus={}, {:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            playlist_ok,
            final_playlist_status,
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "hls-preview-stops-after-ingest-disconnect",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "playlistStatus": playlist_status.as_u16(),
            "playlistOk": playlist_ok,
            "playlistError": playlist_error,
            "hlsPreviewActiveError": active_result.err(),
            "inputOffError": off_result.err(),
            "hlsPreviewInactiveError": inactive_result.err(),
            "finalPlaylistStatus": final_playlist_status.as_u16(),
            "finalPlaylistGone": final_playlist_gone,
        }));
    }

    // ── 9. File ingest EOF clears runtime state and allows restart ──────
    {
        let pipeline = api
            .post_json(
                "/api/v1/pipelines",
                json!({"name": "fault-file-eof", "streamKey": "fault-file-eof"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let fixture_name = fixture_h264
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let media_dest =
            PathBuf::from(std::env::var("RESTREAM_MEDIA_DIR").unwrap_or_else(|_| "media".into()))
                .join(&fixture_name);
        if !media_dest.exists() {
            std::fs::copy(&fixture_h264, &media_dest).map_err(|e| e.to_string())?;
        }

        api.put_json(
            &format!("/api/v1/pipelines/{pid}/file-ingest"),
            json!({"filename": fixture_name, "loop": false}),
        )
        .await?;

        let ingest_list = api.get_json("/api/v1/ingests").await?;
        let ingest_id = ingest_list
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|i| i["streamKey"].as_str() == Some("fault-file-eof"))
            })
            .and_then(|i| i["id"].as_str())
            .ok_or("file ingest not found in list")?
            .to_string();

        api.post_json(&format!("/api/v1/ingests/{ingest_id}/start"), json!({}))
            .await?;
        wait_for_api_input_live(&api, &pid, Duration::from_secs(30)).await?;
        let running_result =
            wait_for_pipeline_file_ingest_running_state(&api, &pid, true, Duration::from_secs(10))
                .await;
        let started = Instant::now();
        let off_result = wait_for_api_input_off(&api, &pid, Duration::from_secs(60)).await;
        let stopped_result =
            wait_for_pipeline_file_ingest_running_state(&api, &pid, false, Duration::from_secs(10))
                .await;

        let restart_result = if off_result.is_ok() && stopped_result.is_ok() {
            let restart = api
                .post_json(&format!("/api/v1/ingests/{ingest_id}/start"), json!({}))
                .await;
            match restart {
                Ok(_) => {
                    if let Err(error) =
                        wait_for_api_input_live(&api, &pid, Duration::from_secs(30)).await
                    {
                        Err(error)
                    } else {
                        api.post_json(&format!("/api/v1/ingests/{ingest_id}/stop"), json!({}))
                            .await
                            .map(|_| ())
                    }
                }
                Err(error) => Err(error),
            }
        } else {
            Err("skipped restart because EOF cleanup did not complete".to_string())
        };
        let elapsed = started.elapsed();

        let passed = running_result.is_ok()
            && off_result.is_ok()
            && stopped_result.is_ok()
            && restart_result.is_ok();
        println!(
            "[fault] File ingest EOF clears runtime state: {} ({:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "file-ingest-eof-clears-and-restarts",
            "passed": passed,
            "elapsedMs": elapsed.as_millis(),
            "runningError": running_result.err(),
            "inputOffError": off_result.err(),
            "stoppedError": stopped_result.err(),
            "restartError": restart_result.err(),
        }));
    }

    let history_contract = verify_live_history_contract(&api, &["egress.failed"]).await?;
    println!("[fault-resilience] history contract verified");

    stop_child(&mut child).await;

    let all_passed = results.iter().all(|r| r["passed"] == true);
    let result = json!({
        "mode": "fault-resilience",
        "passed": all_passed,
        "tests": results,
        "historyContract": history_contract,
    });

    let result_path = work_dir.join("fault-resilience.json");
    std::fs::write(&result_path, serde_json::to_string_pretty(&result).unwrap())
        .map_err(|e| e.to_string())?;
    println!("artifact={}", result_path.display());

    if !all_passed {
        return Err("fault-resilience: not all tests passed".to_string());
    }
    Ok(result)
}

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn suite_mode_is_parallelizable(mode: &str, preflight_only: bool) -> bool {
    !preflight_only && !measurement_mode_requires_bench_profile(mode)
}

struct SuiteModeOutcome {
    index: usize,
    mode: String,
    mode_dir: PathBuf,
    started_at: String,
    finished_at: String,
    exit_ok: bool,
}

async fn suite_run_mode(
    exe: PathBuf,
    mode: String,
    mode_dir: PathBuf,
    command: String,
    has_unshare: bool,
    use_host_net: bool,
    index: usize,
) -> Result<SuiteModeOutcome, String> {
    let started_at = Utc::now().to_rfc3339();
    let spawn_mode_dir = mode_dir.clone();
    let exit_ok = tokio::task::spawn_blocking(move || {
        suite_spawn_mode(&exe, &command, &spawn_mode_dir, has_unshare, use_host_net)
    })
    .await
    .map_err(|e| format!("suite worker join failed for {mode}: {e}"))??;
    let finished_at = Utc::now().to_rfc3339();
    Ok(SuiteModeOutcome {
        index,
        mode,
        mode_dir,
        started_at,
        finished_at,
        exit_ok,
    })
}

async fn suite_run_parallel_batch(
    exe: &Path,
    modes: &[String],
    work_root: &Path,
    preflight_only: bool,
    has_unshare: bool,
    use_host_net: bool,
) -> Result<Vec<SuiteModeOutcome>, String> {
    let mut join_set = tokio::task::JoinSet::new();
    for (offset, mode) in modes.iter().enumerate() {
        let mode_dir = work_root.join(mode);
        std::fs::create_dir_all(&mode_dir).map_err(|e| e.to_string())?;
        let command = if preflight_only {
            "preflight".to_string()
        } else {
            mode.clone()
        };
        println!(
            "[suite] {} {mode}",
            if preflight_only { "preflight" } else { "run" }
        );
        join_set.spawn(suite_run_mode(
            exe.to_path_buf(),
            mode.clone(),
            mode_dir,
            command,
            has_unshare,
            use_host_net,
            offset,
        ));
    }

    let mut outcomes: Vec<Option<SuiteModeOutcome>> = (0..modes.len()).map(|_| None).collect();
    while let Some(result) = join_set.join_next().await {
        let outcome = result.map_err(|e| format!("suite batch join failed: {e}"))??;
        let index = outcome.index;
        outcomes[index] = Some(outcome);
    }

    outcomes
        .into_iter()
        .map(|outcome| outcome.ok_or("suite batch produced an empty result slot".to_string()))
        .collect()
}

async fn suite_run() -> Result<Value, String> {
    let raw: Vec<String> = std::env::args().skip(2).collect();
    let mut modes: Vec<String> = SUITE_DEFAULT_MODES.iter().map(|s| s.to_string()).collect();
    let mut continue_on_fail = false;
    let mut preflight_only = false;
    let mut use_host_net = std::env::var("TEST_HARNESS_USE_HOST_NET")
        .ok()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    let mut run_id = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut work_root: Option<PathBuf> = std::env::var_os("WORK_ROOT").map(PathBuf::from);

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--only-modes" => {
                i += 1;
                modes = raw
                    .get(i)
                    .ok_or("--only-modes requires a value")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--run-id" => {
                i += 1;
                run_id = raw.get(i).ok_or("--run-id requires a value")?.clone();
            }
            "--work-root" => {
                i += 1;
                work_root = Some(PathBuf::from(
                    raw.get(i).ok_or("--work-root requires a value")?,
                ));
            }
            "--no-netns" => use_host_net = true,
            "--continue-on-fail" => continue_on_fail = true,
            "--preflight-only" => preflight_only = true,
            other => return Err(format!("unknown suite option: {other}")),
        }
        i += 1;
    }

    if modes.is_empty() {
        return Err("--only-modes produced an empty mode list".to_string());
    }

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let work_root = {
        let r = work_root.unwrap_or_else(|| cwd.join("test/artifacts").join(&run_id));
        if r.is_absolute() { r } else { cwd.join(r) }
    };
    std::fs::create_dir_all(&work_root).map_err(|e| e.to_string())?;

    let results_jsonl = work_root.join("results.jsonl");
    let manifest_path = work_root.join("manifest.json");
    std::fs::File::create(&results_jsonl).map_err(|e| e.to_string())?;

    let started_at = Utc::now().to_rfc3339();
    suite_write_manifest(
        &manifest_path,
        "RUNNING",
        &started_at,
        None,
        &run_id,
        &modes,
        &work_root,
        &results_jsonl,
    )?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let has_unshare = !use_host_net && netns_available();
    let mut overall_ok = true;

    let mut index = 0usize;
    while index < modes.len() {
        if suite_mode_is_parallelizable(&modes[index], preflight_only) && has_unshare {
            let batch_end = modes[index..]
                .iter()
                .take_while(|mode| suite_mode_is_parallelizable(mode, preflight_only))
                .count()
                + index;
            let outcomes = suite_run_parallel_batch(
                &exe,
                &modes[index..batch_end],
                &work_root,
                preflight_only,
                has_unshare,
                use_host_net,
            )
            .await?;
            for outcome in outcomes {
                let mode_status = if outcome.exit_ok { "PASS" } else { "FAIL" };
                if !outcome.exit_ok {
                    overall_ok = false;
                }
                suite_append_result(
                    &results_jsonl,
                    &outcome.mode,
                    mode_status,
                    &outcome.started_at,
                    &outcome.finished_at,
                    &outcome.mode_dir,
                )?;
                println!("[suite] {}: {mode_status}", outcome.mode);
            }
            index = batch_end;
        } else {
            let mode = &modes[index];
            let mode_dir = work_root.join(mode);
            std::fs::create_dir_all(&mode_dir).map_err(|e| e.to_string())?;
            let mode_started = Utc::now().to_rfc3339();

            let command = if preflight_only {
                "preflight"
            } else {
                mode.as_str()
            };
            println!(
                "[suite] {} {mode}",
                if preflight_only { "preflight" } else { "run" }
            );

            let exit_ok = suite_spawn_mode(&exe, command, &mode_dir, has_unshare, use_host_net)?;
            let mode_status = if exit_ok { "PASS" } else { "FAIL" };
            if !exit_ok {
                overall_ok = false;
            }

            let mode_finished = Utc::now().to_rfc3339();
            suite_append_result(
                &results_jsonl,
                mode,
                mode_status,
                &mode_started,
                &mode_finished,
                &mode_dir,
            )?;
            println!("[suite] {mode}: {mode_status}");
            index += 1;
        }

        if !overall_ok && !continue_on_fail {
            break;
        }
    }

    let finished_at = Utc::now().to_rfc3339();
    let final_status = if overall_ok { "PASS" } else { "FAIL" };
    suite_write_manifest(
        &manifest_path,
        final_status,
        &started_at,
        Some(&finished_at),
        &run_id,
        &modes,
        &work_root,
        &results_jsonl,
    )?;
    println!("[suite] manifest={}", manifest_path.display());

    if overall_ok {
        Ok(json!({ "status": "PASS", "manifest": manifest_path }))
    } else {
        Err("suite failed".to_string())
    }
}

fn suite_spawn_mode(
    exe: &Path,
    command: &str,
    mode_dir: &Path,
    has_unshare: bool,
    use_host_net: bool,
) -> Result<bool, String> {
    let log_path = mode_dir.join("run.log");
    let log_file = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let log_copy = log_file.try_clone().map_err(|e| e.to_string())?;

    let status = if has_unshare {
        std::process::Command::new("unshare")
            .args(["--net", "--user", "--map-root-user"])
            .arg(exe)
            .arg(command)
            .env("WORK_DIR", mode_dir)
            .env("RESTREAM_HARNESS_IN_NETNS", "1")
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_copy))
            .status()
            .map_err(|e| format!("failed to spawn {command}: {e}"))?
    } else {
        let mut child = std::process::Command::new(exe);
        child
            .arg(command)
            .env("WORK_DIR", mode_dir)
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_copy));
        if use_host_net {
            child.env("TEST_HARNESS_USE_HOST_NET", "1");
        }
        child
            .status()
            .map_err(|e| format!("failed to spawn {command}: {e}"))?
    };
    Ok(status.success())
}

fn suite_write_manifest(
    path: &Path,
    status: &str,
    started_at: &str,
    finished_at: Option<&str>,
    run_id: &str,
    modes: &[String],
    work_root: &Path,
    results_jsonl: &Path,
) -> Result<(), String> {
    let manifest = json!({
        "kind": "suite",
        "status": status,
        "runId": run_id,
        "startedAt": started_at,
        "finishedAt": finished_at,
        "workRoot": work_root,
        "modes": modes,
        "resultsJsonl": results_jsonl,
    });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn suite_append_result(
    path: &Path,
    mode: &str,
    status: &str,
    started_at: &str,
    finished_at: &str,
    mode_dir: &Path,
) -> Result<(), String> {
    let line = json!({
        "mode": mode,
        "status": status,
        "startedAt": started_at,
        "finishedAt": finished_at,
        "workDir": mode_dir,
        "log": mode_dir.join("run.log"),
    });
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&line).map_err(|e| e.to_string())?
    )
    .map_err(|e| e.to_string())
}

// ── Preflight check ───────────────────────────────────────────────────────────
//
// `preflight` validates the local environment before a suite run:
// binary exists and is executable, required tools are in PATH, and the
// artifact directory has enough free space.  Outputs one JSON object per check.

async fn preflight_check() -> Result<Value, String> {
    let restream_bin = default_restream_bin();
    let harness_bin = std::env::current_exe().map_err(|e| e.to_string())?;

    let binary_check = if std::fs::metadata(&restream_bin)
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o111 != 0
        })
        .unwrap_or(false)
    {
        json!({ "check": "binary", "path": restream_bin.display().to_string(), "status": "ok" })
    } else {
        json!({
            "check": "binary",
            "path": restream_bin.display().to_string(),
            "status": "fail",
            "hint": "build restream in target/debug or target/release, or set RESTREAM_BIN"
        })
    };

    let required_tools = ["ffmpeg", "ffprobe", "mediamtx", "curl"];
    let missing: Vec<&str> = required_tools
        .iter()
        .copied()
        .filter(|tool| {
            std::process::Command::new("which")
                .arg(tool)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| !s.success())
                .unwrap_or(true)
        })
        .collect();
    let deps_check = if missing.is_empty() {
        json!({ "check": "deps", "missing": [], "status": "ok" })
    } else {
        json!({ "check": "deps", "missing": missing, "status": "fail" })
    };

    let artifact_root = PathBuf::from("test/artifacts");
    let min_free_mb: u64 = std::env::var("RESTREAM_ARTIFACT_MIN_FREE_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);
    let disk_check = match nix::sys::statvfs::statvfs(&artifact_root) {
        Ok(stat) => {
            let free_mb = stat.block_size() as u64 * stat.blocks_available() / 1_048_576;
            if free_mb >= min_free_mb {
                json!({ "check": "artifact-disk", "freeMb": free_mb, "minFreeMb": min_free_mb, "status": "ok" })
            } else {
                json!({ "check": "artifact-disk", "freeMb": free_mb, "minFreeMb": min_free_mb, "status": "fail",
                         "hint": "prune test/artifacts or lower RESTREAM_ARTIFACT_MIN_FREE_MB" })
            }
        }
        Err(_) => {
            json!({ "check": "artifact-disk", "status": "skip", "hint": "could not stat artifact directory" })
        }
    };

    let profile_check = if is_bench_profile(&harness_bin) && is_bench_profile(&restream_bin) {
        json!({
            "check": "profile",
            "harness": harness_bin.display().to_string(),
            "restream": restream_bin.display().to_string(),
            "required": "bench",
            "status": "ok"
        })
    } else {
        json!({
            "check": "profile",
            "harness": harness_bin.display().to_string(),
            "restream": restream_bin.display().to_string(),
            "required": "bench",
            "status": "fail",
            "hint": "measurement modes require bench-profile binaries; run `scripts/build-bench-harness.sh` and use `target/bench/test_harness`"
        })
    };

    let all_ok = binary_check["status"] == "ok"
        && deps_check["status"] == "ok"
        && disk_check["status"] != "fail"
        && profile_check["status"] == "ok";

    let result = json!({
        "checks": [binary_check, deps_check, disk_check, profile_check],
        "status": if all_ok { "ok" } else { "fail" },
    });

    if all_ok {
        Ok(result)
    } else {
        Err(format!(
            "preflight failed: {}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_work_db_path_stays_under_work_dir() {
        let work_dir = Path::new("test/artifacts/example");
        assert_eq!(
            default_work_db_path(work_dir, "suite.db"),
            work_dir.join("suite.db")
        );
    }

    #[test]
    fn harness_source_does_not_use_repo_root_data_db_fallback() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/bin/test_harness.rs"
        ));
        assert!(
            !source.contains("PathBuf::from(\"data.db\")"),
            "harness modes must keep mutable DB state under WORK_DIR"
        );
    }

    #[test]
    fn strip_netns_opt_removes_only_the_opt_out_flag() {
        let raw = vec![
            "bitrate-sweep".to_string(),
            "--no-netns".to_string(),
            "--work-root".to_string(),
            "test/artifacts/example".to_string(),
        ];
        assert_eq!(
            strip_netns_opt(&raw),
            vec![
                "bitrate-sweep".to_string(),
                "--work-root".to_string(),
                "test/artifacts/example".to_string(),
            ]
        );
    }

    #[test]
    fn only_non_measurement_modes_parallelize_in_suite() {
        assert!(suite_mode_is_parallelizable("correctness-hevc-srt", false));
        assert!(suite_mode_is_parallelizable("fault-egress-retry", false));
        assert!(suite_mode_is_parallelizable("fault-resilience", false));
        assert!(suite_mode_is_parallelizable("recovery", false));
        assert!(!suite_mode_is_parallelizable("bitrate-sweep", false));
        assert!(!suite_mode_is_parallelizable("preflight", true));
    }

    #[test]
    fn synthesized_harness_ports_are_high_and_distinct() {
        let mut reserved = HashSet::new();
        let http = env_or_allocated_port("RESTREAM_HTTP", 3030, &mut reserved);
        let rtmp = env_or_allocated_port("RESTREAM_RTMP", 1935, &mut reserved);
        let srt = env_or_allocated_port("RESTREAM_SRT", 10080, &mut reserved);
        let mtx_api = env_or_allocated_port("MTX_API", 9997, &mut reserved);
        let unique: HashSet<u16> = [http, rtmp, srt, mtx_api].into_iter().collect();

        assert_eq!(unique.len(), 4);
        assert!(unique.iter().all(|port| *port >= 20_000));
    }

    #[test]
    fn parse_log_fields_handles_json_string_payloads() {
        let log = json!({
            "fields": r#"{"correlation_id":"out-0001","phase":"connect"}"#
        });

        let fields = parse_log_fields(&log).expect("parsed fields");
        assert_eq!(fields["correlation_id"], "out-0001");
        assert_eq!(fields["phase"], "connect");
    }

    #[test]
    fn log_has_correlation_id_detects_both_field_spellings() {
        let snake = json!({
            "fields": r#"{"correlation_id":"out-0001"}"#
        });
        let camel = json!({
            "fields": r#"{"correlationId":"stage-0002"}"#
        });
        let none = json!({
            "fields": r#"{"phase":"connect"}"#
        });

        assert!(log_has_correlation_id(&snake));
        assert!(log_has_correlation_id(&camel));
        assert!(!log_has_correlation_id(&none));
    }
}
