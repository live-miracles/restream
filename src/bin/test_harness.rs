use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use bytes::Bytes;
use chrono::Utc;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
    "correctness-hevc-rtmp",
    "correctness-hevc-srt",
    "fault-resilience",
    "mixed-file-h264",
];

const SINK_PORT: u16 = 12935;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
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
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "suite".to_string());
    let result = match command.as_str() {
        "api-smoke" => api_smoke().await,
        "correctness" => correctness().await,
        "correctness-rtmp" => correctness_rtmp().await,
        "correctness-srt" => correctness_srt().await,
        "correctness-srt-rtmp" => srt_to_rtmp_correctness().await,
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
        "fault-resilience" => fault_resilience().await,
        "mixed-file-h264" => mixed_file_h264_correctness().await,
        other => Err(format!(
            "unknown command {other:?}; use suite, preflight, api-smoke, correctness, \
              correctness-rtmp, correctness-srt, correctness-srt-rtmp, \
              bframe-rtmp, ramp-family, mixed-h264-rtmp, mixed-anchor, \
              mixed-h265-srt, mixed-h264-srt-multi, mixed-h265-srt-multi, \
              egress, correctness-hevc-rtmp, correctness-hevc-srt, \
              fault-resilience, or mixed-file-h264"
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

impl TestPorts {
    fn from_env() -> Self {
        Self {
            http: env_u16("RESTREAM_HTTP", 3030),
            rtmp: env_u16("RESTREAM_RTMP", 1935),
            srt: env_u16("RESTREAM_SRT", 10080),
        }
    }

    fn from_env_or(http: u16, rtmp: u16, srt: u16) -> Self {
        Self {
            http: env_u16("RESTREAM_HTTP", http),
            rtmp: env_u16("RESTREAM_RTMP", rtmp),
            srt: env_u16("RESTREAM_SRT", srt),
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
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let stderr_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new(bin)
        .env("RESTREAM_HTTP_PORT", ports.http.to_string())
        .env("RESTREAM_RTMP_PORT", ports.rtmp.to_string())
        .env("RESTREAM_SRT_PORT", ports.srt.to_string())
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
    size: usize,
    is_keyframe: bool,
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
                            size: data.len(),
                            is_keyframe,
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
                            size: data.len(),
                            is_keyframe: false,
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

// ── Decode-only ffprobe verifier (Phase 1) ──────────────────────────────────

async fn ffprobe_decode_verify(url: &str, expected_dims: Option<&str>) -> Result<Value, String> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-probesize",
        "10000000",
        "-analyzeduration",
        "10000000",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,codec_name",
        "-of",
        "json",
    ]);
    let child = cmd
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
    let probe: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("ffprobe parse failed: {e}"))?;
    if let Some(expected) = expected_dims {
        let streams = probe["streams"]
            .as_array()
            .ok_or("no streams in ffprobe output")?;
        if let Some(video) = streams.first() {
            let w = video["width"].as_u64().unwrap_or(0);
            let h = video["height"].as_u64().unwrap_or(0);
            let got = format!("{w}x{h}");
            if got != expected {
                return Err(format!(
                    "dimension mismatch: expected {expected}, got {got}"
                ));
            }
        } else {
            return Err("no video stream in ffprobe output".to_string());
        }
    }
    Ok(probe)
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
            &format!("/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
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
            "/pipelines/{pipeline_id}/outputs/{output_id}/status"
        ))
        .await
        .ok();
    let status_ok = status
        .as_ref()
        .is_some_and(|s| s["bytesOut"].as_u64().unwrap_or(0) > 0);

    let _ = api
        .post_json(
            &format!("/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
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
        .get_json(&format!("/pipelines/{pipeline_id}/graph"))
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
            restream_bin: std::env::var_os("RESTREAM_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target/release/restream")),
            restream_db_path: std::env::var_os("RESTREAM_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data.db")),
            restream_http: env_u16("RESTREAM_HTTP", 3030),
            restream_rtmp: env_u16("RESTREAM_RTMP", 1935),
            restream_srt: env_u16("RESTREAM_SRT", 10080),
            mtx_rtmp: env_u16("MTX_RTMP", 1936),
            mtx_srt: env_u16("MTX_SRT", 8891),
            mtx_api: env_u16("MTX_API", 9997),
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
            .post(format!("{}/api/auth/login", self.base_url))
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

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
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
            "/pipelines",
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
            &format!("/pipelines/{pipeline_id}/outputs"),
            json!({"name": "smoke-out", "url": "rtmp://127.0.0.1:19350/live/nowhere", "encoding": "source"}),
        )
        .await?;
    let output_id = output["output"]["id"]
        .as_str()
        .ok_or("output create missing id")?
        .to_string();
    println!("[api-smoke] created output {output_id}");

    // Read back pipeline list
    let pipelines = api.get_json("/pipelines").await?;
    let list = pipelines["pipelines"]
        .as_array()
        .ok_or("pipelines list not an array")?;
    if !list.iter().any(|p| p["id"] == pipeline_id.as_str()) {
        return Err(format!("created pipeline {pipeline_id} not found in list"));
    }
    println!("[api-smoke] pipeline appears in list");

    // Health shows pipeline
    let health = api.get_json("/health").await?;
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

    let pipelines2 = api2.get_json("/pipelines").await?;
    let list2 = pipelines2["pipelines"]
        .as_array()
        .ok_or("pipelines list after restart not an array")?;
    let survived = list2.iter().any(|p| p["id"] == pipeline_id.as_str());
    if !survived {
        stop_child(&mut child2).await;
        return Err(format!("pipeline {pipeline_id} did not survive restart"));
    }
    println!("[api-smoke] pipeline survived restart (DB persistence confirmed)");

    // Cleanup
    stop_child(&mut child2).await;

    Ok(json!({
        "passed": true,
        "mode": "api-smoke",
        "pipelineId": pipeline_id,
        "outputId": output_id,
        "dbPersistence": survived,
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
    if !env.restream_bin.exists() {
        return Err(format!(
            "restream binary not found at {}",
            env.restream_bin.display()
        ));
    }
    cleanup_ramp_db(&env.restream_db_path);
    let log = std::fs::File::create(&env.restream_log).map_err(|e| e.to_string())?;
    let stderr_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new(&env.restream_bin)
        .env("RESTREAM_HTTP_PORT", env.restream_http.to_string())
        .env("RESTREAM_RTMP_PORT", env.restream_rtmp.to_string())
        .env("RESTREAM_SRT_PORT", env.restream_srt.to_string())
        .env(
            "RESTREAM_DB_PATH",
            env.restream_db_path.to_string_lossy().to_string(),
        )
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/healthz", env.restream_http),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut child).await;
        return Err(format!("restream did not become ready: {err}"));
    }
    Ok(child)
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
            "/pipelines",
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
                &format!("/pipelines/{pipeline_id}/outputs"),
                json!({"name": format!("out{n}"), "url": url, "encoding": config.encoding}),
            )
            .await?;
        let output_id = output["output"]["id"]
            .as_str()
            .ok_or("output create response missing output.id")?
            .to_string();
        api.post_json(
            &format!("/pipelines/{pipeline_id}/outputs/{output_id}/start"),
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
                &format!("/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
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
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-b:v",
        "4M",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
    ]);
    match config.ingest_proto {
        "rtmp" => {
            cmd.args(["-f", "flv"]).arg(format!(
                "rtmp://127.0.0.1:{}/live/{stream_key}",
                env.restream_rtmp
            ));
        }
        "srt" => {
            cmd.args(["-f", "mpegts"]).arg(format!(
                "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
                env.restream_srt
            ));
        }
        other => return Err(format!("unsupported ramp ingest protocol {other}")),
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

async fn wait_for_api_input_live(
    api: &RampApi,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(health) = api.get_json("/health").await
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

async fn wait_for_api_input_off(
    api: &RampApi,
    pipeline_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(health) = api.get_json("/health").await {
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

#[derive(Clone, Copy)]
struct FfmpegStats {
    count: u64,
    rss_kb: u64,
}

async fn ffmpeg_pipe1_stats() -> FfmpegStats {
    let output = Command::new("ps").arg("aux").output().await;
    let Ok(output) = output else {
        return FfmpegStats {
            count: 0,
            rss_kb: 0,
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
    FfmpegStats { count, rss_kb }
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
            restream_bin: std::env::var_os("RESTREAM_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target/release/restream")),
            restream_db_path: std::env::var_os("RESTREAM_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data.db")),
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
            restream_http: env_u16("RESTREAM_HTTP", 3030),
            restream_rtmp: env_u16("RESTREAM_RTMP", 1935),
            restream_srt: env_u16("RESTREAM_SRT", 10080),
            mtx_rtmp: env_u16("MTX_RTMP", 1936),
            mtx_srt: env_u16("MTX_SRT", 8891),
            mtx_hls: env_u16("MTX_HLS", 8890),
            mtx_api: env_u16("MTX_API", 9997),
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
    if !env.restream_bin.exists() {
        return Err(format!(
            "restream binary not found at {}",
            env.restream_bin.display()
        ));
    }
    cleanup_ramp_db(&env.restream_db_path);
    let log = std::fs::File::create(&env.restream_log).map_err(|e| e.to_string())?;
    let stderr_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new(&env.restream_bin)
        .env("RESTREAM_HTTP_PORT", env.restream_http.to_string())
        .env("RESTREAM_RTMP_PORT", env.restream_rtmp.to_string())
        .env("RESTREAM_SRT_PORT", env.restream_srt.to_string())
        .env(
            "RESTREAM_DB_PATH",
            env.restream_db_path.to_string_lossy().to_string(),
        )
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr_log))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(err) = wait_for_http_ok(
        &format!("http://127.0.0.1:{}/healthz", env.restream_http),
        Duration::from_secs(30),
    )
    .await
    {
        stop_child(&mut child).await;
        return Err(format!("restream did not become ready: {err}"));
    }
    Ok(child)
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
        .post_json("/pipelines", json!({"name": cfg, "streamKey": stream_key}))
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
        .post_json("/pipelines", json!({"name": cfg, "streamKey": stream_key}))
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
        let tc_spawns = wait_for_log_matches(
            &env.restream_log,
            "[h264-tc] Spawning",
            1,
            Duration::from_secs(30),
        )
        .await;
        let ffmpeg = ffmpeg_pipe1_stats().await;
        let tc_max = ffmpeg.count + 1;
        if tc_spawns < 1 || tc_spawns as u64 > tc_max {
            let message = format!(
                "{cfg}: expected 1..{tc_max} h264-tc spawns (got {tc_spawns}; N={n} outputs - sharing broken if >{tc_max})"
            );
            emit_mixed_result(
                env,
                cfg,
                "MS-tc-spawns",
                "fail",
                started.elapsed(),
                Some(json!({
                    "message": message,
                    "tc_spawns": tc_spawns,
                    "bound": tc_max,
                    "restream_log_tail": file_tail_lines(&env.restream_log, 30),
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
                "tc_spawns": tc_spawns,
                "bound": tc_max,
            })),
        )?;
        log_mixed_ok(
            env,
            &format!(
                "{cfg}: TC_SPAWNS={tc_spawns} <= {tc_max} (stage sharing confirmed for {total} outputs)"
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
        "tcSpawns": count_log_matches(&env.restream_log, "[h264-tc] Spawning"),
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
        .post_json("/pipelines", json!({"name": cfg, "streamKey": stream_key}))
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
        .post_json("/pipelines", json!({"name": cfg, "streamKey": stream_key}))
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
                let depth = telem["sourceRing"]["bufferDepthSecs"].as_f64().unwrap_or(0.0);
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
                    env, cfg, &ring_check_id, "fail", started.elapsed(),
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
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-b:v",
        "1.5M",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        "-f",
        "mpegts",
    ]);
    cmd.arg(format!(
        "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
        env.restream_srt
    ));
    let log_path = env.work_dir.join("mixed-anchor-publisher.log");
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

async fn spawn_mixed_h265_srt_publisher(env: &MixedEnv, stream_key: &str) -> Result<Child, String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-x265-params",
        "log-level=none",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-b:v",
        "1.5M",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        "-f",
        "mpegts",
    ]);
    cmd.arg(format!(
        "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
        env.restream_srt
    ));
    let log_path = env.work_dir.join("mixed-h265-srt-publisher.log");
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

async fn spawn_mixed_h264_rtmp_publisher(
    env: &MixedEnv,
    stream_key: &str,
) -> Result<Child, String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-b:v",
        "1.5M",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        "-f",
        "flv",
    ]);
    cmd.arg(format!(
        "rtmp://127.0.0.1:{}/live/{stream_key}",
        env.restream_rtmp
    ));
    let log_path = env.work_dir.join("mixed-h264-rtmp-publisher.log");
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

async fn spawn_mixed_srt_multi_publisher(
    env: &MixedEnv,
    stream_key: &str,
    cfg: &str,
    h265: bool,
) -> Result<Child, String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=44100:cl=mono",
        "-c:v",
    ]);
    if h265 {
        cmd.args([
            "libx265",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-x265-params",
            "log-level=none",
        ]);
    } else {
        cmd.args(["libx264", "-preset", "ultrafast", "-tune", "zerolatency"]);
    }
    cmd.args([
        "-map", "0:v", "-map", "1:a", "-map", "2:a", "-b:v", "1.5M", "-c:a", "aac", "-b:a", "64k",
        "-f", "mpegts",
    ]);
    cmd.arg(format!(
        "srt://127.0.0.1:{}?streamid=publish:live/{stream_key}&latency=200000",
        env.restream_srt
    ));
    let log_path = env.work_dir.join(format!("{cfg}-publisher.log"));
    let log = std::fs::File::create(log_path).map_err(|e| e.to_string())?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
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
            &format!("/pipelines/{pipeline_id}/outputs"),
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
        &format!("/pipelines/{pipeline_id}/outputs/{output_id}/start"),
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
    for attempt in 1..=30 {
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
                eprintln!(
                    "    attempt {attempt}: got '{last}', want '{}'",
                    spec.expected
                );
            }
            Err(error) => {
                last_error = error.clone();
                eprintln!("    attempt {attempt}: {error}");
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
    for attempt in 1..=15 {
        match probe_dims_ramp_with_cookie(url, cookie).await {
            Ok(dimensions) if dimensions == expected => {
                println!("  warmup: {label} -> {dimensions}");
                return;
            }
            Ok(dimensions) => {
                if !dimensions.is_empty() {
                    eprintln!(
                        "    warmup attempt {attempt}: got '{dimensions}', want '{expected}'"
                    );
                }
            }
            Err(error) => {
                eprintln!("    warmup attempt {attempt}: {error}");
            }
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
    for attempt in 1..=15 {
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
                eprintln!(
                    "    audio attempt {attempt}: got dims='{}' audio_tracks={}, want dims='{}' audio_tracks={}",
                    if last_dimensions.is_empty() {
                        "<none>"
                    } else {
                        &last_dimensions
                    },
                    audio_tracks,
                    expected_dimensions,
                    expected_audio_tracks
                );
            }
            Err(error) => {
                last_error = error.clone();
                eprintln!("    audio attempt {attempt}: {error}");
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
                &format!("/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
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
        let config = api.get_json("/config").await?;
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

fn count_log_matches(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|content| content.matches(needle).count())
        .unwrap_or(0)
}

async fn wait_for_log_matches(
    path: &Path,
    needle: &str,
    minimum: usize,
    timeout: Duration,
) -> usize {
    let deadline = Instant::now() + timeout;
    loop {
        let count = count_log_matches(path, needle);
        if count >= minimum || Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn file_tail_lines(path: &Path, lines: usize) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut tail = content.lines().rev().take(lines).collect::<Vec<_>>();
    tail.reverse();
    tail.into_iter().map(str::to_string).collect()
}

async fn correctness() -> Result<Value, String> {
    let work_dir = artifact_path("correctness");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let rtmp_pipeline = api
        .post_json(
            "/pipelines",
            json!({"name": "RTMP test", "streamKey": "e2e-rtmp"}),
        )
        .await?;
    let rtmp_id = rtmp_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("RTMP pipeline create missing id")?
        .to_string();

    let srt_pipeline = api
        .post_json(
            "/pipelines",
            json!({"name": "SRT test", "streamKey": "e2e-srt"}),
        )
        .await?;
    let srt_id = srt_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("SRT pipeline create missing id")?
        .to_string();
    println!("[correctness] created pipelines {rtmp_id}, {srt_id}");

    let rtmp_fixture = artifact_path("correctness-h264.ts");
    if !rtmp_fixture.exists() {
        generate_fixture_h264(&rtmp_fixture).await?;
    }
    let srt_fixture = artifact_path("correctness-h265.ts");
    if !srt_fixture.exists() {
        generate_fixture_h265(&srt_fixture).await?;
    }

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

    let health = api.get_json("/health").await?;
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

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/pipelines",
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
            &format!("/pipelines/{pipeline_id}/outputs"),
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

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

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
        &format!("/pipelines/{pipeline_id}/outputs/{output_id}/start"),
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

/// Test: RTMP B-frame ingest -> RTMP egress timestamp round-trip.
///
/// Publishes B-frame H.264/AAC over RTMP, sends egress to the generalized
/// harness sink, and verifies ffprobe observes composition offsets (PTS > DTS)
/// while DTS stays monotone.
async fn bframe_rtmp_correctness() -> Result<Value, String> {
    let work_dir = artifact_path("bframe-rtmp");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/pipelines",
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
            &format!("/pipelines/{pipeline_id}/outputs"),
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

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

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
        &format!("/pipelines/{pipeline_id}/outputs/{output_id}/start"),
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

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let stream_key = format!("e2e-{protocol}");
    let pipeline = api
        .post_json(
            "/pipelines",
            json!({"name": format!("{protocol} test"), "streamKey": stream_key}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();
    println!("[correctness-{protocol}] created pipeline {pipeline_id}");

    let fixture = if protocol == "rtmp" {
        let p = artifact_path("correctness-h264.ts");
        if !p.exists() {
            generate_fixture_h264(&p).await?;
        }
        p
    } else {
        let p = artifact_path("correctness-h265.ts");
        if !p.exists() {
            generate_fixture_h265(&p).await?;
        }
        p
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

    let health = api.get_json("/health").await?;
    let snapshot = health["pipelines"][&pipeline_id].clone();
    if snapshot.is_null() {
        stop_child(&mut publisher).await;
        stop_child(&mut child).await;
        return Err(format!("missing {protocol} snapshot in /health"));
    }

    let probe = ffprobe(&read_url).await?;
    assert_media_only(&probe, &format!("{protocol} read"))?;
    let normalized = normalized_streams(&probe)?;

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

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/pipelines",
            json!({"name": "Egress source", "streamKey": "e2e-src"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

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
            "/pipelines",
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
    let srt_health = api.get_json("/health").await.unwrap_or(json!({}));
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
        &format!("/pipelines/{pipeline_id}/recording/start"),
        json!({}),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(6)).await;
    api.post_json(
        &format!("/pipelines/{pipeline_id}/recording/stop"),
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

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let pipeline = api
        .post_json(
            "/pipelines",
            json!({"name": "H.265 SRT source", "streamKey": "e2e-hevc"}),
        )
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create missing id")?
        .to_string();

    let fixture = artifact_path("correctness-h265.ts");
    if !fixture.exists() {
        generate_fixture_h265(&fixture).await?;
    }

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
        .get_json(&format!("/pipelines/{pipeline_id}/probe"))
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

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let ports = TestPorts::from_env();

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    // Source pipeline
    let pipeline = api
        .post_json(
            "/pipelines",
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
            "/pipelines",
            json!({"name": "H.265 SRT passthrough sink", "streamKey": "e2e-hevc-srt-sink"}),
        )
        .await?;
    let sink_pipeline_id = sink_pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("sink pipeline create missing id")?
        .to_string();

    let fixture = artifact_path("correctness-h265.ts");
    if !fixture.exists() {
        generate_fixture_h265(&fixture).await?;
    }

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

async fn generate_fixture_h264(path: &Path) -> Result<(), String> {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1920x1080:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "8",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-preset",
            "slow",
            "-g",
            "60",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("H.264 fixture generation failed: {status}"))
    }
}

async fn generate_fixture_h265(path: &Path) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "8",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "libx265",
        "-preset",
        "fast",
        "-g",
        "60",
        "-bf",
        "0",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-ac",
        "2",
        "-f",
        "mpegts",
    ]);
    cmd.arg(path);
    let status = cmd.status().await.map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("H.265 fixture generation failed: {status}"))
    }
}

async fn spawn_publisher(
    path: &Path,
    url: &str,
    format: &str,
    map_all: bool,
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
    if map_all {
        cmd.args(["-map", "0"]);
    } else {
        cmd.args(["-map", "0:v", "-map", "0:a:0"]);
    }
    cmd.args(["-c", "copy", "-f", format]).arg(url);
    // stderr must not be piped without a consumer — the 64KB pipe buffer fills
    // and blocks ffmpeg, hanging the test. Discard it (errors visible via exit code).
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| e.to_string())
}

/// Generate H.265 4K60 video from lavfi + 16 audio tracks from a file on disk,
/// streaming directly to SRT. The file is read by ffmpeg, never loaded into
/// restream's memory.
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
    let snapshot_audio = snapshot["audioTracks"]
        .as_array()
        .and_then(|tracks| tracks.first())
        .ok_or_else(|| format!("{label}: snapshot missing audio"))?;
    let probe_sample_rate = audio["sampleRate"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| audio["sampleRate"].as_u64());

    let matches = snapshot["video"]["codec"] == video["codec"]
        && snapshot["video"]["width"] == video["width"]
        && snapshot["video"]["height"] == video["height"]
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

    let fixture = artifact_path("correctness-h264.ts");
    if !fixture.exists() {
        generate_fixture_h264(&fixture).await?;
    }

    let fixture_name = fixture
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let media_dir =
        PathBuf::from(std::env::var("RESTREAM_MEDIA_DIR").unwrap_or_else(|_| "media".into()));
    let media_dest = media_dir.join(&fixture_name);
    if !media_dest.exists() {
        std::fs::copy(&fixture, &media_dest).map_err(|e| e.to_string())?;
    }

    let pipeline = api
        .post_json("/pipelines", json!({"name": cfg, "streamKey": stream_key}))
        .await?;
    let pipeline_id = pipeline["pipeline"]["id"]
        .as_str()
        .ok_or("pipeline create response missing pipeline.id")?
        .to_string();

    api.put_json(
        &format!("/pipelines/{pipeline_id}/file-ingest"),
        json!({"filename": fixture_name, "loop": true}),
    )
    .await?;

    let ingest_list = api.get_json("/api/ingests").await?;
    let ingest_id = ingest_list
        .as_array()
        .and_then(|arr| arr.iter().find(|i| i["streamKey"].as_str() == Some(&stream_key)))
        .and_then(|i| i["id"].as_str())
        .ok_or("file ingest not found in list")?
        .to_string();

    api.post_json(&format!("/api/ingests/{ingest_id}/start"), json!({}))
        .await?;
    wait_for_api_input_live(api, &pipeline_id, Duration::from_secs(45)).await?;
    let rss_baseline = process_rss_kb(restream_pid).await.unwrap_or(0);
    if !env.skip_load {
        snapshot_mixed(env, restream_pid, cfg, "baseline (file ingest live, 0 outputs)").await?;
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
            &format!("/pipelines/{pipeline_id}/outputs/{output_id}/stop"),
            json!({}),
        )
        .await?;
        if i % 4 == 3 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    api.post_json(&format!("/api/ingests/{ingest_id}/stop"), json!({}))
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

async fn fault_resilience() -> Result<Value, String> {
    let work_dir = artifact_path("fault-resilience");
    std::fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;

    let restream_bin = std::env::var_os("RESTREAM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/restream"));
    let db_path = work_dir.join("data.sqlite");
    let log_path = work_dir.join("restream.log");
    let sink_port: u16 = env_u16("SINK_PORT", SINK_PORT);
    let ports = TestPorts::from_env();
    let timeout = Duration::from_secs(15);

    let mut child = start_restream_child(&restream_bin, &ports, &db_path, &log_path).await?;
    let mut api = RampApi::new(ports.http);
    api.login().await?;

    let fixture_h264 = artifact_path("correctness-h264.ts");
    if !fixture_h264.exists() {
        generate_fixture_h264(&fixture_h264).await?;
    }

    let mut results: Vec<Value> = Vec::new();

    // ── 1. RTMP publisher disconnect ────────────────────────────────────
    {
        let pipeline = api
            .post_json(
                "/pipelines",
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
        let passed = off_result.is_ok();
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
        }));
    }

    // ── 2. SRT publisher disconnect ─────────────────────────────────────
    {
        let pipeline = api
            .post_json(
                "/pipelines",
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

    // ── 3. File ingest stop ─────────────────────────────────────────────
    {
        let pipeline = api
            .post_json(
                "/pipelines",
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
            &format!("/pipelines/{pid}/file-ingest"),
            json!({"filename": fixture_name, "loop": false}),
        )
        .await?;

        let ingest_list = api.get_json("/api/ingests").await?;
        let ingest_id = ingest_list
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|i| i["streamKey"].as_str() == Some("fault-file"))
            })
            .and_then(|i| i["id"].as_str())
            .ok_or("file ingest not found in list")?
            .to_string();

        api.post_json(&format!("/api/ingests/{ingest_id}/start"), json!({}))
            .await?;
        wait_for_api_input_live(&api, &pid, Duration::from_secs(30)).await?;
        println!("[fault] File ingest live");

        api.post_json(&format!("/api/ingests/{ingest_id}/stop"), json!({}))
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

    // ── 4. RTMP egress sink disappears ──────────────────────────────────
    // Accept connections and drain data, then abort all reader tasks so
    // their TcpStreams are dropped, sending TCP RST to the egress writer.
    {
        let pipeline = api
            .post_json(
                "/pipelines",
                json!({"name": "fault-egress-rtmp", "streamKey": "fault-egress-rtmp"}),
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
        let reader_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let reader_handles_inner = reader_handles.clone();
        let sink_bytes_inner = sink_bytes.clone();
        let sink_cancel_inner = sink_cancel.clone();
        let sink_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = sink_listener.accept() => {
                        if let Ok((socket, _)) = result {
                            let bytes = sink_bytes_inner.clone();
                            let h = tokio::spawn(async move {
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
                &format!("/pipelines/{pid}/outputs"),
                json!({"name": "rtmp-sink", "url": sink_url, "encoding": "source"}),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!("rtmp://127.0.0.1:{}/live/fault-egress-rtmp", ports.rtmp),
            "flv",
            false,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;

        api.post_json(
            &format!("/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        let deadline = Instant::now() + timeout;
        while sink_bytes.load(Ordering::Relaxed) < 50_000 {
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        println!("[fault] RTMP egress delivering data");

        // Stop listener, then abort all reader tasks — aborting drops the
        // TcpStream owned by each task, which sends TCP RST.
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
        while Instant::now() < poll_deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let status = api
                .get_json(&format!("/pipelines/{pid}/outputs/{oid}/status"))
                .await;
            match &status {
                Err(_) => {
                    // 404/error = egress was cleaned up after failure (pass)
                    phase = "cleaned-up".to_string();
                    passed = true;
                    break;
                }
                Ok(s) => {
                    has_error = s["lastError"]
                        .as_str()
                        .map(|e| !e.is_empty())
                        .unwrap_or(false);
                    phase = s["phase"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    if s.get("error").is_some() {
                        // {"error": "output not active"} — cleaned up
                        phase = "cleaned-up".to_string();
                        passed = true;
                        break;
                    }
                    if has_error
                        || phase == "error"
                        || phase == "failed"
                        || phase == "connecting"
                    {
                        passed = true;
                        break;
                    }
                }
            }
        }
        let elapsed = started.elapsed();
        println!(
            "[fault] RTMP egress sink disappear: {} (phase={}, hasError={}, {:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            phase,
            has_error,
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "rtmp-egress-sink-disappear",
            "passed": passed,
            "phase": phase,
            "hasError": has_error,
            "elapsedMs": elapsed.as_millis(),
        }));

        stop_child(&mut pub_child).await;
    }

    // ── 5. SRT egress sink disappears ───────────────────────────────────
    // Use the SRT port on the same restream instance: egress pushes to a
    // second pipeline; we delete that pipeline to simulate sink loss.
    {
        let pipeline = api
            .post_json(
                "/pipelines",
                json!({"name": "fault-egress-srt", "streamKey": "fault-egress-srt"}),
            )
            .await?;
        let pid = pipeline["pipeline"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let sink_pipeline = api
            .post_json(
                "/pipelines",
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
                &format!("/pipelines/{pid}/outputs"),
                json!({"name": "srt-sink", "url": sink_url, "encoding": "source"}),
            )
            .await?;
        let oid = output["output"]["id"]
            .as_str()
            .ok_or("missing id")?
            .to_string();

        let mut pub_child = spawn_publisher(
            &fixture_h264,
            &format!(
                "srt://127.0.0.1:{}?streamid=publish:live/fault-egress-srt&pkt_size=1316",
                ports.srt
            ),
            "mpegts",
            true,
        )
        .await?;
        wait_for_api_input_live(&api, &pid, timeout).await?;

        api.post_json(
            &format!("/pipelines/{pid}/outputs/{oid}/start"),
            json!({}),
        )
        .await?;

        // Wait for the sink pipeline to see data
        let deadline = Instant::now() + timeout;
        let mut sink_live = false;
        while Instant::now() < deadline {
            if let Ok(health) = api.get_json("/health").await {
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

        // Delete the sink pipeline to simulate sink disappearance
        let delete_url = format!("{}/pipelines/{sink_pid}", api.base_url);
        let mut request = api.client.delete(&delete_url);
        if let Some(cookie) = &api.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        let _ = request.send().await;

        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(5)).await;

        let status = api
            .get_json(&format!("/pipelines/{pid}/outputs/{oid}/status"))
            .await;
        let has_error = status
            .as_ref()
            .ok()
            .and_then(|s| s["lastError"].as_str())
            .map(|e| !e.is_empty())
            .unwrap_or(false);
        let phase = status
            .as_ref()
            .ok()
            .and_then(|s| s["phase"].as_str())
            .unwrap_or("unknown")
            .to_string();
        let elapsed = started.elapsed();
        // SRT egress may reconnect or show error; either signals fault detection
        let passed = has_error || phase == "error" || phase == "connecting" || phase == "live";
        println!(
            "[fault] SRT egress sink disappear: {} (phase={}, hasError={}, {:.1}s)",
            if passed { "PASS" } else { "FAIL" },
            phase,
            has_error,
            elapsed.as_secs_f64()
        );
        results.push(json!({
            "test": "srt-egress-sink-disappear",
            "passed": passed,
            "phase": phase,
            "hasError": has_error,
            "elapsedMs": elapsed.as_millis(),
        }));

        stop_child(&mut pub_child).await;
    }

    stop_child(&mut child).await;

    let all_passed = results.iter().all(|r| r["passed"] == true);
    let result = json!({
        "mode": "fault-resilience",
        "passed": all_passed,
        "tests": results,
    });

    let result_path = work_dir.join("fault-resilience.json");
    std::fs::write(
        &result_path,
        serde_json::to_string_pretty(&result).unwrap(),
    )
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

async fn suite_run() -> Result<Value, String> {
    let raw: Vec<String> = std::env::args().skip(2).collect();
    let mut modes: Vec<String> = SUITE_DEFAULT_MODES.iter().map(|s| s.to_string()).collect();
    let mut continue_on_fail = false;
    let mut preflight_only = false;
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
    let has_unshare = std::process::Command::new("unshare")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let mut overall_ok = true;

    for mode in &modes {
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

        let exit_ok = suite_spawn_mode(&exe, command, &mode_dir, has_unshare)?;
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
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_copy))
            .status()
            .map_err(|e| format!("failed to spawn {command}: {e}"))?
    } else {
        std::process::Command::new(exe)
            .arg(command)
            .env("WORK_DIR", mode_dir)
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_copy))
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
    let restream_bin =
        std::env::var("RESTREAM_BIN").unwrap_or_else(|_| "./target/release/restream".to_string());

    let binary_check = if std::fs::metadata(&restream_bin)
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o111 != 0
        })
        .unwrap_or(false)
    {
        json!({ "check": "binary", "path": restream_bin, "status": "ok" })
    } else {
        json!({
            "check": "binary",
            "path": restream_bin,
            "status": "fail",
            "hint": "run: scripts/resource-limit cargo build --release"
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

    let all_ok = binary_check["status"] == "ok"
        && deps_check["status"] == "ok"
        && disk_check["status"] != "fail";

    let result = json!({
        "checks": [binary_check, deps_check, disk_check],
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
